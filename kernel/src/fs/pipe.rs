//! Pipes: a bounded byte buffer with a read end and a write end.
//!
//! Each end is one inode, shared by every descriptor that refers to it
//! (after `dup` or `spawn`), so the end closes when its last descriptor
//! does (`Drop`). Reading an empty pipe blocks until data arrives, or
//! returns 0 (end of file) once the write end is closed; writing blocks
//! while the pipe is full, and fails with BrokenPipe once the read end is
//! closed.

use super::{FsError, Inode, Kind, Result};
use crate::sync::SpinLock;
use crate::task::WaitQueue;
use alloc::{collections::VecDeque, sync::Arc};

const CAPACITY: usize = 64 * 1024;

struct State {
    data: VecDeque<u8>,
    reader_open: bool,
    writer_open: bool,
}

struct Pipe {
    state: SpinLock<State>,
    /// Readers wait for data or for the writer to close.
    readable: WaitQueue,
    /// Writers wait for room or for the reader to close.
    writable: WaitQueue,
}

pub struct ReadEnd(Arc<Pipe>);
pub struct WriteEnd(Arc<Pipe>);

pub fn new() -> (Arc<ReadEnd>, Arc<WriteEnd>) {
    let pipe = Arc::new(Pipe {
        state: SpinLock::new(State { data: VecDeque::new(), reader_open: true, writer_open: true }),
        readable: WaitQueue::new(),
        writable: WaitQueue::new(),
    });
    (Arc::new(ReadEnd(pipe.clone())), Arc::new(WriteEnd(pipe)))
}

impl Inode for ReadEnd {
    fn kind(&self) -> Kind {
        Kind::Pipe
    }

    fn size(&self) -> u64 {
        self.0.state.lock().data.len() as u64
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        let pipe = &self.0;
        let mut state = pipe.state.lock();
        while state.data.is_empty() {
            if !state.writer_open || buf.is_empty() {
                return Ok(0);
            }
            state = pipe.readable.wait_interruptible(state).map_err(|_| FsError::Interrupted)?;
        }
        let n = buf.len().min(state.data.len());
        for (slot, byte) in buf.iter_mut().zip(state.data.drain(..n)) {
            *slot = byte;
        }
        drop(state);
        pipe.writable.wake_all();
        Ok(n)
    }
}

impl Inode for WriteEnd {
    fn kind(&self) -> Kind {
        Kind::Pipe
    }

    fn size(&self) -> u64 {
        self.0.state.lock().data.len() as u64
    }

    /// Writes everything, waiting for room as needed.
    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        let pipe = &self.0;
        let mut written = 0;
        while written < buf.len() {
            let mut state = pipe.state.lock();
            while state.reader_open && state.data.len() == CAPACITY {
                state = match pipe.writable.wait_interruptible(state) {
                    Ok(state) => state,
                    Err(_) if written > 0 => return Ok(written),
                    Err(_) => return Err(FsError::Interrupted),
                };
            }
            if !state.reader_open {
                return if written > 0 { Ok(written) } else { Err(FsError::BrokenPipe) };
            }
            let n = (buf.len() - written).min(CAPACITY - state.data.len());
            state.data.extend(&buf[written..written + n]);
            written += n;
            drop(state);
            pipe.readable.wake_all();
        }
        Ok(written)
    }
}

impl Drop for ReadEnd {
    fn drop(&mut self) {
        self.0.state.lock().reader_open = false;
        self.0.writable.wake_all();
    }
}

impl Drop for WriteEnd {
    fn drop(&mut self) {
        self.0.state.lock().writer_open = false;
        self.0.readable.wake_all();
    }
}
