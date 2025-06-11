use std::{
    ffi::CString,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use efs::fs::ext2::inode::ROOT_DIRECTORY_INODE;
use lwext4_rs::{Ext4Blockdev, Ext4BlockdevIface, Ext4File, Ext4Fs, FileKind, O_CREAT, O_RDWR};

use crate::{
    fs::PAGE_SIZE, ino_to_objid, objid_to_ino, ExternalFile, ExternalKind, ObjID, PagedObjectStore,
    PagingImp,
};

pub struct Ext4Store<P: PagingImp> {
    p: PhantomData<P>,
    fs: Mutex<Ext4Fs>,
}

pub trait Device: Read + Write + Seek {}

impl<T: Read + Write + Seek> Device for T {}

struct Ext4Bd {
    device: Box<dyn Device>,
    phys_bcount: u64,
}

impl Ext4BlockdevIface for Ext4Bd {
    fn phys_block_size(&mut self) -> u32 {
        PHYSICAL_BSIZE
    }

    fn phys_block_count(&mut self) -> u64 {
        self.phys_bcount
    }

    fn open(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn read(&mut self, buf: *mut u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        self.device.seek(SeekFrom::Start(start))?;
        let slice = unsafe { core::slice::from_raw_parts_mut(buf, len as usize) };
        let len = self.device.read(slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn write(&mut self, buf: *const u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        self.device.seek(SeekFrom::Start(start))?;
        let slice = unsafe { core::slice::from_raw_parts(buf, len as usize) };
        let len = self.device.write(slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }
}

impl Ext4Bd {
    fn new(device: impl Device + 'static, _name: &str, phys_bcount: u64) -> Self {
        Self {
            device: Box::new(device),
            phys_bcount,
        }
    }
}

impl From<FileKind> for ExternalKind {
    fn from(value: FileKind) -> Self {
        match value {
            FileKind::Regular => ExternalKind::Regular,
            FileKind::Directory => ExternalKind::Directory,
            FileKind::Symlink => ExternalKind::SymLink,
            FileKind::Other => ExternalKind::Other,
        }
    }
}

static BDEV_ID: AtomicU64 = AtomicU64::new(0);

const LOGICAL_BSIZE: u32 = 512;
const PHYSICAL_BSIZE: u32 = 512;

impl<P: PagingImp> Ext4Store<P> {
    pub fn new<D: Device + 'static>(mut device: D, name: &str) -> std::io::Result<Self> {
        let bdname = format!("blockdev-{}", BDEV_ID.fetch_add(1, Ordering::SeqCst));
        let max = device.seek(SeekFrom::End(0))?;
        let bcount = max / LOGICAL_BSIZE as u64;
        let phys_bcount = max / PHYSICAL_BSIZE as u64;
        let bd = Ext4Blockdev::new(
            Ext4Bd::new(device, bdname.as_str(), phys_bcount),
            LOGICAL_BSIZE,
            bcount,
            name,
        )?;

        let mut fs = Ext4Fs::new(bd, CString::new(name).unwrap(), false)?;

        match fs.create_dir("ids") {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                return Err(e);
            }
            _ => {}
        }

        Ok(Self {
            p: PhantomData,
            fs: Mutex::new(fs),
        })
    }

    pub fn get_id_path(&self, id: ObjID) -> (String, String) {
        let top = id.to_be_bytes()[0];
        let us = format!("ids/{:x}", top);
        (us, format!("ids/{:x}/{:x}", top, id))
    }

    pub fn get_object_as_file(&self, id: ObjID, create: bool) -> std::io::Result<Ext4File> {
        let flags = if create { O_RDWR | O_CREAT } else { O_RDWR };
        if let Some(ino) = objid_to_ino(id) {
            let mut fs = self.fs.lock().unwrap();
            return fs.open_file_from_inode(ino, flags);
        }
        let path = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        if create {
            match fs.create_dir(&path.0) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        fs.open_file(&path.1, flags)
    }
}

impl<P: PagingImp> PagedObjectStore<P> for Ext4Store<P> {
    fn create_object(&self, id: crate::ObjID) -> std::io::Result<()> {
        self.get_object_as_file(id, true)?;
        Ok(())
    }

    fn delete_object(&self, id: crate::ObjID) -> std::io::Result<()> {
        let path = self.get_id_path(id);
        self.fs.lock().unwrap().remove_file(&path.1)
    }

    fn len(&self, id: crate::ObjID) -> std::io::Result<u64> {
        let mut file = self.get_object_as_file(id, false)?;
        Ok(file.len())
    }

    fn read_object(&self, id: crate::ObjID, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut file = self.get_object_as_file(id, false)?;
        file.seek(SeekFrom::Start(offset))?;
        file.read(buf)
    }

    fn write_object(&self, id: crate::ObjID, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        let mut file = self.get_object_as_file(id, false)?;
        if offset > file.len() {
            self.fs.lock().unwrap().ensure_backing(&file, offset)?;
            file.truncate(offset)?;
        }
        file.seek(SeekFrom::Start(offset))?;
        // TODO
        file.write(buf)?;
        Ok(())
    }

    fn get_config_id(&self) -> std::io::Result<ObjID> {
        let mut buf = [0; 16];
        self.read_object(0, 0, &mut buf).and_then(|len| {
            if len == 16 && buf.iter().find(|x| **x != 0).is_some() {
                Ok(ObjID::from_le_bytes(buf))
            } else {
                Err(ErrorKind::InvalidData.into())
            }
        })
    }

    fn set_config_id(&self, id: ObjID) -> std::io::Result<()> {
        let _ = self.delete_object(0);
        self.create_object(0)?;
        self.write_object(0, 0, &id.to_le_bytes())
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn page_in_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest<P>],
    ) -> std::io::Result<usize> {
        let file = self.get_object_as_file(id, false)?;
        let mut fs = self.fs.lock().unwrap();
        let mut inode = fs.get_file_inode(&file)?;
        let blocks_per_page = P::page_size() / fs.block_size()? as usize;
        let blocks = reqs
            .iter()
            .map(|req| {
                (
                    &req.imp,
                    (req.start_page..(req.start_page + req.nr_pages as i64))
                        .map(|p| {
                            let mut block = p as u32;
                            if objid_to_ino(id).is_some() {
                                // External files don't have null pages
                                block -= 1;
                            }
                            inode
                                .get_data_block(block * blocks_per_page as u32, false)
                                .ok()
                        })
                        .collect::<Vec<Option<u64>>>(),
                )
            })
            .collect::<Vec<_>>();
        tracing::debug!("paging  in request for {} reqs", reqs.len());
        for br in blocks {
            let _plen = br.1.len();
            let _len = br.0.page_in(br.1.into_iter())?;
        }
        Ok(reqs.len())
    }

    fn page_out_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a [crate::PageRequest<P>],
    ) -> std::io::Result<usize> {
        let end_offset = reqs
            .iter()
            .max_by_key(|req| req.start_page as u64 + req.nr_pages as u64)
            .map(|end_req| {
                (end_req.start_page as u64 + end_req.nr_pages as u64) * P::page_size() as u64
            });

        let mut file = self.get_object_as_file(id, false)?;
        if end_offset.unwrap_or(0) >= file.len() {
            self.write_object(id, end_offset.unwrap_or(0), &[0u8; PAGE_SIZE])?;
        }
        let file = self.get_object_as_file(id, false)?;
        let mut fs = self.fs.lock().unwrap();
        let mut inode = fs.get_file_inode(&file)?;
        let blocks_per_page = P::page_size() / fs.block_size()? as usize;
        let blocks = reqs
            .iter()
            .map(|req| {
                Ok::<_, std::io::Error>((
                    &req.imp,
                    (req.start_page..(req.start_page + req.nr_pages as i64))
                        .map(|p| inode.get_data_block(p as u32 * blocks_per_page as u32, true))
                        .try_collect::<Vec<u64>>()?,
                ))
            })
            .try_collect::<Vec<_>>()?;
        tracing::debug!("paging out request for {} reqs", reqs.len());
        for br in blocks {
            let mut pages = &br.1[..];
            while pages.len() > 0 {
                let len = br.0.page_out(pages.into_iter().map(|p| Some(*p)))?;
                pages = &pages[len..];
            }
        }
        Ok(reqs.len())
    }

    fn enumerate_external(&self, id: ObjID) -> std::io::Result<Vec<ExternalFile>> {
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = ROOT_DIRECTORY_INODE;
        }
        let mut inode = fs.get_inode(inonr)?;
        let diriter = fs.dirents(&mut inode)?;

        Ok(diriter
            .filter_map(|de| {
                de.1.ok()
                    .map(|ino| ExternalFile::new(&de.0, ino.kind().into(), ino_to_objid(ino.num())))
            })
            .collect())
    }

    fn find_external(&self, id: ObjID) -> std::io::Result<usize> {
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = ROOT_DIRECTORY_INODE;
        }
        let inode = fs.get_inode(inonr)?;
        Ok(inode.size() as usize)
    }
}
