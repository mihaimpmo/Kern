//! ulib: the tiny standard library for Kern user programs.
//! Programs define `#[no_mangle] fn main() -> i32` and get `print!`, `println!`
//! and safe wrappers around every system call.

#![no_std]

use core::arch::asm;
use core::fmt;

pub mod sys {
    pub const EXIT: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const READ: u64 = 2;
    pub const GETPID: u64 = 3;
    pub const SLEEP_MS: u64 = 4;
    pub const YIELD: u64 = 5;
    pub const UPTIME_MS: u64 = 6;
    pub const SPAWN: u64 = 7;
    pub const WAIT: u64 = 8;
    pub const PROGRAMS: u64 = 9;
    pub const SYSINFO: u64 = 10;
    pub const PS: u64 = 11;
    pub const TRY_WAIT: u64 = 12;
}

#[inline(always)]
pub unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    asm!("syscall",
         inlateout("rax") n as i64 => ret,
         in("rdi") a, in("rsi") b, in("rdx") c,
         out("rcx") _, out("r11") _,
         options(nostack));
    ret
}

pub fn exit(code: i32) -> ! {
    unsafe { syscall3(sys::EXIT, code as i64 as u64, 0, 0) };
    loop {}
}

pub fn write(fd: u64, bytes: &[u8]) -> i64 {
    unsafe { syscall3(sys::WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) }
}

/// Blocks until at least one key is available.
pub fn read(buf: &mut [u8]) -> i64 {
    unsafe { syscall3(sys::READ, 0, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub fn getpid() -> u64 {
    unsafe { syscall3(sys::GETPID, 0, 0, 0) as u64 }
}

pub fn sleep_ms(ms: u64) {
    unsafe { syscall3(sys::SLEEP_MS, ms, 0, 0) };
}

pub fn yield_now() {
    unsafe { syscall3(sys::YIELD, 0, 0, 0) };
}

pub fn uptime_ms() -> u64 {
    unsafe { syscall3(sys::UPTIME_MS, 0, 0, 0) as u64 }
}

/// Start a program by name; returns its pid or a negative error.
pub fn spawn(name: &str) -> i64 {
    unsafe { syscall3(sys::SPAWN, name.as_ptr() as u64, name.len() as u64, 0) }
}

/// Wait for a child to finish; returns its exit code.
pub fn wait(pid: u64) -> i64 {
    unsafe { syscall3(sys::WAIT, pid, 0, 0) }
}

/// Collect a finished child without blocking: Some((pid, exit code)).
pub fn try_wait() -> Option<(u64, i64)> {
    let mut code: i64 = 0;
    let pid = unsafe { syscall3(sys::TRY_WAIT, &mut code as *mut i64 as u64, 0, 0) };
    (pid > 0).then_some((pid as u64, code))
}

pub fn programs(buf: &mut [u8]) -> i64 {
    unsafe { syscall3(sys::PROGRAMS, buf.as_mut_ptr() as u64, buf.len() as u64, 0) }
}

pub fn ps(buf: &mut [u8]) -> i64 {
    unsafe { syscall3(sys::PS, buf.as_mut_ptr() as u64, buf.len() as u64, 0) }
}

pub struct SysInfo {
    pub total_memory: u64,
    pub free_memory: u64,
    pub uptime_ms: u64,
    pub tasks: u64,
}

pub fn sysinfo() -> SysInfo {
    let mut raw = [0u64; 4];
    unsafe { syscall3(sys::SYSINFO, raw.as_mut_ptr() as u64, 0, 0) };
    SysInfo { total_memory: raw[0], free_memory: raw[1], uptime_ms: raw[2], tasks: raw[3] }
}

// ------------------------------------------------------------------- printing

pub struct Stdout;

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(1, s.as_bytes());
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let _ = fmt::Write::write_fmt(&mut Stdout, args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

// -------------------------------------------------------------------- runtime

#[no_mangle]
pub extern "C" fn _start() -> ! {
    extern "Rust" {
        fn main() -> i32;
    }
    let code = unsafe { main() };
    exit(code)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("\x1b[91mpanic: {}\x1b[0m", info.message());
    exit(101)
}
