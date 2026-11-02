//! selftest: checks the kernel from user space. Prints one TAP-style line per
//! check and exits with the number of failures, so `make test` can boot with
//! `init=selftest qemu-exit` and read the result from QEMU's exit status.
//!
//! It also starts copies of itself in helper modes (`selftest exit 3`, ...)
//! to get child processes that behave in a known way.

#![no_std]
#![no_main]

use ulib::{println, sys, syscall6, Errno};

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
        ulib::sleep_ms(20);
    };
    round();
    let before = ulib::sysinfo().free_memory;
    round();
    let after = ulib::sysinfo().free_memory;
    check!(t, "processes return all their memory", after == before, "free {} -> {} bytes", before, after);
}
