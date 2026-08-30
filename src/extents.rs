//! Per-object cache of the object-page -> disk-block mapping.
//!
//! A paging request whose extents are all present is served under a read lock and never touches the
//! global fs lock. A page with no covering entry is *unknown*, so dropping an entry -- or a whole
//! object -- can only cost a walk, never correctness.
//!
//! Invalidation is a generation counter rather than a lock, so it can be performed while holding
//! the fs lock. That is what keeps the two locks unordered with respect to each other:
//! `page_out_object` invalidates via `write_object` while paging, and `open_external` does not
//! learn the ObjID until it is already inside the fs lock.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use lru::LruCache;

use crate::{
    paged_object_store::{Vec, INLINE_LEN},
    DevicePage, ObjID,
};

/// Objects tracked before the least-recently-used is dropped.
const MAX_TRACKED_OBJECTS: usize = 1024;
/// Extents held for one object. A file fragmented past this stops being tracked rather than growing
/// without bound inside the process that manages memory pressure.
const MAX_EXTENTS_PER_OBJECT: usize = 64;

/// Times [`ExtentMap::insert`] hit the cap and threw the whole object's map away.
pub static OVERFLOW_CLEARS: AtomicU64 = AtomicU64::new(0);

/// Append `item` to `out`, merging it into the previous entry when the two are contiguous.
pub fn push_device_page(out: &mut Vec<DevicePage, INLINE_LEN>, item: DevicePage) {
    if let Some(prev) = out.last_mut() {
        if prev.try_extend(&item) {
            return;
        }
    }
    out.push(item);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    /// `None` records a known hole.
    pblock: Option<u64>,
    len: u32,
}

impl Extent {
    /// True if `next` continues `self` with no gap, so the two may be stored as one entry.
    fn continues(&self, next: &Extent) -> bool {
        match (self.pblock, next.pblock) {
            (None, None) => true,
            (Some(a), Some(b)) => a + self.len as u64 == b,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct ExtentMap {
    /// Keyed by first object page. Entries are disjoint and coalesced; a page with no covering
    /// entry is unknown.
    runs: BTreeMap<u64, Extent>,
    /// Generation these entries were learned under.
    generation: u64,
}

impl ExtentMap {
    fn new(generation: u64) -> Self {
        Self {
            runs: BTreeMap::new(),
            generation,
        }
    }

    fn reset(&mut self, generation: u64) {
        self.runs.clear();
        self.generation = generation;
    }

    /// The extent covering `page`, with the page it starts at.
    fn covering(&self, page: u64) -> Option<(u64, Extent)> {
        let (&start, &ext) = self.runs.range(..=page).next_back()?;
        (page < start + ext.len as u64).then_some((start, ext))
    }

    /// Append device pages for `[start, start + nr)` to `out`, stopping at the first page that is
    /// unknown -- or, under `create`, a hole, which has to be allocated. Returns pages appended.
    fn emit(
        &self,
        start: u64,
        nr: u32,
        create: bool,
        out: &mut Vec<DevicePage, INLINE_LEN>,
    ) -> u32 {
        let end = start + nr as u64;
        let mut page = start;
        while page < end {
            let Some((estart, ext)) = self.covering(page) else {
                break;
            };
            let skip = page - estart;
            let len = (ext.len as u64 - skip).min(end - page) as u32;
            let item = match ext.pblock {
                Some(pblock) => DevicePage::Run(pblock + skip, len),
                None if create => break,
                None => DevicePage::Hole(len),
            };
            push_device_page(out, item);
            page += len as u64;
        }
        (page - start) as u32
    }

    /// Whether `emit` would serve all of `[start, start + nr)`. Walks the same runs without
    /// building any output, for callers deciding whether to start the work at all.
    fn covers(&self, start: u64, nr: u32) -> bool {
        let end = start + nr as u64;
        let mut page = start;
        while page < end {
            let Some((estart, ext)) = self.covering(page) else {
                return false;
            };
            page += (ext.len as u64 - (page - estart)).min(end - page);
        }
        true
    }

    /// Drop everything covering `[start, end)`, trimming entries that straddle either edge.
    fn punch(&mut self, start: u64, end: u64) {
        let mut removed: std::vec::Vec<u64> = std::vec::Vec::new();
        let mut kept: std::vec::Vec<(u64, Extent)> = std::vec::Vec::new();

        // An entry starting before `start` may reach into the range, and may even span past `end`.
        if let Some((&estart, &ext)) = self.runs.range(..start).next_back() {
            let eend = estart + ext.len as u64;
            if eend > start {
                removed.push(estart);
                kept.push((
                    estart,
                    Extent {
                        pblock: ext.pblock,
                        len: (start - estart) as u32,
                    },
                ));
                if eend > end {
                    kept.push((
                        end,
                        Extent {
                            pblock: ext.pblock.map(|p| p + (end - estart)),
                            len: (eend - end) as u32,
                        },
                    ));
                }
            }
        }

        // Entries starting inside the range. Since entries are disjoint, none of these can exist if
        // the one above already spanned past `end`.
        for (&estart, &ext) in self.runs.range(start..end) {
            let eend = estart + ext.len as u64;
            removed.push(estart);
            if eend > end {
                kept.push((
                    end,
                    Extent {
                        pblock: ext.pblock.map(|p| p + (end - estart)),
                        len: (eend - end) as u32,
                    },
                ));
            }
        }

        for start in removed {
            self.runs.remove(&start);
        }
        for (start, ext) in kept {
            self.runs.insert(start, ext);
        }
    }

    /// Record that object pages `[start, start + len)` live at `pblock` (or are a hole).
    fn insert(&mut self, start: u64, pblock: Option<u64>, len: u32) {
        if len == 0 {
            return;
        }
        if self.runs.len() >= MAX_EXTENTS_PER_OBJECT {
            // Too fragmented to track. Start over rather than grow; the object simply stops
            // benefiting from the cache.
            //
            // Counted because this is a *destructive* overflow policy -- it discards everything
            // known about the object, not the least useful entry -- and it is the leading
            // suspicion for why widening the walk (EXTENT_WALK_AHEAD_PAGES) learned 71k pages and
            // moved request-level coverage by one request: more runs per call trips this limit and
            // wipes the cache the walk was filling. If this fires, the lever is the policy and the
            // cap, not the walk width. If it never fires, that theory is dead and the walk-ahead
            // failed for some other reason.
            OVERFLOW_CLEARS.fetch_add(1, Ordering::Relaxed);
            self.runs.clear();
        }

        let end = start + len as u64;
        self.punch(start, end);

        let mut new_start = start;
        let mut new = Extent { pblock, len };

        if let Some((&pstart, &prev)) = self.runs.range(..start).next_back() {
            let joined = prev.len.checked_add(new.len);
            if pstart + prev.len as u64 == start && prev.continues(&new) {
                if let Some(len) = joined {
                    self.runs.remove(&pstart);
                    new_start = pstart;
                    new = Extent {
                        pblock: prev.pblock,
                        len,
                    };
                }
            }
        }

        if let Some((&nstart, &next)) = self.runs.range(end..).next() {
            let joined = new.len.checked_add(next.len);
            if nstart == end && new.continues(&next) {
                if let Some(len) = joined {
                    self.runs.remove(&nstart);
                    new = Extent {
                        pblock: new.pblock,
                        len,
                    };
                }
            }
        }

        self.runs.insert(new_start, new);
    }
}

/// One object's tracked extents.
#[derive(Debug)]
pub struct ObjectExtents {
    generation: AtomicU64,
    map: RwLock<ExtentMap>,
}

impl ObjectExtents {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            map: RwLock::new(ExtentMap::new(0)),
        }
    }

    /// Snapshot the generation before walking the fs, and hand it back to `commit`.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Forget everything known about this object.
    ///
    /// Must be called *after* the mutation it describes. Bumping first is not enough: a filler
    /// could snapshot the new generation, win the fs lock ahead of the mutation, and commit
    /// pre-mutation extents that then look current.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Serve what is known of `[start, start + nr)` into `out`.
    ///
    /// Returns the pages served and whether this object had *any* valid extents on entry. The
    /// second is what separates "the workload never revisits this object" from "it revisits it but
    /// always at a fresh range" -- and both from a cache that is simply not committing.
    pub fn emit(
        &self,
        start: u64,
        nr: u32,
        create: bool,
        out: &mut Vec<DevicePage, INLINE_LEN>,
    ) -> (u32, bool) {
        let generation = self.generation();
        let map = self.map.read().unwrap();
        if map.generation != generation {
            return (0, false);
        }
        let warm = !map.runs.is_empty();
        (map.emit(start, nr, create, out), warm)
    }

    /// Whether `[start, start + nr)` can be served entirely from here, i.e. whether a page-in of
    /// that range would have to walk the block map under the fs lock.
    pub fn covers(&self, start: u64, nr: u32) -> bool {
        let generation = self.generation();
        let map = self.map.read().unwrap();
        map.generation == generation && map.covers(start, nr)
    }

    /// Record extents learned by a walk that started at generation `generation`. A racing
    /// invalidation makes this a no-op: what we learned is already stale.
    pub fn commit(&self, generation: u64, learned: &[(u64, Option<u64>, u32)]) {
        if learned.is_empty() || self.generation() != generation {
            return;
        }
        let mut map = self.map.write().unwrap();
        if self.generation() != generation {
            return;
        }
        if map.generation != generation {
            map.reset(generation);
        }
        for &(start, pblock, len) in learned {
            map.insert(start, pblock, len);
        }
    }
}

/// Bumps an object's generation when dropped, so a `?` out of a partially-applied mutation cannot
/// skip the invalidation.
pub struct InvalidateOnDrop(Arc<ObjectExtents>);

impl Drop for InvalidateOnDrop {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

pub struct ExtentTracker {
    objects: Mutex<LruCache<ObjID, Arc<ObjectExtents>>>,
}

impl ExtentTracker {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_TRACKED_OBJECTS).unwrap(),
            )),
        }
    }

    /// The entry for `id`, creating it if absent.
    ///
    /// Entries are only ever created fresh here, never re-inserted. An entry evicted while a filler
    /// holds it is thereby detached: the filler commits into a map nothing will read, and the
    /// replacement starts empty, which is "unknown", which is correct.
    pub fn entry(&self, id: ObjID) -> Arc<ObjectExtents> {
        self.objects
            .lock()
            .unwrap()
            .get_or_insert(id, || Arc::new(ObjectExtents::new()))
            .clone()
    }

    /// The entry for `id` if there is one, without creating it or promoting it in the LRU.
    ///
    /// For probes: asking whether a range is cached must not evict the entry of some other object
    /// to record that it wasn't, nor reorder the recency the real fillers established.
    pub fn peek(&self, id: ObjID) -> Option<Arc<ObjectExtents>> {
        self.objects.lock().unwrap().peek(&id).cloned()
    }

    /// Invalidate `id` when the returned guard drops.
    pub fn invalidate_on_drop(&self, id: ObjID) -> InvalidateOnDrop {
        InvalidateOnDrop(self.entry(id))
    }
}

impl Default for ExtentTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(map: &ExtentMap, start: u64, nr: u32, create: bool) -> std::vec::Vec<DevicePage> {
        let mut out = Vec::<DevicePage, INLINE_LEN>::new();
        map.emit(start, nr, create, &mut out);
        out.iter().copied().collect()
    }

    fn runs(pages: &[DevicePage]) -> std::vec::Vec<(Option<u64>, u32)> {
        pages
            .iter()
            .map(|p| match p {
                DevicePage::Run(start, len) => (Some(*start), *len),
                DevicePage::Hole(len) => (None, *len),
            })
            .collect()
    }

    #[test]
    fn unknown_pages_stop_the_emit() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 4);
        // Only the first 4 of 8 pages are known.
        assert_eq!(runs(&emit(&map, 0, 8, false)), vec![(Some(100), 4)]);
    }

    #[test]
    fn emit_starts_mid_extent() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 8);
        assert_eq!(runs(&emit(&map, 3, 2, false)), vec![(Some(103), 2)]);
    }

    #[test]
    fn holes_serve_reads_but_not_allocation() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 2);
        map.insert(2, None, 2);
        map.insert(4, Some(200), 2);

        assert_eq!(
            runs(&emit(&map, 0, 6, false)),
            vec![(Some(100), 2), (None, 2), (Some(200), 2)]
        );
        // Under create the hole must be allocated, so the emit stops short of it.
        assert_eq!(runs(&emit(&map, 0, 6, true)), vec![(Some(100), 2)]);
    }

    #[test]
    fn covers_agrees_with_emit() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 4);
        map.insert(4, None, 2);

        // Everything `emit` serves for a read, `covers` reports -- holes included, since a read
        // needs no block for one.
        assert!(map.covers(0, 6));
        assert!(map.covers(3, 2));
        assert!(!map.covers(0, 8));
        assert!(!map.covers(6, 1));
    }

    #[test]
    fn contiguous_inserts_coalesce() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 4);
        map.insert(4, Some(104), 4);
        assert_eq!(map.runs.len(), 1);
        assert_eq!(runs(&emit(&map, 0, 8, false)), vec![(Some(100), 8)]);

        // A discontiguous physical run stays separate.
        map.insert(8, Some(500), 4);
        assert_eq!(map.runs.len(), 2);
    }

    #[test]
    fn insert_fills_a_gap_between_neighbours() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 2);
        map.insert(4, Some(104), 2);
        map.insert(2, Some(102), 2);
        assert_eq!(map.runs.len(), 1);
        assert_eq!(runs(&emit(&map, 0, 6, false)), vec![(Some(100), 6)]);
    }

    #[test]
    fn overwrite_splits_the_straddling_extent() {
        let mut map = ExtentMap::new(0);
        map.insert(0, Some(100), 10);
        // A page-out allocating page 4 elsewhere must not leave the old mapping behind.
        map.insert(4, Some(900), 1);
        assert_eq!(
            runs(&emit(&map, 0, 10, false)),
            vec![(Some(100), 4), (Some(900), 1), (Some(105), 5)]
        );
    }

    #[test]
    fn hole_overwritten_by_allocation() {
        let mut map = ExtentMap::new(0);
        map.insert(0, None, 8);
        map.insert(0, Some(300), 8);
        assert_eq!(runs(&emit(&map, 0, 8, true)), vec![(Some(300), 8)]);
    }

    #[test]
    fn fragmentation_bound_clears_rather_than_grows() {
        let mut map = ExtentMap::new(0);
        // Deliberately discontiguous so nothing coalesces.
        for i in 0..(MAX_EXTENTS_PER_OBJECT as u64 + 8) {
            map.insert(i * 2, Some(i * 1000), 1);
        }
        assert!(map.runs.len() <= MAX_EXTENTS_PER_OBJECT);
    }

    #[test]
    fn stale_generation_reads_as_unknown() {
        let obj = ObjectExtents::new();
        let generation = obj.generation();
        obj.commit(generation, &[(0, Some(100), 4)]);

        let mut out = Vec::<DevicePage, INLINE_LEN>::new();
        assert_eq!(obj.emit(0, 4, false, &mut out).0, 4);

        obj.invalidate();
        let mut out = Vec::<DevicePage, INLINE_LEN>::new();
        assert_eq!(obj.emit(0, 4, false, &mut out).0, 0);
    }

    #[test]
    fn commit_racing_invalidation_is_dropped() {
        let obj = ObjectExtents::new();
        // Walk snapshots the generation, then the object is invalidated under it.
        let generation = obj.generation();
        obj.invalidate();
        obj.commit(generation, &[(0, Some(100), 4)]);

        let mut out = Vec::<DevicePage, INLINE_LEN>::new();
        assert_eq!(obj.emit(0, 4, false, &mut out).0, 0);
    }
}
