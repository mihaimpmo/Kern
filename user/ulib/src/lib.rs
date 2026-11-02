//! ulib: the tiny standard library for Kern user programs.
//!
//! Programs define `#[no_mangle] fn main() -> i32` and get `print!`,
//! `println!`, their arguments through `args()`, a heap (`extern crate
//! alloc;` for `Vec`, `String`, `Box`), and safe wrappers around every system
//! call. Fallible calls return `Result<_, Errno>`.

#![no_std]

pub mod heap;

use core::arch::{asm, global_asm};
use core::fmt;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// System call numbers; must match `syscall_dispatch` in the kernel.
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
    pub const BRK: u64 = 13;
    pub const MMAP: u64 = 14;
    pub const MUNMAP: u64 = 15;
    pub const MPROTECT: u64 = 16;
}

/// Raw system call: number in rax, up to six arguments, result in rax.
///
/// # Safety
/// The arguments must be valid for the call being made; pointers are only
/// checked by the kernel to the extent of "is this the caller's memory".
#[inline(always)]
pub unsafe fn syscall6(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    asm!("syscall",
         inlateout("rax") n as i64 => ret,
         in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5,
         out("rcx") _, out("r11") _,
         options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    syscall6(n, a0, a1, a2, 0, 0, 0)
}

// --------------------------------------------------------------------- errors

/// A kernel error number (the positive value; syscalls return it negated).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i64);

impl Errno {
    pub const ENOENT: Errno = Errno(2);
    pub const E2BIG: Errno = Errno(7);
    pub const ENOEXEC: Errno = Errno(8);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const ENOMEM: Errno = Errno(12);
    pub const EFAULT: Errno = Errno(14);
    pub const EINVAL: Errno = Errno(22);
    pub const ENOSYS: Errno = Errno(38);

    pub fn name(self) -> &'static str {
        match self.0 {
            2 => "no such program",
            7 => "argument list too long",
            8 => "not an executable",
            9 => "bad file descriptor",
            10 => "no such child",
            12 => "out of memory",
            14 => "bad address",
            22 => "invalid argument",
            38 => "not implemented",
            _ => "unknown error",
        }
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Errno({}: {})", self.0, self.name())
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

fn check(ret: i64) -> Result<u64, Errno> {
    if ret < 0 {
        Err(Errno(-ret))
    } else {
        Ok(ret as u64)
    }
}

// ------------------------------------------------------------------ processes

pub type Pid = u64;

pub fn exit(code: i32) -> ! {
    unsafe { syscall(sys::EXIT, code as i64 as u64, 0, 0) };
    unreachable!("exit returned")
}

pub fn getpid() -> Pid {
    unsafe { syscall(sys::GETPID, 0, 0, 0) as Pid }
}

/// Most arguments `spawn` accepts (the kernel's limit).
pub const MAX_ARGS: usize = 64;

/// Start a program by name. `args` become its `argv[1..]`; `argv[0]` is the
/// name. Returns the new process's pid.
pub fn spawn(name: &str, args: &[&str]) -> Result<Pid, Errno> {
    if args.len() + 1 > MAX_ARGS {
        return Err(Errno::E2BIG);
    }
    let mut pairs = [[0u64; 2]; MAX_ARGS];
    for (pair, arg) in pairs.iter_mut().zip(core::iter::once(&name).chain(args)) {
        *pair = [arg.as_ptr() as u64, arg.len() as u64];
    }
    let argc = args.len() as u64 + 1;
    let ret = unsafe {
        syscall6(sys::SPAWN, name.as_ptr() as u64, name.len() as u64, pairs.as_ptr() as u64, argc, 0, 0)
    };
    check(ret)
}

/// Wait for child `pid` to finish and return its exit code.
pub fn wait(pid: Pid) -> Result<i64, Errno> {
    let mut code: i64 = 0;
    check(unsafe { syscall(sys::WAIT, pid, &mut code as *mut i64 as u64, 0) })?;
    Ok(code)
}

/// Collect any finished child without blocking: Some((pid, exit code)).
pub fn try_wait() -> Option<(Pid, i64)> {
    let mut code: i64 = 0;
    let pid = unsafe { syscall(sys::TRY_WAIT, &mut code as *mut i64 as u64, 0, 0) };
    (pid > 0).then_some((pid as Pid, code))
}

// ------------------------------------------------------------------- time

pub fn sleep_ms(ms: u64) {
    unsafe { syscall(sys::SLEEP_MS, ms, 0, 0) };
}

pub fn yield_now() {
    unsafe { syscall(sys::YIELD, 0, 0, 0) };
}

pub fn uptime_ms() -> u64 {
    unsafe { syscall(sys::UPTIME_MS, 0, 0, 0) as u64 }
}

// -------------------------------------------------------------------- I/O

pub const STDIN: u64 = 0;
pub const STDOUT: u64 = 1;
pub const STDERR: u64 = 2;

pub fn write(fd: u64, bytes: &[u8]) -> Result<usize, Errno> {
    check(unsafe { syscall(sys::WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) }).map(|n| n as usize)
}

/// Blocks until at least one byte is available.
pub fn read(fd: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    check(unsafe { syscall(sys::READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) }).map(|n| n as usize)
}

// ------------------------------------------------------------------ memory

/// Memory protection bits (Linux values).
pub mod prot {
    pub const NONE: u64 = 0;
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;
}

const MAP_PRIVATE: u64 = 0x02;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

/// Move the end of the heap to `addr` (0 to ask where it is). Returns the
/// new end; it stays where it was if the request can't be met.
pub fn brk(addr: u64) -> u64 {
    unsafe { syscall(sys::BRK, addr, 0, 0) as u64 }
}

/// `len` bytes of zeroed memory, mapped somewhere free. Pages are allocated
/// when first touched.
pub fn mmap(len: usize, prot: u64) -> Result<*mut u8, Errno> {
    let flags = MAP_PRIVATE | MAP_ANONYMOUS;
    check(unsafe { syscall6(sys::MMAP, 0, len as u64, prot, flags, 0, 0) }).map(|a| a as *mut u8)
}

/// Like `mmap`, but at exactly `addr`, replacing whatever was mapped there.
pub fn mmap_fixed(addr: *mut u8, len: usize, prot: u64) -> Result<*mut u8, Errno> {
    let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED;
    check(unsafe { syscall6(sys::MMAP, addr as u64, len as u64, prot, flags, 0, 0) }).map(|a| a as *mut u8)
}

/// # Safety
/// Nothing may use the memory afterwards.
pub unsafe fn munmap(addr: *mut u8, len: usize) -> Result<(), Errno> {
    check(syscall(sys::MUNMAP, addr as u64, len as u64, 0)).map(|_| ())
}

pub fn mprotect(addr: *mut u8, len: usize, prot: u64) -> Result<(), Errno> {
    check(unsafe { syscall(sys::MPROTECT, addr as u64, len as u64, prot) }).map(|_| ())
}

// ------------------------------------------------------------- system info

/// Names of the available programs, one per line. Returns bytes written.
pub fn programs(buf: &mut [u8]) -> Result<usize, Errno> {
    check(unsafe { syscall(sys::PROGRAMS, buf.as_mut_ptr() as u64, buf.len() as u64, 0) }).map(|n| n as usize)
}

/// A process table as text. Returns bytes written.
pub fn ps(buf: &mut [u8]) -> Result<usize, Errno> {
    check(unsafe { syscall(sys::PS, buf.as_mut_ptr() as u64, buf.len() as u64, 0) }).map(|n| n as usize)
}

pub struct SysInfo {
    pub total_memory: u64,
    pub free_memory: u64,
    pub uptime_ms: u64,
    pub tasks: u64,
    pub cpus: u64,
}

pub fn sysinfo() -> SysInfo {
    let mut raw = [0u64; 5];
    unsafe { syscall(sys::SYSINFO, raw.as_mut_ptr() as u64, 0, 0) };
    SysInfo { total_memory: raw[0], free_memory: raw[1], uptime_ms: raw[2], tasks: raw[3], cpus: raw[4] }
}

// ------------------------------------------------------------------ printing

pub struct Stdout;

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(STDOUT, s.as_bytes()).map(|_| ()).map_err(|_| fmt::Error)
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

// ------------------------------------------------------------------ arguments

static ARGC: AtomicUsize = AtomicUsize::new(0);
static ARGV: AtomicPtr<*const u8> = AtomicPtr::new(core::ptr::null_mut());

/// The program's arguments, starting with its name. An argument that is not
/// valid UTF-8 reads as "".
pub fn args() -> impl ExactSizeIterator<Item = &'static str> {
    let argv = ARGV.load(Ordering::Relaxed);
    (0..ARGC.load(Ordering::Relaxed)).map(move |i| unsafe {
        let s = core::ffi::CStr::from_ptr(*argv.add(i) as *const core::ffi::c_char);
        s.to_str().unwrap_or("")
    })
}

// -------------------------------------------------------------------- runtime

// The kernel starts us with rsp pointing at argc (System V layout, see the
// kernel's task/exec.rs). Pass that pointer on and get a normal call frame.
global_asm!(
    ".global _start",
    "_start:",
    "    xor ebp, ebp",
    "    mov rdi, rsp",
    "    and rsp, -16",
    "    call ulib_start",
    "    ud2",
);

#[no_mangle]
extern "C" fn ulib_start(sp: *const u64) -> ! {
    extern "Rust" {
        fn main() -> i32;
    }
    unsafe {
        ARGC.store(*sp as usize, Ordering::Relaxed);
        ARGV.store(sp.add(1) as *mut *const u8, Ordering::Relaxed);
    }
    let code = unsafe { main() };
    exit(code)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("\x1b[91mpanic: {}\x1b[0m", info.message());
    exit(101)
}
