//! crash: deliberately touches memory it doesn't own. The kernel kills this
//! process and everything else keeps running - that is memory protection.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    println!("crash: writing to kernel memory at 0xffffffff80000000...");
    unsafe { core::ptr::write_volatile(0xffff_ffff_8000_0000 as *mut u64, 0xdead) };
    println!("crash: this line should never be printed");
    0
}
