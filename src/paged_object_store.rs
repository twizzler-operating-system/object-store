#![allow(async_fn_in_trait)]
pub type ObjID = u128;

use std::{future::Future, io::ErrorKind, path::Path, thread::yield_now};

use async_io::block_on;
use libc::mode_t;
pub use pager_dynamic::{
    ino_to_objid, objid_to_ino, ExternalFile, ExternalFileSbHdr, ExternalKind,
};
use twizzler::Result;
pub use twizzler_abi::pager::PhysRange;

use crate::PAGE_SIZE;

const PAGED_MEM_WIRED: u32 = 1;
const PAGED_MEM_COMPLETED: u32 = 2;
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagedPhysMem {
    pub range: PhysRange,
    flags: u32,
}

impl PagedPhysMem {
    pub fn new(range: PhysRange) -> Self {
        PagedPhysMem { range, flags: 0 }
    }

    pub fn is_completed(&self) -> bool {
        self.flags & PAGED_MEM_COMPLETED != 0
    }

    pub fn is_wired(&self) -> bool {
        self.flags & PAGED_MEM_WIRED != 0
    }

    pub fn set_completed(&mut self) {
        self.flags |= PAGED_MEM_COMPLETED;
    }

    pub fn completed(mut self) -> Self {
        self.set_completed();
        self
    }

    pub fn wired(mut self) -> Self {
        self.set_wired();
        self
    }

    pub fn set_wired(&mut self) {
        self.flags |= PAGED_MEM_WIRED;
    }

    pub fn len(&self) -> usize {
        (self.range.end - self.range.start) as usize
    }

    pub fn nr_pages(&self) -> usize {
        self.len() / PAGE_SIZE
    }

    /// True if `other` carries the same flags, so the two may be reported as one run.
    pub fn same_flags(&self, other: &Self) -> bool {
        self.flags == other.flags
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DevicePage {
    Run(u64, u32),
    Hole(u32),
}

impl DevicePage {
    pub fn from_array(array: &[u64]) -> Vec<Self, MAYHEAP_LEN> {
        let mut tmp = Vec::<Self, MAYHEAP_LEN>::new();
        for item in array {
            let item = if *item == 0 {
                DevicePage::Hole(1)
            } else {
                DevicePage::Run(*item, 1)
            };
            if let Some(prev) = tmp.last_mut() {
                if !prev.try_extend(&item) {
                    tmp.push(item).unwrap();
                }
            } else {
                tmp.push(item).unwrap();
            }
        }
        tmp
    }

    pub fn try_extend(&mut self, other: &DevicePage) -> bool {
        let new_val = match (*self, other) {
            (DevicePage::Hole(len1), &DevicePage::Hole(len2)) => {
                Some(DevicePage::Hole(len1 + len2))
            }
            (DevicePage::Run(start1, len1), &DevicePage::Run(start2, len2)) => {
                if start1 + len1 as u64 == start2 {
                    Some(DevicePage::Run(start1, len1 + len2))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(new_val) = new_val {
            *self = new_val;
            true
        } else {
            false
        }
    }

    pub fn nr_pages(&self) -> usize {
        match self {
            DevicePage::Run(_, len) => *len as usize,
            DevicePage::Hole(len) => *len as usize,
        }
    }

    pub fn as_hole(&self) -> Option<u32> {
        match self {
            DevicePage::Hole(len) => Some(*len),
            _ => None,
        }
    }

    pub fn offset(&mut self, avail_len: &mut usize) {
        let new = match self {
            DevicePage::Run(start, len) => {
                let new_len = len.saturating_sub(*avail_len as u32);
                let diff = *len - new_len;
                *avail_len = diff as usize;
                DevicePage::Run(*start + diff as u64, new_len)
            }
            DevicePage::Hole(len) => {
                let new_len = len.saturating_sub(*avail_len as u32);
                let diff = *len - new_len;
                *avail_len = diff as usize;
                DevicePage::Hole(new_len)
            }
        };
        *self = new;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_extend_holes() {
        let mut hole1 = DevicePage::Hole(10);
        let hole2 = DevicePage::Hole(5);

        assert!(hole1.try_extend(&hole2));
        match hole1 {
            DevicePage::Hole(len) => assert_eq!(len, 15),
            _ => panic!("Expected Hole"),
        }
    }

    #[test]
    fn test_try_extend_consecutive_runs() {
        let mut run1 = DevicePage::Run(100, 10);
        let run2 = DevicePage::Run(110, 5);

        assert!(run1.try_extend(&run2));
        match run1 {
            DevicePage::Run(start, len) => {
                assert_eq!(start, 100);
                assert_eq!(len, 15);
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_try_extend_non_consecutive_runs() {
        let mut run1 = DevicePage::Run(100, 10);
        let run2 = DevicePage::Run(120, 5);

        assert!(!run1.try_extend(&run2));
        match run1 {
            DevicePage::Run(start, len) => {
                assert_eq!(start, 100);
                assert_eq!(len, 10); // Should remain unchanged
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_try_extend_mixed_types() {
        let mut hole = DevicePage::Hole(10);
        let run = DevicePage::Run(100, 5);

        assert!(!hole.try_extend(&run));
        match hole {
            DevicePage::Hole(len) => assert_eq!(len, 10), // Should remain unchanged
            _ => panic!("Expected Hole"),
        }

        let mut run = DevicePage::Run(100, 10);
        let hole = DevicePage::Hole(5);

        assert!(!run.try_extend(&hole));
        match run {
            DevicePage::Run(start, len) => {
                assert_eq!(start, 100);
                assert_eq!(len, 10); // Should remain unchanged
            }
            _ => panic!("Expected Run"),
        }
    }

    struct MockDevice {
        /// Total pages phys_addrs hands out, regardless of how many were asked for.
        alloc_pages: usize,
        /// Pages per phys_list entry.
        alloc_chunk: usize,
        /// If set, what a transfer reports instead of the actually-available count.
        forced_tfer: Option<usize>,
    }

    impl PagedDevice for MockDevice {
        async fn phys_addrs(
            &self,
            _start_obj_page: i64,
            _nr_obj_pages: u32,
            _pages: &[DevicePage],
            phys_list: &mut Vec<PagedPhysMem, MAYHEAP_LEN>,
        ) -> Result<()> {
            let mut done = 0;
            while done < self.alloc_pages {
                let n = self.alloc_chunk.min(self.alloc_pages - done);
                let start = (done * PAGE_SIZE) as u64;
                phys_list
                    .push(PagedPhysMem::new(PhysRange {
                        start,
                        end: start + (n * PAGE_SIZE) as u64,
                    }))
                    .unwrap();
                done += n;
            }
            Ok(())
        }

        async fn sequential_read(
            &self,
            _start: u64,
            nr_pages: usize,
            list: &[PagedPhysMem],
            inner_cursor: usize,
        ) -> Result<usize> {
            if let Some(forced) = self.forced_tfer {
                return Ok(forced);
            }
            assert!(
                !list.is_empty(),
                "transfer issued with an exhausted phys list"
            );
            let avail = list.iter().fold(0usize, |acc, p| acc + p.nr_pages()) - inner_cursor;
            Ok(nr_pages.min(avail))
        }

        async fn sequential_write(
            &self,
            start: u64,
            nr_pages: usize,
            list: &[PagedPhysMem],
            inner_cursor: usize,
        ) -> Result<usize> {
            self.sequential_read(start, nr_pages, list, inner_cursor)
                .await
        }

        async fn len(&self) -> Result<usize> {
            Ok(self.alloc_pages * PAGE_SIZE)
        }
    }

    /// A run longer than the phys memory we got must stop at the end of the list rather than
    /// re-issuing transfers against an empty slice.
    #[test]
    fn test_page_in_short_phys_alloc() {
        let dev = MockDevice {
            alloc_pages: 4,
            alloc_chunk: 4,
            forced_tfer: None,
        };
        let mut req = PageRequest::new(0, 8);
        let n = block_on(req.page_in(&[DevicePage::Run(100, 8)], &dev)).unwrap();
        assert_eq!(n, 4);
        assert_eq!(req.nr_pages, 4);
    }

    #[test]
    fn test_page_out_short_phys_alloc() {
        let dev = MockDevice {
            alloc_pages: 4,
            alloc_chunk: 4,
            forced_tfer: None,
        };
        let mut req = PageRequest::new(0, 8);
        block_on(req.setup_phys(&[DevicePage::Run(100, 8)], &dev)).unwrap();
        let n = block_on(req.page_out(&[DevicePage::Run(100, 8)], &dev)).unwrap();
        assert_eq!(n, 4);
    }

    /// A device reporting zero transferred pages is an error, not a reason to spin.
    #[test]
    fn test_page_in_no_progress_is_error() {
        let dev = MockDevice {
            alloc_pages: 8,
            alloc_chunk: 8,
            forced_tfer: Some(0),
        };
        let mut req = PageRequest::new(0, 8);
        assert!(block_on(req.page_in(&[DevicePage::Run(100, 8)], &dev)).is_err());
    }

    /// Entries no page landed in are dropped, not retained as zero-length ranges.
    #[test]
    fn test_page_in_truncates_untouched_entries() {
        let dev = MockDevice {
            alloc_pages: 4,
            alloc_chunk: 2,
            forced_tfer: None,
        };
        let mut req = PageRequest::new(0, 2);
        let n = block_on(req.page_in(&[DevicePage::Run(100, 2)], &dev)).unwrap();
        assert_eq!(n, 2);
        assert_eq!(req.phys_list.len(), 1);
        assert_eq!(req.nr_pages, 2);
        assert_eq!(req.completed, 2);
    }
}

pub trait PagedDevice {
    /// Append the needed paged phys mem for this device page, return the number of appended pages.
    async fn phys_addrs(
        &self,
        _start_obj_page: i64,
        _nr_obj_pages: u32,
        _pages: &[DevicePage],
        _phys_list: &mut Vec<PagedPhysMem, MAYHEAP_LEN>,
    ) -> Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    async fn free_phys_range(&self, _range: PhysRange) {}

    async fn sequential_read(
        &self,
        start: u64,
        nr_pages: usize,
        list: &[PagedPhysMem],
        inner_cursor: usize,
    ) -> Result<usize>;
    async fn sequential_write(
        &self,
        start: u64,
        nr_pages: usize,
        list: &[PagedPhysMem],
        inner_cursor: usize,
    ) -> Result<usize>;

    async fn len(&self) -> Result<usize>;

    fn yield_now(&self) {
        yield_now();
    }

    fn run_async<R: 'static>(&self, f: impl Future<Output = R>) -> R {
        block_on(f)
    }
}

use mayheap::Vec;

pub const MAYHEAP_LEN: usize = 16;
#[derive(Debug)]
pub struct PageRequest {
    pub start_page: i64,
    pub nr_pages: u32,
    pub completed: u32,
    pub phys_list: Vec<PagedPhysMem, MAYHEAP_LEN>,
}

impl PageRequest {
    pub fn new(start_page: i64, nr_pages: u32) -> Self {
        Self {
            start_page,
            phys_list: Vec::new(),
            nr_pages,
            completed: 0,
        }
    }

    pub fn new_from_list(
        phys_list: Vec<PagedPhysMem, MAYHEAP_LEN>,
        start_page: i64,
        nr_pages: u32,
    ) -> Self {
        Self {
            start_page,
            phys_list,
            nr_pages,
            completed: 0,
        }
    }

    pub fn into_list(self) -> Vec<PagedPhysMem, MAYHEAP_LEN> {
        self.phys_list
    }

    async fn setup_phys<PD: PagedDevice>(
        &mut self,
        disk_pages: &[DevicePage],
        device: &PD,
    ) -> Result<()> {
        tracing::trace!(
            "setup_phys: start_page = {}, nr_pages = {}, disk_pages = {:?}",
            self.start_page,
            self.nr_pages,
            disk_pages
        );
        let nr_pages = disk_pages
            .iter()
            .fold(0usize, |acc, range| acc + range.nr_pages());
        device
            .phys_addrs(
                self.start_page,
                nr_pages as u32,
                disk_pages,
                &mut self.phys_list,
            )
            .await?;

        // We may have allocated fewer pages, so recalculate nr_pages.
        self.nr_pages = self
            .phys_list
            .iter()
            .fold(0usize, |acc, range| acc + range.nr_pages())
            .min(nr_pages) as u32;
        self.completed = self
            .phys_list
            .iter()
            .filter(|p| p.is_completed())
            .fold(0usize, |acc, range| acc + range.nr_pages())
            .min(nr_pages) as u32;
        Ok(())
    }

    fn advance(
        &mut self,
        mut cursor: usize,
        mut inner_cursor: usize,
        mut count: usize,
    ) -> (usize, usize) {
        while count > 0 && cursor < self.phys_list.len() {
            if self.phys_list[cursor].is_completed() {
                cursor += 1;
                inner_cursor = 0;
                continue;
            }
            if count >= self.phys_list[cursor].nr_pages() - inner_cursor {
                count -= self.phys_list[cursor].nr_pages() - inner_cursor;
                self.phys_list[cursor].set_completed();
                self.completed += self.phys_list[cursor].nr_pages() as u32;
                cursor += 1;
                inner_cursor = 0;
            } else {
                inner_cursor += count;
                count = 0;
            }
        }

        (cursor, inner_cursor)
    }

    pub async fn page_in<PD: PagedDevice>(
        &mut self,
        disk_pages: &[DevicePage],
        device: &PD,
    ) -> Result<usize> {
        let _time0 = std::time::Instant::now();
        self.setup_phys(disk_pages, device).await?;
        if self.phys_list.iter().all(|p| p.is_completed()) {
            return Ok(self.nr_pages as usize);
        }
        let _time1 = std::time::Instant::now();

        let mut cursor = self
            .phys_list
            .iter()
            .position(|p| !p.is_completed())
            .unwrap_or(0);
        let mut inner_cursor = 0;
        let mut tfer_count = 0;
        for disk_page in disk_pages {
            tracing::trace!(
                "page_in: disk_page = {:?}, cursor = {}, tfer_count = {}: {:?}",
                disk_page,
                cursor,
                tfer_count,
                &self.phys_list[cursor..]
            );
            let count = match disk_page {
                DevicePage::Hole(len) => {
                    (cursor, inner_cursor) = self.advance(cursor, inner_cursor, *len as usize);
                    *len as usize
                }
                DevicePage::Run(start, len) => {
                    let mut count = 0;
                    while count < *len as usize {
                        // setup_phys tolerates a short allocation, so we can run out of phys
                        // memory partway through a run. Stop here and let the truncation path
                        // below report what we actually transferred.
                        if cursor >= self.phys_list.len() {
                            break;
                        }
                        let r = device
                            .sequential_read(
                                *start + count as u64,
                                *len as usize - count,
                                &self.phys_list[cursor..],
                                inner_cursor,
                            )
                            .await
                            .inspect_err(|e| tracing::error!("read err: {}", e))?;
                        if r == 0 {
                            tracing::error!(
                                "page_in: no progress reading {} pages at {} (cursor {})",
                                *len as usize - count,
                                *start + count as u64,
                                cursor
                            );
                            return Err(ErrorKind::UnexpectedEof.into());
                        }

                        (cursor, inner_cursor) = self.advance(cursor, inner_cursor, r);
                        count += r;
                    }
                    count
                }
            };

            tfer_count += count;

            if cursor >= self.phys_list.len() {
                break;
            }
        }
        let _time2 = std::time::Instant::now();

        tracing::trace!(
            "timings: setup_phys = {}, page_in = {}",
            (_time1 - _time0).as_millis(),
            (_time2 - _time1).as_millis()
        );

        if tfer_count
            < self
                .phys_list
                .iter()
                .fold(0usize, |acc, range| acc + range.nr_pages())
        {
            // With inner_cursor == 0 nothing landed in the entry at cursor, so drop it entirely
            // rather than retaining a zero-length range that callers would count as no pages.
            let truncate = if inner_cursor == 0 {
                cursor
            } else {
                cursor + 1
            };
            while cursor < self.phys_list.len() {
                let range = &mut self.phys_list[cursor];
                let adj_range = PhysRange {
                    start: range.range.start + inner_cursor as u64 * PAGE_SIZE as u64,
                    end: range.range.end,
                };
                range.range = PhysRange {
                    start: range.range.start,
                    end: adj_range.start,
                };
                device.free_phys_range(adj_range).await;

                cursor += 1;
                inner_cursor = 0;
            }
            self.phys_list.truncate(truncate);

            // Keep the request self-consistent with the shortened list.
            self.nr_pages =
                self.phys_list
                    .iter()
                    .fold(0usize, |acc, range| acc + range.nr_pages()) as u32;
            self.completed =
                self.phys_list
                    .iter()
                    .filter(|p| p.is_completed())
                    .fold(0usize, |acc, range| acc + range.nr_pages()) as u32;
        }

        Ok(tfer_count)
    }

    pub async fn page_out<PD: PagedDevice>(
        &mut self,
        disk_pages: &[DevicePage],
        device: &PD,
    ) -> Result<usize> {
        if self.phys_list.iter().all(|p| p.is_completed()) {
            return Ok(self.nr_pages as usize);
        }
        let mut cursor = self
            .phys_list
            .iter()
            .position(|p| !p.is_completed())
            .unwrap_or(0);
        let mut inner_cursor = 0;
        let mut tfer_count = 0;
        for disk_page in disk_pages {
            tracing::trace!(
                "page_out: disk_page = {:?}, cursor = {}, tfer_count = {}: {:?}",
                disk_page,
                cursor,
                tfer_count,
                &self.phys_list[cursor..]
            );
            let count = match disk_page {
                DevicePage::Hole(len) => {
                    tracing::error!(
                        "page_out: encountered hole of length {} at cursor {}",
                        len,
                        cursor
                    );
                    (cursor, inner_cursor) = self.advance(cursor, inner_cursor, *len as usize);
                    *len as usize
                }
                DevicePage::Run(start, len) => {
                    let mut count = 0;
                    while count < *len as usize {
                        if cursor >= self.phys_list.len() {
                            break;
                        }
                        let r = device
                            .sequential_write(
                                *start + count as u64,
                                *len as usize - count,
                                &self.phys_list[cursor..],
                                inner_cursor,
                            )
                            .await
                            .inspect_err(|e| tracing::error!("write err: {}", e))?;
                        if r == 0 {
                            tracing::error!(
                                "page_out: no progress writing {} pages at {} (cursor {})",
                                *len as usize - count,
                                *start + count as u64,
                                cursor
                            );
                            return Err(ErrorKind::UnexpectedEof.into());
                        }

                        (cursor, inner_cursor) = self.advance(cursor, inner_cursor, r);
                        count += r;
                    }
                    count
                }
            };

            tfer_count += count;

            if cursor >= self.phys_list.len() {
                break;
            }
        }

        Ok(tfer_count)
    }
}

pub trait PagedObjectStore {
    async fn get_config_id(&self) -> Result<ObjID> {
        let mut buf = [0; 16];
        self.read_object(0, 0, &mut buf).await.and_then(|len| {
            if len == 16 && buf.iter().find(|x| **x != 0).is_some() {
                Ok(ObjID::from_le_bytes(buf))
            } else {
                Err(ErrorKind::InvalidData.into())
            }
        })
    }

    async fn set_config_id(&self, id: ObjID) -> Result<()> {
        let _ = self.delete_object(0).await;
        self.create_object(0).await?;
        self.write_object(0, 0, &id.to_le_bytes()).await
    }

    async fn create_object(&self, id: ObjID) -> Result<()>;
    async fn delete_object(&self, id: ObjID) -> Result<()>;

    async fn len(&self, id: ObjID) -> Result<u64>;

    async fn read_object(&self, id: ObjID, offset: u64, buf: &mut [u8]) -> Result<usize>;
    async fn write_object(&self, id: ObjID, offset: u64, buf: &[u8]) -> Result<()>;

    /// Fill `out` with the device pages backing object pages `[start_page, start_page + nr_pages)`
    /// of `id`.
    ///
    /// With `create`, holes in the range are allocated backing blocks, so `out` contains no
    /// `DevicePage::Hole`. Without it, holes are reported as such.
    ///
    /// Not async: this is locks and arithmetic, and an implementation that caches its mappings can
    /// answer entirely without touching the store's own locks.
    fn get_disk_blocks(
        &self,
        id: ObjID,
        start_page: u64,
        nr_pages: u32,
        create: bool,
        out: &mut Vec<DevicePage, MAYHEAP_LEN>,
    ) -> Result<()>;

    async fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest]) -> Result<usize>;
    async fn page_out_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest]) -> Result<usize>;

    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ExternalOpenFlags: u32 {
        const READ = 0b0001;
        const WRITE = 0b0010;
        const CREATE = 0b0100;
        const TRUNCATE = 0b1000;
    }
}

pub trait ExternalFileStore {
    async fn open_external(
        &self,
        at: Option<ObjID>,
        path: impl AsRef<Path>,
        flags: ExternalOpenFlags,
        mode: mode_t,
        link_to: Option<ObjID>,
    ) -> Result<ExternalFile>;

    async fn unlink_external(&self, at: Option<ObjID>, path: impl AsRef<Path>) -> Result<()>;
    async fn readlink_external(&self, id: ObjID) -> Result<String>;

    async fn readdir_external(
        &self,
        dir: ObjID,
        skip: usize,
        count: usize,
        entries: &mut std::vec::Vec<ExternalFile>,
    ) -> Result<()>;

    async fn link_external(
        &self,
        file: &ExternalFile,
        at: Option<ObjID>,
        path: impl AsRef<Path>,
    ) -> Result<()>;

    async fn stat_external(&self, path: impl AsRef<Path>) -> Result<libc::stat>;
    async fn fstat_external(&self, file: Option<ObjID>) -> Result<libc::stat>;

    async fn symlink_external(
        &self,
        at: Option<ObjID>,
        target: impl AsRef<Path>,
        linkpath: impl AsRef<Path>,
    ) -> Result<()>;
}

pub trait PosIo {
    async fn read(&self, start: u64, buf: &mut [u8]) -> Result<usize>;
    async fn write(&self, start: u64, buf: &[u8]) -> Result<usize>;
}
