//! spin: burns CPU for 3 seconds without ever yielding. Run `spin &` and the
//! shell still responds, because the timer interrupt preempts this process.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let start = ulib::uptime_ms();
    let mut loops: u64 = 0;
    let mut next_report = start + 1000;
    while ulib::uptime_ms() < start + 3000 {
        loops = loops.wrapping_add(1);
        for _ in 0..1000 {
            core::hint::black_box(loops);
        }
        if ulib::uptime_ms() >= next_report {
            println!("spin: still busy ({} s)", (next_report - start) / 1000);
            next_report += 1000;
        }
    }
    println!("spin: done");
    0
}
