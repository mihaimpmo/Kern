//! Starting the other CPUs (application processors, APs).
//!
//! Limine has already woken every CPU and parked it in a loop watching its
//! `MpInfo::goto_address`. The boot CPU registers each one with the scheduler
//! (so CPU indices follow start-up order), then stores the address of
//! `ap_entry`. The AP arrives on a 64 KiB stack from Limine, with Limine's
//! GDT and page tables and interrupts off, and sets itself up the way `kmain`
//! set up the boot CPU. Its boot thread then becomes its idle task.

use super::{apic, cpu, idt, percpu};
use crate::boot::{self, MpInfo};
use crate::{mm::vmm, syscall, task};
use core::sync::atomic::{AtomicUsize, Ordering};

static ONLINE: AtomicUsize = AtomicUsize::new(1); // the boot CPU

/// Start every other CPU and wait for them. Returns how many CPUs are online.
pub fn start_aps() -> usize {
    let Some(mp) = boot::MP.response() else { return 1 };
    let mut started = 0;
    for info in mp.cpus().filter(|c| c.lapic_id != mp.bsp_lapic_id) {
        let index = 1 + started;
        if index >= percpu::MAX_CPUS {
            kprintln!("\x1b[93m[warn]\x1b[0m more than {} CPUs; the rest stay parked", percpu::MAX_CPUS);
            break;
        }
        info.extra_argument.store(index as u64, Ordering::Relaxed);
        info.goto_address.store(ap_entry as *const () as u64, Ordering::Release);
        started += 1;
        // One at a time, so CPUs register with the scheduler in index order.
        // Microseconds on real hardware; generous for slow emulators.
        let deadline = 2_000_000_000u64;
        let mut spins = 0;
        while ONLINE.load(Ordering::Acquire) < index + 1 {
            spins += 1;
            if spins == deadline {
                panic!("CPU with LAPIC id {} did not come online", info.lapic_id);
            }
            core::hint::spin_loop();
        }
    }
    ONLINE.load(Ordering::Acquire)
}

pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

extern "C" fn ap_entry(info: &'static MpInfo) -> ! {
    let index = info.extra_argument.load(Ordering::Relaxed) as usize;
    unsafe { cpu::write_cr3(vmm::kernel_pml4()) };
    vmm::init_cpu();
    percpu::init(index); // our GDT first: the IDT refers to its selectors
    idt::load();
    cpu::enable_protections();
    apic::init_local();
    syscall::init();
    task::add_cpu();
    apic::start_local_timer();
    ONLINE.fetch_add(1, Ordering::Release);
    task::idle();
}
