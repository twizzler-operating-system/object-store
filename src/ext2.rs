use std::{io::ErrorKind, marker::PhantomData, str::FromStr, sync::Mutex};

use efs::{
    file::{Directory, File, Type},
    fs::{
        ext2::{error::Ext2Error, Ext2Fs},
        FileSystem,
    },
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, UnixStr},
    permissions::Permissions,
    types::{Gid, Uid},
};
use obliviate_core::consts::PAGE_SIZE;

use crate::paged_object_store::{ObjID, PageRequest, PagedObjectStore, PagingImp};

struct Ext2ObjectStore<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> {
    fs: Mutex<Ext2Fs<Device>>,
    _pd: PhantomData<P>,
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
        if offset > len {
            let mut missing = offset - len;
            e2result_to_std(file.seek(SeekFrom::End(0)))?;
            let pos = e2result_to_std(file.seek(SeekFrom::Current(0)))?;
            while missing > 0 {
                let buf = [0; PAGE_SIZE];
                let thislen = std::cmp::min(missing as usize, buf.len());
                let pos = e2result_to_std(file.seek(SeekFrom::Current(0)))?;
                e2result_to_std(file.write_all(&buf[0..thislen]))?;
                missing -= thislen as u64;
            }
        }
        let len = file.stat().size.0 as u64;
        e2result_to_std(file.seek(SeekFrom::Start(offset)))?;
        e2result_to_std(file.write_all(buf))
    }

    fn page_in_object<'a>(
        &self,
        id: crate::paged_object_store::ObjID,
        reqs: &'a mut [PageRequest<P>],
    ) -> std::io::Result<usize> {
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

    fn page_out_object<'a>(
        &self,
        id: crate::paged_object_store::ObjID,
        reqs: &'a [PageRequest<P>],
    ) -> std::io::Result<usize> {
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

impl<Device: efs::dev::Device<u8, Ext2Error>, P: PagingImp> Ext2ObjectStore<Device, P> {
    fn new(fs: Ext2Fs<Device>) -> Self {
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
        Self {
            fs: Mutex::new(fs),
            _pd: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::ErrorKind;
    use std::str::FromStr;

    use efs::file::{Directory, ReadOnlyFile, Regular, Type};
    use efs::fs::ext2::Ext2Fs;
    use efs::fs::FileSystem;
    use efs::io::Write;
    use efs::path::UnixStr;
    use efs::permissions::Permissions;
    use efs::types::{Gid, Uid};
    use obliviate_core::consts::PAGE_SIZE;

    use crate::paged_object_store::{PageRequest, PagedObjectStore, PagingImp};

    use super::Ext2ObjectStore;

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

        fn phys_addr(&self) -> &Self::PhysAddr {
            &self.phys_page
        }
    }
    #[test]
    fn ext2_config_id() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs);
        let res = os.get_config_id();
        assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound);
        os.set_config_id(123).unwrap();
        let res = os.get_config_id();
        assert_eq!(res.unwrap(), 123);
        os.delete_object(0);
    }

    #[test]
    fn ext2_obj_page() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::new(fs);
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
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let os = Ext2ObjectStore::<_, TestPageRequest>::new(fs);
        let id = rand::random::<u128>();
        os.create_object(id).unwrap();
        let mut buf = [1; 1024];
        let mut buf2 = [0; 1024];
        let mut buf_0 = [0; 1024];
        os.write_object(id, 0, &mut buf).unwrap();
        let len = os.read_object(id, 0, &mut buf2);
        assert_eq!(buf2, buf);
        os.write_object(id, 4096, &mut buf).unwrap();
        let len = os.read_object(id, 4096, &mut buf2);
        assert_eq!(buf2, buf);

        let len = os.read_object(id, 2048, &mut buf2);
        assert_eq!(len.unwrap(), 1024);
        assert_eq!(buf2, buf_0);
    }

    //#[test]
    fn _ext2_basic() {
        let file = File::options()
            .read(true)
            .write(true)
            .open("image.ext2")
            .expect("failed to open test image");
        let fs = Ext2Fs::new(file, 0).expect("failed to open image as ext2");
        let mut root = fs.root().expect("failed to get root");
        let name = UnixStr::from_str("test").unwrap();
        let test_entry = if let Some(test_entry) = root.entry(name.clone()).ok().flatten() {
            test_entry
        } else {
            root.add_entry(
                name.clone(),
                Type::Regular,
                Permissions::USER_WRITE | Permissions::USER_WRITE,
                Uid(0),
                Gid(0),
            )
            .expect("failed to add entry test");
            root.entry(name.clone())
                .ok()
                .flatten()
                .expect("failed to make new test entry")
        };
        let mut reg = match test_entry {
            efs::file::TypeWithFile::Regular(reg) => reg,
            _ => panic!("unexpect test entry type"),
        };
        let stat = reg.stat();

        reg.truncate(0).expect("failed to truncate test");
        reg.write_all(&[0; 4096]).unwrap();
        let stat = reg.stat();

        let ext2 = fs.lock();
        let inode = ext2.inode(stat.ino.0 as u32).unwrap();
        let ib = inode.indirected_blocks(&*ext2).unwrap();
    }
}
