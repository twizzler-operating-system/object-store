pub type ObjID = u128;

use std::io::Result;

pub trait PagedObjectStore {
    type PageRequest;

    fn get_config_id(&self) -> Result<ObjID>;
    fn set_config_id(&self, id: ObjID) -> Result<()>;

    fn create_object(&self, id: ObjID) -> Result<()>;
    fn delete_object(&self, id: ObjID) -> Result<()>;

    fn read_object(&self, id: ObjID, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn write_object(&self, id: ObjID, offset: u64, buf: &[u8]) -> Result<()>;

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [Self::PageRequest]) -> Result<usize>;
    fn page_out_object<'a>(&self, id: ObjID, reqs: &'a [Self::PageRequest]) -> Result<usize>;
}
