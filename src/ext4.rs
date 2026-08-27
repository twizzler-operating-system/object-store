#[cfg(not(target_os = "twizzler"))]
use std::io::Result;
use std::{
    collections::HashMap,
    ffi::CString,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
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
    PAGE_SIZE,
};

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
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        let slice = unsafe { core::slice::from_raw_parts_mut(buf, len as usize) };
        let len = self.device.run_async(self.device.read(start, slice))?;
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
        let len = self.device.run_async(self.device.write(start, slice))?;
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
        if calls.is_power_of_two() {
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
        if calls.is_power_of_two() {
            tracing::info!(
                "EXTENTSTATS: {} calls ({} warm-object), {} lookups; read pages {} hit / {} walked / {} past-eof; create pages {} hit / {} walked; concurrency max {} in flight, max {} fs-lock-free",
                calls,
                self.warm_object_calls.load(Ordering::Relaxed),
                self.walk_lookups.load(Ordering::Relaxed),
                self.hit_pages.load(Ordering::Relaxed),
                self.walk_pages.load(Ordering::Relaxed),
                self.eof_pages.load(Ordering::Relaxed),
                self.create_hit_pages.load(Ordering::Relaxed),
                self.create_walk_pages.load(Ordering::Relaxed),
                self.max_inflight.load(Ordering::Relaxed),
                self.max_cached_inflight.load(Ordering::Relaxed),
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
        if n.is_power_of_two() {
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
    pub async fn new(device: D, name: &str) -> Result<Self> {
        let bdname = format!("blockdev-{}", BDEV_ID.fetch_add(1, Ordering::SeqCst));
        let max = device.len().await? as u64;
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
    pub fn page_in_would_block(&self, id: ObjID, start_page: u64, nr_pages: u32) -> bool {
        if !self.len_is_cached(id) {
            return true;
        }
        self.extents
            .peek(id)
            .is_none_or(|entry| !entry.covers(start_page, nr_pages))
    }

    async fn readlink(&self, id: ObjID) -> Result<String> {
        // By inode rather than through `read_object`: a target short enough to live in the inode's
        // block map is stored there with no data block behind it, so reading one as file data
        // yields a block of unrelated bytes -- which, being zeros, is valid UTF-8 and passes for a
        // 4096-byte target rather than failing outright.
        let ino = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        let buf = self.fs.lock().unwrap().readlink_from_inode(ino)?;
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
        let mut fs = self.fs.lock().unwrap();
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
    fn child_kind(fs: &mut MutexGuard<'_, Ext4Fs>, dir_ino: u32, name: &str) -> Option<FileKind> {
        let mut inode = fs.get_inode(dir_ino).ok()?;
        let target = name.as_bytes();
        fs.dirents(&mut inode)
            .ok()?
            .find(|(nm, _)| nm.as_slice() == target)
            .and_then(|(_, iref)| iref.ok().map(|i| i.kind()))
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
                return Err(ErrorKind::InvalidInput.into());
            }
            let parent = {
                let mut inode = fs.get_inode(ino)?;
                fs.dirents(&mut inode)?
                    .find(|(nm, _)| nm.as_slice() == b"..")
                    .and_then(|(_, iref)| iref.ok().map(|i| i.num()))
                    .ok_or(ErrorKind::InvalidInput)?
            };
            let mut parent_inode = fs.get_inode(parent)?;
            let component = fs
                .dirents(&mut parent_inode)?
                .filter(|(nm, _)| nm.as_slice() != b"." && nm.as_slice() != b"..")
                .find(|(_, iref)| iref.as_ref().is_ok_and(|i| i.num() == ino))
                .map(|(nm, _)| nm)
                .ok_or(ErrorKind::InvalidInput)?;
            // ext4 names are arbitrary non-NUL bytes; one that is not UTF-8 cannot be spliced into
            // a path string, and a lossy conversion would name a different file.
            parts.push(String::from_utf8(component).map_err(|_| ErrorKind::InvalidInput)?);
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
        let flags = if create { O_RDWR | O_CREAT } else { O_RDWR };
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
        let mut lookups = 0u64;
        let res = 'chunks: loop {
            let mut fs = self.fs.lock().unwrap();
            let mut file = match self.get_object_as_file(&mut fs, id, false) {
                Ok(file) => file,
                Err(e) => break Err(e.into()),
            };
            let mut inode = match file.get_file_inode() {
                Ok(inode) => inode,
                Err(e) => break Err(e.into()),
            };

            for _ in 0..BLOCKS_PER_LOCK {
                if page >= end {
                    break 'chunks Ok(());
                }

                let block = page_to_block(id, page) as u32 * self.blocks_per_page;
                let rem_blocks = (end - page) as u32 * self.blocks_per_page;
                let ask = if create {
                    rem_blocks
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
                let emit = nr_blocks.min(rem_blocks);
                push_device_page(
                    out,
                    match pblock {
                        Some(pblock) => DevicePage::Run(pblock, emit),
                        None => DevicePage::Hole(emit),
                    },
                );
                page += emit as u64;
            }

            drop(inode);
            drop(file);
            drop(fs);
            self.device.yield_now();
        };
        EXTENT_STATS
            .walk_lookups
            .fetch_add(lookups, Ordering::Relaxed);
        res
    }

    pub fn get_object_as_file<'a>(
        &self,
        fs: &'a mut MutexGuard<'_, Ext4Fs>,
        id: ObjID,
        create: bool,
    ) -> std::io::Result<Ext4File<'a>> {
        if let Some(ino) = self.lookup_ino_cache(id) {
            return fs.open_file_from_inode(ino, O_RDWR);
        }
        let mut file = self.do_get_object_as_file(fs, id, create)?;
        let ino = file.get_file_inode()?.num();
        self.insert_ino_cache(id, ino);
        Ok(file)
    }
}

impl<D: Device> PagedObjectStore for Ext4Store<D> {
    async fn create_object(&self, id: crate::ObjID) -> Result<()> {
        // An id can be re-created after deletion, and an external id can land on a recycled inode.
        let _inval = self.extents.invalidate_on_drop(id);
        let mut fs = self.fs.lock().unwrap();
        let mut file = self
            .get_object_as_file(&mut fs, id, true)
            .map_err(|e| log_ext4_errno("create", id, e))?;
        let len = file.len();
        drop(file);
        self.set_len_in_cache(id, len);
        flush_fs(&mut fs).inspect_err(|e| tracing::warn!("ext4 flush after create of {:x} failed: {}", id, e))?;
        Ok(())
    }

    async fn delete_object(&self, id: crate::ObjID) -> Result<()> {
        if objid_to_ino(id).is_some() {
            // An inode-derived ID has no synthetic ids/xx/... path; unlinking one goes through
            // unlink_external. Say so rather than reporting a confusing NotFound.
            return Err(ErrorKind::Unsupported.into());
        }
        let _inval = self.extents.invalidate_on_drop(id);
        let path = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        // Drop the caches before checking the result: a failed remove must not leave them
        // pointing at an inode/length we can no longer trust.
        self.remove_ino_cache(id);
        self.invalidate_len(id);
        fs.remove_file(&path.1)?;
        flush_fs(&mut fs)?;
        self.set_absent_in_cache(id);
        Ok(())
    }

    async fn mtime(&self, id: crate::ObjID) -> Result<u32> {
        // Only external (ino-backed) files carry a store mtime; native objects report 0.
        let Some(ino) = objid_to_ino(id) else {
            return Ok(0);
        };
        let mut fs = self.fs.lock().unwrap();
        Ok(fs.get_inode(ino)?.mtime())
    }

    async fn len(&self, id: crate::ObjID) -> Result<u64> {
        match self.get_len_from_cache(id) {
            Some(Some(len)) => return Ok(len),
            Some(None) => return Err(ErrorKind::NotFound.into()),
            None => {}
        }
        let mut fs = self.fs.lock().unwrap();
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

    async fn read_object(&self, id: crate::ObjID, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut fs = self.fs.lock().unwrap();
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

    async fn write_object(&self, id: crate::ObjID, offset: u64, buf: &[u8]) -> Result<()> {
        // ext4_fwrite may allocate, and the ensure_backing/truncate pair below certainly does.
        let _inval = self.extents.invalidate_on_drop(id);
        let mut fs = self.fs.lock().unwrap();
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

    async fn flush(&self) -> Result<()> {
        let mut fs = self.fs.lock().unwrap();
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

    async fn page_in_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest],
    ) -> Result<usize> {
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
            let _len = br.0.page_in(pages, &self.device).await?;
        }
        PAGE_IN_STATS.record("page_in", _time1 - _time0, Instant::now() - _time1);

        Ok(reqs.len())
    }

    async fn page_out_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest],
    ) -> Result<usize> {
        let end_offset = reqs
            .iter()
            .map(|req| req.start_page as u64 + req.nr_pages as u64)
            .max()
            .map(|end_page| page_to_block(id, end_page) * PAGE_SIZE as u64);

        let start = Instant::now();
        let needs_extend = {
            let mut fs = self.fs.lock().unwrap();
            let mut file = self.get_object_as_file(&mut fs, id, false)?;
            tracing::trace!(
                "paging out request for {} reqs, end_offset = {:?}, len = {}",
                reqs.len(),
                end_offset,
                file.len()
            );
            end_offset.unwrap_or(0) > file.len()
        };
        if needs_extend {
            let end_offset = end_offset.unwrap_or(0);
            // i_size has to cover the pages about to be written, but lwext4's truncate only
            // shrinks, so growth has to come from a write. Write one page *past* the end and
            // then trim back to the exact length: the touched page is beyond the final size and
            // is never one the device-direct transfers below write, so the cached zeros cannot
            // race with them.
            self.write_object(id, end_offset, &[0u8; PAGE_SIZE]).await?;
            self.set_len(id, end_offset)?;
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
            let _len = br.0.page_out(pages, &self.device).await?;
        }
        let mut fs = self.fs.lock().unwrap();
        flush_fs(&mut fs)?;
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
    async fn open_external(
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

        let mut fs = self.fs.lock().unwrap();

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
            fs.link(
                path.as_ref().to_string_lossy().as_ref(),
                at_ino,
                objid_to_ino(link_to).ok_or(ErrorKind::InvalidInput)?,
            )
            .inspect_err(|e| tracing::warn!("failed to link: {}", e))?;
        }

        let mut file = fs.open_file_from_container(
            at_ino,
            path.as_ref().to_string_lossy().as_ref(),
            oflags,
            mode,
        )?;

        let id = ino_to_objid(file.get_file_inode()?.num());
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

    async fn unlink_external(&self, at: Option<ObjID>, path: impl AsRef<Path>) -> Result<()> {
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
            objid_to_ino(at).ok_or(ErrorKind::InvalidInput)?
        } else {
            2
        };
        if at_ino < 2 {
            at_ino = 2;
        }
        let mut fs = self.fs.lock().unwrap();
        let name = path.as_ref().to_string_lossy();
        // ext4_fremove refuses directories -- but returns the still-EOK `r` when it does, so a
        // directory reaching it reports success while removing nothing. Before this change a
        // subdirectory could not get here at all (the root-only check rejected it), so widening
        // the check means this refusal now has to be explicit. External rmdir stays unimplemented
        // (namerbugs.md); this keeps it an honest error rather than a silent no-op.
        if matches!(
            Self::child_kind(&mut fs, at_ino, name.as_ref()),
            Some(FileKind::Directory)
        ) {
            return Err(ErrorKind::Unsupported.into());
        }
        // Resolve the inode before unlinking: an external ObjID is derived from it, and both
        // caches are keyed on that ObjID. Leaving them populated is doubly wrong here, because
        // ext4 reuses inode numbers -- a later file can land on the same ObjID and inherit this
        // one's cached length. Best effort: a target we cannot open (a symlink, say) just leaves
        // the caches as they are today.
        let id = fs
            .open_file_from_container(at_ino, name.as_ref(), O_RDONLY, 0)
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
            tracing::debug!("unlink_external: could not resolve inode for {:?}", name);
        }
        // lwext4 exposes removal only as a path operation (ext4_fremove, which handles the
        // truncate/unlink/free-inode sequence under one transaction), while everything else here
        // is container-relative. Rather than reimplement that sequence against the raw bindings --
        // a lot of unsafe on a path where a mistake corrupts the filesystem -- turn the container
        // back into a path and reuse it.
        let full = Self::path_of(&mut fs, at_ino, name.as_ref())?;
        fs.remove_file(&full)?;
        return Ok(());
    }

    async fn readlink_external(&self, at: ObjID) -> Result<String> {
        self.readlink(at).await
    }

    async fn readdir_external(
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
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(dir).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = 2;
        }

        let mut inode = fs.get_inode(inonr)?;
        let diriter = fs.dirents(&mut inode)?;

        // Filter first, then skip: `skip` counts entries the caller was actually handed, so a
        // dropped dirent does not desynchronize a cursor walking this namespace across calls. The
        // iterator reads each inode either way, so skipping later costs nothing.
        let diriter = diriter
            .filter_map(|de| {
                // ext4 names are arbitrary non-NUL bytes. A lossy conversion would not round-trip
                // through a later lookup, so drop names we cannot represent.
                let name = core::str::from_utf8(&de.0)
                    .inspect_err(|_| {
                        tracing::warn!("skipping non-utf8 dirent in namespace {:x}", dir)
                    })
                    .ok()?;
                // A dirent whose inode will not load is data loss in a listing, so say so rather
                // than dropping it silently.
                let ino =
                    de.1.inspect_err(|err| {
                        tracing::warn!(
                            "skipping dirent {} in namespace {:x}, no inode: {}",
                            name,
                            dir,
                            err
                        )
                    })
                    .ok()?;
                Some(ExternalFile::new(
                    name,
                    ino.kind().into(),
                    ino_to_objid(ino.num()),
                ))
            })
            .skip(skip)
            .take(count);

        for entry in diriter {
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

    async fn link_external(
        &self,
        _file: &ExternalFile,
        _at: Option<ObjID>,
        _path: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }

    async fn stat_external(&self, _path: impl AsRef<Path>) -> Result<libc::stat> {
        todo!()
    }

    async fn fstat_external(&self, _file: Option<ObjID>) -> Result<libc::stat> {
        todo!()
    }

    async fn symlink_external(
        &self,
        _at: Option<ObjID>,
        _target: impl AsRef<Path>,
        _linkpath: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }
}
