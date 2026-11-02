//! mkdir <dir...>: create directories.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    if ulib::args().len() == 1 {
        println!("usage: mkdir <dir...>");
        return 2;
    }
    let mut failed = false;
    for path in ulib::args().skip(1) {
        if let Err(e) = ulib::fs::mkdir(path) {
            println!("mkdir: {}: {}", path, e);
            failed = true;
        }
    }
    failed as i32
}
