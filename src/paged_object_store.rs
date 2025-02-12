pub type ObjID = u128;

use std::io::Result;

pub trait PagingImp {
    type PhysAddr;

    fn fill_from_buffer(&mut self, buf: &[u8]);
    fn read_to_buffer(&self, buf: &mut [u8]);

    fn phys_addr(&self) -> &Self::PhysAddr;
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
    fn get_config_id(&self) -> Result<ObjID>;
    fn set_config_id(&self, id: ObjID) -> Result<()>;

    fn create_object(&self, id: ObjID) -> Result<()>;
    fn delete_object(&self, id: ObjID) -> Result<()>;

    fn read_object(&self, id: ObjID, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn write_object(&self, id: ObjID, offset: u64, buf: &[u8]) -> Result<()>;

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [PageRequest<P>]) -> Result<usize>;
    fn page_out_object<'a>(&self, id: ObjID, reqs: &'a [PageRequest<P>]) -> Result<usize>;
}
