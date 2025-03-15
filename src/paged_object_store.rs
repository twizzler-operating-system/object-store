pub type ObjID = u128;

use core::str;
use std::{
    io::{ErrorKind, Result},
    path::Path,
};

use obliviate_core::consts::PAGE_SIZE;

pub trait PagingImp {
    type PhysAddr;

    fn page_size() -> usize {
        PAGE_SIZE
    }

    fn fill_from_buffer(&self, buf: &[u8]);
    fn read_to_buffer(&self, buf: &mut [u8]);

    fn phys_addrs(&self) -> impl Iterator<Item = &'_ Self::PhysAddr>;

    fn page_in(&self, _disk_pages: impl Iterator<Item = Option<u64>>) -> std::io::Result<usize> {
        todo!()
    }

    fn page_out(&self, _disk_pages: impl Iterator<Item = Option<u64>>) -> std::io::Result<usize> {
        todo!()
    }
}

#[derive(Debug)]
pub struct PageRequest<P: PagingImp> {
    pub start_page: i64,
    pub imp: P,
    pub nr_pages: u32,
}

impl<P: PagingImp> PageRequest<P> {
    pub fn new(imp: P, start_page: i64, nr_pages: u32) -> Self {
        Self {
            start_page,
            imp,
            nr_pages,
        }
    }
}

pub trait PagedObjectStore<P: PagingImp> {
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

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest<P>]) -> Result<usize> {
        let mut buf = [0; PAGE_SIZE];
        for req in reqs.iter_mut() {
            for i in 0..req.nr_pages {
                let page = req.start_page as usize + i as usize;
                self.read_object(id, (page * PAGE_SIZE) as u64, &mut buf)?;
                req.imp.fill_from_buffer(&buf);
            }
        }
        Ok(reqs.len())
    }

    fn page_out_object<'a>(&self, id: ObjID, reqs: &'a [PageRequest<P>]) -> Result<usize> {
        let mut buf = [0; PAGE_SIZE];
        for req in reqs.iter() {
            for i in 0..req.nr_pages {
                req.imp.read_to_buffer(&mut buf);
                let page = req.start_page as usize + i as usize;
                self.write_object(id, (page * PAGE_SIZE) as u64, &buf)?;
            }
        }
        Ok(reqs.len())
    }

    fn enumerate_external(&self, _id: ObjID) -> std::io::Result<Vec<ExternalFile>> {
        Err(ErrorKind::Unsupported.into())
    }

    fn find_external(&self, _id: ObjID) -> std::io::Result<usize> {
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
