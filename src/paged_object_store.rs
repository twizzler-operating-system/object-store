pub type ObjID = u128;

use core::str;
use std::{io::ErrorKind, ops::Add};

#[cfg(target_os = "twizzler")]
use twizzler::Result;
#[cfg(target_os = "twizzler")]
pub use twizzler_abi::pager::PhysRange;

#[cfg(not(target_os = "twizzler"))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PhysRange {
    pub start: u64,
    pub end: u64,
}
#[cfg(not(target_os = "twizzler"))]
use std::io::Result;

use crate::PAGE_SIZE;

const PAGED_MEM_WIRED: u32 = 1;
const PAGED_MEM_COMPLETED: u32 = 2;
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagedPhysMem {
    pub range: PhysRange,
    flags: u32,
}

impl core::ops::Add<u64> for PagedPhysMem {
    type Output = Self;

    fn add(self, rhs: u64) -> Self::Output {
        if rhs == 0 {
            Self {
                range: self.range,
                flags: self.flags,
            }
        } else {
            Self {
                range: PhysRange {
                    start: self.range.end,
                    end: self.range.end + PAGE_SIZE as u64,
                },
                flags: self.flags,
            }
        }
    }
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
}

#[derive(Clone, Copy, Debug)]
pub enum DevicePage {
    Run(u64, u32),
    Hole(u32),
}

impl DevicePage {
    pub fn from_array(array: &[u64]) -> Vec<Self> {
        let mut tmp = Vec::<Self>::new();
        for item in array {
            let item = if *item == 0 {
                DevicePage::Hole(1)
            } else {
                DevicePage::Run(*item, 1)
            };
            if let Some(prev) = tmp.last_mut() {
                if !prev.try_extend(&item) {
                    tmp.push(item);
                }
            } else {
                tmp.push(item);
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
}

pub trait PagedDevice {
    /// Append the needed paged phys mem for this device page, return the number of appended pages.
    fn phys_addrs(&self, _start: DevicePage, _phys_list: &mut Vec<PagedPhysMem>) -> Result<usize> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn sequential_read(&self, start: u64, list: &[PhysRange]) -> Result<usize>;
    fn sequential_write(&self, start: u64, list: &[PhysRange]) -> Result<usize>;

    fn len(&self) -> Result<usize>;
}

#[derive(Debug)]
pub struct PageRequest {
    pub start_page: i64,
    pub nr_pages: u32,
    pub completed: u32,
    pub phys_list: Vec<PagedPhysMem>,
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

    pub fn new_from_list(phys_list: Vec<PagedPhysMem>, start_page: i64, nr_pages: u32) -> Self {
        Self {
            start_page,
            phys_list,
            nr_pages,
            completed: 0,
        }
    }

    pub fn into_list(self) -> Vec<PagedPhysMem> {
        self.phys_list
    }

    fn setup_phys(&mut self, disk_pages: &[DevicePage], device: &dyn PagedDevice) -> Result<()> {
        // TODO: recover these
        self.phys_list.clear();
        for page in disk_pages {
            let mut count = 0;
            while count < page.nr_pages() {
                match device.phys_addrs(*page, &mut self.phys_list) {
                    Ok(r) => {
                        count += r;
                    }
                    Err(e) if Into::<std::io::Error>::into(e).kind() == ErrorKind::OutOfMemory => {
                        if self.phys_list.is_empty() {
                            return Err(e);
                        } else {
                            break;
                        }
                    }
                    Err(e) => Err(e)?,
                }
            }

            if count < page.nr_pages() {
                break;
            }
        }

        self.nr_pages =
            self.phys_list
                .iter()
                .fold(0usize, |acc, range| acc + range.len() / PAGE_SIZE) as u32;
        self.completed =
            self.phys_list
                .iter()
                .filter(|p| p.is_completed())
                .fold(0usize, |acc, range| acc + range.len() / PAGE_SIZE) as u32;
        Ok(())
    }

    pub fn page_in(
        &mut self,
        disk_pages: &[DevicePage],
        device: &dyn PagedDevice,
    ) -> Result<usize> {
        self.setup_phys(disk_pages, device)?;
        if self.phys_list.iter().all(|p| p.is_completed()) {
            return Ok(self.nr_pages as usize);
        }

        let mut cursor = 0;
        let mut inner_cursor = 0;
        let mut tfer_count = 0;
        let mut tmp: Vec<PhysRange> = Vec::new();
        for disk_page in disk_pages {
            let mut count = 0;
            tmp.clear();
            while count < disk_page.nr_pages() {
                let thislen = (disk_page.nr_pages() - count)
                    .min(self.phys_list[cursor].nr_pages() - inner_cursor);

                let new_range = PhysRange {
                    start: self.phys_list[cursor].range.start + (inner_cursor * PAGE_SIZE) as u64,
                    end: self.phys_list[cursor].range.start
                        + (inner_cursor * PAGE_SIZE) as u64
                        + (thislen * PAGE_SIZE) as u64,
                };

                tmp.push(new_range);

                inner_cursor += thislen;
                if inner_cursor >= self.phys_list[cursor].nr_pages() {
                    cursor += 1;
                    inner_cursor = 0;
                    if cursor >= self.phys_list.len() {
                        break;
                    }
                }

                count += thislen;
            }

            if let DevicePage::Run(start, _len) = disk_page {
                let mut count = 0;
                while count < tmp.len() {
                    let r = device.sequential_read(*start + count as u64, &tmp[count..])?;
                    count += r;
                }
            }

            tfer_count += count;

            if cursor >= self.phys_list.len() {
                break;
            }
        }

        Ok(tfer_count)
    }

    pub fn page_out(
        &mut self,
        disk_pages: &[DevicePage],
        device: &dyn PagedDevice,
    ) -> Result<usize> {
        let mut cursor = 0;
        let mut inner_cursor = 0;
        let mut tfer_count = 0;
        let mut tmp: Vec<PhysRange> = Vec::new();
        for disk_page in disk_pages {
            let mut count = 0;
            tmp.clear();
            while count < disk_page.nr_pages() {
                let thislen = (disk_page.nr_pages() - count)
                    .min(self.phys_list[cursor].nr_pages() - inner_cursor);

                let new_range = PhysRange {
                    start: self.phys_list[cursor].range.start + (inner_cursor * PAGE_SIZE) as u64,
                    end: self.phys_list[cursor].range.start
                        + (inner_cursor * PAGE_SIZE) as u64
                        + (thislen * PAGE_SIZE) as u64,
                };

                tmp.push(new_range);

                inner_cursor += thislen;
                if inner_cursor >= self.phys_list[cursor].nr_pages() {
                    cursor += 1;
                    inner_cursor = 0;
                    if cursor >= self.phys_list.len() {
                        break;
                    }
                }

                count += thislen;
            }

            if let DevicePage::Run(start, _len) = disk_page {
                let mut count = 0;
                while count < tmp.len() {
                    let r = device.sequential_write(*start + count as u64, &tmp[count..])?;
                    count += r;
                }
            }

            tfer_count += count;

            if cursor >= self.phys_list.len() {
                break;
            }
        }

        Ok(tfer_count)
    }
}

pub trait PagedObjectStore {
    fn get_config_id(&self) -> Result<ObjID> {
        let mut buf = [0; 16];
        self.read_object(0, 0, &mut buf).and_then(|len| {
            if len == 16 && buf.iter().find(|x| **x != 0).is_some() {
                Ok(ObjID::from_le_bytes(buf))
            } else {
                Err(ErrorKind::InvalidData.into())
            }
        })
    }

    fn set_config_id(&self, id: ObjID) -> Result<()> {
        let _ = self.delete_object(0);
        self.create_object(0)?;
        self.write_object(0, 0, &id.to_le_bytes())
    }

    fn create_object(&self, id: ObjID) -> Result<()>;
    fn delete_object(&self, id: ObjID) -> Result<()>;

    fn len(&self, id: ObjID) -> Result<u64>;

    fn read_object(&self, id: ObjID, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn write_object(&self, id: ObjID, offset: u64, buf: &[u8]) -> Result<()>;

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest]) -> Result<usize>;
    fn page_out_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest]) -> Result<usize>;

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn enumerate_external(&self, _id: ObjID) -> Result<Vec<ExternalFile>> {
        Err(ErrorKind::Unsupported.into())
    }

    fn find_external(&self, _id: ObjID) -> Result<usize> {
        Err(ErrorKind::Unsupported.into())
    }
}

pub const MAX_EXTERNAL_PATH: usize = 4096;
pub const NAME_MAX: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Ord, Eq, Hash)]
#[repr(C)]
pub struct ExternalFile {
    pub id: ObjID,
    pub name: [u8; NAME_MAX],
    pub name_len: u32,
    pub kind: ExternalKind,
}

impl ExternalFile {
    pub fn new(iname: &[u8], kind: ExternalKind, id: ObjID) -> Self {
        let name_len = iname.len().min(NAME_MAX);
        let sname = &iname[0..name_len];
        let mut name = [0; NAME_MAX];
        name[0..name_len].copy_from_slice(&sname);
        Self {
            id,
            name,
            kind,
            name_len: name_len as u32,
        }
    }

    pub fn name(&self) -> Option<&str> {
        str::from_utf8(&self.name[0..(self.name_len as usize)]).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Ord, Eq, Hash)]
#[repr(u32)]
pub enum ExternalKind {
    Regular,
    Directory,
    SymLink,
    Other,
}

pub fn objid_to_ino(id: ObjID) -> Option<u32> {
    if id == 1 {
        return Some(0);
    };
    let (hi, lo) = ((id >> 64) as u64, id as u64);
    if hi == (1u64 << 63) {
        let ino = lo & !(1u64 << 63);
        Some(ino as u32)
    } else {
        None
    }
}

pub fn ino_to_objid(ino: u32) -> ObjID {
    if ino == 0 {
        return 1;
    }
    (1u128 << 127) | (ino as u128) | (1u128 << 63)
}

pub(crate) fn _consecutive_slices<T: PartialEq + Add<u64> + Copy>(
    data: &[T],
) -> impl Iterator<Item = &[T]>
where
    T::Output: PartialEq<T>,
{
    let mut slice_start = 0;
    (1..=data.len()).flat_map(move |i| {
        if i == data.len() || data[i - 1] + 1u64 != data[i] {
            let begin = slice_start;
            slice_start = i;
            Some(&data[begin..i])
        } else {
            None
        }
    })
}

pub trait PosIo {
    fn read(&self, start: u64, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, start: u64, buf: &[u8]) -> Result<usize>;
}
