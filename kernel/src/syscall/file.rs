//! File system calls. File operations may block (console input, a sleeping
//! lock), so no spinlock is held across them: the open file is looked up,
//! the descriptor table unlocked, then the file used.

use super::{check_user, copy_from_user, copy_out, copy_to_user, user_path, Errno, SysResult};
use super::{EFAULT, EINVAL, ENOTDIR, ERANGE};
use crate::fs::{self, Kind, OpenFile, MAX_NAME};
use crate::task::{self, Process};
use alloc::sync::Arc;

/// Bytes moved per step. The console's lock keeps interrupts off, so device
/// writes go in small pieces; files take bigger ones.
const DEVICE_CHUNK: usize = 256;
const FILE_CHUNK: usize = 4096;

fn process() -> Result<Arc<Process>, Errno> {
    task::current_process().ok_or(EFAULT)
}

fn file(fd: u64) -> Result<Arc<OpenFile>, Errno> {
    Ok(process()?.files.lock().get(fd)?)
}

fn cwd() -> Result<alloc::string::String, Errno> {
    Ok(process()?.cwd.lock().clone())
}

fn chunk_for(file: &OpenFile) -> usize {
    if file.inode.kind() == Kind::File { FILE_CHUNK } else { DEVICE_CHUNK }
}

pub fn read(fd: u64, buf: u64, len: u64) -> SysResult {
    let file = file(fd)?;
    // Check before blocking, so input is never consumed and then lost.
    check_user(buf, len, true)?;
    let mut chunk = [0u8; FILE_CHUNK];
    let step = chunk_for(&file).min(FILE_CHUNK);
    let mut done = 0;
    while done < len {
        let want = (len - done).min(step as u64) as usize;
        let n = match file.read(&mut chunk[..want]) {
            Ok(n) => n,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e.into()),
        };
        copy_to_user(buf + done, &chunk[..n])?;
        done += n as u64;
        // Devices return what they have; files are read until full or EOF.
        if n < want || file.inode.kind() != Kind::File {
            break;
        }
    }
    Ok(done)
}

pub fn write(fd: u64, buf: u64, len: u64) -> SysResult {
    let file = file(fd)?;
    check_user(buf, len, false)?;
    let mut chunk = [0u8; FILE_CHUNK];
    let step = chunk_for(&file);
    let mut done = 0;
    while done < len {
        let n = (len - done).min(step as u64) as usize;
        copy_from_user(&mut chunk[..n], buf + done)?;
        match file.write(&chunk[..n]) {
            Ok(written) => done += written as u64,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(done)
}

pub fn open(path: u64, len: u64, flags: u64) -> SysResult {
    let path = user_path(path, len)?;
    let file = fs::open(&cwd()?, &path, flags)?;
    Ok(process()?.files.lock().insert(file, flags & fs::flags::CLOSE_ON_SPAWN != 0)?)
}

pub fn close(fd: u64) -> SysResult {
    let closed = process()?.files.lock().close(fd)?;
    drop(closed); // outside the table's lock
    Ok(0)
}

pub fn lseek(fd: u64, offset: i64, whence: u64) -> SysResult {
    Ok(file(fd)?.seek(offset, whence)?)
}

/// How a kind is numbered in Stat and readdir entries.
fn kind_code(kind: Kind) -> u8 {
    match kind {
        Kind::File => 1,
        Kind::Dir => 2,
        Kind::Device => 3,
        Kind::Pipe => 4,
    }
}

/// `struct Stat { kind: u64 (1 file, 2 directory, 3 device, 4 pipe), size: u64 }`
fn put_stat(inode: &Arc<dyn fs::Inode>, ptr: u64) -> SysResult {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&(kind_code(inode.kind()) as u64).to_le_bytes());
    out[8..].copy_from_slice(&inode.size().to_le_bytes());
    copy_to_user(ptr, &out)?;
    Ok(0)
}

pub fn stat(path: u64, len: u64, ptr: u64) -> SysResult {
    let path = user_path(path, len)?;
    put_stat(&fs::lookup(&cwd()?, &path)?, ptr)
}

pub fn fstat(fd: u64, ptr: u64) -> SysResult {
    put_stat(&file(fd)?.inode, ptr)
}

/// One directory entry per call: a kind byte (as in Stat) then the name.
/// The buffer must hold the longest possible entry, 1 + MAX_NAME bytes, so
/// an entry is never consumed without being delivered.
pub fn readdir(fd: u64, buf: u64, len: u64) -> SysResult {
    if len < 1 + MAX_NAME as u64 {
        return Err(EINVAL);
    }
    let file = file(fd)?;
    if file.inode.kind() != Kind::Dir {
        return Err(ENOTDIR);
    }
    check_user(buf, len, true)?;
    match file.next_entry()? {
        None => Ok(0),
        Some((name, kind)) => {
            let mut out = alloc::vec::Vec::with_capacity(1 + name.len());
            out.push(kind_code(kind));
            out.extend_from_slice(name.as_bytes());
            copy_out(&out, buf, len)
        }
    }
}

pub fn mkdir(path: u64, len: u64) -> SysResult {
    let path = user_path(path, len)?;
    fs::mkdir(&cwd()?, &path)?;
    Ok(0)
}

pub fn unlink(path: u64, len: u64) -> SysResult {
    let path = user_path(path, len)?;
    fs::unlink(&cwd()?, &path)?;
    Ok(0)
}

pub fn chdir(path: u64, len: u64) -> SysResult {
    let path = user_path(path, len)?;
    let process = process()?;
    let target = fs::canonical(&process.cwd.lock().clone(), &path)?;
    if fs::lookup("/", &target)?.kind() != Kind::Dir {
        return Err(ENOTDIR);
    }
    *process.cwd.lock() = target;
    Ok(0)
}

pub fn getcwd(buf: u64, len: u64) -> SysResult {
    let cwd = cwd()?;
    if (cwd.len() as u64) > len {
        return Err(ERANGE);
    }
    copy_out(cwd.as_bytes(), buf, len)
}

/// `flags` is 0 or CLOSE_ON_SPAWN (like F_DUPFD_CLOEXEC).
pub fn dup(fd: u64, flags: u64) -> SysResult {
    if flags & !fs::flags::CLOSE_ON_SPAWN != 0 {
        return Err(EINVAL);
    }
    let process = process()?;
    let mut files = process.files.lock();
    let file = files.get(fd)?;
    Ok(files.insert(file, flags != 0)?)
}

pub fn dup2(fd: u64, new_fd: u64) -> SysResult {
    let process = process()?;
    let replaced = {
        let mut files = process.files.lock();
        let file = files.get(fd)?;
        if fd == new_fd {
            return Ok(new_fd);
        }
        files.insert_at(new_fd, file)?
    };
    drop(replaced); // closed outside the table's lock
    Ok(new_fd)
}

/// A new pipe: `fds[0]` reads, `fds[1]` writes. `flags` may be
/// CLOSE_ON_SPAWN (O_CLOEXEC) for both ends.
pub fn pipe(fds: u64, flags: u64) -> SysResult {
    if flags & !fs::flags::CLOSE_ON_SPAWN != 0 {
        return Err(EINVAL);
    }
    let cloexec = flags != 0;
    check_user(fds, 16, true)?;
    let (read_end, write_end) = fs::pipe::new();
    let process = process()?;
    let (r, w) = {
        let mut files = process.files.lock();
        let r = files.insert(OpenFile::new(read_end, true, false, false), cloexec)?;
        match files.insert(OpenFile::new(write_end, false, true, false), cloexec) {
            Ok(w) => (r, w),
            Err(e) => {
                let _ = files.close(r);
                return Err(e.into());
            }
        }
    };
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&r.to_le_bytes());
    out[8..].copy_from_slice(&w.to_le_bytes());
    copy_to_user(fds, &out)?;
    Ok(0)
}
