pub type ObjID = u128;

use core::str;
use std::{io::ErrorKind, ops::Add};

use obliviate_core::consts::PAGE_SIZE;
#[cfg(target_os = "twizzler")]
use twizzler::Result;
#[cfg(target_os = "twizzler")]
pub use twizzler_abi::pager::PhysRange;

#[cfg(not(target_os = "twizzler"))]
#[derive(Debug)]
pub struct PhysRange {
    pub start: u64,
    pub end: u64,
}
#[cfg(not(target_os = "twizzler"))]
use std::io::Result;

pub trait PagedDevice {
    fn phys_addrs(
        &self,
        _start: Option<u64>,
        _len: u64,
        _allow_failed_alloc: bool,
    ) -> Result<(PhysRange, bool)> {
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
    pub phys_list: Vec<(PhysRange, bool)>,
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
        phys_list: Vec<(PhysRange, bool)>,
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

    pub fn into_list(self) -> Vec<(PhysRange, bool)> {
        self.phys_list
    }

    fn setup_phys(&mut self, disk_pages: &[Option<u64>], device: &dyn PagedDevice) -> Result<()> {
        // TODO: recover these
        self.phys_list.clear();
        self.completed = 0;
        for page in disk_pages {
            let range = match device.phys_addrs(*page, PAGE_SIZE as u64, !self.phys_list.is_empty())
            {
                Ok(range) => range,
                Err(e) if Into::<std::io::Error>::into(e).kind() == ErrorKind::OutOfMemory => {
                    if self.phys_list.is_empty() {
                        return Err(e);
                    } else {
                        self.nr_pages = self.phys_list.iter().fold(0u64, |acc, range| {
                            acc + (range.0.end - range.0.start) / PAGE_SIZE as u64
                        }) as u32;
                        return Ok(());
                    }
                }
                Err(e) => Err(e)?,
            };
            if range.1 {
                self.completed += ((range.0.end - range.0.start) / PAGE_SIZE as u64) as u32;
            }
            self.phys_list.push(range);
        }
        self.nr_pages = self.phys_list.iter().fold(0u64, |acc, range| {
            acc + (range.0.end - range.0.start) / PAGE_SIZE as u64
        }) as u32;
        Ok(())
    }

    pub fn page_in(
        &mut self,
        disk_pages: &[Option<u64>],
        device: &dyn PagedDevice,
    ) -> Result<usize> {
        self.setup_phys(disk_pages, device)?;
        if self.phys_list.iter().all(|p| p.1) {
            return Ok(self.nr_pages as usize);
        }
        let mut pairs = disk_pages
            .iter()
            .zip(&self.phys_list)
            // Has disk pages to read
            .filter_map(|x| x.0.map(|y| (y, x.1)))
            // Has not completed
            .filter_map(|x| if x.1 .1 { None } else { Some((x.0, x.1 .0)) })
            .collect::<Vec<_>>();
        pairs.sort_by_key(|p| p.0);
        let (dp, pp): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let mut offset = 0;
        let runs = consecutive_slices(&dp).map(|run| {
            let pair = (run, &pp[offset..(offset + run.len())]);
            offset += run.len();
            pair
        });
        let mut count = 0;
        for (dp, mut pp) in runs {
            let mut offset = 0;
            while pp.len() > 0 {
                let len = device.sequential_read(dp[0] + offset as u64, pp)?;
                count += len;
                offset += len;
                pp = &pp[len..];
            }
        }
        Ok(count + self.completed as usize)
    }

    pub fn page_out(
        &mut self,
        disk_pages: &[Option<u64>],
        device: &dyn PagedDevice,
    ) -> Result<usize> {
        let mut pairs = disk_pages
            .iter()
            .zip(&self.phys_list)
            // Has disk pages to read
            .filter_map(|x| x.0.map(|y| (y, x.1)))
            // Has not completed
            .filter_map(|x| if x.1 .1 { None } else { Some((x.0, x.1 .0)) })
            .collect::<Vec<_>>();
        pairs.sort_by_key(|p| p.0);
        let (dp, pp): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let mut offset = 0;
        let runs = consecutive_slices(&dp).map(|run| {
            let pair = (run, &pp[offset..(offset + run.len())]);
            offset += run.len();
            pair
        });
        let mut count = 0;
        for (dp, mut pp) in runs {
            let mut offset = 0;
            while pp.len() > 0 {
                let len = device.sequential_write(dp[0] + offset as u64, pp)?;
                count += len;
                offset += len;
                pp = &pp[len..];
            }
        }
        Ok(count)
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

pub(crate) fn consecutive_slices<T: PartialEq + Add<u64> + Copy>(
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
