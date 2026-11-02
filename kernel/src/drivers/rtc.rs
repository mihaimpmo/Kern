//! The CMOS real-time clock, read once at boot to learn the date. After
//! that the time of day is the boot time plus the uptime.
//!
//! The clock is assumed to run in UTC and in the 21st century (the CMOS
//! century register isn't standard).

use crate::arch::cpu::{inb, outb};
use core::sync::atomic::{AtomicU64, Ordering};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;

/// Unix time (seconds) when the clock was read, and the uptime then (ms).
static BOOT_SECONDS: AtomicU64 = AtomicU64::new(0);
static READ_AT_MS: AtomicU64 = AtomicU64::new(0);

fn register(index: u8) -> u8 {
    unsafe {
        outb(INDEX, index); // bit 7 clear: setting it would disable NMIs
        inb(DATA)
    }
}

fn update_in_progress() -> bool {
    register(0x0a) & 0x80 != 0
}

/// Seconds, minutes, hours, day, month, year (two digits), as stored.
fn raw_time() -> [u8; 6] {
    while update_in_progress() {
        core::hint::spin_loop();
    }
    [register(0x00), register(0x02), register(0x04), register(0x07), register(0x08), register(0x09)]
}

/// Read the clock. Returns the Unix time, or None if it reads as nonsense
/// (no RTC, or not set).
pub fn init(uptime_ms: u64) -> Option<u64> {
    // Read until two reads agree, so an update between registers can't
    // give a mixed-up time.
    let mut time = raw_time();
    loop {
        let again = raw_time();
        if again == time {
            break;
        }
        time = again;
    }
    let status_b = register(0x0b);
    let bcd = status_b & 0x04 == 0;
    let decode = |v: u8| if bcd { (v & 0x0f) + (v >> 4) * 10 } else { v };
    let [second, minute, hour_raw, day, month, year] = time;
    let pm = hour_raw & 0x80 != 0;
    let mut hour = decode(hour_raw & 0x7f);
    if status_b & 0x02 == 0 {
        // 12-hour clock: 12 AM is 0, 12 PM is 12.
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    let (second, minute, day, month, year) = (decode(second), decode(minute), decode(day), decode(month), decode(year));
    if second > 59 || minute > 59 || hour > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&month) || year > 99 {
        return None;
    }
    let days = days_from_civil(2000 + year as i64, month as i64, day as i64);
    let seconds = days as u64 * 86_400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
    BOOT_SECONDS.store(seconds, Ordering::Relaxed);
    READ_AT_MS.store(uptime_ms, Ordering::Relaxed);
    Some(seconds)
}

/// Current Unix time in milliseconds, or 0 if the clock couldn't be read.
pub fn now_ms(uptime_ms: u64) -> u64 {
    match BOOT_SECONDS.load(Ordering::Relaxed) {
        0 => 0,
        boot => boot * 1000 + uptime_ms.saturating_sub(READ_AT_MS.load(Ordering::Relaxed)),
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
