//! Stopping the system: when pid 1 exits, and on panic.

use crate::arch::{apic, cpu, smp};
use crate::cmdline;
use core::sync::atomic::{AtomicBool, Ordering};

/// QEMU's isa-debug-exit device: writing `v` exits QEMU with status `(v << 1) | 1`.
const QEMU_DEBUG_EXIT: u16 = 0xf4;

static HALTING: AtomicBool = AtomicBool::new(false);

/// Make every other CPU halt (from its NMI handler, so even one spinning
/// with interrupts off stops). Returns false if another CPU got here first;
/// that one is stopping the system, so the caller should just halt.
pub fn stop_other_cpus() -> bool {
    if HALTING.swap(true, Ordering::SeqCst) {
        return false;
    }
    if smp::online() > 1 {
        apic::send_nmi_to_others();
    }
    true
}

/// Checked by the NMI handler.
pub fn halting() -> bool {
    HALTING.load(Ordering::SeqCst)
}

/// Pid 1 exited. There is no one left to adopt orphans or run a shell, so stop.
pub fn init_exited(code: i64) -> ! {
    cpu::cli();
    if !stop_other_cpus() {
        cpu::halt_forever();
    }
    kprintln!("\n[kernel] init exited with code {}; system halted", code);
    crate::drivers::serial::stop_buffering(); // the timer won't drain it any more
    if cmdline::flag("qemu-exit") {
        unsafe { cpu::outl(QEMU_DEBUG_EXIT, code as u32) };
    }
    cpu::halt_forever();
}
