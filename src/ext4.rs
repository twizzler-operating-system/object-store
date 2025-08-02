#[cfg(not(target_os = "twizzler"))]
use std::io::Result;
use std::{
    ffi::CString,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use efs::fs::ext2::inode::ROOT_DIRECTORY_INODE;
use lwext4_rs::{Ext4Blockdev, Ext4BlockdevIface, Ext4File, Ext4Fs, FileKind, O_CREAT, O_RDWR};
#[cfg(target_os = "twizzler")]
use twizzler::Result;

use crate::{
    ino_to_objid, objid_to_ino, DevicePage, ExternalFile, ExternalKind, ObjID, PagedDevice,
    PagedObjectStore, PosIo, PAGE_SIZE,
};

pub struct Ext4Store {
    fs: Mutex<Ext4Fs>,
    device: Arc<dyn Device>,
}

pub trait Device: PosIo + PagedDevice + Sync + Send {}

impl<T: PosIo + PagedDevice + Sync + Send> Device for T {}

struct Ext4Bd {
    device: Arc<dyn Device>,
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
        let slice = unsafe { core::slice::from_raw_parts_mut(buf, len as usize) };
        let len = self.device.read(start, slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn write(&mut self, buf: *const u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        let slice = unsafe { core::slice::from_raw_parts(buf, len as usize) };
        let len = self.device.write(start, slice)?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }
}

impl Ext4Bd {
    fn new(device: Arc<dyn Device>, _name: &str, phys_bcount: u64) -> Self {
        Self {
            device,
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

impl Ext4Store {
    pub fn new<D: Device + 'static>(device: D, name: &str) -> Result<Self> {
        let bdname = format!("blockdev-{}", BDEV_ID.fetch_add(1, Ordering::SeqCst));
        let max = device.len()? as u64;
        let bcount = max / LOGICAL_BSIZE as u64;
        let phys_bcount = max / PHYSICAL_BSIZE as u64;
        let device = Arc::new(device);
        let bd = Ext4Blockdev::new(
            Ext4Bd::new(device.clone(), bdname.as_str(), phys_bcount),
            LOGICAL_BSIZE,
            bcount,
            name,
        )?;

        let mut fs = Ext4Fs::new(bd, CString::new(name).unwrap(), false)?;

        match fs.create_dir("ids") {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                return Err(e.into());
            }
            _ => {}
        }

        Ok(Self {
            fs: Mutex::new(fs),
            device,
        })
    }

    pub fn get_id_path(&self, id: ObjID) -> (String, String) {
        let top = id.to_be_bytes()[0];
        let us = format!("ids/{:x}", top);
        (us, format!("ids/{:x}/{:x}", top, id))
    }

    pub fn get_object_as_file(&self, id: ObjID, create: bool) -> Result<Ext4File> {
        let flags = if create { O_RDWR | O_CREAT } else { O_RDWR };
        if let Some(ino) = objid_to_ino(id) {
            let mut fs = self.fs.lock().unwrap();
            return Ok(fs.open_file_from_inode(ino, flags)?);
        }
        let path = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        if create {
            match fs.create_dir(&path.0) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => Err(e)?,
            }
        }
        Ok(fs.open_file(&path.1, flags)?)
    }
}

impl PagedObjectStore for Ext4Store {
    fn create_object(&self, id: crate::ObjID) -> Result<()> {
        self.get_object_as_file(id, true)?;
        Ok(())
    }

    fn delete_object(&self, id: crate::ObjID) -> Result<()> {
        let path = self.get_id_path(id);
        Ok(self.fs.lock().unwrap().remove_file(&path.1)?)
    }

    fn len(&self, id: crate::ObjID) -> Result<u64> {
        let mut file = self.get_object_as_file(id, false)?;
        Ok(file.len())
    }

    fn read_object(&self, id: crate::ObjID, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut file = self.get_object_as_file(id, false)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buf)?)
    }

    fn write_object(&self, id: crate::ObjID, offset: u64, buf: &[u8]) -> Result<()> {
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

    fn get_config_id(&self) -> Result<ObjID> {
        let mut buf = [0; 16];
        self.read_object(0, 0, &mut buf).and_then(|len| {
            if len == 16 && buf.iter().find(|x| **x != 0).is_some() {
                Ok(ObjID::from_le_bytes(buf))
            } else {
                Err(ErrorKind::InvalidData.into())
            }
        })
    }

    fn set_config_id(&self, id: ObjID) -> Result<()> {
        let _ = self.delete_object(0);
        self.create_object(0)?;
        self.write_object(0, 0, &id.to_le_bytes())
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn page_in_object<'a>(&self, id: ObjID, reqs: &'a mut [crate::PageRequest]) -> Result<usize> {
        let file = self.get_object_as_file(id, false)?;
        let mut fs = self.fs.lock().unwrap();
        let mut inode = fs.get_file_inode(&file)?;
        let blocks_per_page = PAGE_SIZE / fs.block_size()? as usize;
        tracing::debug!("paging  in request for {} reqs", reqs.len());
        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut disk_pages = Vec::<DevicePage>::new();

                let mut page = req.start_page;
                let end = req.start_page + req.nr_pages as i64;

                while page < end {
                    let mut block = page as u32;
                    if objid_to_ino(id).is_some() {
                        // External files don't have null pages
                        block -= 1;
                    }
                    block = block * blocks_per_page as u32;
                    let rem_blocks = (end - page) as u32 * blocks_per_page as u32;

                    let item = match inode.get_data_blocks(block, rem_blocks, false) {
                        Ok((dblock, nr_dblk)) if nr_dblk > 0 => {
                            if dblock == 0 {
                                DevicePage::Hole(nr_dblk)
                            } else {
                                DevicePage::Run(dblock, nr_dblk)
                            }
                        }
                        _ => match inode.get_data_block(block, false)? {
                            0 => DevicePage::Hole(1),
                            dpg => DevicePage::Run(dpg, 1),
                        },
                    };
                    page += item.nr_pages() as i64;
                    if let Some(prev) = disk_pages.last_mut() {
                        if !prev.try_extend(&item) {
                            disk_pages.push(item);
                        }
                    } else {
                        disk_pages.push(item);
                    }
                }
                Result::Ok((req, disk_pages))
            })
            .try_collect::<Vec<_>>()?;
        drop(fs);
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            let _len = br.0.page_in(pages, &*self.device)?;
        }

        Ok(reqs.len())
    }

    fn page_out_object<'a>(&self, id: ObjID, reqs: &'a mut [crate::PageRequest]) -> Result<usize> {
        let end_offset = reqs
            .iter()
            .max_by_key(|req| req.start_page as u64 + req.nr_pages as u64)
            .map(|end_req| {
                (end_req.start_page as u64 + end_req.nr_pages as u64) * PAGE_SIZE as u64
            });

        let mut file = self.get_object_as_file(id, false)?;
        if end_offset.unwrap_or(0) >= file.len() {
            self.write_object(id, end_offset.unwrap_or(0), &[0u8; PAGE_SIZE])?;
        }
        let file = self.get_object_as_file(id, false)?;
        let mut fs = self.fs.lock().unwrap();
        let mut inode = fs.get_file_inode(&file)?;
        let blocks_per_page = PAGE_SIZE / fs.block_size()? as usize;
        tracing::debug!("paging out request for {} reqs", reqs.len());

        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut disk_pages = Vec::<DevicePage>::new();
                for page in req.start_page..(req.start_page + req.nr_pages as i64) {
                    let mut block = page as u32;
                    if objid_to_ino(id).is_some() {
                        // External files don't have null pages
                        block -= 1;
                    }
                    let item = match inode.get_data_block(block * blocks_per_page as u32, true)? {
                        0 => Result::Err(ErrorKind::Other.into())?,
                        dpg => DevicePage::Run(dpg, 1),
                    };
                    if let Some(prev) = disk_pages.last_mut() {
                        if !prev.try_extend(&item) {
                            disk_pages.push(item);
                        }
                    } else {
                        disk_pages.push(item);
                    }
                }
                Result::Ok((req, disk_pages))
            })
            .try_collect::<Vec<_>>()?;

        drop(fs);
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            let _len = br.0.page_out(pages, &*self.device)?;
        }
        Ok(reqs.len())
    }

    fn enumerate_external(&self, id: ObjID) -> Result<Vec<ExternalFile>> {
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

    fn find_external(&self, id: ObjID) -> Result<usize> {
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = ROOT_DIRECTORY_INODE;
        }
        let inode = fs.get_inode(inonr)?;
        Ok(inode.size() as usize)
    }
}
