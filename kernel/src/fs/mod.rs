//! The virtual file system: one tree of inodes rooted at `/`.
//!
//! The root is an in-memory filesystem (`tmpfs`), filled at boot from the
//! initramfs (`initramfs.rs`) and given `/dev` (devices) and `/tmp`. Paths
//! are resolved textually: `.` and `..` are folded away before walking the
//! tree (there are no symlinks), so a path's canonical form is also what
//! `getcwd` reports.
//!
//! Open files (`OpenFile`) are shared between file descriptors after `dup`
//! and between parent and child after `spawn`, along with their offset.

pub mod dev;
pub mod initramfs;
pub mod pipe;
pub mod tmpfs;

use crate::sync::{Mutex, SpinLock};
use alloc::{string::String, sync::Arc, vec::Vec};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Device,
    Pipe,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    Invalid,
    NameTooLong,
    BadFd,
    TooManyFiles,
    NoMemory,
    /// A signal arrived during a blocking read or write.
    Interrupted,
    /// Writing to a pipe nobody reads.
    BrokenPipe,
}

pub type Result<T> = core::result::Result<T, FsError>;

pub const MAX_NAME: usize = 255;
pub const MAX_PATH: usize = 4096;

/// A file, directory or device. Directory operations default to NotDir and
/// data operations to IsDir, so each kind implements only its own.
pub trait Inode: Send + Sync {
    fn kind(&self) -> Kind;

    fn size(&self) -> u64 {
        0
    }

    /// Read at `offset`. Devices may block until data arrives.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }

    fn truncate(&self) -> Result<()> {
        Err(FsError::Invalid)
    }

    fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>> {
        Err(FsError::NotDir)
    }

    /// Create a new file or directory called `name`.
    fn create(&self, _name: &str, _kind: Kind) -> Result<Arc<dyn Inode>> {
        Err(FsError::NotDir)
    }

    /// Remove `name` (a directory only if it is empty).
    fn unlink(&self, _name: &str) -> Result<()> {
        Err(FsError::NotDir)
    }

    /// The `index`-th entry, or None past the end.
    fn entry(&self, _index: usize) -> Result<Option<(String, Kind)>> {
        Err(FsError::NotDir)
    }

    /// Whether this directory has no entries.
    fn is_empty(&self) -> bool {
        true
    }
}

static ROOT: SpinLock<Option<Arc<dyn Inode>>> = SpinLock::new(None);

/// Create the root filesystem with /dev and /tmp.
pub fn init() {
    let devices = Arc::new(tmpfs::Dir::new());
    devices.attach("console", Arc::new(dev::Console)).unwrap();
    devices.attach("null", Arc::new(dev::Null)).unwrap();
    let root = Arc::new(tmpfs::Dir::new());
    root.attach("dev", devices).unwrap();
    root.create("tmp", Kind::Dir).unwrap();
    *ROOT.lock() = Some(root);
}

fn root() -> Arc<dyn Inode> {
    ROOT.lock().clone().expect("filesystem not initialised")
}

// ---------------------------------------------------------------- paths

/// `path` as a canonical absolute path: relative paths start from `cwd`;
/// empty, `.` and `..` components are folded away (`..` at `/` stays `/`).
pub fn canonical(cwd: &str, path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    if path.len() > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    let mut parts: Vec<&str> = Vec::new();
    let start = if path.starts_with('/') { "" } else { cwd };
    for part in start.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name if name.len() > MAX_NAME => return Err(FsError::NameTooLong),
            name => parts.push(name),
        }
    }
    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// The inode at canonical path `path`.
fn walk(path: &str) -> Result<Arc<dyn Inode>> {
    let mut node = root();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        node = node.lookup(part)?;
    }
    Ok(node)
}

pub fn lookup(cwd: &str, path: &str) -> Result<Arc<dyn Inode>> {
    walk(&canonical(cwd, path)?)
}

/// The directory that would contain `path`, and the last component's name.
fn parent_of(cwd: &str, path: &str) -> Result<(Arc<dyn Inode>, String)> {
    let full = canonical(cwd, path)?;
    let (dir, name) = full.rsplit_once('/').unwrap(); // canonical paths start with '/'
    if name.is_empty() {
        return Err(FsError::Exists); // the root itself
    }
    let dir = walk(if dir.is_empty() { "/" } else { dir })?;
    if dir.kind() != Kind::Dir {
        return Err(FsError::NotDir);
    }
    Ok((dir, String::from(name)))
}

pub fn mkdir(cwd: &str, path: &str) -> Result<()> {
    let (dir, name) = parent_of(cwd, path)?;
    dir.create(&name, Kind::Dir).map(|_| ())
}

pub fn unlink(cwd: &str, path: &str) -> Result<()> {
    let (dir, name) = parent_of(cwd, path)?;
    dir.unlink(&name)
}

/// Create the file `path` (its directory must exist) and fill it.
pub fn write_file(path: &str, data: &[u8]) -> Result<()> {
    let (dir, name) = parent_of("/", path)?;
    let file = dir.create(&name, Kind::File)?;
    file.write_at(0, data).map(|_| ())
}

/// The whole contents of a regular file.
pub fn read_file(inode: &Arc<dyn Inode>) -> Result<Vec<u8>> {
    if inode.kind() != Kind::File {
        return Err(FsError::IsDir);
    }
    let mut data = Vec::new();
    data.try_reserve_exact(inode.size() as usize).map_err(|_| FsError::NoMemory)?;
    data.resize(inode.size() as usize, 0);
    let n = inode.read_at(0, &mut data)?;
    data.truncate(n);
    Ok(data)
}

// ----------------------------------------------------------- open files

/// Flags for `open`, with Linux's values.
pub mod flags {
    pub const READ_ONLY: u64 = 0;
    pub const WRITE_ONLY: u64 = 1;
    pub const READ_WRITE: u64 = 2;
    pub const ACCESS_MASK: u64 = 3;
    pub const CREATE: u64 = 0x40;
    pub const EXCLUSIVE: u64 = 0x80;
    pub const TRUNCATE: u64 = 0x200;
    pub const APPEND: u64 = 0x400;
    pub const DIRECTORY: u64 = 0x1_0000;
    /// O_CLOEXEC: don't pass the descriptor on to spawned children (also
    /// accepted by `pipe`).
    pub const CLOSE_ON_SPAWN: u64 = 0x8_0000;
}

/// An open file: the inode, how it was opened, and the current offset (for
/// directories, the index of the next entry).
pub struct OpenFile {
    pub inode: Arc<dyn Inode>,
    readable: bool,
    writable: bool,
    append: bool,
    offset: Mutex<u64>,
}

impl OpenFile {
    pub fn new(inode: Arc<dyn Inode>, readable: bool, writable: bool, append: bool) -> Arc<OpenFile> {
        Arc::new(OpenFile { inode, readable, writable, append, offset: Mutex::new(0) })
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.readable {
            return Err(FsError::BadFd);
        }
        let mut offset = self.offset.lock();
        let n = self.inode.read_at(*offset, buf)?;
        *offset += n as u64;
        Ok(n)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        if !self.writable {
            return Err(FsError::BadFd);
        }
        let mut offset = self.offset.lock();
        if self.append {
            *offset = self.inode.size();
        }
        let n = self.inode.write_at(*offset, buf)?;
        *offset += n as u64;
        Ok(n)
    }

    /// `whence`: 0 from the start, 1 from the current offset, 2 from the end.
    pub fn seek(&self, delta: i64, whence: u64) -> Result<u64> {
        let mut offset = self.offset.lock();
        let base = match whence {
            0 => 0,
            1 => *offset,
            2 => self.inode.size(),
            _ => return Err(FsError::Invalid),
        };
        let target = base.checked_add_signed(delta).ok_or(FsError::Invalid)?;
        *offset = target;
        Ok(target)
    }

    /// The next directory entry, advancing the offset.
    pub fn next_entry(&self) -> Result<Option<(String, Kind)>> {
        let mut offset = self.offset.lock();
        let entry = self.inode.entry(*offset as usize)?;
        if entry.is_some() {
            *offset += 1;
        }
        Ok(entry)
    }
}

/// Open `path` the way `open(2)` would.
pub fn open(cwd: &str, path: &str, how: u64) -> Result<Arc<OpenFile>> {
    let access = how & flags::ACCESS_MASK;
    let (readable, writable) = match access {
        flags::READ_ONLY => (true, false),
        flags::WRITE_ONLY => (false, true),
        flags::READ_WRITE => (true, true),
        _ => return Err(FsError::Invalid),
    };
    let inode = match lookup(cwd, path) {
        Ok(_) if how & flags::CREATE != 0 && how & flags::EXCLUSIVE != 0 => return Err(FsError::Exists),
        Ok(inode) => inode,
        Err(FsError::NotFound) if how & flags::CREATE != 0 => {
            let (dir, name) = parent_of(cwd, path)?;
            dir.create(&name, Kind::File)?
        }
        Err(e) => return Err(e),
    };
    match inode.kind() {
        Kind::Dir if writable => return Err(FsError::IsDir),
        Kind::Dir => {}
        _ if how & flags::DIRECTORY != 0 => return Err(FsError::NotDir),
        _ => {}
    }
    if how & flags::TRUNCATE != 0 && writable && inode.kind() == Kind::File {
        inode.truncate()?;
    }
    Ok(OpenFile::new(inode, readable, writable, how & flags::APPEND != 0))
}

// ------------------------------------------------------ descriptor tables

pub const MAX_FDS: usize = 64;

/// A process's file descriptors.
#[derive(Default)]
pub struct FdTable {
    slots: Vec<Option<Slot>>,
}

struct Slot {
    file: Arc<OpenFile>,
    /// Not passed on to spawned children (O_CLOEXEC).
    close_on_spawn: bool,
}

impl FdTable {
    /// stdin, stdout and stderr on the console.
    pub fn with_console() -> FdTable {
        let console = lookup("/", "/dev/console").expect("no /dev/console");
        let file = OpenFile::new(console, true, true, false);
        let slot = || Some(Slot { file: file.clone(), close_on_spawn: false });
        FdTable { slots: alloc::vec![slot(), slot(), slot()] }
    }

    /// The descriptors a spawned child starts with: the same open files,
    /// minus those marked close-on-spawn.
    pub fn for_child(&self) -> FdTable {
        let slots = self
            .slots
            .iter()
            .map(|s| match s {
                Some(s) if !s.close_on_spawn => Some(Slot { file: s.file.clone(), close_on_spawn: false }),
                _ => None,
            })
            .collect();
        FdTable { slots }
    }

    pub fn get(&self, fd: u64) -> Result<Arc<OpenFile>> {
        match self.slots.get(fd as usize) {
            Some(Some(slot)) => Ok(slot.file.clone()),
            _ => Err(FsError::BadFd),
        }
    }

    /// Install `file` at the lowest free descriptor.
    pub fn insert(&mut self, file: Arc<OpenFile>, close_on_spawn: bool) -> Result<u64> {
        let fd = match self.slots.iter().position(|s| s.is_none()) {
            Some(free) => free,
            None if self.slots.len() < MAX_FDS => {
                self.slots.push(None);
                self.slots.len() - 1
            }
            None => return Err(FsError::TooManyFiles),
        };
        self.slots[fd] = Some(Slot { file, close_on_spawn });
        Ok(fd as u64)
    }

    /// Install `file` at `fd` (inherited by children, as after `dup2`).
    /// Returns what was there, for the caller to drop (close) after
    /// releasing the table's lock.
    pub fn insert_at(&mut self, fd: u64, file: Arc<OpenFile>) -> Result<Option<Arc<OpenFile>>> {
        let fd = fd as usize;
        if fd >= MAX_FDS {
            return Err(FsError::BadFd);
        }
        if self.slots.len() <= fd {
            self.slots.resize_with(fd + 1, || None);
        }
        Ok(self.slots[fd].replace(Slot { file, close_on_spawn: false }).map(|s| s.file))
    }

    pub fn close(&mut self, fd: u64) -> Result<Arc<OpenFile>> {
        match self.slots.get_mut(fd as usize).and_then(|s| s.take()) {
            Some(slot) => Ok(slot.file),
            None => Err(FsError::BadFd),
        }
    }

    /// Take every open file out, to be dropped by the caller.
    pub fn take_all(&mut self) -> Vec<Arc<OpenFile>> {
        self.slots.drain(..).flatten().map(|s| s.file).collect()
    }
}
