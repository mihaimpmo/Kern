//! Files and directories.

use crate::{check, syscall, syscall6, sys, Errno};
use alloc::vec::Vec;

pub type Fd = u64;

/// `open` flags (Linux values). Pick one of READ_ONLY / WRITE_ONLY /
/// READ_WRITE and add any of the others.
pub mod flags {
    pub const READ_ONLY: u64 = 0;
    pub const WRITE_ONLY: u64 = 1;
    pub const READ_WRITE: u64 = 2;
    /// Create the file if it doesn't exist.
    pub const CREATE: u64 = 0x40;
    /// With CREATE: fail if it already exists.
    pub const EXCLUSIVE: u64 = 0x80;
    /// Empty the file.
    pub const TRUNCATE: u64 = 0x200;
    /// Every write goes to the end.
    pub const APPEND: u64 = 0x400;
    /// Fail unless it is a directory.
    pub const DIRECTORY: u64 = 0x1_0000;
}

/// Longest name of a directory entry.
pub const MAX_NAME: usize = 255;

pub fn open(path: &str, flags: u64) -> Result<Fd, Errno> {
    check(unsafe { syscall(sys::OPEN, path.as_ptr() as u64, path.len() as u64, flags) })
}

pub fn close(fd: Fd) -> Result<(), Errno> {
    check(unsafe { syscall(sys::CLOSE, fd, 0, 0) }).map(|_| ())
}

pub enum Whence {
    Start,
    Current,
    End,
}

/// Move the file offset; returns the new one.
pub fn seek(fd: Fd, offset: i64, whence: Whence) -> Result<u64, Errno> {
    let whence = match whence {
        Whence::Start => 0,
        Whence::Current => 1,
        Whence::End => 2,
    };
    check(unsafe { syscall(sys::LSEEK, fd, offset as u64, whence) })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Device,
}

fn kind_from(code: u64) -> Kind {
    match code {
        1 => Kind::File,
        2 => Kind::Dir,
        _ => Kind::Device,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stat {
    pub kind: Kind,
    /// Bytes in a file, entries in a directory.
    pub size: u64,
}

pub fn stat(path: &str) -> Result<Stat, Errno> {
    let mut raw = [0u64; 2];
    let ret = unsafe { syscall6(sys::STAT, path.as_ptr() as u64, path.len() as u64, raw.as_mut_ptr() as u64, 0, 0, 0) };
    check(ret)?;
    Ok(Stat { kind: kind_from(raw[0]), size: raw[1] })
}

pub fn fstat(fd: Fd) -> Result<Stat, Errno> {
    let mut raw = [0u64; 2];
    check(unsafe { syscall(sys::FSTAT, fd, raw.as_mut_ptr() as u64, 0) })?;
    Ok(Stat { kind: kind_from(raw[0]), size: raw[1] })
}

pub fn mkdir(path: &str) -> Result<(), Errno> {
    check(unsafe { syscall(sys::MKDIR, path.as_ptr() as u64, path.len() as u64, 0) }).map(|_| ())
}

/// Remove a file, or an empty directory.
pub fn unlink(path: &str) -> Result<(), Errno> {
    check(unsafe { syscall(sys::UNLINK, path.as_ptr() as u64, path.len() as u64, 0) }).map(|_| ())
}

pub fn chdir(path: &str) -> Result<(), Errno> {
    check(unsafe { syscall(sys::CHDIR, path.as_ptr() as u64, path.len() as u64, 0) }).map(|_| ())
}

/// The working directory, written into `buf`.
pub fn getcwd(buf: &mut [u8]) -> Result<&str, Errno> {
    let n = check(unsafe { syscall(sys::GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0) })? as usize;
    core::str::from_utf8(&buf[..n]).map_err(|_| Errno::EINVAL)
}

/// Another descriptor for the same open file (sharing its offset).
pub fn dup(fd: Fd) -> Result<Fd, Errno> {
    check(unsafe { syscall(sys::DUP, fd, 0, 0) })
}

/// Make `new_fd` refer to `fd`'s open file, closing what `new_fd` was.
pub fn dup2(fd: Fd, new_fd: Fd) -> Result<Fd, Errno> {
    check(unsafe { syscall(sys::DUP2, fd, new_fd, 0) })
}

/// Everything from the current offset to the end.
pub fn read_to_end(fd: Fd) -> Result<Vec<u8>, Errno> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = crate::read(fd, &mut chunk)?;
        if n == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&chunk[..n]);
    }
}

/// The whole contents of the file at `path`.
pub fn read_file(path: &str) -> Result<Vec<u8>, Errno> {
    let fd = open(path, flags::READ_ONLY)?;
    let data = read_to_end(fd);
    let _ = close(fd);
    data
}

pub struct DirEntry {
    pub kind: Kind,
    name: [u8; MAX_NAME],
    len: usize,
}

impl DirEntry {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.len]).unwrap_or("?")
    }
}

/// The entries of a directory, in name order. Closes it when dropped.
pub struct ReadDir {
    fd: Fd,
}

pub fn read_dir(path: &str) -> Result<ReadDir, Errno> {
    Ok(ReadDir { fd: open(path, flags::READ_ONLY | flags::DIRECTORY)? })
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buf = [0u8; 1 + MAX_NAME];
        let n = match check(unsafe { syscall(sys::READDIR, self.fd, buf.as_mut_ptr() as u64, buf.len() as u64) }) {
            Ok(0) => return None,
            Ok(n) => n as usize,
            Err(e) => return Some(Err(e)),
        };
        let mut entry = DirEntry { kind: kind_from(buf[0] as u64), name: [0; MAX_NAME], len: n - 1 };
        entry.name[..n - 1].copy_from_slice(&buf[1..n]);
        Some(Ok(entry))
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}
