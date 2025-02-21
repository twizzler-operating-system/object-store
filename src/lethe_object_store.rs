use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::{Error, ErrorKind},
    sync::{Mutex, MutexGuard},
};

use chacha20::{
    cipher::{KeyIvInit, StreamCipher, StreamCipherSeek},
    ChaCha20,
};
pub use fatfs::StdIoWrapper;
use fatfs::{
    DefaultTimeProvider, Dir, IoBase, LossyOemCpConverter, Read as _, ReadWriteProxy, Seek,
    SeekFrom, Write as _,
};
use obliviate_core::kms::{PersistableKeyManagementScheme, StableKeyManagementScheme};

use crate::{
    fs::{Disk, FileSystem, PAGE_SIZE},
    kms::Kms,
    paged_object_store::{PagedObjectStore, PagingImp},
    wrapped_extent::WrappedExtent,
};

type EncodedObjectId = String;

fn encode_obj_id(obj_id: u128) -> EncodedObjectId {
    format!("{:0>32x}", obj_id)
}
pub struct LetheObjectStore<D: Disk> {
    fs: FileSystem<D>,
    kms: Kms<D>,
    root_key: [u8; 32],
}

fn get_dir_path<'a, D>(
    fs: &'a mut fatfs::FileSystem<D, DefaultTimeProvider, LossyOemCpConverter>,
    encoded_obj_id: &EncodedObjectId,
) -> Result<Dir<'a, D, DefaultTimeProvider, LossyOemCpConverter>, Error>
where
    D: Disk,
    std::io::Error: From<fatfs::Error<D::Error>>,
{
    let subdir = fs
        .root_dir()
        .create_dir("ids")?
        .create_dir(&encoded_obj_id[0..1])?;
    Ok(subdir)
}

// while 'a represents the lifetime of the Disk
impl<D> LetheObjectStore<D>
where
    D: Disk,
    std::io::Error: From<fatfs::Error<D::Error>>,
    fatfs::Error<std::io::Error>: From<<D as IoBase>::Error>,
    fatfs::Error<<D as IoBase>::Error>: From<std::io::Error>,
    std::io::Error: From<D::Error>,
    D::Error: std::error::Error + Send + Sync + 'static,
{
    /// Overwrites the existing disk with a new format.
    /// # Safety
    /// Might not securely delete what used to be on the disk.
    ///
    /// # Panics
    /// When there is a Disk error or when a lock is not
    /// able to be claimed
    pub fn reformat(&mut self, mut disk: D, root_key: Option<[u8; 32]>) -> std::io::Result<()> {
        FileSystem::format(&mut disk);
        self.root_key = root_key.unwrap_or(self.root_key);
        self.fs = FileSystem::open_fs(disk)?;
        self.kms = Kms::open(self.fs.fs_as_owned(), self.root_key);
        Ok(())
    }
    /// Reopens Object Store from disk.
    /// Useful for testing persistance/recovery
    pub fn reopen(&mut self) {
        self.fs.reopen();
        Self::restore_khf(&self.fs().lock().unwrap());
        self.kms = Kms::open(self.fs.fs_as_owned(), self.root_key);
    }

    fn fs(&self) -> &Mutex<fatfs::FileSystem<D>> {
        self.fs.fs()
    }
    fn wipe_old_khf_file(fs: &MutexGuard<'_, fatfs::FileSystem<D>>) {
        let old_file = fs.root_dir().open_file("old/khf");
        let mut old_file = match old_file {
            Err(fatfs::Error::NotFound) => return,
            v => v.unwrap(),
        };
        // override old file with zeroes
        let extents_ct = old_file.extents().collect::<Vec<_>>().len();
        for _ in 0..extents_ct {
            old_file.write(&[0u8; PAGE_SIZE]).unwrap();
        }
        // delete old file
        fs.root_dir().remove("old/khf").unwrap();
    }
    fn restore_khf(fs: &MutexGuard<'_, fatfs::FileSystem<D>>) {
        let lethe = fs.root_dir().create_dir("lethe/").unwrap();
        let tmp_khf = fs.root_dir().open_file("tmp/khf");
        let old_khf = fs.root_dir().open_file("old/khf");
        // Step one: save khf to old/khf if khf exists.
        let step_one = || {
            let res = lethe.rename("khf", &fs.root_dir(), "old/khf");
            match res {
                Err(fatfs::Error::NotFound) => {
                    // it's fine if there currently isn't a khf,
                    // since we're about to add one from tmp/khf.
                    // However if there was one we should make sure to
                    // save it.
                }
                r => r.unwrap(),
            };
        };
        // Step two: write what's in tmp/khf to lethe/khf
        // and delete the old khf file.
        let step_two = || {
            fs.root_dir().rename("tmp/khf", &lethe, "khf").unwrap();
            Self::wipe_old_khf_file(&fs);
        };
        match (tmp_khf, old_khf) {
            (Ok(_new), Ok(_old)) => {
                // don't need to do step one since the prev khf is already
                // in old/khf.
                step_two();
            }
            (Err(fatfs::Error::NotFound), Ok(_old)) => {
                // if there isn't a new khf and there isn't an existing
                // khf, move the old khf to the existing khf.
                match fs.root_dir().rename("old/khf", &lethe, "khf") {
                    // Otherwise just delete the old khf.
                    Err(fatfs::Error::AlreadyExists) => {
                        // just didn't get to deleting old/khf
                        // delete it now:
                        Self::wipe_old_khf_file(&fs);
                    }
                    v => v.unwrap(),
                };
            }
            (Ok(_new), Err(fatfs::Error::NotFound)) => {
                step_one();
                step_two();
            }
            (Err(fatfs::Error::NotFound), Err(fatfs::Error::NotFound)) => {
                // how it should be after an epoch.
            }
            (e, e2) => {
                e.unwrap();
                e2.unwrap();
                panic!("unexpected error during restoration")
            }
        };
    }
    /// Will either open the disk if it is properly formatted
    /// or will reformat the disk.
    /// # Safety
    /// If the disk gets corrupted then it might not securely delete
    /// what used to be on the disk.
    pub fn open(disk: D, root_key: [u8; 32]) -> std::io::Result<Self> {
        let fs = FileSystem::open_fs(disk)?;
        let fs_ref = fs.fs_as_owned();
        Self::restore_khf(&fs.fs().lock().unwrap());
        let out = Self {
            fs,
            kms: Kms::open(fs_ref, root_key),
            root_key,
        };
        Ok(out)
    }

    /// Returns the disk length of a given object on disk.
    pub fn disk_length(&self, obj_id: u128) -> Result<u64, Error> {
        let mut fs = self.fs().lock().unwrap();
        let id = encode_obj_id(obj_id);
        let dir = get_dir_path(&mut fs, &id)?;
        let mut file = dir.open_file(&id)?;
        let len = file.seek(SeekFrom::End(0))?;
        Ok(len)
    }
    /// Either gets a previously set config_id from disk or returns None
    pub fn do_get_config_id(&self) -> Result<Option<u128>, Error> {
        let fs = self.fs().lock().unwrap();
        let file = fs.root_dir().open_file("config_id");
        let mut file = match file {
            Ok(file) => file,
            Err(fatfs::Error::NotFound) => return Ok(None),
            err => err?,
        };
        let mut buf = [0u8; 16];
        file.read_exact(&mut buf)?;
        Ok(Some(u128::from_le_bytes(buf)))
    }
    /// Stores a config_id onto the disk.
    pub fn do_set_config_id(&self, id: u128) -> Result<(), Error> {
        let fs = self.fs().lock().unwrap();
        let mut file = fs.root_dir().create_file("config_id")?;
        file.truncate()?;
        let bytes = id.to_le_bytes();
        file.write_all(&bytes)?;
        Ok(())
    }

    /// Returns true if file was created and false if the file already existed.
    pub fn do_create_object(&self, obj_id: u128) -> Result<bool, Error> {
        let b64 = encode_obj_id(obj_id);
        let mut fs = self.fs().lock().unwrap();
        let subdir = get_dir_path(&mut fs, &b64)?;
        // try to open it to check if it exists.
        let res = subdir.open_file(&b64);
        match res {
            Ok(_) => Ok(false),
            Err(e) => match e {
                fatfs::Error::NotFound => {
                    // khf.derive_mut(&wal, hash_obj_id(obj_id))
                    //     .expect("shouldn't panic since khf implementation doesn't panic");
                    subdir.create_file(&b64)?;
                    Ok(true)
                }
                _ => Err(e.into()),
            },
        }
    }

    fn kms(&self) -> &Kms<D> {
        &self.kms
    }
    /// unlinks (aka deletes) the object at `obj_id`.
    /// # Safety
    /// To do secure deletion on deletes you must call an epoch
    /// before saving.
    pub fn unlink_object(&self, obj_id: u128) -> Result<(), Error> {
        let b64 = encode_obj_id(obj_id);
        // let (khf, wal) = (kms.khf_mut(), kms.wal_mut());
        // khf.delete(&wal, hash_obj_id(obj_id))
        //     .map_err(Error::other)?;
        let extents = {
            let mut fs = self.fs().lock().unwrap();
            let subdir = get_dir_path(&mut fs, &b64)?;
            let mut file = subdir.open_file(&b64)?;
            file.extents().collect::<Vec<_>>().into_iter()
        };
        for extent in extents {
            let id = disk_offset_to_id(extent?.offset);
            let kms = self.kms();

            kms.khf_lock()
                .delete(&kms.wal_lock(), id)
                .map_err(Error::other)?;
        }
        let mut fs = self.fs().lock().unwrap();
        let subdir = get_dir_path(&mut fs, &b64)?;
        subdir.remove(&b64)?;
        Ok(())
    }

    pub fn get_all_object_ids(&self) -> Result<Vec<u128>, Error> {
        let fs = self.fs().lock().unwrap();
        let id_root = fs.root_dir().create_dir("ids")?;
        let mut out = Vec::new();
        for folder in id_root.iter() {
            let folder = folder?;
            for file in folder.to_dir().iter() {
                let file = file?;
                let name = file.file_name();
                if name.len() != 32 {
                    continue; // ., ..
                }
                let id = u128::from_str_radix(&name, 16);
                if let Ok(id) = id {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    fn get_symmetric_cipher(&self, disk_offset: u64) -> Result<ChaCha20, Error> {
        let kms = self.kms();
        let chunk_id = disk_offset_to_id(disk_offset);
        //println!("Chunk id: {}", chunk_id);
        let key = kms
            .khf_lock()
            .derive_mut(&kms.wal_lock(), chunk_id)
            .map_err(Error::other)?;
        //println!("Key for {}:{:?}", disk_offset, key);
        get_symmetric_cipher_from_key(disk_offset, key)
    }

    pub fn read_exact(&self, obj_id: u128, buf: &mut [u8], off: u64) -> Result<(), Error> {
        let b64 = encode_obj_id(obj_id);
        let mut fs = self.fs().lock().unwrap();
        let subdir = get_dir_path(&mut fs, &b64)?;
        let mut file = subdir.open_file(&b64)?;
        file.seek(fatfs::SeekFrom::Start(off))?;
        let mut rw_proxy = ReadWriteProxy::new(
            &mut file,
            |disk: &mut D,
             disk_offset: u64,
             buffer: &mut [u8]|
             -> Result<usize, fatfs::Error<D::Error>> {
                let out = disk.read(buffer)?;
                //println!("reading @ {}", disk_offset);
                let mut cipher = self
                    .get_symmetric_cipher(disk_offset)
                    .map_err(Error::other)?;
                cipher.apply_keystream(buffer);
                Ok(out)
            },
            || {},
        );
        fatfs::Read::read_exact(&mut rw_proxy, buf)?;
        Ok(())
    }

    pub fn get_obj_segments(&self, obj_id: u128) -> Result<HashSet<WrappedExtent>, Error> {
        let b64 = encode_obj_id(obj_id);
        // call to get_khf_locks to make sure that khf is already initialized for
        // the later "get_symmetric_cipher" call
        let mut fs = self.fs().lock().unwrap();
        let subdir = get_dir_path(&mut fs, &b64)?;
        let mut file = subdir.open_file(&b64)?;
        let out_hm: HashSet<WrappedExtent> = file
            .extents()
            .map(|v| v.map(WrappedExtent::from))
            .try_collect()?;
        Ok(out_hm)
    }

    pub fn write_all(&self, obj_id: u128, buf: &[u8], off: u64) -> Result<(), Error> {
        let b64 = encode_obj_id(obj_id);
        let mut fs = self.fs().lock().unwrap();
        let subdir = get_dir_path(&mut fs, &b64)?;
        let mut file = subdir.open_file(&b64)?;
        let _new_pos = file.seek(fatfs::SeekFrom::Start(off))?;
        let extents_before: HashSet<WrappedExtent> = file
            .extents()
            .map(|v| v.map(WrappedExtent::from))
            .try_collect()?;
        let mut rw_proxy = ReadWriteProxy::new(
            &mut file,
            || {},
            |disk: &mut D, offset: u64, buffer: &[u8]| -> Result<usize, fatfs::Error<D::Error>> {
                //println!("writing @ {}", offset);
                let mut cipher = self.get_symmetric_cipher(offset)?;
                let mut encrypted = vec![0u8; buffer.len()];
                cipher
                    .apply_keystream_b2b(buffer, &mut encrypted)
                    .map_err(Error::other)?;
                let out = disk.write(&encrypted)?;
                Ok(out)
            },
        );
        fatfs::Write::write_all(&mut rw_proxy, buf)?;
        let extents_after: HashSet<WrappedExtent> = file
            .extents()
            .map(|v| v.map(WrappedExtent::from))
            .try_collect()?;
        // Should never add extents to a file after writing to a file.
        assert_eq!(extents_before.difference(&extents_after).next(), None);
        Ok(())
    }

    pub fn advance_epoch(&self) -> Result<(), Error> {
        let kms = self.kms();
        let updated_keys = kms
            .khf_lock()
            .update(&kms.wal_lock())
            .map_err(Error::other)?;
        for (id, key) in updated_keys {
            //println!("{}", id_to_disk_offset(id));
            let mut buf = vec![0; PAGE_SIZE];
            let mut disk = self.fs.disk().clone();
            let disk_offset = id_to_disk_offset(id);
            disk.seek(SeekFrom::Start(disk_offset))?;
            disk.read_exact(buf.as_mut_slice())?;
            let mut cipher =
                get_symmetric_cipher_from_key(disk_offset, key).map_err(Error::other)?;
            cipher.apply_keystream(&mut buf);
            disk.seek(SeekFrom::Start(disk_offset))?;
            let mut cipher = self
                .get_symmetric_cipher(disk_offset)
                .map_err(Error::other)?;
            cipher.apply_keystream(&mut buf);
            disk.write_all(&buf)?;
        }
        let kms = self.kms();
        {
            let mut khf = kms.khf_lock();
            let fs = self.fs().lock().unwrap();
            fs.root_dir().create_dir("tmp/")?;
            fs.root_dir().create_dir("old/")?;
            khf.persist(self.root_key, "tmp/khf", &fs)
                .map_err(Error::other)?;
            Self::wipe_old_khf_file(&fs);
            // let lethe = fs.root_dir().create_dir("lethe/")?;
            Self::restore_khf(&fs);
        }
        kms.wal_lock().clear().map_err(Error::other)?;
        Ok(())
    }

    pub fn get_lethe_key_from_offset(&self, offset: u64) -> Result<[u8; 32], Error> {
        let kms = self.kms();
        kms.khf_lock()
            .derive_mut(&kms.wal_lock(), disk_offset_to_id(offset))
            .map_err(|_| ErrorKind::Other.into())
    }

    pub fn get_lethe_state(&self) -> Result<LetheState, Error> {
        let mut objs = Vec::new();
        for id in self.get_all_object_ids()? {
            let mut perobj = PerObjLetheState::default();
            perobj.id = id;
            let extents = self.get_obj_segments(id)?;
            for extent in extents {
                let chunk_id = extent.0.offset / crate::fs::PAGE_SIZE as u64;
                let kms = self.kms();
                let key = kms
                    .khf_lock()
                    .derive_mut(&kms.wal_lock(), chunk_id)
                    .map_err(Error::other)?;
                perobj.keys.insert(extent, key);
            }
            objs.push(perobj);
        }
        let kms = self.kms();
        let roots = kms
            .khf_lock()
            .roots()
            .iter()
            .map(|root| (root.pos.0, root.pos.1, root.key))
            .collect();
        Ok(LetheState { list: objs, roots })
    }
}

#[derive(Default, Debug)]
pub struct PerObjLetheState {
    pub id: u128,
    pub keys: HashMap<WrappedExtent, [u8; 32]>,
}

pub fn key_fprint(key: &[u8; 32]) -> u32 {
    key.as_chunks::<4>()
        .0
        .iter()
        .fold(0, |acc, chunk| acc ^ u32::from_le_bytes(*chunk))
}

impl Display for PerObjLetheState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "id: {:x} ({} extents)", self.id, self.keys.len())?;
        let mut keys = self.keys.iter().collect::<Vec<_>>();
        keys.sort_by(|x, y| x.0 .0.offset.cmp(&y.0 .0.offset));
        for key in keys.iter().take(4) {
            writeln!(
                f,
                "  -- {} ({}): {:8x}",
                key.0 .0.offset,
                disk_offset_to_id(key.0 .0.offset),
                key_fprint(key.1)
            )?;
        }
        writeln!(f, "  -- ...")?;
        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct LetheState {
    pub list: Vec<PerObjLetheState>,
    pub roots: Vec<(u64, u64, [u8; 32])>,
}

pub fn disk_offset_to_id(offset: u64) -> u64 {
    (offset - 1024) / super::fs::PAGE_SIZE as u64
}

pub fn id_to_disk_offset(id: u64) -> u64 {
    id * super::fs::PAGE_SIZE as u64 + 1024
}

// // FIXME should use a randomly generated root key for each device.
// pub const ROOT_KEY: [u8; 32] = [0; 32];

fn get_symmetric_cipher_from_key(disk_offset: u64, key: [u8; 32]) -> Result<ChaCha20, Error> {
    let chunk_id = disk_offset_to_id(disk_offset);
    let offset = disk_offset - chunk_id;
    let bytes = chunk_id.to_le_bytes();
    let nonce: [u8; 12] = [
        0, 0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ];

    let mut cipher = ChaCha20::new(&key.into(), &nonce.into());
    cipher.seek(offset);
    Ok(cipher)
}

impl<D: Disk, P: PagingImp> PagedObjectStore<P> for LetheObjectStore<D>
where
    D: Disk,
    std::io::Error: From<fatfs::Error<D::Error>>,
    fatfs::Error<std::io::Error>: From<<D as IoBase>::Error>,
    fatfs::Error<<D as IoBase>::Error>: From<std::io::Error>,
    std::io::Error: From<D::Error>,
    D::Error: std::error::Error + Send + Sync + 'static,
{
    fn create_object(&self, id: crate::paged_object_store::ObjID) -> std::io::Result<()> {
        self.do_create_object(id).map(|_| ())
    }

    fn delete_object(&self, id: crate::paged_object_store::ObjID) -> std::io::Result<()> {
        self.unlink_object(id)
    }

    fn read_object(
        &self,
        id: crate::paged_object_store::ObjID,
        offset: u64,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        self.read_exact(id, buf, offset)?;
        Ok(buf.len())
    }

    fn write_object(
        &self,
        id: crate::paged_object_store::ObjID,
        offset: u64,
        buf: &[u8],
    ) -> std::io::Result<()> {
        self.write_all(id, buf, offset)
    }

    fn flush(&self) -> std::io::Result<()> {
        self.advance_epoch()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{File, OpenOptions},
        io::{Seek, Write},
        ops::Deref,
        path::Path,
        sync::{Arc, LazyLock, Mutex, MutexGuard},
    };

    use fatfs::{IoBase, StdIoWrapper};

    use super::*;
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

    static OBJECT_STORE: LazyLock<Mutex<LetheObjectStore<FileDisk>>> = LazyLock::new(|| {
        let disk = FileDisk::open("/tmp/get_unique_id.img");
        Mutex::new(LetheObjectStore::open(disk, [0u8; 32]))
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

    fn get_unique_id<OsRef: Deref<Target = LetheObjectStore<FileDisk>>>(fs: &OsRef) -> u128 {
        let mut id: u128 = rand::random();
        while !fs.do_create_object(id).unwrap() {
            id = rand::random();
        }
        id
    }

    fn make_and_check_file<OsRef>(fs: &OsRef, buf1: &mut [u8], buf2: &mut [u8]) -> (Vec<u8>, u128)
    where
        OsRef: Deref<Target = LetheObjectStore<FileDisk>>,
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
        os.do_create_object(0).unwrap();
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
        assert!(os.do_create_object(id2).unwrap());
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
        os.do_create_object(id).unwrap();
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
