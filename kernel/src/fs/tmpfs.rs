//! In-memory files and directories. Contents live in the kernel heap and
//! are gone at reboot.

use super::{FsError, Inode, Kind, Result};
use crate::sync::Mutex;
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

pub struct File {
    data: Mutex<Vec<u8>>,
}

impl Inode for File {
    fn kind(&self) -> Kind {
        Kind::File
    }

    fn size(&self) -> u64 {
        self.data.lock().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.data.lock();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }

    /// Writing past the end fills the gap with zeros.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let mut data = self.data.lock();
        let end = (offset as usize).checked_add(buf.len()).ok_or(FsError::Invalid)?;
        if end > data.len() {
            let more = end - data.len();
            data.try_reserve(more).map_err(|_| FsError::NoMemory)?;
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn truncate(&self) -> Result<()> {
        let mut data = self.data.lock();
        data.clear();
        data.shrink_to_fit();
        Ok(())
    }
}

pub struct Dir {
    entries: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
}

impl Dir {
    pub fn new() -> Dir {
        Dir { entries: Mutex::new(BTreeMap::new()) }
    }

    /// Put an existing inode (a device, another directory) under `name`.
    pub fn attach(&self, name: &str, inode: Arc<dyn Inode>) -> Result<()> {
        let mut entries = self.entries.lock();
        if entries.contains_key(name) {
            return Err(FsError::Exists);
        }
        entries.insert(String::from(name), inode);
        Ok(())
    }
}

impl Inode for Dir {
    fn kind(&self) -> Kind {
        Kind::Dir
    }

    fn size(&self) -> u64 {
        self.entries.lock().len() as u64
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        self.entries.lock().get(name).cloned().ok_or(FsError::NotFound)
    }

    fn create(&self, name: &str, kind: Kind) -> Result<Arc<dyn Inode>> {
        let inode: Arc<dyn Inode> = match kind {
            Kind::File => Arc::new(File { data: Mutex::new(Vec::new()) }),
            Kind::Dir => Arc::new(Dir::new()),
            Kind::Device => return Err(FsError::Invalid),
        };
        self.attach(name, inode.clone())?;
        Ok(inode)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut entries = self.entries.lock();
        let inode = entries.get(name).ok_or(FsError::NotFound)?;
        if inode.kind() == Kind::Dir && !inode.is_empty() {
            return Err(FsError::NotEmpty);
        }
        // Open files keep the inode alive until they are closed.
        entries.remove(name);
        Ok(())
    }

    fn entry(&self, index: usize) -> Result<Option<(String, Kind)>> {
        Ok(self.entries.lock().iter().nth(index).map(|(name, inode)| (name.clone(), inode.kind())))
    }

    fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}
