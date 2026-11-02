//! rm <path...>: remove files and empty directories.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    if ulib::args().len() == 1 {
        println!("usage: rm <path...>");
        return 2;
    }
    let mut failed = false;
    for path in ulib::args().skip(1) {
        if let Err(e) = ulib::fs::unlink(path) {
            println!("rm: {}: {}", path, e);
            failed = true;
        }
    }
    failed as i32
}
