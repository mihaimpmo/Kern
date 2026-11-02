//! Devices in /dev.

use super::{FsError, Inode, Kind, Result};
use crate::drivers::{keyboard, serial};

/// The keyboard (reading) and screen + serial port (writing).
pub struct Console;

impl Inode for Console {
    fn kind(&self) -> Kind {
        Kind::Device
    }

    /// Blocks until at least one key is available (or a signal arrives).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        keyboard::read(buf).map_err(|_| FsError::Interrupted)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        // Let the serial buffer drain rather than making the console fall
        // back to slow synchronous output (2x: '\r' is added per '\n').
        while !serial::has_room(2 * buf.len()) {
            crate::task::sleep_ms(1);
        }
        crate::console::write_bytes(buf);
        Ok(buf.len())
    }
}

/// Reads as empty, swallows writes.
pub struct Null;

impl Inode for Null {
    fn kind(&self) -> Kind {
        Kind::Device
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}
