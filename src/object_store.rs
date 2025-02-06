use std::{
    collections::HashSet,
    io::Error,
    sync::{Arc, Mutex, MutexGuard},
};

use chacha20::{
    cipher::{KeyIvInit, StreamCipher, StreamCipherSeek},
    ChaCha20,
};
use fatfs::{
    DefaultTimeProvider, Dir, IoBase, LossyOemCpConverter, NullTimeProvider, Read as _,
    ReadWriteProxy, Seek, SeekFrom, Write as _,
};
use obliviate_core::{
    crypter::{aes::Aes256Ctr, ivs::SequentialIvg},
    hasher::sha3::{Sha3_256, SHA3_256_MD_SIZE},
    kms::{
        khf::Khf, KeyManagementScheme, PersistableKeyManagementScheme, StableKeyManagementScheme,
    },
    wal::SecureWAL,
};
use rand::rngs::OsRng;

use crate::{
    fs::{Disk, FileSystem, PAGE_SIZE},
    kms::Kms,
    wrapped_extent::WrappedExtent,
};

type EncodedObjectId = String;

fn encode_obj_id(obj_id: u128) -> EncodedObjectId {
    format!("{:0>32x}", obj_id)
}
pub struct ObjectStore<D: Disk> {
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
impl<D> ObjectStore<D>
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
    pub fn reformat(&mut self, mut disk: D, root_key: Option<[u8; 32]>) {
        FileSystem::format(&mut disk);
        self.root_key = root_key.unwrap_or(self.root_key);
        self.fs = FileSystem::open_fs(disk);
        self.kms = Kms::open(self.fs.fs_as_owned(), self.root_key);
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
    pub fn open(disk: D, root_key: [u8; 32]) -> Self {
        let fs = FileSystem::open_fs(disk);
        let fs_ref = fs.fs_as_owned();
        Self::restore_khf(&fs.fs().lock().unwrap());
        let out = Self {
            fs,
            kms: Kms::open(fs_ref, root_key),
            root_key,
        };
        out
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
    pub fn get_config_id(&self) -> Result<Option<u128>, Error> {
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
    pub fn set_config_id(&self, id: u128) -> Result<(), Error> {
        let fs = self.fs().lock().unwrap();
        let mut file = fs.root_dir().create_file("config_id")?;
        file.truncate()?;
        let bytes = id.to_le_bytes();
        file.write_all(&bytes)?;
        Ok(())
    }

    /// Returns true if file was created and false if the file already existed.
    pub fn create_object(&self, obj_id: u128) -> Result<bool, Error> {
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
            let id = extent?.offset / crate::fs::PAGE_SIZE as u64;
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
