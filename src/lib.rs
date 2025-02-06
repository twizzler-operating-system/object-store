#![feature(iterator_try_collect)]
#![feature(slice_as_chunks)]
// mod disk;
mod fs;
// mod nvme;
mod kms;
mod object_store;
mod wrapped_extent;
// pub use fs::FS;
pub use object_store::*;

pub mod fat {
    pub use ::fatfs::*;
}
#[cfg(test)]
mod tests {
    use std::{
        fs::{File, OpenOptions},
        io::{Seek, Write},
        ops::Deref,
        path::Path,
        sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock},
    };

    use fatfs::{IoBase, StdIoWrapper};
    use object_store::ObjectStore;
    #[derive(Clone)]
    struct FileDisk {
        disk: Arc<Mutex<StdIoWrapper<File>>>,
    }

    fn arc_mutex_wrap<T>(v: T) -> Arc<Mutex<T>> {
        Arc::new(Mutex::new(v))
    }

    impl FileDisk {
        fn file_wrap(file: File) -> Arc<Mutex<StdIoWrapper<File>>> {
            arc_mutex_wrap(StdIoWrapper::new(file))
        }

        pub fn open<T: AsRef<Path>>(path: T) -> Self {
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            let target_len: u64 = 0x3_0000_0000;
            let curr_len = file.seek(std::io::SeekFrom::End(0)).unwrap();
            if curr_len < target_len {
                for _ in (curr_len..target_len).step_by(4096) {
                    file.write(&[0u8; 4096]).unwrap();
                }
                file.write(&[0u8; 4096]).unwrap();
            }
            file.seek(std::io::SeekFrom::Start(0)).unwrap();
            let v = file.seek(std::io::SeekFrom::Current(0)).unwrap();
            println!("{:?}", v);
            Self {
                disk: Self::file_wrap(file),
            }
        }

        fn lock(&self) -> MutexGuard<'_, StdIoWrapper<File>> {
            self.disk.lock().unwrap()
        }
    }

    static OBJECT_STORE: LazyLock<Mutex<ObjectStore<FileDisk>>> = LazyLock::new(|| {
        let disk = FileDisk::open("/tmp/get_unique_id.img");
        Mutex::new(ObjectStore::open(disk, [0u8; 32]))
    });

    impl IoBase for FileDisk {
        type Error = std::io::Error;
    }

    impl fatfs::Read for FileDisk {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            self.lock().read(buf)
        }
    }

    impl fatfs::Seek for FileDisk {
        fn seek(&mut self, pos: fatfs::SeekFrom) -> Result<u64, Self::Error> {
            self.lock().seek(pos)
        }
    }

    impl fatfs::Write for FileDisk {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.lock().write(buf)
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.lock().flush()
        }
    }

    use super::*;

    fn get_unique_id<OsRef: Deref<Target = ObjectStore<FileDisk>>>(fs: &OsRef) -> u128 {
        let mut id: u128 = rand::random();
        while !fs.create_object(id).unwrap() {
            id = rand::random();
        }
        id
    }

    fn make_and_check_file<OsRef>(fs: &OsRef, buf1: &mut [u8], buf2: &mut [u8]) -> (Vec<u8>, u128)
    where
        OsRef: Deref<Target = ObjectStore<FileDisk>>,
    {
        let id: u128 = get_unique_id(fs);
        let random_value = rand::random();
        // println!("{}", random_value);
        buf1.fill_with(|| random_value);
        fs.write_all(id, buf1, 0).unwrap();
        fs.read_exact(id, buf2, 0).unwrap();
        assert!(buf1 == buf2);
        (buf2.into(), id)
    }

    #[test]
    pub fn zero_length_file() {
        let buf = vec![0u8; 5000];
        let os = OBJECT_STORE.lock().unwrap();
        os.create_object(0).unwrap();
        os.write_all(0, &buf, 0).unwrap();
        os.unlink_object(0).unwrap();
    }

    #[test]
    fn get_all_ids() {
        let _all_ids = OBJECT_STORE.lock().unwrap().get_all_object_ids().unwrap();
    }

    #[test]
    fn test_lfn() {
        let os = OBJECT_STORE.lock().unwrap();
        let id1: u128 = get_unique_id(&os);
        let id2: u128 = id1 + 1;
        assert!(os.create_object(id2).unwrap());
        os.write_all(id1, b"asdf", 0).unwrap();
        os.write_all(id2, b"ghjk", 0).unwrap();

        let mut b1: [u8; 4] = [0; 4];
        let mut b2: [u8; 4] = [0; 4];
        os.read_exact(id1, &mut b1, 0).unwrap();
        os.read_exact(id2, &mut b2, 0).unwrap();
        assert!(&b1 == b"asdf");
        assert!(&b2 == b"ghjk");
    }

    #[test]
    fn test_khf_serde() {
        let os = OBJECT_STORE.lock().unwrap();
        let id: u128 = get_unique_id(&os);
        os.create_object(id).unwrap();
        os.write_all(id, b"asdf", 0).unwrap();
        os.advance_epoch().unwrap();
        drop(os);
        let mut os = OBJECT_STORE.lock().unwrap();
        os.reopen();
        drop(os);
        let os = OBJECT_STORE.lock().unwrap();
        let mut buf = [0u8; 4];
        os.read_exact(id, &mut buf, 0).unwrap();
        assert!(&buf == b"asdf");
    }

    #[test]
    fn it_works() {
        let mut working_bufs = (vec![0; 5000], vec![0; 5000]);
        let mut os = OBJECT_STORE.lock().unwrap();
        // println!("{:?}", KHF.lock().unwrap());
        let out = (0..10)
            .map(|_i| make_and_check_file(&os, &mut working_bufs.0, &mut working_bufs.1))
            .collect::<Vec<_>>();
        os.advance_epoch().unwrap();
        os.reopen();

        // println!("{:?}", KHF.lock().unwrap());
        for (value, id) in out {
            // make sure buf == read
            let mut buf = vec![0; 5000];
            let v = os.get_obj_segments(id).unwrap();
            println!("{:?}", v);
            os.read_exact(id, &mut buf, 0).unwrap();
            for (i, (b1, b2)) in value.iter().zip(buf.iter()).enumerate() {
                let diff = (*b1 as i16) - (*b2 as i16);
                if diff != 0 {
                    print!("D @ {i}: {diff}\t");
                }
            }
            assert!(value == buf);
            // unlink
            os.unlink_object(id).unwrap();
            os.advance_epoch().unwrap();
            os.reopen();
            // println!("{:?}", KHF.lock().unwrap());
            // make sure object is unlinked
            let v = os.read_exact(id, &mut buf, 0).expect_err("should be error");
            assert!(v.kind() == std::io::ErrorKind::NotFound);
        }
    }
}
