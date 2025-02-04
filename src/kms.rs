mod vis;

use std::sync::{Arc, Mutex, MutexGuard};

use fatfs::{LossyOemCpConverter, NullTimeProvider};
use obliviate_core::{
    crypter::{aes::Aes256Ctr, ivs::SequentialIvg},
    hasher::sha3::{Sha3_256, SHA3_256_MD_SIZE},
    kms::{khf::Khf, KeyManagementScheme, PersistableKeyManagementScheme},
    wal::SecureWAL,
};
use rand::rngs::OsRng;

use crate::fs::Disk;

type MyKhf = Khf<OsRng, SequentialIvg, Aes256Ctr, Sha3_256, SHA3_256_MD_SIZE>;
type MyWal<D> = SecureWAL<
    D,
    <MyKhf as KeyManagementScheme>::LogEntry,
    SequentialIvg,
    Aes256Ctr,
    SHA3_256_MD_SIZE,
>;

pub(crate) struct Kms<D: Disk> {
    wal: Mutex<MyWal<D>>,
    khf: Mutex<MyKhf>,
}

impl<D> Kms<D>
where
    D: Disk,
    std::io::Error: From<fatfs::Error<D::Error>>,
{
    fn open_khf(
        fs: Arc<Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>>>,
        root_key: [u8; 32],
    ) -> MyKhf {
        let khf = MyKhf::load(root_key, "lethe/khf", &fs.lock().unwrap())
            .unwrap_or_else(|_e| MyKhf::new());
        khf
    }

    fn open_wal(
        fs: Arc<Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>>>,
        root_key: [u8; 32],
    ) -> SecureWAL<
        D,
        <MyKhf as KeyManagementScheme>::LogEntry,
        SequentialIvg,
        Aes256Ctr,
        SHA3_256_MD_SIZE,
    > {
        fs.lock().unwrap().root_dir().create_dir("lethe").unwrap();
        SecureWAL::open("lethe/wal".to_string(), root_key, fs.clone()).unwrap()
    }
    pub fn open(
        fs: Arc<Mutex<fatfs::FileSystem<D, NullTimeProvider, LossyOemCpConverter>>>,
        root_key: [u8; 32],
    ) -> Self {
        Self {
            khf: Mutex::new(Self::open_khf(fs.clone(), root_key)),
            wal: Mutex::new(Self::open_wal(fs, root_key)),
        }
    }

    pub fn khf_lock(&self) -> MutexGuard<'_, MyKhf> {
        self.khf.lock().unwrap()
    }

    pub fn wal_lock(&self) -> MutexGuard<'_, MyWal<D>> {
        self.wal.lock().unwrap()
    }
}
