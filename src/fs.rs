use std::sync::{Arc, Mutex};

use fatfs::{
    FatType, FormatVolumeOptions, IoBase, LossyOemCpConverter, NullTimeProvider, ReadWriteSeek,
};

pub trait Disk: fatfs::ReadWriteSeek + IoBase + Clone {}

impl<T> Disk for T where T: ReadWriteSeek + IoBase + Clone {}
#[derive(Clone)]
pub(crate) struct FileSystem<D: Disk> {
    disk: D,
    fs: Arc<Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>>>,
}

pub const PAGE_SIZE: usize = 4096;
pub const SECTOR_SIZE: usize = 512;

impl<D: Disk> FileSystem<D> {
    pub fn format(disk: &mut D) {
        println!("reformatting");
        let options = FormatVolumeOptions::new()
            .bytes_per_sector(SECTOR_SIZE as u16)
            .bytes_per_cluster(PAGE_SIZE as u32)
            .fat_type(FatType::Fat32);
        fatfs::format_volume(disk, options).unwrap();
    }
    /// Will attempt to open the filesystem
    /// and will reformat the filesystem if it is unable to open it
    pub fn open_fs(mut disk: D) -> FileSystem<D> {
        let fs_options = fatfs::FsOptions::new().update_accessed_date(false);
        let fs = fatfs::FileSystem::new(disk.clone(), fs_options);
        if let Ok(fs) = fs {
            return Self {
                fs: Arc::new(Mutex::new(fs)),
                disk,
            };
        }
        drop(fs);
        disk.seek(fatfs::SeekFrom::Start(0)).unwrap();
        Self::format(&mut disk);
        let fs = fatfs::FileSystem::new(disk.clone(), fs_options)
            .expect("disk should be formatted now so no more errors.");
        Self {
            fs: Arc::new(Mutex::new(fs)),
            disk,
        }
    }

    pub fn reopen(&mut self) {
        let fs_options = fatfs::FsOptions::new().update_accessed_date(false);
        self.disk.seek(fatfs::SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(self.disk.clone(), fs_options);
        if let Ok(fs) = fs {
            *self.fs.lock().unwrap() = fs;
        }
        self.disk.seek(fatfs::SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(self.disk.clone(), fs_options)
            .expect("disk should be formatted now so no more errors.");
        *self.fs.lock().unwrap() = fs;
    }

    pub fn fs(&self) -> &Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>> {
        &self.fs
    }

    pub fn fs_as_owned(
        &self,
    ) -> Arc<Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>>> {
        self.fs.clone()
    }

    pub fn disk(&self) -> &D {
        &self.disk
    }
}
