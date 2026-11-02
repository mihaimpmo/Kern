//! sleep <seconds>: wait, e.g. `sleep 2` or `sleep 0.25`.

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    match ulib::args().nth(1).and_then(parse_ms) {
        Some(ms) if ulib::args().len() == 2 => {
            ulib::sleep_ms(ms);
            0
        }
        _ => {
            println!("usage: sleep <seconds>");
            2
        }
    }
}

/// "2" -> 2000, "0.25" -> 250 (to the millisecond).
fn parse_ms(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > 3 || (whole.is_empty() && frac.is_empty()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let frac: u64 = if frac.is_empty() { 0 } else { frac.parse::<u64>().ok()? * 10u64.pow(3 - frac.len() as u32) };
    whole.checked_mul(1000)?.checked_add(frac)
}
