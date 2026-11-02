//! ulib: the tiny standard library for Kern user programs.
//!
//! Programs define `#[no_mangle] fn main() -> i32` and get `print!`,
//! `println!`, their arguments through `args()`, a heap (`extern crate
//! alloc;` for `Vec`, `String`, `Box`), and safe wrappers around every system
//! call. Fallible calls return `Result<_, Errno>`.

#![no_std]

extern crate alloc;

pub mod fs;
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
    pub const SYSINFO: u64 = 10;
    pub const PS: u64 = 11;
    pub const TRY_WAIT: u64 = 12;
    pub const BRK: u64 = 13;
    pub const MMAP: u64 = 14;
    pub const MUNMAP: u64 = 15;
    pub const MPROTECT: u64 = 16;
    pub const OPEN: u64 = 17;
    pub const CLOSE: u64 = 18;
    pub const LSEEK: u64 = 19;
    pub const STAT: u64 = 20;
    pub const FSTAT: u64 = 21;
    pub const READDIR: u64 = 22;
    pub const MKDIR: u64 = 23;
    pub const UNLINK: u64 = 24;
    pub const CHDIR: u64 = 25;
    pub const GETCWD: u64 = 26;
    pub const DUP: u64 = 27;
    pub const DUP2: u64 = 28;
    pub const PIPE: u64 = 29;
    pub const KILL: u64 = 30;
    pub const SETPGID: u64 = 31;
    pub const SET_FOREGROUND: u64 = 32;
    pub const TIME_MS: u64 = 33;
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
pub(crate) unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    syscall6(n, a0, a1, a2, 0, 0, 0)
}

// --------------------------------------------------------------------- errors

/// A kernel error number (the positive value; syscalls return it negated).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i64);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const ESRCH: Errno = Errno(3);
    pub const EINTR: Errno = Errno(4);
    pub const E2BIG: Errno = Errno(7);
    pub const ENOEXEC: Errno = Errno(8);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const ENOMEM: Errno = Errno(12);
    pub const EFAULT: Errno = Errno(14);
    pub const EEXIST: Errno = Errno(17);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const EMFILE: Errno = Errno(24);
    pub const EPIPE: Errno = Errno(32);
    pub const ERANGE: Errno = Errno(34);
    pub const ENAMETOOLONG: Errno = Errno(36);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);

    pub fn name(self) -> &'static str {
        match self.0 {
            1 => "not permitted",
            2 => "no such file or directory",
            3 => "no such process",
            4 => "interrupted",
            7 => "argument list too long",
            8 => "not an executable",
            9 => "bad file descriptor",
            10 => "no such child",
            12 => "out of memory",
            14 => "bad address",
            17 => "already exists",
            20 => "not a directory",
            21 => "is a directory",
            22 => "invalid argument",
            24 => "too many open files",
            32 => "broken pipe",
            34 => "buffer too small",
            36 => "name too long",
            38 => "not implemented",
            39 => "directory not empty",
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

pub(crate) fn check(ret: i64) -> Result<u64, Errno> {
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

/// Start a program: a path, or a bare name meaning /bin/<name>. `args` become
/// its `argv[1..]`; `argv[0]` is `name`. The child inherits our file
/// descriptors and working directory. Returns its pid.
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

/// How a process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitStatus {
    Exited(i32),
    /// Killed by this signal (see `signal`).
    Killed(u8),
}

impl ExitStatus {
    fn decode(raw: u64) -> ExitStatus {
        if raw >> 32 == 1 {
            ExitStatus::Killed(raw as u8)
        } else {
            ExitStatus::Exited(raw as u32 as i32)
        }
    }

    pub fn success(self) -> bool {
        self == ExitStatus::Exited(0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ExitStatus::Exited(code) => write!(f, "exit code {}", code),
            ExitStatus::Killed(sig) => write!(f, "killed by {}", signal::name(sig)),
        }
    }
}

/// Wait for child `pid` to end.
pub fn wait(pid: Pid) -> Result<ExitStatus, Errno> {
    let mut raw: u64 = 0;
    check(unsafe { syscall(sys::WAIT, pid, &mut raw as *mut u64 as u64, 0) })?;
    Ok(ExitStatus::decode(raw))
}

/// Collect any ended child without blocking: Some((pid, status)).
pub fn try_wait() -> Option<(Pid, ExitStatus)> {
    let mut raw: u64 = 0;
    let pid = unsafe { syscall(sys::TRY_WAIT, &mut raw as *mut u64 as u64, 0, 0) };
    (pid > 0).then(|| (pid as Pid, ExitStatus::decode(raw)))
}

/// Signals (Linux numbers). Every signal kills the process it is sent to.
pub mod signal {
    pub const SIGINT: u8 = 2;
    pub const SIGILL: u8 = 4;
    pub const SIGTRAP: u8 = 5;
    pub const SIGBUS: u8 = 7;
    pub const SIGFPE: u8 = 8;
    pub const SIGKILL: u8 = 9;
    pub const SIGSEGV: u8 = 11;
    pub const SIGTERM: u8 = 15;

    pub fn name(sig: u8) -> &'static str {
        match sig {
            SIGINT => "SIGINT",
            SIGILL => "SIGILL",
            SIGTRAP => "SIGTRAP",
            SIGBUS => "SIGBUS",
            SIGFPE => "SIGFPE",
            SIGKILL => "SIGKILL",
            SIGSEGV => "SIGSEGV",
            SIGTERM => "SIGTERM",
            _ => "a signal",
        }
    }

    /// "INT", "SIGINT" or "2" -> 2.
    pub fn parse(s: &str) -> Option<u8> {
        if let Ok(n) = s.parse::<u8>() {
            return (1..=31).contains(&n).then_some(n);
        }
        let s = s.strip_prefix("SIG").unwrap_or(s);
        [SIGINT, SIGILL, SIGTRAP, SIGBUS, SIGFPE, SIGKILL, SIGSEGV, SIGTERM].into_iter().find(|&n| &name(n)[3..] == s)
    }
}

/// Send `sig` to process `pid` (> 0), process group `-pid` (< 0), or our
/// own process group (0).
pub fn kill(pid: i64, sig: u8) -> Result<(), Errno> {
    check(unsafe { syscall(sys::KILL, pid as u64, sig as u64, 0) }).map(|_| ())
}

/// Move process `pid` (0: ourselves) into process group `pgid` (0: a new
/// group with the same number as `pid`).
pub fn setpgid(pid: Pid, pgid: Pid) -> Result<(), Errno> {
    check(unsafe { syscall(sys::SETPGID, pid, pgid, 0) }).map(|_| ())
}

/// Make Ctrl+C on the console send SIGINT to process group `pgid` (0: no
/// one; the key then arrives as byte 3).
pub fn set_foreground(pgid: Pid) {
    unsafe { syscall(sys::SET_FOREGROUND, pgid, 0, 0) };
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

/// Milliseconds since 1970-01-01 UTC, or 0 if the machine's clock couldn't
/// be read.
pub fn time_ms() -> u64 {
    unsafe { syscall(sys::TIME_MS, 0, 0, 0) as u64 }
}

// -------------------------------------------------------------------- I/O

pub const STDIN: u64 = 0;
pub const STDOUT: u64 = 1;
pub const STDERR: u64 = 2;

/// Write some of `bytes`; returns how many were written.
pub fn write(fd: u64, bytes: &[u8]) -> Result<usize, Errno> {
    check(unsafe { syscall(sys::WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) }).map(|n| n as usize)
}

/// Write all of `bytes`.
pub fn write_all(fd: u64, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        let n = write(fd, bytes)?;
        if n == 0 {
            return Err(Errno::EINVAL);
        }
        bytes = &bytes[n..];
    }
    Ok(())
}

/// Read up to `buf.len()` bytes; 0 means end of file. On the console it
/// blocks until at least one key is available.
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

/// Collects formatted output and sends it in as few `write` calls as
/// possible, so a line printed by one process isn't split up by another
/// process printing at the same time on another CPU.
struct Buffered {
    buf: [u8; 256],
    len: usize,
}

impl Buffered {
    fn flush(&mut self) {
        if self.len > 0 {
            let _ = write(STDOUT, &self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl fmt::Write for Buffered {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for chunk in s.as_bytes().chunks(self.buf.len()) {
            if self.len + chunk.len() > self.buf.len() {
                self.flush();
            }
            self.buf[self.len..self.len + chunk.len()].copy_from_slice(chunk);
            self.len += chunk.len();
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let mut out = Buffered { buf: [0; 256], len: 0 };
    let _ = fmt::Write::write_fmt(&mut out, args);
    out.flush();
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
