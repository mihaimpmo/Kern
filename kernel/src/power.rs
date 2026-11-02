//! What happens when the system has nothing left to run.

use crate::arch::cpu;
use crate::cmdline;

/// QEMU's isa-debug-exit device: writing `v` exits QEMU with status `(v << 1) | 1`.
const QEMU_DEBUG_EXIT: u16 = 0xf4;

/// Pid 1 exited. There is no one left to adopt orphans or run a shell, so stop.
pub fn init_exited(code: i64) -> ! {
    cpu::cli();
    kprintln!("\n[kernel] init exited with code {}; system halted", code);
    if cmdline::flag("qemu-exit") {
        crate::drivers::serial::stop_buffering(); // flush the log before QEMU goes away
        unsafe { cpu::outl(QEMU_DEBUG_EXIT, code as u32) };
    }
    cpu::halt_forever();
}
