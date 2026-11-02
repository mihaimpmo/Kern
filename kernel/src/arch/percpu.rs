//! Per-CPU data, reached through the GS segment base while in kernel mode.
//!
//! Each CPU gets one `PerCpu` (leaked, so it lives forever) holding the
//! fields the syscall entry path needs at fixed offsets, the CPU's index, and
//! its own GDT/TSS with IST stacks.
//!
//! A task can move to another CPU whenever it is preempted, so anything read
//! from here is only meaningful while interrupts are off.

use super::cpu::{self, msr, wrmsr};
use super::gdt::{self, CpuTables};
use crate::mm::kstack::KernelStack;
use alloc::boxed::Box;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

pub const MAX_CPUS: usize = 64;

#[repr(C)]
pub struct PerCpu {
    /// gs:[0]: kernel stack of the running task (read by syscall_entry).
    kernel_rsp: AtomicU64,
    /// gs:[8]: scratch slot for the user stack pointer during syscall entry.
    user_rsp: AtomicU64,
    /// gs:[16]: this struct's own address, so `this()` is a single load.
    this: *const PerCpu,
    /// 0 for the boot CPU, then 1, 2, ... in start-up order.
    pub index: usize,
    /// Touched only by the owning CPU, with interrupts off.
    tables: UnsafeCell<CpuTables>,
    _ist_stacks: [KernelStack; gdt::IST_COUNT],
}

// Fields are either atomics, immutable after init, or (`tables`) only used
// by the owning CPU with interrupts off.
unsafe impl Sync for PerCpu {}

/// Set up the calling CPU: its GDT/TSS and GS base. Needs the heap.
pub fn init(index: usize) {
    assert!(index < MAX_CPUS);
    let ist_stacks = [(); gdt::IST_COUNT].map(|_| KernelStack::new().expect("out of memory for IST stacks"));
    let tables = CpuTables::new(ist_stacks.each_ref().map(|s| s.top()));
    let percpu: &'static mut PerCpu = Box::leak(Box::new(PerCpu {
        kernel_rsp: AtomicU64::new(0),
        user_rsp: AtomicU64::new(0),
        this: core::ptr::null(),
        index,
        tables: UnsafeCell::new(tables),
        _ist_stacks: ist_stacks,
    }));
    percpu.this = percpu;
    let percpu: &'static PerCpu = percpu;
    unsafe {
        (*percpu.tables.get()).load(); // reloads GS with a null selector...
        // ...so GS base is set after it. While in the kernel GS_BASE points at
        // our block; user mode gets KERNEL_GS_BASE (swapped with `swapgs`).
        wrmsr(msr::GS_BASE, percpu as *const PerCpu as u64);
        wrmsr(msr::KERNEL_GS_BASE, 0);
    }
}

/// The calling CPU's block. Only valid after `init` on this CPU.
pub fn this() -> &'static PerCpu {
    let ptr: *const PerCpu;
    unsafe { asm!("mov {}, gs:[16]", out(reg) ptr, options(nostack, readonly, preserves_flags)) };
    unsafe { &*ptr }
}

/// Index of the calling CPU (see the module note on preemption).
pub fn index() -> usize {
    this().index
}

/// Kernel stack for the task about to run on this CPU: used by both the
/// syscall entry path and interrupts from user mode (via the TSS).
pub fn set_kernel_stack(top: u64) {
    debug_assert!(!cpu::interrupts_enabled());
    let me = this();
    me.kernel_rsp.store(top, Ordering::Relaxed);
    unsafe { (*me.tables.get()).set_kernel_stack(top) };
}
