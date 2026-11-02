//! hello: prints a few lines with pauses. Run it with `hello &` a couple of
//! times to watch several processes take turns. `hello <n>` prints n lines.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let count = match ulib::args().nth(1) {
        None => 5,
        Some(arg) => match arg.parse::<u32>() {
            Ok(n) => n,
            Err(_) => {
                println!("usage: hello [count]");
                return 2;
            }
        },
    };
    let pid = ulib::getpid();
    for i in 1..=count {
        println!("hello from process {} ({}/{})", pid, i, count);
        ulib::sleep_ms(500);
    }
    0
}
