use crate::fs::Disk;

use super::Kms;

impl<D> Kms<D>
where
    D: Disk,
    std::io::Error: From<fatfs::Error<D::Error>>,
{
    fn generate_disk_vis(&self) -> String {
        self.khf_lock();
        todo!()
    }
}
