//! System calls via the `syscall` instruction.
//!
//! ABI (same registers as Linux): number in rax, arguments in rdi, rsi, rdx,
//! r10, r8, r9; result in rax. Negative results are errors.
//!
//!   0 exit(code)            6 uptime_ms()
//!   1 write(fd, buf, len)   7 spawn(name, name_len) -> pid
//!   2 read(fd, buf, len)    8 wait(pid) -> exit code
//!   3 getpid()              9 programs(buf, len) -> bytes
//!   4 sleep_ms(ms)         10 sysinfo(buf: *mut [u64; 4])
//!   5 yield()              11 ps(buf, len) -> bytes
//!                          12 try_wait(code: *mut i64) -> pid of a finished child, or 0

use crate::arch::cpu::{self, msr, rdmsr, wrmsr};
use crate::arch::gdt;
use crate::mm::{pmm, vmm, PAGE_SIZE};
use crate::{drivers::keyboard, modules, task};
use alloc::string::String;
use core::arch::global_asm;
use core::fmt::Write;

pub const ENOSYS: i64 = -38;
pub const EFAULT: i64 = -14;
pub const ENOENT: i64 = -2;
pub const EBADF: i64 = -9;
pub const ECHILD: i64 = -10;
pub const ENOMEM: i64 = -12;

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
    let (a0, a1, a2) = (f.rdi, f.rsi, f.rdx);
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
        7 => sys_spawn(a0, a1),
        8 => task::wait(a0).unwrap_or(ECHILD),
        9 => sys_programs(a0, a1),
        10 => sys_sysinfo(a0),
        11 => sys_ps(a0, a1),
        12 => sys_try_wait(a0),
        _ => ENOSYS,
    };
    f.rax = ret as u64;

    // sysret to a non-canonical address would fault in ring 0.
    if f.rip >= vmm::USER_END {
        task::exit_current(-1);
    }
    f.rflags = (f.rflags & !0x3000) | 0x200; // IOPL 0, interrupts on
}

/// Check that [ptr, ptr+len) is mapped user memory (and writable, if asked).
fn user_range_ok(ptr: u64, len: u64, write: bool) -> bool {
    let Some(end) = ptr.checked_add(len) else { return false };
    if end > vmm::USER_END {
        return false;
    }
    let pml4 = cpu::read_cr3() & !0xfff;
    let mut page = ptr & !(PAGE_SIZE - 1);
    while page < end {
        match vmm::translate(pml4, page) {
            Some((_, flags)) if flags & vmm::flags::USER != 0 => {
                if write && flags & vmm::flags::WRITABLE == 0 {
                    return false;
                }
            }
            _ => return false,
        }
        page += PAGE_SIZE;
    }
    true
}

fn user_slice(ptr: u64, len: u64) -> Option<&'static [u8]> {
    user_range_ok(ptr, len, false).then(|| unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) })
}

fn user_slice_mut(ptr: u64, len: u64) -> Option<&'static mut [u8]> {
    user_range_ok(ptr, len, true).then(|| unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) })
}

fn copy_out(text: &str, buf: u64, len: u64) -> i64 {
    let Some(out) = user_slice_mut(buf, len) else { return EFAULT };
    let n = text.len().min(out.len());
    out[..n].copy_from_slice(&text.as_bytes()[..n]);
    n as i64
}

fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    let Some(bytes) = user_slice(buf, len) else { return EFAULT };
    crate::console::write_bytes(bytes);
    len as i64
}

fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 0 {
        return EBADF;
    }
    let Some(out) = user_slice_mut(buf, len) else { return EFAULT };
    keyboard::read(out) as i64
}

fn sys_spawn(name: u64, len: u64) -> i64 {
    let Some(bytes) = user_slice(name, len.min(256)) else { return EFAULT };
    let Ok(name) = core::str::from_utf8(bytes) else { return ENOENT };
    let Some(image) = modules::find(name) else { return ENOENT };
    match task::spawn_user(name, image, Some(task::current_pid())) {
        Ok(pid) => pid as i64,
        Err(_) => ENOMEM,
    }
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
    let Some(out) = user_slice_mut(buf, 32) else { return EFAULT };
    let (free, total) = pmm::stats();
    let fields = [total, free, task::uptime_ms(), task::list().len() as u64];
    for (i, v) in fields.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    0
}

fn sys_ps(buf: u64, len: u64) -> i64 {
    let mut text = String::new();
    let _ = writeln!(text, "  PID  PPID  STATE     NAME");
    for (pid, parent, state, name) in task::list() {
        let ppid = parent.map_or(String::from("-"), |p| alloc::format!("{}", p));
        let _ = writeln!(text, "{:>5} {:>5}  {:<9} {}", pid, ppid, alloc::format!("{:?}", state), name);
    }
    copy_out(&text, buf, len)
}

fn sys_try_wait(code_ptr: u64) -> i64 {
    if !user_range_ok(code_ptr, 8, true) {
        return EFAULT;
    }
    match task::try_wait_any() {
        Some((pid, code)) => {
            unsafe { (code_ptr as *mut i64).write_unaligned(code) };
            pid as i64
        }
        None => 0,
    }
}
