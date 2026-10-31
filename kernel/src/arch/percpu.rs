//! Per-CPU data, reached through the GS segment base while in kernel mode.
//! The syscall entry path uses it to find the kernel stack.

use super::cpu::{msr, wrmsr};
use crate::sync::Racy;

#[repr(C)]
pub struct PerCpu {
    /// Offset 0: kernel stack for the running task (read by syscall_entry).
    pub kernel_rsp: u64,
    /// Offset 8: scratch slot for the user stack pointer during syscall entry.
    pub user_rsp: u64,
}

static BSP: Racy<PerCpu> = Racy::new(PerCpu { kernel_rsp: 0, user_rsp: 0 });

pub fn init() {
    unsafe {
        // While in the kernel GS_BASE points at our per-CPU block; user mode
        // gets the value parked in KERNEL_GS_BASE (swapped with `swapgs`).
        wrmsr(msr::GS_BASE, BSP.get() as u64);
        wrmsr(msr::KERNEL_GS_BASE, 0);
    }
}

pub fn set_kernel_stack(top: u64) {
    unsafe { (*BSP.get()).kernel_rsp = top };
}
