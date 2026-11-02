//! date: the current date and time (UTC).

#![no_std]
#![no_main]

use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let ms = ulib::time_ms();
    if ms == 0 {
        println!("date: the clock is not available");
        return 1;
    }
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    println!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", year, month, day, rest / 3600, rest / 60 % 60, rest % 60);
    0
}

/// Days since 1970-01-01 -> (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + (month <= 2) as i64;
    (year, month, day)
}
