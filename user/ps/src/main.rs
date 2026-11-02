//! ps: list processes.

#![no_std]
#![no_main]

#[no_mangle]
fn main() -> i32 {
    let mut buf = [0u8; 8192];
    match ulib::ps(&mut buf) {
        Ok(n) => ulib::write_all(ulib::STDOUT, &buf[..n]).is_err() as i32,
        Err(_) => 1,
    }
}
