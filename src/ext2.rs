use std::{io::ErrorKind, marker::PhantomData, str::FromStr, sync::Mutex};

use efs::{
    dev::Device,
    file::{Directory, File, Type},
    fs::{
        ext2::{block::Block, error::Ext2Error, inode::Inode, Ext2, Ext2Fs},
        FileSystem,
    },
    io::{Read, Seek, SeekFrom, StdIOWrapper, Write},
    path::{Path, UnixStr},
    permissions::Permissions,
    types::{Gid, Uid},
};
use obliviate_core::consts::PAGE_SIZE;

use crate::paged_object_store::{ObjID, PageRequest, PagedObjectStore, PagingImp};

pub struct Ext2ObjectStore<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> {
    fs: Mutex<Ext2Fs<Device>>,
    _pd: PhantomData<P>,
}

impl<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> Ext2ObjectStore<Device, P> {
    fn with_inode<R>(
        &self,
        id: u128,
        f: impl FnOnce(&Inode, &Ext2Fs<Device>, &Ext2<Device>) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let file = self.get_object_as_file(id)?;
        let ino_number = file.stat().ino.0 as u32;
        let fs = self.fs.lock().unwrap();
        let ext2 = fs.ext2_interface().lock();
        let inode = e2result_to_std(ext2.inode(ino_number))?;
        f(&inode, &*fs, &*ext2)
    }
}

fn e2error_to_std(err: efs::error::Error<Ext2Error>) -> std::io::Error {
    match err {
        efs::error::Error::Device(dev_error) => match dev_error {
            efs::dev::error::DevError::WriteZero => ErrorKind::WriteZero.into(),
            _ => ErrorKind::UnexpectedEof.into(),
        },
        efs::error::Error::Fs(fs_error) => match fs_error {
            efs::fs::error::FsError::EntryAlreadyExist(_) => ErrorKind::AlreadyExists,
            efs::fs::error::FsError::Implementation(_) => ErrorKind::Other,
            efs::fs::error::FsError::Loop(_) => ErrorKind::Other,
            efs::fs::error::FsError::NameTooLong(_) => ErrorKind::InvalidInput,
            efs::fs::error::FsError::NotDir(_) => ErrorKind::NotADirectory,
            efs::fs::error::FsError::NoEnt(_) => ErrorKind::NotFound,
            efs::fs::error::FsError::NotFound(_) => ErrorKind::NotFound,
            efs::fs::error::FsError::RemoveRefused => ErrorKind::ResourceBusy,
            efs::fs::error::FsError::WrongFileType { .. } => ErrorKind::Other,
        }
        .into(),
        efs::error::Error::Path(path_error) => match path_error {
            efs::path::PathError::AbsolutePathRequired(_) => ErrorKind::Other,
            efs::path::PathError::InvalidCString(_) => ErrorKind::InvalidInput,
            efs::path::PathError::InvalidFilename(_) => ErrorKind::Other,
        }
        .into(),
        efs::error::Error::IO(e) => std::io::Error::other(e),
    }
}

fn e2result_to_std<T>(err: Result<T, efs::error::Error<Ext2Error>>) -> Result<T, std::io::Error> {
    err.map_err(|e| e2error_to_std(e))
}

impl<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> PagedObjectStore<P>
    for Ext2ObjectStore<Device, P>
{
    fn get_config_id(&self) -> std::io::Result<crate::paged_object_store::ObjID> {
        let mut buf = [0; 16];
        self.read_object(0, 0, &mut buf).and_then(|len| {
            if len == 16 && buf.iter().find(|x| **x != 0).is_some() {
                Ok(ObjID::from_le_bytes(buf))
            } else {
                Err(ErrorKind::InvalidData.into())
            }
        })
    }

    fn set_config_id(&self, id: crate::paged_object_store::ObjID) -> std::io::Result<()> {
        let _ = self.delete_object(0);
        self.create_object(0)?;
        self.write_object(0, 0, &id.to_le_bytes())
    }

    fn create_object(&self, id: crate::paged_object_store::ObjID) -> std::io::Result<()> {
        let (subdir, path) = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        let root = fs.root().unwrap();
        let ids =
            e2result_to_std(fs.get_file(&Path::new(UnixStr::new("ids").unwrap()), root, false))?;
        let mut ids = match ids {
            efs::file::TypeWithFile::Directory(d) => d,
            _ => return Err(ErrorKind::Other.into()),
        };
        if fs
            .get_file(&Path::new(subdir.clone()), ids.clone(), false)
            .is_err()
        {
            e2result_to_std(ids.add_entry(
                subdir,
                Type::Directory,
                Permissions::USER_READ | Permissions::USER_WRITE,
                Uid(0),
                Gid(0),
            ))?;
        }
        let _ = e2result_to_std(fs.create_file(
            &path,
            efs::file::Type::Regular,
            Permissions::USER_READ | Permissions::USER_WRITE,
            Uid(0),
            Gid(0),
        ))?;
        Ok(())
    }

    fn delete_object(&self, id: crate::paged_object_store::ObjID) -> std::io::Result<()> {
        let path = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        e2result_to_std(fs.remove_file(path.1))
    }

    fn read_object(
        &self,
        id: crate::paged_object_store::ObjID,
        offset: u64,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        let mut file = self.get_object_as_file(id)?;
        e2result_to_std(file.seek(SeekFrom::Start(offset)))?;
        e2result_to_std(file.read(buf))
    }

    fn write_object(
        &self,
        id: crate::paged_object_store::ObjID,
        offset: u64,
        buf: &[u8],
    ) -> std::io::Result<()> {
        let mut file = self.get_object_as_file(id)?;
        let len = file.stat().size.0 as u64;
        if offset + buf.len() as u64 >= len {
            let mut missing = offset + buf.len() as u64 - len;
            e2result_to_std(file.seek(SeekFrom::End(0)))?;
            while missing > 0 {
                let buf = [0; PAGE_SIZE];
                let thislen = std::cmp::min(missing as usize, buf.len());
                e2result_to_std(file.write_all(&buf[0..thislen]))?;
                missing -= thislen as u64;
            }
        }

        if offset.is_multiple_of(PAGE_SIZE as u64) && buf.len().is_multiple_of(PAGE_SIZE) {
            let ino_number = file.stat().ino.0 as u32;
            for p in 0..(buf.len() / PAGE_SIZE) {
                let thisoffset = offset + (p * PAGE_SIZE) as u64;
                let fs = self.fs.lock().unwrap();
                let ext2 = fs.ext2_interface().lock();
                let inode = e2result_to_std(ext2.inode(ino_number))?;
                let blocks = e2result_to_std(inode.indirected_blocks(&ext2))?;
                let logblock = thisoffset / ext2.superblock().block_size() as u64;
                let block = blocks.block_at_offset(logblock as u32).unwrap();
                let mut block = Block::new(fs.clone(), block);
                drop(ext2);
                drop(fs);
                e2result_to_std(block.write(&buf[(p * PAGE_SIZE)..((p + 1) * PAGE_SIZE)]))?;
            }
            return Ok(());
        }
        e2result_to_std(file.seek(SeekFrom::Start(offset)))?;
        e2result_to_std(file.write_all(buf))
    }

    fn page_in_object<'a>(
        &self,
        id: crate::paged_object_store::ObjID,
        reqs: &'a mut [PageRequest<P>],
    ) -> std::io::Result<usize> {
        let blocks = self.with_inode(id, |inode, _, ext2| {
            let ib = e2result_to_std(inode.indirected_blocks(ext2))?;
            let blocks_per_page = P::page_size() / ext2.superblock().block_size() as usize;
            let blocks = reqs
                .iter()
                .map(|req| {
                    (
                        &req.imp,
                        (req.start_page..(req.start_page + req.nr_pages as i64))
                            .map(|p| {
                                ib.block_at_offset(p as u32 * blocks_per_page as u32)
                                    .map(|x| x as u64)
                            })
                            .collect::<Vec<Option<u64>>>(),
                    )
                })
                .collect::<Vec<_>>();
            Ok(blocks)
        })?;
        tracing::debug!("paging request for {} reqs", reqs.len());
        for br in blocks {
            tracing::debug!("==> {:?}", br.1);
            let _plen = br.1.len();
            let _len = br.0.page_in(br.1.into_iter())?;
        }
        Ok(reqs.len())
    }

    fn page_out_object<'a>(
        &self,
        id: crate::paged_object_store::ObjID,
        reqs: &'a [PageRequest<P>],
    ) -> std::io::Result<usize> {
        let end_offset = reqs
            .iter()
            .max_by_key(|req| req.start_page as u64 + req.nr_pages as u64)
            .map(|end_req| {
                (end_req.start_page as u64 + end_req.nr_pages as u64) * P::page_size() as u64
            });

        let mut file = self.get_object_as_file(id)?;
        if end_offset.unwrap_or(0) >= file.size().0 {
            self.write_object(id, end_offset.unwrap_or(0), &[0u8; PAGE_SIZE])?;
        }
        let blocks = self.with_inode(id, |inode, _, ext2| {
            let ib = e2result_to_std(inode.indirected_blocks(ext2))?;
            let blocks_per_page = P::page_size() / ext2.superblock().block_size() as usize;
            let blocks = reqs
                .iter()
                .map(|req| {
                    (
                        &req.imp,
                        (req.start_page..(req.start_page + req.nr_pages as i64))
                            .map(|p| {
                                ib.block_at_offset(p as u32 * blocks_per_page as u32)
                                    .map(|x| x as u64)
                            })
                            .collect::<Vec<Option<u64>>>(),
                    )
                })
                .collect::<Vec<_>>();
            Ok(blocks)
        })?;
        tracing::debug!("paging request for {} reqs", reqs.len());
        for br in blocks {
            tracing::debug!("==> {:?}", br.1);
            let plen = br.1.len();
            let len = br.0.page_out(br.1.into_iter())?;
            assert_eq!(len, plen);
        }
        Ok(reqs.len())
    }
}

impl<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> Ext2ObjectStore<Device, P> {
    pub fn get_id_path(&self, id: ObjID) -> (UnixStr, Path) {
        let top = id.to_be_bytes()[0];
        let us = UnixStr::from_str(&format!("{:x}", top)).unwrap();
        (
            us,
            Path::from_str(&format!("/ids/{:x}/{:x}", top, id)).unwrap(),
        )
    }

    pub fn get_object_as_file(
        &self,
        id: ObjID,
    ) -> std::io::Result<efs::fs::ext2::file::Regular<Device>> {
        let path = self.get_id_path(id);
        let fs = self.fs.lock().unwrap();
        let root = fs.root().unwrap();
        let file = e2result_to_std(fs.get_file(&path.1, root, false))?;
        match file {
            efs::file::TypeWithFile::Regular(reg) => Ok(reg),
            _ => Err(ErrorKind::Other.into()),
        }
    }
}

impl<D: std::io::Read + std::io::Write + std::io::Seek, P: PagingImp>
    Ext2ObjectStore<StdIOWrapper<D, Ext2Error>, P>
{
    pub fn new(device: D, device_id: u32) -> Result<Self, efs::error::Error<Ext2Error>> {
        let device = StdIOWrapper::new(device);
        let fs = Ext2Fs::new(device, device_id)?;
        let path = Path::from_str("ids").unwrap();
        let root = fs.root().unwrap();
        if fs.get_file(&path, root, false).is_err() {
            let mut root = fs.root().unwrap();
            root.add_entry(
                path.as_unix_str().clone(),
                Type::Directory,
                Permissions::USER_WRITE | Permissions::USER_READ | Permissions::USER_EXECUTION,
                Uid(0),
                Gid(0),
            )
            .expect("failed to setup ids directory");
        }
        Ok(Self {
            fs: Mutex::new(fs),
            _pd: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::ErrorKind};

    use efs::{fs::ext2::Ext2Fs, io::StdIOWrapper};
    use obliviate_core::consts::PAGE_SIZE;
    use rand::{seq::SliceRandom, RngCore};

    use super::Ext2ObjectStore;
    use crate::paged_object_store::{PageRequest, PagedObjectStore, PagingImp};

    struct TestPageRequest {
        phys_page: Box<[u8; PAGE_SIZE]>,
    }

    impl PagingImp for TestPageRequest {
        type PhysAddr = Box<[u8; PAGE_SIZE]>;

        fn fill_from_buffer(&mut self, buf: &[u8]) {
            self.phys_page.copy_from_slice(buf);
        }

        fn read_to_buffer(&self, buf: &mut [u8]) {
            buf.copy_from_slice(&*self.phys_page);
        }

        fn phys_addrs(&self) -> impl Iterator<Item = &'_ Self::PhysAddr> {
            &[self.phys_page]
        }
    }

    #[test]
    fn ext2_config_id() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs, 0);
        let res = os.get_config_id();
        assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound);
        os.set_config_id(123).unwrap();
        run_fsck();
        let res = os.get_config_id();
        assert_eq!(res.unwrap(), 123);
        os.delete_object(0).unwrap();
        run_fsck();
    }

    #[test]
    fn ext2_obj_stress() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::new(fs, 0);
        let id = rand::random::<u128>();
        os.create_object(id).unwrap();
        //eprintln!("building data");
        const DATA_SIZE: usize = 1024 * 1024 * 10;
        let mut data = vec![0; DATA_SIZE];
        rand::thread_rng().fill_bytes(&mut data);

        let nr_pages = DATA_SIZE / PAGE_SIZE;

        let mut reqs = (0..nr_pages)
            .into_iter()
            .map(|p| {
                let mut phys = Box::new([0; PAGE_SIZE]);
                phys.copy_from_slice(&data[(p * PAGE_SIZE)..((p + 1) * PAGE_SIZE)]);
                PageRequest::new(TestPageRequest { phys_page: phys }, p as i64, 1)
            })
            .collect::<Vec<_>>();
        reqs.shuffle(&mut rand::thread_rng());
        let mut read_reqs = (0..nr_pages)
            .into_iter()
            .map(|p| {
                let phys = Box::new([0; PAGE_SIZE]);
                PageRequest::new(TestPageRequest { phys_page: phys }, p as i64, 1)
            })
            .collect::<Vec<_>>();

        //eprintln!("Sending pageout");
        let count = os.page_out_object(id, &reqs).unwrap();
        assert_eq!(count, reqs.len());
        //eprintln!("Page in");
        let count = os.page_in_object(id, &mut read_reqs).unwrap();
        assert_eq!(count, read_reqs.len());

        for p in 0..nr_pages {
            assert_eq!(
                &*read_reqs[p].imp.phys_page,
                &data[(p * PAGE_SIZE)..((p + 1) * PAGE_SIZE)]
            );
        }
        run_fsck();
    }

    fn run_fsck() {
        std::eprintln!("running fsck");
        let status = std::process::Command::new("/opt/homebrew/opt/e2fsprogs/sbin/fsck.ext2")
            .arg("-f")
            .arg("-n")
            .arg("image.ext2")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn many_objects() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs, 0);
        for _i in 0..100 {
            //std::eprintln!("{}", i);
            let id = rand::random::<u128>();
            {
                os.create_object(id).unwrap();
                os.write_object(id, 0, &id.to_le_bytes()).unwrap();
            }
            let mut buf = [0; 16];
            let _len = os.read_object(id, 0, &mut buf).unwrap();
            let read_id = u128::from_le_bytes(buf);

            assert_eq!(id, read_id);
            os.delete_object(id).unwrap();
            assert!(os.read_object(id, 0, &mut []).is_err());
        }
        run_fsck();
    }

    #[test]
    fn many_objects_at_once() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs, 0);
        let mut ids = Vec::new();
        for _i in 0..100 {
            //std::eprintln!("{}", i);
            let id = rand::random::<u128>();
            os.create_object(id).unwrap();
            os.write_object(id, 0, &id.to_le_bytes()).unwrap();
            ids.push(id);
        }
        run_fsck();

        for id in ids {
            let mut buf = [0; 16];
            let _len = os.read_object(id, 0, &mut buf).unwrap();
            let read_id = u128::from_le_bytes(buf);
            assert_eq!(id, read_id);
            os.delete_object(id).unwrap();
            assert!(os.read_object(id, 0, &mut []).is_err());
        }
        run_fsck();
    }

    #[test]
    fn ext2_obj_page() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::new(fs, 0);
        let id = rand::random::<u128>();
        os.create_object(id).unwrap();

        let req = TestPageRequest {
            phys_page: Box::new([3; PAGE_SIZE]),
        };
        let req2 = TestPageRequest {
            phys_page: Box::new([2; PAGE_SIZE]),
        };

        let reqs = [PageRequest::new(req, 8, 1), PageRequest::new(req2, 12, 1)];
        let count = os.page_out_object(id, &reqs).unwrap();
        assert_eq!(count, 2);
        run_fsck();
        let rreq = TestPageRequest {
            phys_page: Box::new([0; PAGE_SIZE]),
        };
        let rreq2 = TestPageRequest {
            phys_page: Box::new([0; PAGE_SIZE]),
        };
        let mut rreqs = [PageRequest::new(rreq, 8, 1), PageRequest::new(rreq2, 12, 1)];
        let count = os.page_in_object(id, &mut rreqs).unwrap();
        assert_eq!(count, 2);
        assert_eq!(rreqs[0].imp.phys_page, reqs[0].imp.phys_page);
        assert_eq!(rreqs[1].imp.phys_page, reqs[1].imp.phys_page);
    }

    #[test]
    fn ext2_obj() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let file = StdIOWrapper::new(file);
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs, 0);
        let id = rand::random::<u128>();
        os.create_object(id).unwrap();
        let mut buf = [1; 1024];
        let mut buf2 = [0; 1024];
        let buf_0 = [0; 1024];
        os.write_object(id, 0, &mut buf).unwrap();
        let _len = os.read_object(id, 0, &mut buf2);
        assert_eq!(buf2, buf);
        os.write_object(id, 4096, &mut buf).unwrap();
        let _len = os.read_object(id, 4096, &mut buf2);
        assert_eq!(buf2, buf);

        let len = os.read_object(id, 2048, &mut buf2);
        assert_eq!(len.unwrap(), 1024);
        assert_eq!(buf2, buf_0);
        os.delete_object(id).unwrap();
        run_fsck();
    }
}
