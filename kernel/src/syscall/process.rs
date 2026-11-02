//! Process system calls: spawn, wait, and process information.

use super::{check_user, copy_from_user, copy_out, copy_to_user, user_bytes, user_path, Errno, SysResult};
use super::{E2BIG, ECHILD, EFAULT, EINTR, EINVAL, EPERM, ESRCH};
use crate::arch::smp;
use crate::drivers::{keyboard, rtc};
use crate::mm::pmm;
use crate::task::{self, exec, signal, Inherit, KillError, KillTarget, SpawnError, WaitError};
use core::sync::atomic::Ordering;
use alloc::{format, string::String, vec, vec::Vec};
use core::fmt::Write;

impl From<SpawnError> for Errno {
    fn from(e: SpawnError) -> Errno {
        match e {
            SpawnError::Fs(e) => e.into(),
            SpawnError::Load(e) => e.into(),
        }
    }
}

/// Start the program at `path` (a bare name means /bin/name) with `argc`
/// arguments; it inherits our file descriptors and working directory.
pub fn spawn(path: u64, path_len: u64, argv: u64, argc: u64) -> SysResult {
    let path = user_path(path, path_len)?;
    if argc > exec::MAX_ARGS as u64 {
        return Err(E2BIG);
    }
    let mut pairs = vec![0u8; argc as usize * 16];
    copy_from_user(&mut pairs, argv)?;
    let mut args = Vec::with_capacity(argc as usize);
    for pair in pairs.chunks_exact(16) {
        let ptr = u64::from_le_bytes(pair[..8].try_into().unwrap());
        let len = u64::from_le_bytes(pair[8..].try_into().unwrap());
        args.push(user_bytes(ptr, len, exec::MAX_ARG_BYTES, E2BIG)?);
    }
    let args: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();

    let me = task::current_process().ok_or(EFAULT)?;
    let inherit = Inherit {
        files: me.files.lock().for_child(),
        cwd: me.cwd.lock().clone(),
        pgid: me.pgid.load(Ordering::Relaxed),
    };
    drop(me);
    match task::spawn_program(&path, &args, Some(task::current_pid()), inherit) {
        Ok(pid) => Ok(pid),
        Err(e @ SpawnError::Load(why)) => {
            kprintln!("\x1b[91m[kernel] cannot start {}: {}\x1b[0m", path, why);
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

pub fn wait(pid: u64, code_ptr: u64) -> SysResult {
    // Check first: once the child is collected its exit code can't be put back.
    check_user(code_ptr, 8, true)?;
    let status = task::wait(pid).map_err(|e| match e {
        WaitError::NotChild => ECHILD,
        WaitError::Interrupted => EINTR,
    })?;
    copy_to_user(code_ptr, &status.encode().to_le_bytes())?;
    Ok(0)
}

pub fn try_wait(code_ptr: u64) -> SysResult {
    check_user(code_ptr, 8, true)?;
    match task::try_wait_any() {
        Some((pid, status)) => {
            copy_to_user(code_ptr, &status.encode().to_le_bytes())?;
            Ok(pid)
        }
        None => Ok(0),
    }
}

pub fn sysinfo(buf: u64) -> SysResult {
    let (free, total) = pmm::stats();
    let fields = [total, free, task::uptime_ms(), task::list().len() as u64, smp::online() as u64];
    let mut out = [0u8; 40];
    for (i, v) in fields.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    copy_to_user(buf, &out)?;
    Ok(0)
}

pub fn ps(buf: u64, len: u64) -> SysResult {
    let mut text = String::new();
    let _ = writeln!(text, "  PID  PPID  STATE     CPU  NAME");
    for t in task::list() {
        let ppid = t.parent.map_or(String::from("-"), |p| format!("{}", p));
        let cpu = t.cpu.map_or(String::from("-"), |c| format!("{}", c));
        let state = format!("{:?}", t.state);
        let _ = writeln!(text, "{:>5} {:>5}  {:<9} {:>3}  {}", t.pid, ppid, state, cpu, t.name);
    }
    copy_out(text.as_bytes(), buf, len)
}

/// `pid` > 0: that process; < 0: the process group -pid; 0: our own group.
pub fn kill(pid: u64, sig: u64) -> SysResult {
    if sig == 0 || sig > signal::MAX as u64 {
        return Err(EINVAL);
    }
    let pid = pid as i64;
    let target = match pid {
        0 => KillTarget::Group(task::current_process().ok_or(EFAULT)?.pgid.load(Ordering::Relaxed)),
        p if p < 0 => KillTarget::Group(p.unsigned_abs()),
        p => KillTarget::Process(p as u64),
    };
    task::kill(target, sig as u8).map_err(kill_errno)?;
    Ok(0)
}

fn kill_errno(e: KillError) -> super::Errno {
    match e {
        KillError::NotFound => ESRCH,
        KillError::Init => EPERM,
    }
}

/// Move process `pid` (0: ourselves) into group `pgid` (0: a new group
/// named after `pid`).
pub fn setpgid(pid: u64, pgid: u64) -> SysResult {
    let pid = if pid == 0 { task::current_pid() } else { pid };
    let pgid = if pgid == 0 { pid } else { pgid };
    task::set_process_group(pid, pgid).map_err(kill_errno)?;
    Ok(0)
}

/// Which process group Ctrl+C interrupts (0: none).
pub fn set_foreground(pgid: u64) -> SysResult {
    keyboard::set_foreground(pgid);
    Ok(0)
}

/// Milliseconds since 1970-01-01 UTC (0 if the clock couldn't be read).
pub fn time_ms() -> SysResult {
    Ok(rtc::now_ms(task::uptime_ms()))
}
