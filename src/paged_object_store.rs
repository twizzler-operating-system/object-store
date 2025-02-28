pub type ObjID = u128;

use std::io::{ErrorKind, Result};

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
}
