//! x87/SSE state for user programs.
//!
//! The kernel is built without floating point (a soft-float target), so it
//! never touches these registers itself: only user tasks have state, saved
//! and restored on every switch (`fxsave`/`fxrstor`, 512 bytes per task).
//! Only the SSE baseline is enabled; AVX would need XSAVE and stays off, so
//! AVX instructions fault (#UD) in user programs.

use super::cpu;
use crate::sync::BootCell;
use alloc::boxed::Box;
use core::arch::asm;

const CR0_MP: u64 = 1 << 1; // WAIT honours TS
const CR0_EM: u64 = 1 << 2; // x87 emulation: off
const CR0_TS: u64 = 1 << 3; // task switched: no lazy switching here
const CR0_NE: u64 = 1 << 5; // x87 errors as #MF exceptions
const CR4_OSFXSR: u64 = 1 << 9; // fxsave/fxrstor and SSE
const CR4_OSXMMEXCPT: u64 = 1 << 10; // SSE errors as #XM exceptions

/// A task's saved x87/SSE registers (the `fxsave` layout).
#[repr(C, align(16))]
#[derive(Clone)]
pub struct FpuState([u8; 512]);

/// Freshly initialised registers (x87 control word 0x37f, MXCSR 0x1f80:
/// all exceptions masked, round to nearest), copied into every new task.
static INITIAL: BootCell<FpuState> = BootCell::new(FpuState([0; 512]));

/// Per CPU: enable x87 and SSE.
pub fn init_cpu() {
    unsafe {
        cpu::write_cr0((cpu::read_cr0() & !(CR0_EM | CR0_TS)) | CR0_MP | CR0_NE);
        cpu::write_cr4(cpu::read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT);
    }
}

/// Boot CPU, once, after `init_cpu`: capture the initial register state.
pub fn init() {
    let mxcsr: u32 = 0x1f80;
    unsafe {
        asm!("fninit", "ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));
        INITIAL.get_mut().save();
    }
}

impl FpuState {
    /// The state a new task starts with.
    pub fn new() -> Box<FpuState> {
        Box::new(unsafe { (*INITIAL.as_ptr()).clone() })
    }

    pub fn save(&mut self) {
        unsafe { asm!("fxsave64 [{}]", in(reg) self.0.as_mut_ptr(), options(nostack, preserves_flags)) };
    }

    pub fn restore(&self) {
        unsafe { asm!("fxrstor64 [{}]", in(reg) self.0.as_ptr(), options(nostack, preserves_flags)) };
    }
}
