//! echo [text...]: print the arguments, separated by spaces.

#![no_std]
#![no_main]

use ulib::print;

#[no_mangle]
fn main() -> i32 {
    for (i, arg) in ulib::args().skip(1).enumerate() {
        print!("{}{}", if i > 0 { " " } else { "" }, arg);
    }
    print!("\n");
    0
}
