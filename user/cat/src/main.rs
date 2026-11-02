//! cat [file...]: copy files (or standard input) to standard output.

#![no_std]
#![no_main]

use ulib::fs::{self, flags, Fd};
use ulib::{println, Errno};

#[no_mangle]
fn main() -> i32 {
    if ulib::args().len() == 1 {
        return copy(ulib::STDIN).is_err() as i32;
    }
    let mut failed = false;
    for path in ulib::args().skip(1) {
        let result = fs::open(path, flags::READ_ONLY).and_then(|fd| {
            let r = copy(fd);
            let _ = fs::close(fd);
            r
        });
        if let Err(e) = result {
            println!("cat: {}: {}", path, e);
            failed = true;
        }
    }
    failed as i32
}

fn copy(fd: Fd) -> Result<(), Errno> {
    let mut buf = [0u8; 4096];
    loop {
        let n = ulib::read(fd, &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        ulib::write_all(ulib::STDOUT, &buf[..n])?;
    }
}
