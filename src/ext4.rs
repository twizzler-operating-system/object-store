#[cfg(not(target_os = "twizzler"))]
use std::io::Result;
use std::{
    collections::HashMap,
    ffi::CString,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
    time::Instant,
};

use libc::{PATH_MAX, mode_t};
use lwext4_rs::{
    Ext4Blockdev, Ext4BlockdevIface, Ext4File, Ext4Fs, MpLock, O_CREAT, O_RDONLY, O_RDWR,
};
use mayheap::Vec;
use pager_dynamic::{ino_to_objid, objid_to_ino, ExternalFile};
#[cfg(target_os = "twizzler")]
use twizzler::Result;

use crate::{
    paged_object_store::MAYHEAP_LEN, DevicePage, ExternalFileStore, ExternalOpenFlags, ObjID,
    PagedDevice, PagedObjectStore, PosIo, PAGE_SIZE,
};

#[derive(Default)]
struct ExtCache {
    ids: HashMap<ObjID, (ExternalFile, usize)>,
    names: HashMap<u32, HashMap<String, (ExternalFile, usize)>>,
}

#[allow(dead_code)]
impl ExtCache {
    pub fn fill_dir(&mut self, ino: u32, items: impl Iterator<Item = (ExternalFile, usize)>) {
        let entry = self.names.entry(ino).or_default();
        for item in items {
            if let Some(name) = item.0.name() {
                entry.insert(name.to_owned(), item.clone());
                self.ids.insert(item.0.id.into(), item);
            }
        }
    }

    /*
    pub fn readdir(&self, ino: u32) -> Option<std::vec::Vec<ExternalFile>> {
        let entry = self.names.get(&ino)?;
        Some(entry.values().map(|e| e.0).collect())
    }
    */

    pub fn reset_dir(&mut self, ino: u32) {
        if let Some(mut map) = self.names.remove(&ino) {
            for item in map.drain() {
                self.ids.remove(&item.1 .0.id.into());
            }
        }
    }

    /*
    pub fn lookup(&self, ino: u32, name: &str) -> Option<(ExternalFile, usize)> {
        let map = self.names.get(&ino)?;
        map.get(name).copied()
    }

    pub fn get_by_id(&self, id: ObjID) -> Option<(ExternalFile, usize)> {
        self.ids.get(&id).copied()
    }
    */
}

pub struct Ext4Store<D: Device> {
    fs: Mutex<Ext4Fs>,
    ext_cache: Mutex<ExtCache>,
    len_cache: Mutex<HashMap<ObjID, u64>>,
    device: D,
}

pub trait Device: PosIo + PagedDevice + Sync + Send + Clone + 'static {}

impl<T: PosIo + PagedDevice + Sync + Send + Clone + 'static> Device for T {}

struct Ext4Bd<D: Device> {
    device: D,
    phys_bcount: u64,
    lock: MpLock,
}

impl<D: Device> Ext4BlockdevIface for Ext4Bd<D> {
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
        let len = self.device.run_async(self.device.read(start, slice))?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn write(&mut self, buf: *const u8, block: u64, bcount: u32) -> std::io::Result<u32> {
        let start = block * PHYSICAL_BSIZE as u64;
        let len = bcount as u64 * PHYSICAL_BSIZE as u64;
        let slice = unsafe { core::slice::from_raw_parts(buf, len as usize) };
        let len = self.device.run_async(self.device.write(start, slice))?;
        Ok((len / PHYSICAL_BSIZE as usize) as u32)
    }

    fn lock(&self) -> std::io::Result<()> {
        self.lock.lock();
        Ok(())
    }

    fn unlock(&self) -> std::io::Result<()> {
        self.lock.unlock();
        Ok(())
    }
}

impl<D: Device> Ext4Bd<D> {
    fn new(device: D, _name: &str, phys_bcount: u64) -> Self {
        Self {
            device,
            phys_bcount,
            lock: MpLock::new(),
        }
    }
}

static BDEV_ID: AtomicU64 = AtomicU64::new(0);

const LOGICAL_BSIZE: u32 = 512;
const PHYSICAL_BSIZE: u32 = 512;

impl<D: Device> Ext4Store<D> {
    pub async fn new(device: D, name: &str) -> Result<Self> {
        let bdname = format!("blockdev-{}", BDEV_ID.fetch_add(1, Ordering::SeqCst));
        let max = device.len().await? as u64;
        let bcount = max / LOGICAL_BSIZE as u64;
        let phys_bcount = max / PHYSICAL_BSIZE as u64;
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
            ext_cache: Mutex::new(ExtCache::default()),
            len_cache: Mutex::new(HashMap::default()),
        })
    }

    fn get_len_from_cache(&self, id: ObjID) -> Option<u64> {
        self.len_cache.lock().unwrap().get(&id).copied()
    }

    async fn readlink(&self, id: ObjID) -> Result<String> {
        let mut buf = vec![0; PATH_MAX as usize];
        let len = self.read_object(id, 0, &mut buf).await?;
        buf.truncate(len);
        String::from_utf8(buf).map_err(|_| ErrorKind::InvalidData.into())
    }

    fn invalidate_len(&self, id: ObjID) {
        self.len_cache.lock().unwrap().remove(&id);
    }

    fn set_len_in_cache(&self, id: ObjID, len: u64) {
        self.len_cache.lock().unwrap().insert(id, len);
    }

    pub fn get_id_path(&self, id: ObjID) -> (String, String) {
        let top = id.to_be_bytes()[0];
        let us = format!("ids/{:x}", top);
        (us, format!("ids/{:x}/{:x}", top, id))
    }

    pub fn set_len(&self, id: ObjID, len: u64) -> Result<()> {
        let mut fs = self.fs.lock().unwrap();
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        file.truncate(len)?;
        self.set_len_in_cache(id, len);
        Ok(())
    }

    pub fn get_object_as_file<'a>(
        &self,
        fs: &'a mut MutexGuard<'_, Ext4Fs>,
        id: ObjID,
        create: bool,
    ) -> Result<Ext4File<'a>> {
        let flags = if create { O_RDWR | O_CREAT } else { O_RDWR };
        if let Some(ino) = objid_to_ino(id) {
            return Ok(fs.open_file_from_inode(ino, flags)?);
        }
        let path = self.get_id_path(id);
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

impl<D: Device> PagedObjectStore for Ext4Store<D> {
    async fn create_object(&self, id: crate::ObjID) -> Result<()> {
        let mut fs = self.fs.lock().unwrap();
        self.get_object_as_file(&mut fs, id, true)?;
        fs.flush()?;
        Ok(())
    }

    async fn delete_object(&self, id: crate::ObjID) -> Result<()> {
        let path = self.get_id_path(id);
        let mut fs = self.fs.lock().unwrap();
        fs.remove_file(&path.1)?;
        fs.flush()?;
        Ok(())
    }

    async fn len(&self, id: crate::ObjID) -> Result<u64> {
        if let Some(len) = self.get_len_from_cache(id) {
            return Ok(len);
        }
        let mut fs = self.fs.lock().unwrap();
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        self.set_len_in_cache(id, file.len());
        Ok(file.len())
    }

    async fn read_object(&self, id: crate::ObjID, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut fs = self.fs.lock().unwrap();
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buf)?)
    }

    async fn write_object(&self, id: crate::ObjID, offset: u64, buf: &[u8]) -> Result<()> {
        let mut fs = self.fs.lock().unwrap();
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        if offset > file.len() {
            file.ensure_backing(offset)
                .inspect_err(|e| tracing::warn!("failed to ensure backing for object: {}", e))?;
            file.truncate(offset).inspect_err(|e| {
                tracing::warn!("failed to initialize object to {}: {}", offset, e)
            })?;
            self.invalidate_len(id);
        }
        file.seek(SeekFrom::Start(offset))?;
        // TODO
        file.write(buf)?;
        drop(file);
        fs.flush()?;
        Ok(())
    }

    async fn page_in_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest],
    ) -> Result<usize> {
        let mut fs = self.fs.lock().unwrap();
        let blocks_per_page = PAGE_SIZE / fs.block_size()? as usize;
        let mut file = self
            .get_object_as_file(&mut fs, id, false)
            .inspect_err(|e| tracing::error!("go err: {}", e))?;
        let mut inode = file
            .get_file_inode()
            .inspect_err(|e| tracing::error!("gfi err: {}", e))?;
        let max_len = inode.size();
        tracing::trace!("paging  in request for {} reqs", reqs.len());

        let mut iters = 0;
        drop(file);
        drop(fs);
        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut disk_pages = Vec::<DevicePage, MAYHEAP_LEN>::new();

                let mut page = req.start_page;
                let end = req.start_page + req.nr_pages as i64;

                let rem_blocks = (end - page) as u32 * blocks_per_page as u32;
                if rem_blocks > 64
                    && page as usize * PAGE_SIZE >= (max_len as usize + PAGE_SIZE * 8)
                {
                    let _ = disk_pages.push(DevicePage::Hole(rem_blocks));
                } else {
                    let mut fs = self.fs.lock().unwrap();
                    while page < end {
                        iters += 1;
                        if iters % 100 == 0 {
                            drop(fs);
                            self.device.yield_now();
                            fs = self.fs.lock().unwrap();
                        }

                        let mut block = page as u32;
                        if objid_to_ino(id).is_some() && block > 0 {
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
                                disk_pages.push(item).unwrap();
                            }
                        } else {
                            disk_pages.push(item).unwrap();
                        }
                    }
                }
                Result::Ok((req, disk_pages))
            })
            .try_collect::<Vec<_, MAYHEAP_LEN>>()?;
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            tracing::trace!("paging in {:?}", pages);
            let _len = br.0.page_in(pages, &self.device).await?;
        }

        Ok(reqs.len())
    }

    async fn page_out_object<'a>(
        &self,
        id: ObjID,
        reqs: &'a mut [crate::PageRequest],
    ) -> Result<usize> {
        let end_offset = reqs
            .iter()
            .max_by_key(|req| req.start_page as u64 + req.nr_pages as u64)
            .map(|end_req| {
                (end_req.start_page as u64 + end_req.nr_pages as u64) * PAGE_SIZE as u64
            });

        let start = Instant::now();
        let mut fs = self.fs.lock().unwrap();
        let blocks_per_page = PAGE_SIZE / fs.block_size()? as usize;
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        if end_offset.unwrap_or(0) >= file.len() {
            drop(file);
            drop(fs);
            self.write_object(id, end_offset.unwrap_or(0), &[0u8; PAGE_SIZE])
                .await?;
            fs = self.fs.lock().unwrap();
        } else {
            drop(file);
        }
        let mut file = self.get_object_as_file(&mut fs, id, false)?;
        let mut inode = file.get_file_inode()?;
        drop(file);
        drop(fs);
        tracing::trace!("paging out {:x} request for {} reqs", id, reqs.len());

        let setup_done = Instant::now();
        let mut iters = 0;
        let mut blocks = reqs
            .iter_mut()
            .map(|req| {
                let mut fs = self.fs.lock().unwrap();
                let mut disk_pages = Vec::<DevicePage, MAYHEAP_LEN>::new();

                let mut page = req.start_page;
                let end = req.start_page + req.nr_pages as i64;
                while page < end {
                    let mut block = page as u32;
                    tracing::trace!("paging out block {}",  block);
                    if objid_to_ino(id).is_some() && block > 0 {
                        // External files don't have null pages
                        block -= 1;
                    }

                    iters += 1;
                    if iters % 100 == 0 {
                        drop(fs);
                        self.device.yield_now();
                        fs = self.fs.lock().unwrap();
                    }

                    block = block * blocks_per_page as u32;
                    let rem_blocks = (end - page) as u32 * blocks_per_page as u32;

                    let item = match inode.get_data_blocks(block, rem_blocks, true) {
                        Ok((dblock, nr_dblk)) if nr_dblk > 0 => {
                            if dblock == 0 {
                                tracing::warn!(
                                    "got unexpected zero block when paging out object {:x}",
                                    id
                                );
                                Result::Err(ErrorKind::Other.into())?
                            } else {
                                DevicePage::Run(dblock, nr_dblk)
                            }
                        }
                        _ => match inode.get_data_block(block, true).inspect_err(|e| tracing::warn!("failed to get_data_block: {}", e))? {
                            0 => {
                                tracing::warn!(
                                    "got unexpected zero block when paging out object {:x} in fallback",
                                    id
                                );
                                Result::Err(ErrorKind::Other.into())?
                            }
                            dpg => DevicePage::Run(dpg, 1),
                        },
                    };
                    page += item.nr_pages() as i64;
                    if let Some(prev) = disk_pages.last_mut() {
                        if !prev.try_extend(&item) {
                            disk_pages.push(item).unwrap();
                        }
                    } else {
                        disk_pages.push(item).unwrap();
                    }
                }
                Result::Ok((req, disk_pages))
            })
            .try_collect::<Vec<_, MAYHEAP_LEN>>()?;
        tracing::trace!(
            "found blocks for paging out in {}ms",
            (Instant::now() - setup_done).as_millis()
        );

        let blocks_found = Instant::now();
        for br in blocks.iter_mut() {
            let pages = &br.1[..];
            let _len = br.0.page_out(pages, &self.device).await?;
        }
        let mut fs = self.fs.lock().unwrap();
        fs.flush()?;
        let io_done = Instant::now();
        tracing::trace!(
            "==> {}ms {}ms {}ms",
            (setup_done - start).as_millis(),
            (blocks_found - setup_done).as_millis(),
            (io_done - blocks_found).as_millis()
        );
        Ok(reqs.len())
    }

    /*
    async fn enumerate_external(&self, id: ObjID) -> Result<std::vec::Vec<ExternalFile>> {
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = 2;
        }

        if let Some(r) = self.ext_cache.lock().unwrap().readdir(inonr) {
            return Ok(r);
        }

        let mut inode = fs.get_inode(inonr)?;
        let diriter = fs.dirents(&mut inode)?;

        let diriter = diriter.filter_map(|de| {
            de.1.ok().map(|ino| {
                (
                    ExternalFile::new(&de.0, ino.kind().into(), ino_to_objid(ino.num())),
                    ino.size() as usize,
                )
            })
        });
        self.ext_cache.lock().unwrap().reset_dir(inonr);
        self.ext_cache.lock().unwrap().fill_dir(inonr, diriter);
        if let Some(r) = self.ext_cache.lock().unwrap().readdir(inonr) {
            Ok(r)
        } else {
            Err(ErrorKind::Other.into())
        }
    }

    async fn find_external(&self, id: ObjID) -> Result<usize> {
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(id).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = 2;
        }
        if let Some(info) = self.ext_cache.lock().unwrap().get_by_id(id) {
            return Ok(info.1);
        }
        let inode = fs.get_inode(inonr)?;
        Ok(inode.size() as usize)
    }
    */
}

impl<D: Device> Ext4Store<D> {
    pub async fn do_open_at(
        at: Option<&ExternalFile>,
        path: impl AsRef<Path>,
        flags: ExternalOpenFlags,
        mode: mode_t,
    ) -> Result<ExternalFile> {
        // Implementation for openat
        unimplemented!()
    }
}

impl<D: Device> ExternalFileStore for Ext4Store<D> {
    async fn open_external(
        &self,
        at: Option<ObjID>,
        path: impl AsRef<Path>,
        flags: ExternalOpenFlags,
        mode: mode_t,
    ) -> Result<ExternalFile> {
        let mut at_ino = if let Some(at) = at {
            objid_to_ino(at).ok_or(ErrorKind::InvalidInput)?
        } else {
            2
        };
        if at_ino < 2 {
            at_ino = 2;
        }
        tracing::trace!(
            "opening external file at {:?} with flags {:?} and mode {:o} at ino {}",
            path.as_ref(),
            flags,
            mode, at_ino
        );

        let mut fs = self.fs.lock().unwrap();

        let mut oflags = if flags.contains(ExternalOpenFlags::READ)
            && flags.contains(ExternalOpenFlags::WRITE)
        {
            O_RDWR
        } else if flags.contains(ExternalOpenFlags::READ) {
            O_RDONLY
        } else {
            O_RDWR
        };

        if flags.contains(ExternalOpenFlags::CREATE) {
            oflags |= O_CREAT;
        }

        let mut file = fs.open_file_from_container(
            at_ino,
            path.as_ref().to_string_lossy().as_ref(),
            oflags,
            mode,
        )?;

        Ok(ExternalFile::new(
            path.as_ref().to_string_lossy().to_string(),
            file.get_file_inode()?.kind().into(),
            ino_to_objid(file.get_file_inode()?.num()),
        ))
    }

    async fn unlink_external(&self, at: Option<ObjID>, path: impl AsRef<Path>) -> Result<()> {
        return Ok(());
    }

    async fn readlink_external(&self, at: ObjID) -> Result<String> {
        self.readlink(at).await
    }

    async fn readdir_external(
        &self,
        dir: ObjID,
        skip: usize,
        count: usize,
        entries: &mut std::vec::Vec<ExternalFile>,
    ) -> Result<()> {
        entries.clear();
        tracing::trace!(
            "enumerating external namespace {:x} (skip {}, count {})",
            dir,
            skip,
            count
        );
        let mut fs = self.fs.lock().unwrap();
        let mut inonr = objid_to_ino(dir).ok_or(ErrorKind::InvalidInput)?;
        if inonr == 0 {
            inonr = 2;
        }

 //       if let Some(_) =  self.ext_cache.lock().unwrap().readdir(inonr){
  //          return Ok(r);
   //     }

        let mut inode = fs.get_inode(inonr)?;
        let diriter = fs.dirents(&mut inode)?;

        let diriter = diriter.skip(skip).take(count).filter_map(|de| {
            de.1.ok().map(|ino| {
                ExternalFile::new(
                    unsafe { str::from_utf8_unchecked(&de.0) },
                    ino.kind().into(),
                    ino_to_objid(ino.num()),
                )
            })
        });

        for entry in diriter {
            tracing::trace!("record external file {} in namespace {:x} with ID {} and kind {:?}",
                entry.name().unwrap_or("<invalid utf8>"),
                dir,
                entry.id,
                entry.kind
            );
            entries.push(entry)
        }
        tracing::trace!("collected {} entries", entries.len());

        Ok(())

        //self.ext_cache.lock().unwrap().reset_dir(inonr);
        //self.ext_cache.lock().unwrap().fill_dir(inonr, diriter);
        /*
        if let Some(r) = self.ext_cache.lock().unwrap().readdir(inonr) {
            Ok(r)
        } else {
            Err(ErrorKind::Other.into())
        }
        */
    }

    async fn link_external(
        &self,
        file: &ExternalFile,
        at: Option<ObjID>,
        path: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }

    async fn stat_external(&self, path: impl AsRef<Path>) -> Result<libc::stat> {
        todo!()
    }

    async fn fstat_external(&self, file: Option<ObjID>) -> Result<libc::stat> {
        todo!()
    }

    async fn symlink_external(
        &self,
        at: Option<ObjID>,
        target: impl AsRef<Path>,
        linkpath: impl AsRef<Path>,
    ) -> Result<()> {
        todo!()
    }
}
