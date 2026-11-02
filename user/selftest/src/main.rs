//! selftest: checks the kernel from user space. Prints one TAP-style line per
//! check and exits with the number of failures, so `make test` can boot with
//! `init=selftest qemu-exit` and read the result from QEMU's exit status.
//!
//! It also starts copies of itself in helper modes (`selftest exit 3`, ...)
//! to get child processes that behave in a known way.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, string::String, vec, vec::Vec};
use core::hint::black_box;
use ulib::{println, prot, sys, syscall6, Errno};

const NAME: &str = "selftest";
/// Exit code of a process killed by a page fault: -(128 + vector 14).
const PAGE_FAULT_EXIT: i64 = -142;
/// Mapped nowhere in user space, and not canonical-adjacent.
const BAD_PTR: u64 = 0xdead_0000_0000;

#[no_mangle]
fn main() -> i32 {
    let mut args = [""; 8];
    let mut argc = 0;
    for (slot, arg) in args.iter_mut().zip(ulib::args()) {
        *slot = arg;
        argc += 1;
    }
    match &args[1..argc] {
        [] => run_all(),
        ["exit", code] => code.parse().unwrap_or(-1),
        ["argcheck", rest @ ..] => (rest != ["one", "two words", ""]) as i32,
        ["fault"] => unsafe {
            core::ptr::write_volatile(0xffff_ffff_8000_0000 as *mut u64, 0);
            0
        },
        ["spin", ms] => {
            let end = ulib::uptime_ms() + ms.parse::<u64>().unwrap_or(0);
            while ulib::uptime_ms() < end {
                core::hint::spin_loop();
            }
            0
        }
        ["use-after-munmap"] => unsafe {
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            ulib::munmap(p, 4096).unwrap();
            core::ptr::read_volatile(p) as i32
        },
        ["write-readonly"] => unsafe {
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            ulib::mprotect(p, 4096, prot::READ).unwrap();
            let _ = core::ptr::read_volatile(p); // reading is still fine
            core::ptr::write_volatile(p, 1);
            0
        },
        ["exec-heap"] => unsafe {
            // A `ret` instruction in writable memory must not be executable (NX).
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            p.write(0xc3);
            let f: extern "C" fn() = core::mem::transmute(p);
            f();
            0
        },
        ["stack", mib] => {
            // About `mib` MiB of stack: fine below 8 MiB, a fault above.
            let depth = mib.parse::<u32>().unwrap_or(0) * 256;
            (recurse(depth) != 0) as i32 // recurse returns 0 when it gets back up
        }
        ["hog", mib] => {
            let mut v = vec![0u8; mib.parse::<usize>().unwrap_or(0) << 20];
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            (black_box(&v).iter().map(|&b| b as usize).sum::<usize>() != v.len() / 4096) as i32
        }
        ["orphan"] => {
            // Our child finishes (and is cleaned up) while we are alive; then
            // we exit without waiting for it.
            let _ = ulib::spawn(NAME, &["exit", "0"]);
            ulib::sleep_ms(100);
            0
        }
        _ => {
            println!("usage: selftest");
            2
        }
    }
}

struct Suite {
    run: u32,
    failed: u32,
}

impl Suite {
    fn check(&mut self, name: &str, ok: bool, detail: core::fmt::Arguments) {
        self.run += 1;
        if ok {
            println!("ok {} - {}", self.run, name);
        } else {
            self.failed += 1;
            println!("\x1b[91mnot ok {} - {}: {}\x1b[0m", self.run, name, detail);
        }
    }
}

macro_rules! check {
    ($suite:expr, $name:expr, $ok:expr) => {
        $suite.check($name, $ok, format_args!("{}", stringify!($ok)))
    };
    ($suite:expr, $name:expr, $ok:expr, $($detail:tt)+) => {
        $suite.check($name, $ok, format_args!($($detail)+))
    };
}

fn raw(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    unsafe { syscall6(n, a0, a1, a2, 0, 0, 0) }
}

/// Run `selftest <args>` and wait for its exit code.
fn run_child(args: &[&str]) -> Result<i64, Errno> {
    ulib::wait(ulib::spawn(NAME, args)?)
}

fn run_all() -> i32 {
    println!("selftest: starting (pid {})", ulib::getpid());
    let mut t = Suite { run: 0, failed: 0 };
    syscall_errors(&mut t);
    processes(&mut t);
    timing(&mut t);
    multicore(&mut t);
    memory(&mut t);
    leaks(&mut t);
    println!("selftest: {} checks, {} failed", t.run, t.failed);
    t.failed as i32
}

fn syscall_errors(t: &mut Suite) {
    let r = raw(999, 0, 0, 0);
    check!(t, "unknown syscall is ENOSYS", r == -Errno::ENOSYS.0, "got {}", r);
    let r = ulib::write(ulib::STDOUT, &[]);
    check!(t, "zero-length write", r == Ok(0), "got {:?}", r);
    let r = raw(sys::WRITE, ulib::STDOUT, 0, 0);
    check!(t, "zero-length write from NULL", r == 0, "got {}", r);
    let r = raw(sys::WRITE, ulib::STDOUT, BAD_PTR, 4);
    check!(t, "write from bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::WRITE, 7, BAD_PTR, 4);
    check!(t, "write to bad fd is EBADF", r == -Errno::EBADF.0, "got {}", r);
    let r = raw(sys::READ, ulib::STDIN, BAD_PTR, 4);
    check!(t, "read into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::SYSINFO, BAD_PTR, 0, 0);
    check!(t, "sysinfo into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    // The kernel's own code is mapped, but not for user mode.
    let r = raw(sys::WRITE, ulib::STDOUT, 0xffff_ffff_8000_0000, 4);
    check!(t, "write from kernel memory is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::WAIT, 1, BAD_PTR, 0);
    check!(t, "wait into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = ulib::wait(1);
    check!(t, "wait for a non-child is ECHILD", r == Err(Errno::ECHILD), "got {:?}", r);
    check!(t, "try_wait without children", ulib::try_wait().is_none());
}

fn processes(t: &mut Suite) {
    let r = ulib::spawn("no-such-program", &[]);
    check!(t, "spawn of a missing program is ENOENT", r == Err(Errno::ENOENT), "got {:?}", r);
    let long = [b'x'; 300];
    let r = ulib::spawn(core::str::from_utf8(&long).unwrap(), &[]);
    check!(t, "spawn with a 300-byte name is E2BIG", r == Err(Errno::E2BIG), "got {:?}", r);
    // Raw call: ulib::spawn would refuse this before asking the kernel.
    let pairs = [[0u64; 2]; ulib::MAX_ARGS + 1];
    let r = unsafe {
        let name = NAME.as_ptr() as u64;
        syscall6(sys::SPAWN, name, NAME.len() as u64, pairs.as_ptr() as u64, pairs.len() as u64, 0, 0)
    };
    check!(t, "spawn with too many arguments is E2BIG", r == -Errno::E2BIG.0, "got {}", r);

    let r = run_child(&["exit", "3"]);
    check!(t, "child exit code", r == Ok(3), "got {:?}", r);
    let r = run_child(&["exit", "-5"]);
    check!(t, "negative exit code is not an error", r == Ok(-5), "got {:?}", r);
    let r = run_child(&["argcheck", "one", "two words", ""]);
    check!(t, "arguments reach the child intact", r == Ok(0), "got {:?}", r);
    let r = run_child(&["fault"]);
    check!(t, "faulting child is killed", r == Ok(PAGE_FAULT_EXIT), "got {:?}", r);

    let pid = ulib::spawn(NAME, &["exit", "9"]);
    ulib::sleep_ms(50);
    let r = ulib::try_wait();
    check!(t, "try_wait collects a finished child", r.is_some() && r == pid.ok().map(|p| (p, 9)), "got {:?}", r);

    // Several children at once, collected in reverse order.
    let mut pids = [0; 16];
    for (i, pid) in pids.iter_mut().enumerate() {
        *pid = ulib::spawn(NAME, &["exit", ["0", "1", "2", "3"][i % 4]]).unwrap_or(0);
    }
    let mut all_ok = pids.iter().all(|&p| p != 0);
    for (i, &pid) in pids.iter().enumerate().rev() {
        all_ok &= ulib::wait(pid) == Ok((i % 4) as i64);
    }
    check!(t, "16 concurrent children", all_ok);
}

fn timing(t: &mut Suite) {
    let start = ulib::uptime_ms();
    ulib::sleep_ms(50);
    let slept = ulib::uptime_ms() - start;
    check!(t, "sleep_ms(50) sleeps about 50 ms", (50..200).contains(&slept), "slept {} ms", slept);

    // A busy child must not keep us from waking up: preemption.
    let spinner = ulib::spawn(NAME, &["spin", "300"]);
    let start = ulib::uptime_ms();
    ulib::sleep_ms(20);
    let late = ulib::uptime_ms() - start;
    check!(t, "busy process does not starve others", late < 100, "woke after {} ms", late);
    if let Ok(pid) = spinner {
        let _ = ulib::wait(pid);
    }
}

fn multicore(t: &mut Suite) {
    let cpus = ulib::sysinfo().cpus;
    check!(t, "sysinfo reports CPUs", cpus >= 1, "cpus = {}", cpus);

    // Many short-lived processes at once: races in spawn/exit/reap show up
    // here, especially with several CPUs.
    let mut all_ok = true;
    for round in 0..8 {
        let mut pids = [0; 24];
        for pid in pids.iter_mut() {
            *pid = ulib::spawn(NAME, &["exit", "7"]).unwrap_or(0);
        }
        for &pid in &pids {
            let r = ulib::wait(pid);
            if pid == 0 || r != Ok(7) {
                println!("  round {}: pid {} -> {:?}", round, pid, r);
                all_ok = false;
            }
        }
    }
    check!(t, "spawn storm: 8 x 24 concurrent children", all_ok);

    if cpus < 2 {
        return;
    }
    // N busy children of 300 ms each finish in about 300 ms with N CPUs,
    // N x 300 ms on one. Accept anything clearly better than serial.
    let n = cpus.min(4);
    let start = ulib::uptime_ms();
    let mut pids = [0; 4];
    for pid in pids[..n as usize].iter_mut() {
        *pid = ulib::spawn(NAME, &["spin", "300"]).unwrap_or(0);
    }
    for &pid in &pids[..n as usize] {
        let _ = ulib::wait(pid);
    }
    let took = ulib::uptime_ms() - start;
    let limit = 150 * (n + 1);
    println!("# {} x 300 ms of busy work took {} ms on {} CPUs", n, took, cpus);
    check!(t, "busy processes run in parallel", took < limit, "{} x 300 ms took {} ms (limit {})", n, took, limit);
}

/// About 4 KiB of stack per level.
fn recurse(depth: u32) -> u64 {
    let buf = black_box([depth as u8; 4000]);
    if depth == 0 {
        buf[0] as u64
    } else {
        recurse(depth - 1) + buf[1] as u64 - buf[2] as u64
    }
}

fn free_memory() -> u64 {
    ulib::sysinfo().free_memory
}

fn memory(t: &mut Suite) {
    // The heap, through the global allocator.
    let v: Vec<u32> = (0..200_000).collect();
    let sum: u64 = v.iter().map(|&x| x as u64).sum();
    check!(t, "Vec of 200k elements", sum == 199_999 * 200_000 / 2, "sum = {}", sum);
    let mut s = String::new();
    for i in 0..1000 {
        s.push_str(if i % 2 == 0 { "ab" } else { "c" });
    }
    check!(t, "String growth", s.len() == 1500 && s.ends_with("abc"), "len = {}", s.len());
    let big = vec![0u8; 16 << 20];
    check!(t, "16 MiB zeroed Box", big.iter().all(|&b| b == 0));
    drop(big);
    let boxes: Vec<Box<[u8; 100]>> = (0..1000).map(|i| Box::new([i as u8; 100])).collect();
    check!(t, "many small boxes", boxes.iter().enumerate().all(|(i, b)| b[99] == i as u8));
    drop(boxes);

    // brk: grow, use, shrink; regrown pages come back zeroed.
    let base = ulib::brk(0);
    let grown = ulib::brk(base + (64 << 10));
    unsafe { core::ptr::write_volatile((base + 4096) as *mut u8, 0x5a) };
    let shrunk = ulib::brk(base);
    let regrown = ulib::brk(base + (64 << 10));
    let zero = unsafe { core::ptr::read_volatile((base + 4096) as *const u8) };
    ulib::brk(base);
    check!(t, "brk grows and shrinks", grown == base + (64 << 10) && shrunk == base && regrown == grown);
    check!(t, "regrown brk memory is zeroed", zero == 0, "byte = {:#x}", zero);
    check!(t, "brk below the heap is refused", ulib::brk(4096) == base);

    // mmap / munmap.
    match ulib::mmap(4 << 20, prot::READ | prot::WRITE) {
        Ok(p) => unsafe {
            for page in 0..1024 {
                p.add(page * 4096).write_volatile(page as u8);
            }
            let ok = (0..1024).all(|page| p.add(page * 4096).read_volatile() == page as u8);
            check!(t, "4 MiB mmap is usable", ok);
            check!(t, "munmap", ulib::munmap(p, 4 << 20).is_ok());
        },
        Err(e) => check!(t, "4 MiB mmap is usable", false, "{:?}", e),
    }

    // Pages are only allocated when touched, and munmap gives them back.
    let before = free_memory();
    let reserved = ulib::mmap(1 << 30, prot::READ | prot::WRITE);
    let after_reserve = free_memory();
    let after_touch = match reserved {
        Ok(p) => unsafe {
            p.add(512 << 20).write_volatile(1);
            free_memory()
        },
        Err(_) => 0,
    };
    if let Ok(p) = reserved {
        let _ = unsafe { ulib::munmap(p, 1 << 30) };
    }
    let after_unmap = free_memory();
    check!(t, "reserving 1 GiB costs (almost) nothing", reserved.is_ok() && before - after_reserve < (64 << 10),
           "{:?}, {} bytes", reserved, before - after_reserve);
    check!(t, "touching a page allocates it", after_reserve - after_touch >= 4096,
           "{} bytes", after_reserve - after_touch);
    check!(t, "munmap frees touched pages", after_unmap + (64 << 10) > before,
           "{} -> {} bytes free", before, after_unmap);

    // The kernel can write into memory the process hasn't touched yet.
    let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap_or(core::ptr::null_mut());
    let r = raw(sys::SYSINFO, p as u64, 0, 0);
    check!(t, "syscall output into untouched memory", r == 0 && unsafe { *(p as *const u64) } > 0, "got {}", r);

    // mprotect keeps contents.
    unsafe { p.write_volatile(42) };
    let r1 = ulib::mprotect(p, 4096, prot::NONE);
    let r2 = raw(sys::WRITE, ulib::STDOUT, p as u64, 1);
    let r3 = ulib::mprotect(p, 4096, prot::READ | prot::WRITE);
    let kept = unsafe { p.read_volatile() };
    check!(t, "kernel can't read PROT_NONE memory", r1.is_ok() && r2 == -Errno::EFAULT.0, "{:?} {}", r1, r2);
    check!(t, "mprotect keeps contents", r3.is_ok() && kept == 42, "{:?} {}", r3, kept);
    let _ = unsafe { ulib::munmap(p, 4096) };

    // Bad arguments.
    let r = ulib::mmap(0, prot::READ);
    check!(t, "mmap of 0 bytes is EINVAL", r == Err(Errno::EINVAL), "got {:?}", r);
    let r = unsafe { syscall6(sys::MMAP, 0, 4096, prot::READ, 0x02, 0, 0) }; // not MAP_ANONYMOUS
    check!(t, "file-backed mmap is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let r = ulib::mmap(4096, 8);
    check!(t, "unknown prot bits are EINVAL", r == Err(Errno::EINVAL), "got {:?}", r);
    let r = raw(sys::MUNMAP, 0x1000_0000_0123, 4096, 0);
    check!(t, "unaligned munmap is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let r = ulib::mprotect(0x1000_0000_0000 as *mut u8, 4096, prot::READ);
    check!(t, "mprotect of unmapped memory is ENOMEM", r == Err(Errno::ENOMEM), "got {:?}", r);
    let fixed = ulib::mmap_fixed(0x2000_0000_0000 as *mut u8, 8192, prot::READ | prot::WRITE);
    check!(t, "MAP_FIXED maps at the address", fixed == Ok(0x2000_0000_0000 as *mut u8), "got {:?}", fixed);
    if let Ok(p) = fixed {
        let _ = unsafe { ulib::munmap(p, 8192) };
    }

    // Faults that must kill, and stack use that must not.
    for mode in ["use-after-munmap", "write-readonly", "exec-heap"] {
        let r = run_child(&[mode]);
        check!(t, mode, r == Ok(PAGE_FAULT_EXIT), "got {:?}", r);
    }
    let r = run_child(&["stack", "6"]);
    check!(t, "6 MiB of stack works", r == Ok(0), "got {:?}", r);
    let r = run_child(&["stack", "9"]);
    check!(t, "stack overflow is a fault", r == Ok(PAGE_FAULT_EXIT), "got {:?}", r);
}

fn leaks(t: &mut Suite) {
    // Children we already collected may still await reaping; let that happen.
    ulib::sleep_ms(50);
    let before = ulib::sysinfo().tasks;
    let r = run_child(&["orphan"]);
    ulib::sleep_ms(50); // let the kernel reap
    let after = ulib::sysinfo().tasks;
    check!(t, "orphaned child leaves no task entry", r == Ok(0) && after == before, "tasks {} -> {}", before, after);

    // A first round lets the kernel heap grow to its working size; after the
    // second, every page must be back.
    let round = || {
        for _ in 0..8 {
            let _ = run_child(&["exit", "0"]);
        }
        let _ = run_child(&["hog", "32"]);
        ulib::sleep_ms(20);
    };
    round();
    let before = ulib::sysinfo().free_memory;
    round();
    let after = ulib::sysinfo().free_memory;
    check!(t, "processes return all their memory", after == before, "free {} -> {} bytes", before, after);
}
