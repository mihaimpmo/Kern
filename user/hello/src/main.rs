//! hello: prints a few lines with pauses. Run it with `hello &` a couple of
//! times to watch several processes take turns.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let pid = ulib::getpid();
    for i in 1..=5 {
        println!("hello from process {} ({}/5)", pid, i);
        ulib::sleep_ms(500);
    }
    0
}
