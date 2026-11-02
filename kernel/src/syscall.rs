//! System calls via the `syscall` instruction.
//!
//! ABI (same registers as Linux): number in rax, arguments in rdi, rsi, rdx,
//! r10, r8, r9; result in rax. Negative results are -errno.
//!
//!   0 exit(code)            6 uptime_ms()
//!   1 write(fd, buf, len)   7 spawn(name, name_len, argv, argc) -> pid
//!   2 read(fd, buf, len)    8 wait(pid, code: *mut i64) -> 0
//!   3 getpid()              9 programs(buf, len) -> bytes
//!   4 sleep_ms(ms)         10 sysinfo(buf: *mut [u64; 5])
//!   5 yield()              11 ps(buf, len) -> bytes
//!                          12 try_wait(code: *mut i64) -> pid of a finished child, or 0
//!  13 brk(addr) -> new end of the heap (0: query; unchanged on failure)
//!  14 mmap(addr, len, prot, flags) -> addr     anonymous memory only
//!  15 munmap(addr, len)
//!  16 mprotect(addr, len, prot)
//!
//! PROT_* and MAP_* values are Linux's; `flags` must include MAP_PRIVATE |
//! MAP_ANONYMOUS, and may add MAP_FIXED.
//!
//! `argv` points to `argc` (ptr, len) pairs: one string per argument, not
//! NUL-terminated. The kernel passes them on in the usual C layout.
//!
//! User memory is only ever touched through `copy_from_user` / `copy_to_user`,
//! which check the pages and open the SMAP window around the copy.

use crate::arch::cpu::{self, msr, rdmsr, wrmsr};
use crate::arch::{gdt, smp};
use crate::mm::{pmm, vmm};
use crate::{drivers::keyboard, drivers::serial, modules, task};
use crate::mm::uspace::{MemError, Prot, UserSpace};
use crate::task::exec;
use alloc::{string::String, vec, vec::Vec};
use core::arch::global_asm;
use core::fmt::Write;

pub const ENOSYS: i64 = -38;
pub const EFAULT: i64 = -14;
pub const ENOENT: i64 = -2;
pub const E2BIG: i64 = -7;
pub const ENOEXEC: i64 = -8;
pub const EBADF: i64 = -9;
pub const ECHILD: i64 = -10;
pub const ENOMEM: i64 = -12;
pub const EINVAL: i64 = -22;

global_asm!(
    r#"
.section .text
.global syscall_entry
syscall_entry:
    swapgs                         // GS -> per-CPU data
    mov gs:[8], rsp                // stash user rsp
    mov rsp, gs:[0]                // kernel stack of this task
    push qword ptr gs:[8]          // user rsp
    push r11                       // user rflags
    push rcx                       // user rip
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    call syscall_dispatch
    cli
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    pop rcx
    pop r11
    pop rsp                        // back on the user stack
    swapgs
    sysretq
"#
);

extern "C" {
    fn syscall_entry();
}

#[repr(C)]
pub struct SyscallFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
    r9: u64,
    r8: u64,
    r10: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rax: u64,
    rip: u64,
    rflags: u64,
    rsp: u64,
}

/// Per CPU: the syscall MSRs are per-CPU registers.
pub fn init() {
    unsafe {
        wrmsr(msr::EFER, rdmsr(msr::EFER) | 1); // SCE: enable syscall/sysret
        // syscall: CS = 0x08, SS = 0x10.
        // sysret:  CS = base + 16, SS = base + 8. The base must already carry
        // RPL 3: Intel forces RPL 3 onto SS, but AMD copies the selector as is,
        // which would leave user mode with SS = 0x18 and make the next iretq
        // back to user mode fault with #GP(0x18).
        let sysret_base = gdt::KERNEL_DS as u64 | 3; // 0x13 -> SS 0x1b, CS 0x23
        let star = (sysret_base << 48) | ((gdt::KERNEL_CS as u64) << 32);
        wrmsr(msr::STAR, star);
        wrmsr(msr::LSTAR, syscall_entry as *const () as u64);
        // Clear IF, TF, DF, AC on entry.
        wrmsr(msr::FMASK, 0x200 | 0x100 | 0x400 | 0x40000);
    }
}

#[no_mangle]
extern "C" fn syscall_dispatch(f: &mut SyscallFrame) {
    cpu::sti();
    let (a0, a1, a2, a3) = (f.rdi, f.rsi, f.rdx, f.r10);
    let ret: i64 = match f.rax {
        0 => task::exit_current(a0 as i64),
        1 => sys_write(a0, a1, a2),
        2 => sys_read(a0, a1, a2),
        3 => task::current_pid() as i64,
        4 => {
            task::sleep_ms(a0);
            0
        }
        5 => {
            task::yield_now();
            0
        }
        6 => task::uptime_ms() as i64,
        7 => sys_spawn(a0, a1, a2, a3),
        8 => sys_wait(a0, a1),
        9 => sys_programs(a0, a1),
        10 => sys_sysinfo(a0),
        11 => sys_ps(a0, a1),
        12 => sys_try_wait(a0),
        13 => with_user_space(|s| s.brk(a0) as i64).unwrap_or(ENOMEM),
        14 => sys_mmap(a0, a1, a2, a3),
        15 => mem_result(with_user_space(|s| s.munmap(a0, a1).map(|()| 0))),
        16 => sys_mprotect(a0, a1, a2),
        _ => ENOSYS,
    };
    f.rax = ret as u64;

    // sysret to a non-canonical address would fault in ring 0.
    if f.rip >= vmm::USER_END {
        task::exit_current(-1);
    }
    f.rflags = (f.rflags & !0x3000) | 0x200; // IOPL 0, interrupts on
}

/// Run `f` on the calling process's memory, locked (so interrupts are off).
/// None for kernel threads, which have no user memory.
fn with_user_space<R>(f: impl FnOnce(&mut UserSpace) -> R) -> Option<R> {
    let space = task::current_space()?;
    let mut space = space.lock();
    Some(f(&mut space))
}

/// Check that [ptr, ptr + len) is memory the process may read (and write, if
/// asked), mapping pages it hasn't touched yet. An empty range is always fine.
fn user_range_ok(ptr: u64, len: u64, write: bool) -> bool {
    len == 0 || with_user_space(|s| s.populate(ptr, len, write).is_ok()).unwrap_or(false)
}

/// Fill `out` from user memory at `src`. Fails without copying anything if
/// any byte of the range isn't readable user memory.
fn copy_from_user(out: &mut [u8], src: u64) -> Result<(), ()> {
    if out.is_empty() {
        return Ok(());
    }
    // The address-space lock keeps interrupts off across check and copy:
    // nothing can unmap the range in between, and RFLAGS.AC can't leak into
    // another task.
    with_user_space(|s| {
        s.populate(src, out.len() as u64, false).map_err(|_| ())?;
        cpu::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, out.as_mut_ptr(), out.len()) };
        cpu::user_access_end();
        Ok(())
    })
    .unwrap_or(Err(()))
}

/// Copy `data` to user memory at `dst`, all or nothing.
fn copy_to_user(dst: u64, data: &[u8]) -> Result<(), ()> {
    if data.is_empty() {
        return Ok(());
    }
    with_user_space(|s| {
        s.populate(dst, data.len() as u64, true).map_err(|_| ())?;
        cpu::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len()) };
        cpu::user_access_end();
        Ok(())
    })
    .unwrap_or(Err(()))
}

/// Copy as much of `text` as fits in the user buffer; returns bytes copied.
fn copy_out(text: &str, buf: u64, len: u64) -> i64 {
    let n = (text.len() as u64).min(len) as usize;
    match copy_to_user(buf, &text.as_bytes()[..n]) {
        Ok(()) => n as i64,
        Err(()) => EFAULT,
    }
}

/// Bytes handed to the console per lock acquisition. The console lock keeps
/// interrupts off, so a big write is split up to let the timer and keyboard in.
const WRITE_CHUNK: usize = 256;

fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    if !user_range_ok(buf, len, false) {
        return EFAULT;
    }
    let mut chunk = [0u8; WRITE_CHUNK];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(WRITE_CHUNK as u64) as usize;
        if copy_from_user(&mut chunk[..n], buf + done).is_err() {
            return if done > 0 { done as i64 } else { EFAULT };
        }
        // Let the serial buffer drain instead of making the console fall
        // back to slow synchronous output (+ n for the '\r' added per '\n').
        while !serial::has_room(2 * n) {
            task::sleep_ms(1);
        }
        crate::console::write_bytes(&chunk[..n]);
        done += n as u64;
    }
    len as i64
}

fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 0 {
        return EBADF;
    }
    // Check before blocking, so keys are never consumed and then lost.
    if !user_range_ok(buf, len, true) {
        return EFAULT;
    }
    let mut keys = [0u8; 256];
    let n = keyboard::read(&mut keys[..(len as usize).min(256)]);
    match copy_to_user(buf, &keys[..n]) {
        Ok(()) => n as i64,
        Err(()) => EFAULT,
    }
}

/// Copy a user string described by (ptr, len), at most `max` bytes.
fn user_bytes(ptr: u64, len: u64, max: usize) -> Result<Vec<u8>, i64> {
    if len > max as u64 {
        return Err(E2BIG);
    }
    let mut buf = vec![0u8; len as usize];
    copy_from_user(&mut buf, ptr).map_err(|_| EFAULT)?;
    Ok(buf)
}

fn sys_spawn(name: u64, name_len: u64, argv: u64, argc: u64) -> i64 {
    let name = match user_bytes(name, name_len, 256) {
        Ok(name) => name,
        Err(e) => return e,
    };
    let Ok(name) = core::str::from_utf8(&name) else { return ENOENT };
    let Some(image) = modules::find(name) else { return ENOENT };

    if argc > exec::MAX_ARGS as u64 {
        return E2BIG;
    }
    let mut pairs = vec![0u8; argc as usize * 16];
    if copy_from_user(&mut pairs, argv).is_err() {
        return EFAULT;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for pair in pairs.chunks_exact(16) {
        let ptr = u64::from_le_bytes(pair[..8].try_into().unwrap());
        let len = u64::from_le_bytes(pair[8..].try_into().unwrap());
        match user_bytes(ptr, len, exec::MAX_ARG_BYTES) {
            Ok(arg) => args.push(arg),
            Err(e) => return e,
        }
    }
    let args: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();

    match task::spawn_user(name, image, &args, Some(task::current_pid())) {
        Ok(pid) => pid as i64,
        Err(why) => {
            kprintln!("\x1b[91m[kernel] cannot start {}: {}\x1b[0m", name, why);
            match why {
                exec::LoadError::NoMemory => ENOMEM,
                exec::LoadError::ArgsTooLong => E2BIG,
                exec::LoadError::BadElf(_) => ENOEXEC,
            }
        }
    }
}

fn sys_wait(pid: u64, code_ptr: u64) -> i64 {
    // Check first: once the child is collected its exit code can't be put back.
    if !user_range_ok(code_ptr, 8, true) {
        return EFAULT;
    }
    let Some(code) = task::wait(pid) else { return ECHILD };
    match copy_to_user(code_ptr, &code.to_le_bytes()) {
        Ok(()) => 0,
        Err(()) => EFAULT,
    }
}

const MAP_PRIVATE: u64 = 0x02;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

fn mem_result(r: Option<Result<u64, MemError>>) -> i64 {
    match r {
        Some(Ok(v)) => v as i64,
        Some(Err(MemError::Invalid)) => EINVAL,
        Some(Err(MemError::Fault)) => EFAULT,
        Some(Err(MemError::NoMemory)) | None => ENOMEM,
    }
}

fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64) -> i64 {
    let Some(prot) = Prot::from_bits(prot) else { return EINVAL };
    if flags & !(MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS) != 0 || flags & (MAP_PRIVATE | MAP_ANONYMOUS) != (MAP_PRIVATE | MAP_ANONYMOUS) {
        return EINVAL;
    }
    let fixed = (flags & MAP_FIXED != 0).then_some(addr);
    mem_result(with_user_space(|s| s.mmap(fixed, len, prot)))
}

fn sys_mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    let Some(prot) = Prot::from_bits(prot) else { return EINVAL };
    mem_result(with_user_space(|s| s.mprotect(addr, len, prot).map(|()| 0)))
}

fn sys_programs(buf: u64, len: u64) -> i64 {
    let mut text = String::new();
    for name in modules::names() {
        text.push_str(&name);
        text.push('\n');
    }
    copy_out(&text, buf, len)
}

fn sys_sysinfo(buf: u64) -> i64 {
    let (free, total) = pmm::stats();
    let fields = [total, free, task::uptime_ms(), task::list().len() as u64, smp::online() as u64];
    let mut out = [0u8; 40];
    for (i, v) in fields.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    match copy_to_user(buf, &out) {
        Ok(()) => 0,
        Err(()) => EFAULT,
    }
}

fn sys_ps(buf: u64, len: u64) -> i64 {
    let mut text = String::new();
    let _ = writeln!(text, "  PID  PPID  STATE     CPU  NAME");
    for t in task::list() {
        let ppid = t.parent.map_or(String::from("-"), |p| alloc::format!("{}", p));
        let cpu = t.cpu.map_or(String::from("-"), |c| alloc::format!("{}", c));
        let state = alloc::format!("{:?}", t.state);
        let _ = writeln!(text, "{:>5} {:>5}  {:<9} {:>3}  {}", t.pid, ppid, state, cpu, t.name);
    }
    copy_out(&text, buf, len)
}

fn sys_try_wait(code_ptr: u64) -> i64 {
    if !user_range_ok(code_ptr, 8, true) {
        return EFAULT;
    }
    match task::try_wait_any() {
        Some((pid, code)) => match copy_to_user(code_ptr, &code.to_le_bytes()) {
            Ok(()) => pid as i64,
            Err(()) => EFAULT,
        },
        None => 0,
    }
}
