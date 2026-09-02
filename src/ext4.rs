#[cfg(not(target_os = "twizzler"))]
use std::io::Result;
use std::{
    collections::HashMap,
    ffi::CString,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use libc::{mode_t, PATH_MAX};
use lwext4_rs::{
    Ext4Blockdev, Ext4BlockdevIface, Ext4File, Ext4Fs, FileKind, MpLock, O_CREAT, O_RDONLY, O_RDWR,
    O_TRUNC,
};
use pager_dynamic::{ino_to_objid, objid_to_ino, ExternalFile};
#[cfg(target_os = "twizzler")]
use twizzler::Result;

use crate::{
    extents::{push_device_page, ExtentTracker},
    paged_object_store::{Vec, INLINE_LEN},
    DevicePage, ExternalFileStore, ExternalOpenFlags, ObjID, PagedDevice, PagedObjectStore, PosIo,
    ProbeMiss, PAGE_SIZE,
};

/// Whether the `pager` diagnostic class was requested via `TWZ_DIAG` (comma list, or `all`).
/// Local copy of pager-srv's `watchdog::diag_enabled` — this crate is a dependency of pager-srv,
/// not the other way around. Init logs the value at boot.
fn diag_enabled() -> bool {
    static SET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let set = SET.get_or_init(|| std::env::var("TWZ_DIAG").unwrap_or_default());
    set.split(',').any(|c| c == "pager" || c == "all")
}

pub struct Ext4Store<D: Device> {
    fs: Mutex<Ext4Fs>,
    ino_cache: Mutex<HashMap<ObjID, u32>>,
    /// `None` records a known-absent object, so a repeated probe costs a hashmap
    /// lookup instead of a directory walk under the fs lock.
    len_cache: Mutex<HashMap<ObjID, Option<u64>>>,
    /// Cached object-page -> disk-block mappings. A request served from here never takes `fs`.
    extents: ExtentTracker,
    /// Validated equal to 1 at mount, but kept explicit so the page/block arithmetic below says
    /// what it means without re-reading the superblock under the fs lock.
    blocks_per_page: u32,
    device: D,
}

/// Contention and hold-time accounting for the one global `Ext4Store::fs` mutex.
///
/// Exists because the fast-lane reservation is about to stop asking "is this answerable from
/// cache?" and start asking "is this lock busy?" -- and the safety of that trade rests entirely on
/// how long the lock is held. `walk_block_map` drops and re-acquires every `BLOCKS_PER_LOCK`
/// lookups, so the *intended* worst case is ~100 lookups rather than a whole transfer; nothing has
/// ever measured whether that is what actually happens. `held` is the live count the dispatcher
/// reads (a plain load, because it is consulted on the dequeue thread, which nothing may stall).
/// Which call site took the fs lock. Only granular enough to answer the question that prompted
/// it -- whether the hold-time tail comes from the block-map walk or from the one-shot metadata
/// operations -- because a per-site breakdown nobody reads is just overhead.
#[derive(Clone, Copy)]
pub enum FsSite {
    Walk = 0,
    PageOut = 1,
    ReadWrite = 2,
    Meta = 3,
    External = 4,
    ExtReaddir = 5,
    ExtUnlink = 6,
    ExtRename = 7,
}

const FS_SITES: usize = 8;
const FS_SITE_NAMES: [&str; FS_SITES] = [
    "walk",
    "page-out",
    "read/write",
    "meta",
    "ext-open",
    "ext-readdir",
    "ext-unlink",
    "ext-rename",
];

thread_local! {
    /// Site whose critical section this thread is currently inside, so the block-device callbacks
    /// -- which run under the fs lock on the calling thread, and have no argument to carry it --
    /// can attribute their reads. Saved and restored rather than cleared, so a nested acquisition
    /// cannot silently orphan its parent's attribution.
    static CUR_SITE: core::cell::Cell<Option<FsSite>> = const { core::cell::Cell::new(None) };
}

fn note_bd_read() {
    if let Some(site) = CUR_SITE.with(|c| c.get()) {
        FS_LOCK_STATS.site_reads[site as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub struct FsLockStats {
    /// Inode-cache outcome for `get_object_as_file`. A hit is `open_file_from_inode`; a miss is a
    /// full path lookup, i.e. directory traversal and the block reads that implies. Neither
    /// `walk_lookups` nor `page_in` counts this path, so a regression that lives here is
    /// invisible to every existing counter -- which is the situation that motivated it.
    ino_hits: AtomicU64,
    ino_misses: AtomicU64,
    /// Block-device reads issued while this site held the lock. `FLUSHSTATS` already reports the
    /// boot total; this says which call site asked for them, which is the part that identifies a
    /// read-amplification regression rather than merely detecting one.
    site_reads: [AtomicU64; FS_SITES],
    site_holds: [AtomicU64; FS_SITES],
    site_hold_ns: [AtomicU64; FS_SITES],
    site_max_ns: [AtomicU64; FS_SITES],
    held: AtomicUsize,
    acquires: AtomicU64,
    contended: AtomicU64,
    hold_ns: AtomicU64,
    hold_max_ns: AtomicU64,
    wait_ns: AtomicU64,
    wait_max_ns: AtomicU64,
}

pub static FS_LOCK_STATS: FsLockStats = FsLockStats {
    ino_hits: AtomicU64::new(0),
    ino_misses: AtomicU64::new(0),
    site_reads: [const { AtomicU64::new(0) }; FS_SITES],
    site_holds: [const { AtomicU64::new(0) }; FS_SITES],
    site_hold_ns: [const { AtomicU64::new(0) }; FS_SITES],
    site_max_ns: [const { AtomicU64::new(0) }; FS_SITES],
    held: AtomicUsize::new(0),
    acquires: AtomicU64::new(0),
    contended: AtomicU64::new(0),
    hold_ns: AtomicU64::new(0),
    hold_max_ns: AtomicU64::new(0),
    wait_ns: AtomicU64::new(0),
    wait_max_ns: AtomicU64::new(0),
};

impl FsLockStats {
    /// Whether anyone holds the fs lock right now. Advisory by nature -- it can change the
    /// instant after it is read -- which is fine for an admission hint and would not be for a
    /// correctness decision.
    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed) > 0
    }

    fn report(&self) {
        if !diag_enabled() {
            return;
        }
        let n = self.acquires.load(Ordering::Relaxed).max(1);
        tracing::info!(
            "FSLOCK: {} acquires, {} found it held ({}%); hold mean {} us max {} us; wait mean {} us max {} us",
            self.acquires.load(Ordering::Relaxed),
            self.contended.load(Ordering::Relaxed),
            100 * self.contended.load(Ordering::Relaxed) / n,
            self.hold_ns.load(Ordering::Relaxed) / n / 1000,
            self.hold_max_ns.load(Ordering::Relaxed) / 1000,
            self.wait_ns.load(Ordering::Relaxed) / n / 1000,
            self.wait_max_ns.load(Ordering::Relaxed) / 1000,
        );
        let mut per_site = String::new();
        for i in 0..FS_SITES {
            let n = self.site_holds[i].load(Ordering::Relaxed);
            if n == 0 {
                continue;
            }
            per_site.push_str(&format!(
                " {} n={} mean={}us max={}us bd-reads={};",
                FS_SITE_NAMES[i],
                n,
                self.site_hold_ns[i].load(Ordering::Relaxed) / n / 1000,
                self.site_max_ns[i].load(Ordering::Relaxed) / 1000,
                self.site_reads[i].load(Ordering::Relaxed),
            ));
        }
        let (h, m) = (
            self.ino_hits.load(Ordering::Relaxed),
            self.ino_misses.load(Ordering::Relaxed),
        );
        tracing::info!(
            "FSLOCK-SITES:{} ino-cache {} hit / {} miss ({}% miss)",
            per_site,
            h,
            m,
            100 * m / (h + m).max(1),
        );
    }
}

fn bump_max(cell: &AtomicU64, v: u64) {
    let mut cur = cell.load(Ordering::Relaxed);
    while v > cur {
        match cell.compare_exchange_weak(cur, v, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
}

/// A held `Ext4Store::fs`, timed. Derefs to the underlying `MutexGuard` so existing call sites
/// that pass `&mut fs` into `get_object_as_file` keep working unchanged.
pub struct FsGuard<'a> {
    guard: MutexGuard<'a, Ext4Fs>,
    since: Instant,
    site: FsSite,
    prev_site: Option<FsSite>,
}

impl<'a> std::ops::Deref for FsGuard<'a> {
    type Target = MutexGuard<'a, Ext4Fs>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a> std::ops::DerefMut for FsGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for FsGuard<'_> {
    fn drop(&mut self) {
        let held = self.since.elapsed().as_nanos() as u64;
        let i = self.site as usize;
        FS_LOCK_STATS.hold_ns.fetch_add(held, Ordering::Relaxed);
        bump_max(&FS_LOCK_STATS.hold_max_ns, held);
        FS_LOCK_STATS.site_holds[i].fetch_add(1, Ordering::Relaxed);
        FS_LOCK_STATS.site_hold_ns[i].fetch_add(held, Ordering::Relaxed);
        bump_max(&FS_LOCK_STATS.site_max_ns[i], held);
        CUR_SITE.with(|c| c.set(self.prev_site));
        FS_LOCK_STATS.held.fetch_sub(1, Ordering::Relaxed);
    }
}

pub trait Device: PosIo + PagedDevice + Sync + Send + Clone + 'static {}

impl<T: PosIo + PagedDevice + Sync + Send + Clone + 'static> Device for T {}

struct Ext4Bd<D: Device> {
    device: D,
    phys_bcount: u64,
    lock: MpLock,
}

impl<D: Device> Ext4BlockdevIface for Ext4Bd<D> {
    fn phys_block_size(&mut self) -> u32 {
        PHYSICAL_BSIZE
    }

    fn phys_block_count(&mut self) -> u64 {
        self.phys_bcount
    }

    fn open(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn read(&mut self, buf: *mut u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        BD_STATS.reads.fetch_add(1, Ordering::Relaxed);
        note_bd_read();
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        let slice = unsafe { core::slice::from_raw_parts_mut(buf, len as usize) };
        let len = self.device.read(start, slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn write(&mut self, buf: *const u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        let start = block * PHYSICAL_BSIZE as u64;
        BD_STATS.writes.fetch_add(1, Ordering::Relaxed);
        BD_STATS
            .write_bytes
            .fetch_add(bcount as u64 * PHYSICAL_BSIZE as u64, Ordering::Relaxed);
        if start < PAGE_SIZE as u64 {
            // The superblock lives at byte 1024, i.e. inside page 0.
            BD_STATS.sb_writes.fetch_add(1, Ordering::Relaxed);
        }
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        let slice = unsafe { core::slice::from_raw_parts(buf, len as usize) };
        let len = self.device.write(start, slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn lock(&self) -> std::io::Result<()> {
        self.lock.lock();
        Ok(())
    }

    fn unlock(&self) -> std::io::Result<()> {
        self.lock.unlock();
        Ok(())
    }
}

impl<D: Device> Ext4Bd<D> {
    fn new(device: D, _name: &str, phys_bcount: u64) -> Self {
        Self {
            device,
            phys_bcount,
            lock: MpLock::new(),
        }
    }
}

static BDEV_ID: AtomicU64 = AtomicU64::new(0);

const LOGICAL_BSIZE: u32 = 512;
const PHYSICAL_BSIZE: u32 = 512;

/// Block lookups performed per acquisition of the fs lock, between yields.
///
/// **Measured inert (2026-08-28).** Setting this to 16 changed nothing: the walk took exactly the
/// same 105 acquisitions as at 100, because a walk covers ~63 pages per lookup and so finishes a
/// request in well under either limit. The chunk bound never binds.
///
/// That matters because it retires an obvious-looking fix. The walk's hold-time tail is 4-12 ms
/// (`FSLOCK-SITES`), and the natural reading -- "100 lookups, each possibly a disk read, so cap
/// the count" -- is wrong: the tail is **one slow lookup**, not an accumulation of many. No value
/// of this constant bounds it. Bounding the walk's hold means not holding `fs` across the
/// block-map read, which is a restructure, not a tuning knob.
const BLOCKS_PER_LOCK: usize = 100;

/// Extra blocks a read-side walk asks about beyond the request.
///
/// `ext4_extent_get_blocks` returns one whole contiguous run per call and stops at `max_blocks`, so
/// asking only for the pages requested means the tracker learns exactly the requested range and
/// nothing more -- and a forward-sequential reader, which asks for the *next* range every time, can
/// then never hit the cache. Measured: 127 lookups covered 66 625 pages, so a wider ask rides the
/// same tree walk and costs nothing.
///
/// Read side only. Widening the ask under `create` would allocate blocks nobody asked for.
const EXTENT_READAHEAD_BLOCKS: u32 = 4096;

/// Pages a read-side walk continues *past* the request, to learn extents it will not emit.
///
/// [`EXTENT_READAHEAD_BLOCKS`] widens the block-map *ask*, but the walk loop still stops as soon as
/// it has covered the request, so the only thing ever learned beyond it is the overhang of the last
/// run -- roughly half an extent. That is enough to make the cache serve a *prefix* of the next
/// request (measured: 90% of pages hit) and never enough to serve one whole (measured: 1 call in
/// 128 avoided the fs lock, and 0 of 128 could be admitted to a fast lane). A demand fault asks
/// about pages nobody has walked yet by definition, so a prefix hit is exactly the case that does
/// not help it.
///
/// 512 pages is one large-page region -- the granularity `ensure_in_core_pager` widens a fault to,
/// so it is the unit the *next* fault will ask about. Clamped to EOF, so a walk never learns holes
/// past the end of a file it might later grow into.
///
/// **Measured at 512 and reverted to 0.** It is nearly free -- ext4 returns long contiguous runs,
/// so 71,514 pages were learned ahead in *fewer* block-map lookups than before (55 vs 57) -- and
/// it bought nothing that matters: `probe_partial` 64 -> 63, fast-lane admissions 0 -> 0,
/// fs-lock-free calls 1 -> 1. Learning 71k pages of extents moved request-level coverage by one
/// request.
///
/// The suspected reason, unmeasured: `MAX_EXTENTS_PER_OBJECT` is 64 and the map *clears itself*
/// on overflow, so a wider walk records more runs per call, trips the limit, and wipes the cache
/// it was filling. If that is right the lever is the map's capacity and eviction policy, not the
/// walk width -- and a counter on the clear path settles it. Until then this stays 0 rather than
/// carrying speculative work whose benefit is measured and whose interaction is not.
const EXTENT_WALK_AHEAD_PAGES: u64 = 0;

/// Block index backing object page `page`. External files have no null page, so their object
/// pages sit one block lower than an internal object's. Both paging paths and the page-out
/// file-extension check go through here so they cannot disagree about the mapping.
fn page_to_block(id: ObjID, page: u64) -> u64 {
    if objid_to_ino(id).is_some() && page > 0 {
        page - 1
    } else {
        page
    }
}

/// Cumulative split of paging time between walking the ext4 block map (under the fs lock) and
/// the actual device transfer, so the cost of block-map lookup is visible without per-call
/// tracing.
struct PageStats {
    collect_ns: AtomicU64,
    io_ns: AtomicU64,
    calls: AtomicU64,
}

impl PageStats {
    const fn new() -> Self {
        Self {
            collect_ns: AtomicU64::new(0),
            io_ns: AtomicU64::new(0),
            calls: AtomicU64::new(0),
        }
    }

    // Lookup counts moved to EXTENTSTATS, which is where the walk now happens.
    fn record(&self, what: &str, collect: Duration, io: Duration) {
        self.collect_ns
            .fetch_add(collect.as_nanos() as u64, Ordering::Relaxed);
        self.io_ns
            .fetch_add(io.as_nanos() as u64, Ordering::Relaxed);
        let calls = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        // Power-of-two cadence: always reports regardless of total call count, while staying
        // sparse under load.
        if calls.is_power_of_two() && diag_enabled() {
            tracing::info!(
                "PAGESTATS {}: {} calls, collect {}ms, io {}ms",
                what,
                calls,
                self.collect_ns.load(Ordering::Relaxed) / 1_000_000,
                self.io_ns.load(Ordering::Relaxed) / 1_000_000,
            );
        }
    }
}

static PAGE_IN_STATS: PageStats = PageStats::new();
static PAGE_OUT_STATS: PageStats = PageStats::new();

/// Whether the extent tracker is actually keeping the block-map walk off the fs lock. Split by
/// `create`, because page-out serving from cache is the case the tracker exists for: a repeated
/// sync of the same dirty object re-walked every time before it.
struct ExtentStats {
    hit_pages: AtomicU64,
    walk_pages: AtomicU64,
    /// Pages answered by the past-EOF shortcut. Counted apart from `walk_pages` because they take
    /// no fs lock *and* are deliberately never cached, so folding them into misses makes the hit
    /// rate look far worse than the cache is actually doing.
    eof_pages: AtomicU64,
    create_hit_pages: AtomicU64,
    create_walk_pages: AtomicU64,
    walk_lookups: AtomicU64,
    /// Entries into `walk_block_map`, as opposed to `FSLOCK-SITES walk`, which counts the
    /// per-`BLOCKS_PER_LOCK` chunk re-acquisitions inside it. Separated because the two moved
    /// together under a change that touched neither -- identical lookups and pages walked, but
    /// 67 acquisitions became 23 -- and one number cannot say whether the walk is entered less
    /// often or merely chunks less often.
    walk_calls: AtomicU64,
    /// `readdir_external`: calls, dirents the iterator was advanced over (each one an inode read
    /// inside `DirIter::next`), entries actually handed back, and the sum of `skip` requested.
    /// Separated because the skip is applied *after* the inode-reading filter, so a paginated
    /// enumeration re-reads every skipped entry -- and only these four numbers say whether that
    /// costs a little or dominates.
    rd_calls: AtomicU64,
    rd_iterated: AtomicU64,
    rd_returned: AtomicU64,
    rd_skip: AtomicU64,
    /// Pages walked *past* the request purely to populate the cache, and the calls that did any.
    ///
    /// Split out because the plain `walk_pages` count is computed against the request's `end` and
    /// therefore cannot see this work at all -- so widening the walk made the walk counter go
    /// *down* (the later hits it bought are visible; its own cost is not). A speed-up whose cost
    /// is invisible to the meter reporting it is not a measurement.
    ahead_pages: AtomicU64,
    ahead_calls: AtomicU64,
    calls: AtomicU64,
    /// Calls that found the object already carrying valid extents. Distinguishes "never revisited"
    /// from "revisited at a fresh range".
    warm_object_calls: AtomicU64,
    /// Concurrency actually achieved: threads inside get_disk_blocks at once, and the subset of
    /// those served wholly from cache -- i.e. holding only a per-object read lock, never `fs`.
    inflight: AtomicU64,
    max_inflight: AtomicU64,
    cached_inflight: AtomicU64,
    max_cached_inflight: AtomicU64,
}

impl ExtentStats {
    const fn new() -> Self {
        Self {
            hit_pages: AtomicU64::new(0),
            walk_pages: AtomicU64::new(0),
            eof_pages: AtomicU64::new(0),
            create_hit_pages: AtomicU64::new(0),
            create_walk_pages: AtomicU64::new(0),
            walk_lookups: AtomicU64::new(0),
            walk_calls: AtomicU64::new(0),
            rd_calls: AtomicU64::new(0),
            rd_iterated: AtomicU64::new(0),
            rd_returned: AtomicU64::new(0),
            rd_skip: AtomicU64::new(0),
            ahead_pages: AtomicU64::new(0),
            ahead_calls: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            warm_object_calls: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            max_inflight: AtomicU64::new(0),
            cached_inflight: AtomicU64::new(0),
            max_cached_inflight: AtomicU64::new(0),
        }
    }

    /// Bump a gauge and keep its high-water mark. Racy by construction -- two threads can both
    /// read a stale max -- which only ever under-reports concurrency, so it cannot manufacture the
    /// result this is measuring.
    fn enter(gauge: &AtomicU64, max: &AtomicU64) -> u64 {
        let now = gauge.fetch_add(1, Ordering::AcqRel) + 1;
        max.fetch_max(now, Ordering::AcqRel);
        now
    }

    fn record(&self, create: bool, hit: u32, walked: u32, past_eof: u32, warm: bool) {
        if warm {
            self.warm_object_calls.fetch_add(1, Ordering::Relaxed);
        }
        let (hits, walks) = if create {
            (&self.create_hit_pages, &self.create_walk_pages)
        } else {
            (&self.hit_pages, &self.walk_pages)
        };
        hits.fetch_add(hit as u64, Ordering::Relaxed);
        walks.fetch_add(walked as u64, Ordering::Relaxed);
        self.eof_pages.fetch_add(past_eof as u64, Ordering::Relaxed);
        let calls = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if calls.is_power_of_two() && diag_enabled() {
            tracing::info!(
                "EXTENTSTATS: {} calls ({} warm-object), {} lookups; read pages {} hit / {} walked / {} past-eof / {} walked-ahead in {} calls; create pages {} hit / {} walked; concurrency max {} in flight, max {} fs-lock-free; map overflow-clears {}; walk entered {}; readdir {} calls, {} dirents iterated, {} returned, {} skipped",
                calls,
                self.warm_object_calls.load(Ordering::Relaxed),
                self.walk_lookups.load(Ordering::Relaxed),
                self.hit_pages.load(Ordering::Relaxed),
                self.walk_pages.load(Ordering::Relaxed),
                self.eof_pages.load(Ordering::Relaxed),
                self.ahead_pages.load(Ordering::Relaxed),
                self.ahead_calls.load(Ordering::Relaxed),
                self.create_hit_pages.load(Ordering::Relaxed),
                self.create_walk_pages.load(Ordering::Relaxed),
                self.max_inflight.load(Ordering::Relaxed),
                self.max_cached_inflight.load(Ordering::Relaxed),
                crate::extents::OVERFLOW_CLEARS.load(Ordering::Relaxed),
                self.walk_calls.load(Ordering::Relaxed),
                self.rd_calls.load(Ordering::Relaxed),
                self.rd_iterated.load(Ordering::Relaxed),
                self.rd_returned.load(Ordering::Relaxed),
                self.rd_skip.load(Ordering::Relaxed),
            );
        }
    }
}

static EXTENT_STATS: ExtentStats = ExtentStats::new();

/// Decrements a concurrency gauge on drop.
struct GaugeGuard(&'static AtomicU64);

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ExtentStats {
    fn in_flight(&'static self) -> GaugeGuard {
        Self::enter(&self.inflight, &self.max_inflight);
        GaugeGuard(&self.inflight)
    }

    fn cache_served(&'static self) -> GaugeGuard {
        Self::enter(&self.cached_inflight, &self.max_cached_inflight);
        GaugeGuard(&self.cached_inflight)
    }
}

/// Block-device traffic, to size what `Ext4Fs::flush` actually costs: how much of it is the
/// unconditional superblock rewrite (a partial-page write, so a read-modify-write on the way
/// down) versus walking the dirty list one buffer at a time.
struct BdStats {
    reads: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
    sb_writes: AtomicU64,
    flushes: AtomicU64,
    flush_ns: AtomicU64,
    flush_reads: AtomicU64,
    flush_writes: AtomicU64,
}

impl BdStats {
    const fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            sb_writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            flush_ns: AtomicU64::new(0),
            flush_reads: AtomicU64::new(0),
            flush_writes: AtomicU64::new(0),
        }
    }

    fn record_flush(&self, dur: Duration, reads: u64, writes: u64) {
        self.flush_ns
            .fetch_add(dur.as_nanos() as u64, Ordering::Relaxed);
        self.flush_reads.fetch_add(reads, Ordering::Relaxed);
        self.flush_writes.fetch_add(writes, Ordering::Relaxed);
        let n = self.flushes.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() && diag_enabled() {
            tracing::info!(
                "FLUSHSTATS: {} flushes, {}ms in flush; per flush: {:.1} bd-reads, {:.1} bd-writes;                  {} sb-writes; totals: {} bd-reads, {} bd-writes, {}KB written",
                n,
                self.flush_ns.load(Ordering::Relaxed) / 1_000_000,
                self.flush_reads.load(Ordering::Relaxed) as f64 / n as f64,
                self.flush_writes.load(Ordering::Relaxed) as f64 / n as f64,
                self.sb_writes.load(Ordering::Relaxed),
                self.reads.load(Ordering::Relaxed),
                self.writes.load(Ordering::Relaxed),
                self.write_bytes.load(Ordering::Relaxed) / 1024,
            );
        }
    }
}

static BD_STATS: BdStats = BdStats::new();

/// `Ext4Fs::flush` also rewrites the superblock via `ext4_fs_fini`, so time it as one unit.
/// Log an lwext4 error's raw errno, which is the only place it still exists.
///
/// `lwext4-rs` builds these with `io::Error::from_raw_os_error` on a POSIX errno, but Twizzler's
/// `decode_error_kind` reads that integer as a packed `(category << 16) | code` TwzError. Every
/// POSIX errno is below 65536, so the category is always 0 -- `Uncategorized` -- and the kind is
/// always `Other`: ENOENT, EIO, ENOSPC and EEXIST are one value by the time anything logs them.
/// `raw_os_error` survives; only `kind()` destroys it. Without this, a failed create is
/// indistinguishable from a full disk or a device error, which is why five of them went
/// unattributed.
fn log_ext4_errno(op: &str, id: ObjID, e: std::io::Error) -> std::io::Error {
    tracing::warn!(
        "ext4 {} of {:x} failed: errno {:?}, kind {:?}",
        op,
        id,
        e.raw_os_error(),
        e.kind()
    );
    e
}

/// Restore the old unconditional flush after every page-out. Kept as a switch rather than a
/// deletion so the two can be A/B'd from one build; see the note at the page-out site.
const PAGE_OUT_ALWAYS_FLUSH: bool = false;

fn flush_fs(fs: &mut Ext4Fs) -> Result<()> {
    let r0 = BD_STATS.reads.load(Ordering::Relaxed);
    let w0 = BD_STATS.writes.load(Ordering::Relaxed);
    let start = Instant::now();
    let res = fs.flush();
    let dur = Instant::now() - start;
    BD_STATS.record_flush(
        dur,
        BD_STATS.reads.load(Ordering::Relaxed) - r0,
        BD_STATS.writes.load(Ordering::Relaxed) - w0,
    );
    Ok(res?)
}

impl<D: Device> Ext4Store<D> {
    pub fn new(device: D, name: &str) -> Result<Self> {
        let bdname = format!("blockdev-{}", BDEV_ID.fetch_add(1, Ordering::SeqCst));
        let max = device.len()? as u64;
        let bcount = max / LOGICAL_BSIZE as u64;
        let phys_bcount = max / PHYSICAL_BSIZE as u64;
        let bd = Ext4Blockdev::new(
            Ext4Bd::new(device.clone(), bdname.as_str(), phys_bcount),
            LOGICAL_BSIZE,
            bcount,
            name,
        )?;

        let mut fs = Ext4Fs::new(bd, CString::new(name).unwrap(), false)?;

        match fs.create_dir("ids") {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                return Err(e.into());
            }
            _ => {}
        }

        // The paging paths feed block counts into DevicePage, whose nr_pages() then advances a
        // page cursor -- correct only while one page is one block. Fail at mount rather than
        // silently mis-mapping every request.
        let block_size = fs.block_size()? as usize;
        if block_size != PAGE_SIZE {
            tracing::error!(
                "ext4 block size {} != page size {}; paging assumes one block per page",
                block_size,
                PAGE_SIZE
            );
            return Err(ErrorKind::Unsupported.into());
        }

        Ok(Self {
            fs: Mutex::new(fs),
            device,
            len_cache: Mutex::new(HashMap::default()),
            ino_cache: Mutex::new(HashMap::default()),
            extents: ExtentTracker::new(),
            blocks_per_page: (PAGE_SIZE / block_size) as u32,
        })
    }

    fn get_len_from_cache(&self, id: ObjID) -> Option<Option<u64>> {
        self.len_cache.lock().unwrap().get(&id).copied()
    }

    /// Whether [Self::len] can answer without the fs lock.
    pub fn len_is_cached(&self, id: ObjID) -> bool {
        self.get_len_from_cache(id).is_some()
    }

    /// Whether paging in `[start_page, start_page + nr_pages)` would have to take the fs lock.
    ///
    /// That lock is global and is held across NVMe round trips (`pagerperf.md` 2), so a thread
    /// that takes it can park for a whole disk transfer behind a thread of any other priority.
    /// Callers that must not do that ask first and route the work elsewhere.
    ///
    /// Both inputs are read from caches under their own short locks, and both answer "yes" when
    /// they don't know -- being wrong in that direction only costs the caller its shortcut. The
    /// answer can also go stale between here and the work (an invalidation racing us), so this
    /// bounds how often a caller blocks rather than guaranteeing it never does.
    /// Whether serving this range needs the fs lock, and if so which cache came up short.
    ///
    /// The reason is the point: a bare bool cannot distinguish "this object is unknown to us" from
    /// "we know it, but not this far into it", and those want opposite fixes -- one needs the disk,
    /// the other only needs to be asked about a smaller range.
    pub fn page_in_would_block(&self, id: ObjID, start_page: u64, nr_pages: u32) -> ProbeMiss {
        if !self.len_is_cached(id) {
            return ProbeMiss::Len;
        }
        match self.extents.peek(id) {
            None => ProbeMiss::NoExtents,
            Some(entry) if !entry.covers(start_page, nr_pages) => ProbeMiss::Partial,
            Some(_) => ProbeMiss::Cached,
        }
    }

    fn readlink(&self, id: ObjID) -> Result<String> {
        // By inode rather than through `read_object`: a target short enough to live in the inode's
        // block map is stored there with no data block behind it, so reading one as file data
        // yields a block of unrelated bytes -- which, being zeros, is valid UTF-8 and passes for a
        // 4096-byte target rather than failing outright.
        let ino = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        let buf = self.lock_fs(FsSite::Meta).readlink_from_inode(ino)?;
        if buf.len() > PATH_MAX as usize {
            return Err(ErrorKind::InvalidData.into());
        }
        String::from_utf8(buf).map_err(|_| ErrorKind::InvalidData.into())
    }

    fn invalidate_len(&self, id: ObjID) {
        self.len_cache.lock().unwrap().remove(&id);
    }

    fn set_len_in_cache(&self, id: ObjID, len: u64) {
        self.len_cache.lock().unwrap().insert(id, Some(len));
    }

    fn set_absent_in_cache(&self, id: ObjID) {
        self.len_cache.lock().unwrap().insert(id, None);
    }

    pub fn get_id_path(&self, id: ObjID) -> (String, String) {
        let top = id.to_be_bytes()[0];
        let us = format!("ids/{:x}", top);
        (us, format!("ids/{:x}/{:x}", top, id))
    }

    pub fn set_len(&self, id: ObjID, len: u64) -> Result<()> {
        // ftruncate frees blocks. The guard bumps on every exit path, so a `?` out of a
        // partially-applied truncate cannot leave stale extents behind.
        let _inval = self.extents.invalidate_on_drop(id);
        let mut fs = self.lock_fs(FsSite::Meta);
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        file.truncate(len)?;
        self.set_len_in_cache(id, len);
        Ok(())
    }

    pub fn lookup_ino_cache(&self, id: ObjID) -> Option<u32> {
        self.ino_cache.lock().unwrap().get(&id).copied()
    }

    pub fn insert_ino_cache(&self, id: ObjID, ino: u32) {
        self.ino_cache.lock().unwrap().insert(id, ino);
    }

    pub fn remove_ino_cache(&self, id: ObjID) {
        self.ino_cache.lock().unwrap().remove(&id);
    }

    /// Kind of the entry `name` inside directory inode `dir_ino`, if it is there.
    ///
    /// Read from the parent's dirents rather than by opening the child: opening a directory or a
    /// symlink does not necessarily succeed, and the caller needs the answer precisely for the
    /// cases an open would not give it.
    /// The `(inode, kind)` of `name` inside directory `dir_ino`, from the dirent alone.
    fn child_entry(
        fs: &mut MutexGuard<'_, Ext4Fs>,
        dir_ino: u32,
        name: &str,
    ) -> Option<(u32, FileKind)> {
        let mut inode = fs.get_inode(dir_ino).ok()?;
        let target = name.as_bytes();
        fs.dirents(&mut inode)
            .ok()?
            .find(|(nm, _, _)| nm.as_slice() == target)
            .map(|(_, ino, kind)| (ino, kind))
    }

    /// Remove `name` from directory `at_ino`, with the cache/extent invalidation removal
    /// requires. The caller holds the fs lock; shared by `unlink_external` and the
    /// replace-on-link path in `open_external`.
    ///
    /// Files only. ext4_fremove refuses directories -- but returns the still-EOK `r` when it does,
    /// so a directory reaching it reports success while removing nothing. Callers that mean to
    /// remove a directory dispatch to `rmdir_child_locked` themselves; the refusal stays here
    /// because the replace-on-link path must *not* silently rmdir a rename destination.
    fn unlink_child_locked(
        &self,
        fs: &mut MutexGuard<'_, Ext4Fs>,
        at_ino: u32,
        name: &str,
    ) -> Result<()> {
        if matches!(
            Self::child_entry(fs, at_ino, name),
            Some((_, FileKind::Directory))
        ) {
            return Err(ErrorKind::Unsupported.into());
        }
        // Resolve the inode before unlinking: an external ObjID is derived from it, and both
        // caches are keyed on that ObjID. Leaving them populated is doubly wrong here, because
        // ext4 reuses inode numbers -- a later file can land on the same ObjID and inherit this
        // one's cached length. Best effort: a target we cannot open (a symlink, say) just leaves
        // the caches as they are today.
        let id = fs
            .open_file_from_container(at_ino, name, O_RDONLY, 0)
            .ok()
            .and_then(|mut file| {
                file.get_file_inode()
                    .ok()
                    .map(|ino| ino_to_objid(ino.num()))
            });
        let _inval = id.map(|id| self.extents.invalidate_on_drop(id));
        if let Some(id) = id {
            self.remove_ino_cache(id);
            self.invalidate_len(id);
        } else {
            tracing::debug!(
                "unlink_child_locked: could not resolve inode for {:?}",
                name
            );
        }
        // lwext4 exposes removal only as a path operation (ext4_fremove, which handles the
        // truncate/unlink/free-inode sequence under one transaction), while everything else here
        // is container-relative. Rather than reimplement that sequence against the raw bindings --
        // a lot of unsafe on a path where a mistake corrupts the filesystem -- turn the container
        // back into a path and reuse it.
        let full = Self::path_of(fs, at_ino, name)?;
        fs.remove_file(&full)?;
        Ok(())
    }

    /// Remove directory `name` (inode `dir_ino`) from directory `at_ino`.
    ///
    /// Empty-only, POSIX rmdir semantics: `ext4_dir_rm` deletes the whole subtree, and a caller
    /// that asked to remove one name must not lose everything under it. `remove_dir_all` in libstd
    /// empties the directory itself before getting here, so nothing is lost by refusing.
    fn rmdir_child_locked(
        &self,
        fs: &mut MutexGuard<'_, Ext4Fs>,
        at_ino: u32,
        dir_ino: u32,
        name: &str,
    ) -> Result<()> {
        let empty = {
            let mut inode = fs.get_inode(dir_ino)?;
            fs.dirents(&mut inode)?
                .all(|(nm, _, _)| nm.as_slice() == b"." || nm.as_slice() == b"..")
        };
        if !empty {
            return Err(ErrorKind::DirectoryNotEmpty.into());
        }
        // Same reasoning as the file path below: the external ObjID is derived from the inode
        // number, ext4 reuses inode numbers, and a later file landing on this one would inherit
        // whatever these caches still hold for it.
        if diag_enabled() {
            tracing::info!("rmdir '{}' ino {} from parent ino {}", name, dir_ino, at_ino);
        }
        let id = ino_to_objid(dir_ino);
        let _inval = self.extents.invalidate_on_drop(id);
        self.remove_ino_cache(id);
        self.invalidate_len(id);
        let full = Self::path_of(fs, at_ino, name)?;
        fs.remove_dir(&full)?;
        Ok(())
    }

    /// Mount-relative path of `name` inside directory inode `dir_ino`.
    ///
    /// ext4 keeps no parent pointer in the inode, but a directory always carries a `..` entry and
    /// -- unlike a file -- can have only one link, so walking up and naming each child in its
    /// parent yields the one true path. Read-only: it touches no metadata.
    fn path_of(fs: &mut MutexGuard<'_, Ext4Fs>, dir_ino: u32, name: &str) -> Result<String> {
        const ROOT_INO: u32 = 2;
        // Depth bound rather than a visited set: a corrupted `..` cycle must terminate, and no
        // legitimate tree here is anywhere near this deep.
        const MAX_DEPTH: usize = 64;

        let mut parts: std::vec::Vec<String> = std::vec::Vec::new();
        let mut ino = dir_ino;
        while ino != ROOT_INO {
            if parts.len() >= MAX_DEPTH {
                tracing::warn!("path_of({}, {}): depth limit at ino {}", dir_ino, name, ino);
                return Err(ErrorKind::InvalidInput.into());
            }
            let parent = {
                let mut inode = fs.get_inode(ino)?;
                match fs
                    .dirents(&mut inode)?
                    .find(|(nm, _, _)| nm.as_slice() == b"..")
                    .map(|(_, ino, _)| ino)
                {
                    Some(parent) => parent,
                    None => {
                        tracing::warn!("path_of({}, {}): ino {} has no `..`", dir_ino, name, ino);
                        return Err(ErrorKind::InvalidInput.into());
                    }
                }
            };
            let mut parent_inode = fs.get_inode(parent)?;
            // Bound to a local so the iterator's borrow of `parent_inode` ends here: the failure
            // arm below needs to walk the same directory again.
            let found = fs
                .dirents(&mut parent_inode)?
                .filter(|(nm, _, _)| nm.as_slice() != b"." && nm.as_slice() != b"..")
                .find(|(_, dino, _)| *dino == ino)
                .map(|(nm, _, _)| nm);
            let component = match found {
                Some(component) => component,
                None => {
                    // Dump what the parent does hold: a same-named entry pointing at a different
                    // inode says the name was re-linked and this inode orphaned, which is a very
                    // different bug from the entry simply being absent.
                    let listing: std::vec::Vec<(String, u32)> = fs
                        .dirents(&mut parent_inode)
                        .map(|it| {
                            it.map(|(nm, i, _)| (String::from_utf8_lossy(&nm).into_owned(), i))
                                .collect()
                        })
                        .unwrap_or_default();
                    tracing::warn!(
                        "path_of({}, {}): ino {} not listed in parent {}; resolved so far {:?}; \
                         parent holds {:?}",
                        dir_ino,
                        name,
                        ino,
                        parent,
                        parts,
                        listing
                    );
                    return Err(ErrorKind::InvalidInput.into());
                }
            };
            // ext4 names are arbitrary non-NUL bytes; one that is not UTF-8 cannot be spliced into
            // a path string, and a lossy conversion would name a different file.
            match String::from_utf8(component) {
                Ok(component) => parts.push(component),
                Err(e) => {
                    tracing::warn!(
                        "path_of({}, {}): ino {} has a non-UTF-8 name {:?}",
                        dir_ino,
                        name,
                        ino,
                        e.as_bytes()
                    );
                    return Err(ErrorKind::InvalidInput.into());
                }
            }
            ino = parent;
        }
        parts.reverse();
        parts.push(name.to_string());
        // No leading slash: the mount point is "/" and `remove_file` prepends it, so returning
        // one here would build "//sysroot/x" and resolve against the wrong path.
        Ok(parts.join("/"))
    }

    fn do_get_object_as_file<'a>(
        &self,
        fs: &'a mut MutexGuard<'_, Ext4Fs>,
        id: ObjID,
        create: bool,
    ) -> std::io::Result<Ext4File<'a>> {
        // `create` implies truncate. The caller used to guarantee a clean object by unlinking it
        // first and letting O_CREAT remake it, which cost a second full path lookup -- and for a
        // fresh id that lookup could only ever fail. O_TRUNC gets the same guarantee from the one
        // lookup we were already doing. It also reuses the inode instead of recycling one, which
        // is strictly safer for any cached inode number; `open_external` above takes the same
        // route for the same reason.
        let flags = if create {
            O_RDWR | O_CREAT | O_TRUNC
        } else {
            O_RDWR
        };
        if let Some(ino) = objid_to_ino(id) {
            return fs.open_file_from_inode(ino, flags);
        }
        let path = self.get_id_path(id);
        if create {
            match fs.create_dir(&path.0) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        // Propagate the real error kind. Flattening everything to NotFound hid device errors,
        // and would make them indistinguishable from genuine absence for the negative cache.
        fs.open_file(&path.1, flags)
    }

    /// Walk the ext4 block map for `[page, end)`, appending to `out` and recording what was learned
    /// in `learned` for the caller to commit to the extent tracker.
    ///
    /// Holds the fs lock in `BLOCKS_PER_LOCK` chunks: an `Ext4InodeRef` points into lwext4's block
    /// cache, which is not reentrant, so it must not outlive the lock.
    fn walk_block_map(
        &self,
        id: ObjID,
        mut page: u64,
        end: u64,
        create: bool,
        learned: &mut Vec<(u64, Option<u64>, u32), INLINE_LEN>,
        out: &mut Vec<DevicePage, INLINE_LEN>,
    ) -> Result<()> {
        // How far to walk for *learning*, as opposed to how far to emit. Never past EOF, and never
        // speculative under `create` -- widening an allocation walk would allocate blocks nobody
        // asked for. An unknown length means no speculation at all rather than a guess.
        let learn_end = if create || EXTENT_WALK_AHEAD_PAGES == 0 {
            end
        } else {
            match self.get_len_from_cache(id) {
                Some(Some(len)) => (end + EXTENT_WALK_AHEAD_PAGES)
                    .min(len.div_ceil(PAGE_SIZE as u64))
                    .max(end),
                _ => end,
            }
        };

        if learn_end > end {
            EXTENT_STATS.ahead_calls.fetch_add(1, Ordering::Relaxed);
        }

        EXTENT_STATS.walk_calls.fetch_add(1, Ordering::Relaxed);
        let mut lookups = 0u64;
        let res = 'chunks: loop {
            let mut fs = self.lock_fs(FsSite::Walk);
            let mut file = match self.get_object_as_file(&mut fs, id, false) {
                Ok(file) => file,
                Err(e) => break Err(e.into()),
            };
            let mut inode = match file.get_file_inode() {
                Ok(inode) => inode,
                Err(e) => break Err(e.into()),
            };

            for _ in 0..BLOCKS_PER_LOCK {
                if page >= learn_end {
                    break 'chunks Ok(());
                }

                let block = page_to_block(id, page) as u32 * self.blocks_per_page;
                let rem_blocks = (learn_end - page) as u32 * self.blocks_per_page;
                let ask = if create {
                    (end - page) as u32 * self.blocks_per_page
                } else {
                    rem_blocks.saturating_add(EXTENT_READAHEAD_BLOCKS)
                };

                lookups += 1;
                let (pblock, nr_blocks) = match inode.get_data_blocks(block, ask, create) {
                    Ok((dblock, nr_dblk)) if nr_dblk > 0 => {
                        ((dblock != 0).then_some(dblock), nr_dblk)
                    }
                    _ => match inode.get_data_block(block, create) {
                        Ok(0) => (None, 1),
                        Ok(dpg) => (Some(dpg), 1),
                        Err(e) => {
                            tracing::warn!("failed to get_data_block: {}", e);
                            break 'chunks Err(e.into());
                        }
                    },
                };

                // Allocation was requested, so a hole here means the allocation silently did not
                // happen -- writing to block 0 would corrupt the superblock.
                if create && pblock.is_none() {
                    tracing::warn!(
                        "got unexpected zero block when paging out object {:x} page {}",
                        id,
                        page
                    );
                    break 'chunks Err(ErrorKind::Other.into());
                }

                // Cache the whole run, but hand back only the pages asked for: the readahead
                // exists to make the *next* request a hit, not to widen this transfer.
                learned.push((page, pblock, nr_blocks));
                // Emit only inside the request. Past `end` the walk is still learning -- those
                // runs go into `learned` and nowhere else, or the caller would be handed pages it
                // never asked for and has no physical memory for.
                if page < end {
                    let emit = nr_blocks.min((end - page) as u32 * self.blocks_per_page);
                    push_device_page(
                        out,
                        match pblock {
                            Some(pblock) => DevicePage::Run(pblock, emit),
                            None => DevicePage::Hole(emit),
                        },
                    );
                }
                page += nr_blocks as u64;
            }

            drop(inode);
            drop(file);
            drop(fs);
            self.device.yield_now();
        };
        EXTENT_STATS
            .walk_lookups
            .fetch_add(lookups, Ordering::Relaxed);
        // Only when the readahead policy actually widened the walk. `page` overshoots `end`
        // routinely without any speculation -- `get_data_blocks` returns whole runs, and a run
        // that extends past the request carries the walk past `end` for free. Counting that as
        // walk-ahead attributed the extent mechanism's own overshoot to a policy that had not
        // run, and read as "231234 pages walked-ahead in 0 calls": a cost with no cause.
        if learn_end > end {
            EXTENT_STATS
                .ahead_pages
                .fetch_add(page.saturating_sub(end), Ordering::Relaxed);
        }
        res
    }

    /// Take the fs lock, timed, and count whether it was already held.
    ///
    /// Every acquisition in this file goes through here so `FS_LOCK_STATS` sees all of them; a
    /// direct `self.fs.lock()` would be invisible to the meter and to the `held` count the
    /// dispatcher reads.
    fn lock_fs(&self, site: FsSite) -> FsGuard<'_> {
        let contended = FS_LOCK_STATS.is_held();
        let t0 = Instant::now();
        let guard = self.fs.lock().unwrap();
        let waited = t0.elapsed().as_nanos() as u64;
        FS_LOCK_STATS.acquires.fetch_add(1, Ordering::Relaxed);
        if contended {
            FS_LOCK_STATS.contended.fetch_add(1, Ordering::Relaxed);
        }
        FS_LOCK_STATS.wait_ns.fetch_add(waited, Ordering::Relaxed);
        bump_max(&FS_LOCK_STATS.wait_max_ns, waited);
        FS_LOCK_STATS.held.fetch_add(1, Ordering::Relaxed);
        let n = FS_LOCK_STATS.acquires.load(Ordering::Relaxed);
        // Power-of-two alone truncates the tail by up to 2x, and asymmetrically between arms: a
        // run ending at 500 acquisitions last reported at 256, while one ending at 520 reported
        // at 512, so an A/B reads as "halved" when nothing changed. Hence a fixed cadence past
        // 256 as well.
        //
        // The cadence was 32, which is inside the window `PAGESTATS page_in` times: `report()`
        // runs under `lock_fs`, `lock_fs` runs inside `walk_block_map`, and `walk_block_map`
        // runs inside the timed `collect`. A tracetest build emitted 1941 report pairs (~1.3 MB
        // of console), enough that the print alone could account for the whole 5.9 s `collect`
        // charged at smp1 -- the instrument was most of what it measured. 2048 keeps the final
        // figure within 3% of the true total and costs ~30 prints instead of ~1941.
        if n.is_power_of_two() || (n > 256 && n % 2048 == 0) {
            FS_LOCK_STATS.report();
        }
        let prev_site = CUR_SITE.with(|c| c.replace(Some(site)));
        FsGuard {
            guard,
            since: Instant::now(),
            site,
            prev_site,
        }
    }

    pub fn get_object_as_file<'a>(
        &self,
        fs: &'a mut MutexGuard<'_, Ext4Fs>,
        id: ObjID,
        create: bool,
    ) -> std::io::Result<Ext4File<'a>> {
        // Only for opens, never for creates: this path opens by inode number and so cannot carry
        // O_TRUNC, and silently returning an untruncated file would reintroduce exactly the stale
        // contents the unlink used to prevent.
        if !create {
            if let Some(ino) = self.lookup_ino_cache(id) {
                FS_LOCK_STATS.ino_hits.fetch_add(1, Ordering::Relaxed);
                return fs.open_file_from_inode(ino, O_RDWR);
            }
            FS_LOCK_STATS.ino_misses.fetch_add(1, Ordering::Relaxed);
        }
        let mut file = self.do_get_object_as_file(fs, id, create)?;
        let ino = file.get_file_inode()?.num();
        self.insert_ino_cache(id, ino);
        Ok(file)
    }
}

impl<D: Device> PagedObjectStore for Ext4Store<D> {
    fn create_object(&self, id: crate::ObjID) -> Result<()> {
        // An id can be re-created after deletion, and an external id can land on a recycled inode.
        let _inval = self.extents.invalidate_on_drop(id);
        let mut fs = self.lock_fs(FsSite::Meta);
        let mut file = self
            .get_object_as_file(&mut fs, id, true)
            .map_err(|e| log_ext4_errno("create", id, e))?;
        let len = file.len();
        drop(file);
        self.set_len_in_cache(id, len);
        flush_fs(&mut fs)
            .inspect_err(|e| tracing::warn!("ext4 flush after create of {:x} failed: {}", id, e))?;
        Ok(())
    }

    fn delete_object(&self, id: crate::ObjID) -> Result<()> {
        if objid_to_ino(id).is_some() {
            // An inode-derived ID has no synthetic ids/xx/... path; unlinking one goes through
            // unlink_external. Say so rather than reporting a confusing NotFound.
            return Err(ErrorKind::Unsupported.into());
        }
        let _inval = self.extents.invalidate_on_drop(id);
        let path = self.get_id_path(id);
        let mut fs = self.lock_fs(FsSite::Meta);
        // Drop the caches before checking the result: a failed remove must not leave them
        // pointing at an inode/length we can no longer trust.
        self.remove_ino_cache(id);
        self.invalidate_len(id);
        fs.remove_file(&path.1)?;
        flush_fs(&mut fs)?;
        self.set_absent_in_cache(id);
        Ok(())
    }

    fn mtime(&self, id: crate::ObjID) -> Result<u32> {
        // Only external (ino-backed) files carry a store mtime; native objects report 0.
        let Some(ino) = objid_to_ino(id) else {
            return Ok(0);
        };
        let mut fs = self.lock_fs(FsSite::Meta);
        // Floor to 1: the image builder leaves inode times at 0, and mtime consumers (neatvi's
        // no-clobber check, `find_meta_ext`) read 0 as "no such file"/"no such ext". An existing
        // file must never report 0.
        Ok(fs.get_inode(ino)?.mtime().max(1))
    }

    fn set_mtime(&self, id: crate::ObjID, mtime: u32) -> Result<()> {
        let Some(ino) = objid_to_ino(id) else {
            return Err(ErrorKind::Unsupported.into());
        };
        let mut fs = self.lock_fs(FsSite::Meta);
        fs.get_inode(ino)?.set_mtime(mtime.max(1));
        Ok(())
    }

    fn nlink(&self, id: crate::ObjID) -> Result<u32> {
        // Only external (ino-backed) files can have more than one name: an internal object is
        // reachable through the single `ids/` entry this store keeps for it.
        let Some(ino) = objid_to_ino(id) else {
            return Ok(1);
        };
        let mut fs = self.lock_fs(FsSite::Meta);
        Ok(fs.get_inode(ino)?.links_count().max(1) as u32)
    }

    /// One `FsSite::Meta` acquisition for all three, rather than one per call.
    fn len_mtime_nlink(&self, id: crate::ObjID) -> Result<(u64, u32, u32)> {
        let Some(ino) = objid_to_ino(id) else {
            return Ok((self.len(id)?, 0, 1));
        };
        // Consult the length cache before taking the fs lock, so a cache mutex is never nested
        // inside a critical section held across disk I/O. (An earlier note here claimed the other
        // order measured 11% worse hold time and pushed contention 25% -> 40%. That was withdrawn:
        // the hold-time ranges overlapped at n=2, and the contention figure is a rate whose
        // denominator this function shrinks, so it rises mechanically. The ordering is kept on
        // principle, not on a measured win.)
        let cached = self.get_len_from_cache(id);
        if let Some(None) = cached {
            return Err(ErrorKind::NotFound.into());
        }
        // One inode fetch, not two. `open_file_from_inode` already calls `get_inode` and stores its
        // `size()` as the file's `fsize`, so opening the file to ask `len()` and then fetching the
        // inode again for `mtime()` read the same inode twice per call. All three fields live on
        // the `Ext4InodeRef`, so an external id needs no `Ext4File` at all.
        //
        // Why this and not fewer acquisitions: `meta` device reads were 884 in *both* arms of the
        // two-acquisition A/B. Collapsing two acquisitions into one removed lock round trips and
        // none of the work inside them, which is why that change moved no hold time. The work is
        // these fetches.
        let mut fs = self.lock_fs(FsSite::Meta);
        // mtimes floored to 1, matching `mtime()`: an existing file must never report 0.
        match (cached, fs.get_inode(ino)) {
            // Length already known: the inode is fetched solely for mtime and link count.
            (Some(Some(len)), Ok(inode)) => {
                Ok((len, inode.mtime().max(1), inode.links_count().max(1) as u32))
            }
            // Inode unreadable but the length is cached. Preserves the previous shape, where mtime
            // came from a separate call whose error both call sites folded to 0 rather than fail a
            // lookup that had a length in hand.
            (Some(Some(len)), Err(_)) => Ok((len, 1, 1)),
            (_, Ok(inode)) => {
                let len = inode.size();
                let mtime = inode.mtime().max(1);
                let nlink = inode.links_count().max(1) as u32;
                drop(inode);
                self.set_len_in_cache(id, len);
                Ok((len, mtime, nlink))
            }
            // No cached length and no inode: nothing to report, so this is a real error.
            (_, Err(e)) => Err(e.into()),
        }
    }

    fn len(&self, id: crate::ObjID) -> Result<u64> {
        match self.get_len_from_cache(id) {
            Some(Some(len)) => return Ok(len),
            Some(None) => return Err(ErrorKind::NotFound.into()),
            None => {}
        }
        let mut fs = self.lock_fs(FsSite::Meta);
        let mut file = match self.get_object_as_file(&mut fs, id, false) {
            Ok(file) => file,
            Err(e) => {
                // Only a confirmed absence is cached, and only for internal IDs: a device error
                // must stay retryable, and an external ID is derived from an inode number that
                // ext4 reuses after unlink, so a negative there could outlive the file it
                // described. Internal IDs are created solely by create_object, which writes a
                // positive entry. External lookups skip the directory walk anyway, so they were
                // never the expensive case this cache exists for.
                if e.kind() == ErrorKind::NotFound && objid_to_ino(id).is_none() {
                    self.set_absent_in_cache(id);
                }
                return Err(e.into());
            }
        };
        let len = file.len();
        drop(file);
        self.set_len_in_cache(id, len);
        Ok(len)
    }

    fn read_object(&self, id: crate::ObjID, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut fs = self.lock_fs(FsSite::ReadWrite);
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        file.seek(SeekFrom::Start(offset))?;
        // Deliberately not read_exact: callers (readlink) pass an oversized buffer and rely on
        // the short count at EOF.
        let mut total = 0;
        while total < buf.len() {
            match file.read(&mut buf[total..])? {
                0 => break,
                n => total += n,
            }
        }
        Ok(total)
    }

    fn write_object(&self, id: crate::ObjID, offset: u64, buf: &[u8]) -> Result<()> {
        // ext4_fwrite may allocate, and the ensure_backing/truncate pair below certainly does.
        let _inval = self.extents.invalidate_on_drop(id);
        let mut fs = self.lock_fs(FsSite::ReadWrite);
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        if offset > file.len() {
            file.ensure_backing(offset)
                .inspect_err(|e| tracing::warn!("failed to ensure backing for object: {}", e))?;
            file.truncate(offset).inspect_err(|e| {
                tracing::warn!("failed to initialize object to {}: {}", offset, e)
            })?;
        }
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(buf)?;
        // Any write can grow the file (page-out appends at exactly offset == len), so refresh the
        // cache unconditionally -- a stale entry here clamps every subsequent page-in short.
        let new_len = file.len();
        drop(file);
        self.set_len_in_cache(id, new_len);
        flush_fs(&mut fs)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        let mut fs = self.lock_fs(FsSite::Meta);
        Ok(fs.sync_super()?)
    }

    fn get_disk_blocks(
        &self,
        id: ObjID,
        start_page: u64,
        nr_pages: u32,
        create: bool,
        out: &mut Vec<DevicePage, INLINE_LEN>,
    ) -> Result<()> {
        if nr_pages == 0 {
            return Ok(());
        }
        let entry = self.extents.entry(id);
        let _inflight = EXTENT_STATS.in_flight();

        // The whole point: a range we already know is served under a read lock, without the fs
        // lock and without an inode ref.
        let (cached, warm) = entry.emit(start_page, nr_pages, create, out);
        if cached == nr_pages {
            // Held across nothing but the accounting here -- the read lock is already dropped --
            // but the gauge is what shows several of these overlapping on different threads.
            let _cached_inflight = EXTENT_STATS.cache_served();
            EXTENT_STATS.record(create, cached, 0, 0, warm);
            return Ok(());
        }

        let page = start_page + cached as u64;
        let end = start_page + nr_pages as u64;

        // Wholly past EOF: a hole, with no walk at all. Sourced from the length cache so the
        // common case costs no fs lock; without a cached length we fall through to the walk, which
        // is slower but correct. Deliberately not committed to the tracker -- i_size moves, and
        // recomputing this is free.
        if !create {
            if let Some(Some(max_len)) = self.get_len_from_cache(id) {
                let rem_blocks = (end - page) as u32 * self.blocks_per_page;
                if rem_blocks > 64
                    && page as usize * PAGE_SIZE >= (max_len as usize + PAGE_SIZE * 8)
                {
                    push_device_page(out, DevicePage::Hole(rem_blocks));
                    EXTENT_STATS.record(create, cached, 0, rem_blocks, warm);
                    return Ok(());
                }
            }
        }

        // Snapshot before the walk: a mutation racing us makes the commit a no-op rather than
        // caching what we are about to read.
        let generation = entry.generation();
        let mut learned = Vec::<_, INLINE_LEN>::new();
        let res = self.walk_block_map(id, page, end, create, &mut learned, out);
        // Commit whatever the walk did establish, even on error -- those entries were read under
        // the fs lock and are no less true for the walk having stopped.
        entry.commit(generation, &learned);
        EXTENT_STATS.record(create, cached, (end - page) as u32, 0, warm);
        res
    }

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [crate::PageRequest]) -> Result<usize> {
        tracing::trace!("paging  in request for {} reqs", reqs.len());

        let _time0 = Instant::now();
        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut disk_pages = Vec::<DevicePage, INLINE_LEN>::new();
                self.get_disk_blocks(
                    id,
                    req.start_page as u64,
                    req.nr_pages,
                    false,
                    &mut disk_pages,
                )?;
                Result::Ok((req, disk_pages))
            })
            .try_collect::<std::vec::Vec<_>>()?;

        let _time1 = Instant::now();
        tracing::trace!("collecting blocks took {}ms", (_time1 - _time0).as_millis());
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            tracing::trace!("paging in {:?}", pages);
            let _len = br.0.page_in(pages, &self.device)?;
        }
        PAGE_IN_STATS.record("page_in", _time1 - _time0, Instant::now() - _time1);

        Ok(reqs.len())
    }

    fn page_out_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest],
        true_len: Option<u64>,
    ) -> Result<usize> {
        // The page requests only bound the length from above: they end at a page boundary, so
        // using them grows i_size by up to PAGE-1 bytes past the real end. That over-estimate used
        // to win, because the caller sets the authoritative length from `MEXT_SIZED` *before*
        // paging out and this then grew it again -- a 7383464-byte file came back as 7385088
        // (1803 pages) with a zero tail. Prefer the real length whenever the caller has one.
        let end_offset = true_len.or_else(|| {
            reqs.iter()
                .map(|req| req.start_page as u64 + req.nr_pages as u64)
                .max()
                .map(|end_page| page_to_block(id, end_page) * PAGE_SIZE as u64)
        });

        let start = Instant::now();
        // i_size has to cover the pages about to be written, and `ext4_ftruncate` only shrinks
        // (it returns EOK untouched when `fsize <= size`). Growing it used to mean writing a zero
        // page *past* the end and truncating back: three fs-lock acquisitions, a buffered page
        // write with its own block allocation, and a truncate that freed it again. Measured at
        // 695-916 us mean over ~866 calls -- the largest single fs-lock hold in a build.
        //
        // `ext4_fclose` only resets the in-memory `ext4_file`; it never writes `f->fsize` back,
        // so the length left stale in `file` here cannot clobber the inode on drop.
        let needs_extend = {
            let mut fs = self.lock_fs(FsSite::PageOut);
            let mut file = self.get_object_as_file(&mut fs, id, false)?;
            let end_offset = end_offset.unwrap_or(0);
            tracing::trace!(
                "paging out request for {} reqs, end_offset = {:?}, len = {}",
                reqs.len(),
                end_offset,
                file.len()
            );
            if end_offset > file.len() {
                file.get_file_inode()?.set_size(end_offset);
                true
            } else {
                false
            }
        };
        if needs_extend {
            self.set_len_in_cache(id, end_offset.unwrap_or(0));
        }
        tracing::trace!("paging out {:x} request for {} reqs", id, reqs.len());

        let setup_done = Instant::now();
        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut disk_pages = Vec::<DevicePage, INLINE_LEN>::new();
                self.get_disk_blocks(
                    id,
                    req.start_page as u64,
                    req.nr_pages,
                    true,
                    &mut disk_pages,
                )?;
                tracing::trace!(
                    "paging out {:x} from {}: {:?}",
                    id,
                    req.start_page,
                    disk_pages
                );
                Result::Ok((req, disk_pages))
            })
            .try_collect::<std::vec::Vec<_>>()?;
        tracing::trace!(
            "found blocks for paging out in {}ms",
            (Instant::now() - setup_done).as_millis()
        );

        let blocks_found = Instant::now();
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            let _len = br.0.page_out(pages, &self.device)?;
        }
        // The blocks above went straight to the device, bypassing ext4's cache, so ext4 holds no
        // dirty state from them. What makes dropping this safe is write-through, not the
        // condition below: `cache_write_back` is 0 and lwext4 brackets each of its own operations
        // with write_back(1)..write_back(0), where the disable flushes the cache -- so the dirty
        // list is already empty here. `needs_extend` is NOT a sufficient guard on its own; the
        // allocating `get_disk_blocks(create=true)` above also dirties extent metadata, on a path
        // where it is false. It is kept only because flushing in that case is the cheaper
        // mistake. Measured: `flush`
        // does 0.0 block reads and 0.0 block writes (lwext4 brackets its own operations and
        // writes through when `cache_write_back` returns to 0), so the unconditional flush bought
        // nothing and cost a second acquisition of the global fs lock on the pager's
        // highest-frequency site -- ~28k acquisitions per boot against a lock with a 25 ms
        // contended tail.
        if PAGE_OUT_ALWAYS_FLUSH || needs_extend {
            let mut fs = self.lock_fs(FsSite::PageOut);
            flush_fs(&mut fs)?;
        }
        let io_done = Instant::now();
        PAGE_OUT_STATS.record(
            "page_out",
            blocks_found - setup_done,
            io_done - blocks_found,
        );
        tracing::trace!(
            "==> {}ms {}ms {}ms",
            (setup_done - start).as_millis(),
            (blocks_found - setup_done).as_millis(),
            (io_done - blocks_found).as_millis()
        );
        Ok(reqs.len())
    }
}

impl<D: Device> ExternalFileStore for Ext4Store<D> {
    fn open_external(
        &self,
        at: Option<ObjID>,
        path: impl AsRef<Path>,
        flags: ExternalOpenFlags,
        mode: mode_t,
        link_to: Option<ObjID>,
    ) -> Result<ExternalFile> {
        let mut at_ino = if let Some(at) = at {
            objid_to_ino(at).ok_or(ErrorKind::InvalidInput)?
        } else {
            2
        };
        if at_ino < 2 {
            at_ino = 2;
        }
        tracing::trace!(
            "opening external file at {:?} with flags {:?} and mode {:o} at ino {}, link_to = {:?}",
            path.as_ref(),
            flags,
            mode,
            at_ino,
            link_to
        );

        let mut fs = self.lock_fs(FsSite::External);

        let mut oflags = if flags.contains(ExternalOpenFlags::READ)
            && flags.contains(ExternalOpenFlags::WRITE)
        {
            O_RDWR
        } else if flags.contains(ExternalOpenFlags::READ) {
            O_RDONLY
        } else {
            O_RDWR
        };

        if flags.contains(ExternalOpenFlags::CREATE) {
            oflags |= O_CREAT;
        }

        if flags.contains(ExternalOpenFlags::TRUNCATE) {
            oflags |= O_TRUNC;
        }

        if let Some(link_to) = link_to {
            let target_ino = objid_to_ino(link_to).ok_or(ErrorKind::InvalidInput)?;
            let name = path.as_ref().to_string_lossy();
            // This is how rename lands (ExtNamespace::replace), and POSIX rename overwrites --
            // but lwext4's low-level link never checks for an existing entry:
            // ext4_dir_add_entry appends a second dirent under the same name, and later lookups
            // can keep resolving to the replaced inode. Drop the destination first, under this
            // same fs lock. A destination already pointing at the target is left alone, since
            // unlinking could free the inode's last link before the re-link.
            match Self::child_entry(&mut fs, at_ino, name.as_ref()) {
                Some((ino, _)) if ino == target_ino => {}
                existing => {
                    if existing.is_some() {
                        self.unlink_child_locked(&mut fs, at_ino, name.as_ref())?;
                    }
                    fs.link(name.as_ref(), at_ino, target_ino)
                        .inspect_err(|e| tracing::warn!("failed to link: {}", e))?;
                }
            }
        }

        let mut file = fs.open_file_from_container(
            at_ino,
            path.as_ref().to_string_lossy().as_ref(),
            oflags,
            mode,
        )?;

        let id = ino_to_objid(file.get_file_inode()?.num());
        // Directories only: low volume, and enough to pair a later stale `..` with the create
        // that set it and the rmdir that invalidated it.
        if mode & libc::S_IFMT == libc::S_IFDIR && diag_enabled() {
            tracing::info!(
                "created dir '{}' ino {} in parent ino {}",
                path.as_ref().display(),
                objid_to_ino(id).unwrap_or(0),
                at_ino
            );
        }
        // Stamp a modification time on creation (and truncation, which is a content change): the
        // image builder and lwext4 both leave inode times at 0, and mtime consumers (fingerprints)
        // need created-later to sort later. Guest wall clock is boot-relative -- monotonic within
        // a boot is the property that matters. An existing nonzero mtime is left alone.
        if flags.contains(ExternalOpenFlags::CREATE) {
            let mut ino = file.get_file_inode()?;
            if ino.mtime() == 0 || flags.contains(ExternalOpenFlags::TRUNCATE) {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);
                ino.set_mtime(now.max(1));
            }
        }
        // O_TRUNC frees blocks; O_CREAT can land on an inode ext4 recycled from a deleted file, so
        // either way any extents held under this id are now wrong. The ObjID is not known until the
        // open returns, hence the invalidation here rather than up front -- and hence a window
        // between the truncate and the bump, which only TRUNCATE has, since a freshly created id is
        // not known to anyone else until we return it.
        if flags.intersects(ExternalOpenFlags::TRUNCATE | ExternalOpenFlags::CREATE) {
            self.extents.entry(id).invalidate();
        }

        Ok(ExternalFile::new(
            path.as_ref().to_string_lossy().to_string(),
            file.get_file_inode()?.kind().into(),
            id,
        ))
    }

    fn unlink_external(&self, at: Option<ObjID>, path: impl AsRef<Path>) -> Result<()> {
        tracing::trace!(
            "unlinking external file at {:?} with path {:?}",
            at,
            path.as_ref()
        );
        // Resolve `at` exactly as open_external does, so a name that can be created in a
        // directory can also be removed from it. This previously accepted only the external root
        // and rejected every subdirectory as Unsupported -- but /sysroot is a real ext4
        // subdirectory (the disk builder mkdirs it), so external unlink never worked anywhere a
        // program actually writes.
        let mut at_ino = if let Some(at) = at {
            match objid_to_ino(at) {
                Some(ino) => ino,
                None => {
                    tracing::warn!("unlink_external: {:x} is not an external id", at);
                    return Err(ErrorKind::InvalidInput.into());
                }
            }
        } else {
            2
        };
        if at_ino < 2 {
            at_ino = 2;
        }
        let mut fs = self.lock_fs(FsSite::ExtUnlink);
        let name = path.as_ref().to_string_lossy();
        // One entry point for both, because the naming layer has one: `remove` does not know
        // which kind it is about to drop, and neither does the `remove_file`/`remove_dir` pair
        // in libstd once it reaches the runtime.
        if let Some((ino, FileKind::Directory)) = Self::child_entry(&mut fs, at_ino, name.as_ref())
        {
            return self.rmdir_child_locked(&mut fs, at_ino, ino, name.as_ref());
        }
        self.unlink_child_locked(&mut fs, at_ino, name.as_ref())
    }

    fn rename_external(
        &self,
        at: Option<ObjID>,
        old: impl AsRef<Path>,
        to: Option<ObjID>,
        new: impl AsRef<Path>,
    ) -> Result<()> {
        let resolve = |id: Option<ObjID>| -> Result<u32> {
            let Some(id) = id else { return Ok(2) };
            match objid_to_ino(id) {
                // The root is reachable as either 0 or 2 through this mapping.
                Some(ino) if ino >= 2 => Ok(ino),
                Some(_) => Ok(2),
                None => {
                    tracing::warn!("rename_external: {:x} is not an external id", id);
                    Err(ErrorKind::InvalidInput.into())
                }
            }
        };
        let at_ino = resolve(at)?;
        let to_ino = resolve(to)?;

        let mut fs = self.lock_fs(FsSite::ExtRename);
        let old_name = old.as_ref().to_string_lossy();
        let new_name = new.as_ref().to_string_lossy();

        // POSIX rename replaces the destination; `ext4_frename` refuses one
        // (`ext4_create_hardlink` returns EEXIST), so drop it first. Never a directory: the
        // naming layer refuses to clobber a namespace, and doing it here would discard a tree.
        // `unlink_child_locked` also drops the caches keyed on the victim's ObjID, which matters
        // because that ObjID is derived from an inode number ext4 will hand out again.
        if let Some((_, kind)) = Self::child_entry(&mut fs, to_ino, new_name.as_ref()) {
            if matches!(kind, FileKind::Directory) {
                return Err(ErrorKind::AlreadyExists.into());
            }
            self.unlink_child_locked(&mut fs, to_ino, new_name.as_ref())?;
        }

        // `ext4_frename` takes paths, like the removal path does, and for the same reason: the
        // container-relative primitives cannot express the whole operation atomically.
        let old_path = Self::path_of(&mut fs, at_ino, old_name.as_ref())?;
        let new_path = Self::path_of(&mut fs, to_ino, new_name.as_ref())?;
        fs.frename(&old_path, &new_path)?;
        Ok(())
    }

    fn readlink_external(&self, at: ObjID) -> Result<String> {
        self.readlink(at)
    }

    fn readdir_external(
        &self,
        dir: ObjID,
        skip: usize,
        count: usize,
        entries: &mut std::vec::Vec<ExternalFile>,
    ) -> Result<()> {
        entries.clear();
        tracing::trace!(
            "enumerating external namespace {:x} (skip {}, count {})",
            dir,
            skip,
            count
        );
        let mut fs = self.lock_fs(FsSite::ExtReaddir);
        let mut inonr = objid_to_ino(dir).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = 2;
        }

        let mut inode = fs.get_inode(inonr)?;
        let diriter = fs.dirents(&mut inode)?;

        // Filter first, then skip: `skip` counts entries the caller was actually handed, so a
        // dropped dirent does not desynchronize a cursor walking this namespace across calls. The
        // iterator reads each inode either way, so skipping later costs nothing.
        EXTENT_STATS.rd_calls.fetch_add(1, Ordering::Relaxed);
        EXTENT_STATS
            .rd_skip
            .fetch_add(skip as u64, Ordering::Relaxed);
        let diriter = diriter
            .inspect(|_| {
                // Counted here rather than after the filter: `DirIter::next` reads the inode for
                // every entry it yields, so this is the number of inode reads the call costs,
                // including the ones `.skip()` below then discards.
                EXTENT_STATS.rd_iterated.fetch_add(1, Ordering::Relaxed);
            })
            // An entry whose inode will not load is no longer dropped: nothing here reads inodes
            // any more, so there is nothing to fail. That is deliberate and matches POSIX readdir,
            // which does not stat -- a name present in the directory is a name, and hiding it makes
            // a listing quietly incomplete exactly when you would most want to see it. A listed
            // name that stats to nothing is this, working, not a new bug.
            //
            // Unverified for that case: as of 2026-08-29, no such entry had appeared in any
            // recorded run -- the old code's drop-path warning matched 0 files under
            // `target/results`, against 123,491 for a string known to be present, and 53
            // object_store events reached the same log in one run, so the emit site existed, the
            // channel worked and the query could match. Dated because a zero describes the moment
            // it was taken: a corpus that later grows such an entry does not make this wrong, it
            // makes it stale. Demonstrating the case needs a constructed entry --
            // `debugfs -w -R "ln <unallocated-ino> name"` against a copy of the data image.
            .filter_map(|(raw, ino, kind)| {
                // ext4 names are arbitrary non-NUL bytes. A lossy conversion would not round-trip
                // through a later lookup, so drop names we cannot represent.
                let name = core::str::from_utf8(&raw)
                    .inspect_err(|_| {
                        tracing::warn!("skipping non-utf8 dirent in namespace {:x}", dir)
                    })
                    .ok()?;
                Some(ExternalFile::new(name, kind.into(), ino_to_objid(ino)))
            })
            .skip(skip)
            .take(count);

        for entry in diriter {
            EXTENT_STATS.rd_returned.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(
                "record external file {} in namespace {:x} with ID {} and kind {:?}",
                entry.name().unwrap_or("<invalid utf8>"),
                dir,
                entry.id,
                entry.kind
            );
            entries.push(entry)
        }
        tracing::trace!("collected {} entries", entries.len());

        Ok(())
    }

    fn link_external(
        &self,
        _file: &ExternalFile,
        _at: Option<ObjID>,
        _path: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }

    fn stat_external(&self, _path: impl AsRef<Path>) -> Result<libc::stat> {
        todo!()
    }

    fn fstat_external(&self, _file: Option<ObjID>) -> Result<libc::stat> {
        todo!()
    }

    fn symlink_external(
        &self,
        _at: Option<ObjID>,
        _target: impl AsRef<Path>,
        _linkpath: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }
}
