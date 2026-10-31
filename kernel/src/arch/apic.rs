//! Local APIC (timer, EOI) and I/O APIC (routing of device IRQs).
//! Supports both xAPIC (MMIO) and x2APIC (MSR) mode, since firmware on newer
//! machines often leaves the CPU in x2APIC mode.

use super::cpu::{self, inb, io_wait, outb, rdmsr, wrmsr};
use super::interrupts::{VECTOR_LEGACY_PIC_BASE, VECTOR_SPURIOUS, VECTOR_TIMER};
use crate::acpi::Madt;
use crate::mm::vmm;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static X2APIC: AtomicBool = AtomicBool::new(false);
static TICKS_PER_MS: AtomicU64 = AtomicU64::new(0);

const REG_ID: u32 = 0x20;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xB0;
const REG_SVR: u32 = 0xF0;
const REG_LVT_TIMER: u32 = 0x320;
const REG_TIMER_INIT: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3E0;

fn lapic_read(reg: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        unsafe { rdmsr(0x800 + (reg >> 4)) as u32 }
    } else {
        unsafe { core::ptr::read_volatile((LAPIC_BASE.load(Ordering::Relaxed) + reg as u64) as *const u32) }
    }
}

fn lapic_write(reg: u32, value: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        unsafe { wrmsr(0x800 + (reg >> 4), value as u64) }
    } else {
        unsafe { core::ptr::write_volatile((LAPIC_BASE.load(Ordering::Relaxed) + reg as u64) as *mut u32, value) }
    }
}

pub fn eoi() {
    lapic_write(REG_EOI, 0);
}

pub fn lapic_id() -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        lapic_read(REG_ID)
    } else {
        lapic_read(REG_ID) >> 24
    }
}

pub fn is_x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

/// Remap the legacy 8259 PICs out of the exception range and mask them.
fn disable_legacy_pic() {
    unsafe {
        outb(0x20, 0x11);
        io_wait();
        outb(0xA0, 0x11);
        io_wait();
        outb(0x21, VECTOR_LEGACY_PIC_BASE as u8);
        io_wait();
        outb(0xA1, VECTOR_LEGACY_PIC_BASE as u8 + 8);
        io_wait();
        outb(0x21, 4);
        io_wait();
        outb(0xA1, 2);
        io_wait();
        outb(0x21, 1);
        io_wait();
        outb(0xA1, 1);
        io_wait();
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
    }
}

pub fn init(madt: &Madt) {
    disable_legacy_pic();

    let base_msr = unsafe { rdmsr(cpu::msr::APIC_BASE) };
    if base_msr & (1 << 10) != 0 {
        // Firmware left the CPU in x2APIC mode (common on newer machines);
        // it can't be switched back to xAPIC, so use the MSR interface.
        X2APIC.store(true, Ordering::Relaxed);
    } else {
        let virt = vmm::map_mmio(madt.lapic_phys, 0x1000);
        LAPIC_BASE.store(virt, Ordering::Relaxed);
        // Make sure the APIC is globally enabled.
        unsafe { cpu::wrmsr(cpu::msr::APIC_BASE, base_msr | (1 << 11)) };
    }

    lapic_write(REG_TPR, 0);
    lapic_write(REG_SVR, 0x100 | VECTOR_SPURIOUS as u32); // software-enable + spurious vector

    let mut ioapics = IOAPICS.lock();
    for io in &madt.ioapics {
        let virt = vmm::map_mmio(io.phys, 0x1000);
        let count = ((ioapic_read(virt, 1) >> 16) & 0xff) + 1;
        for i in 0..count {
            ioapic_write(virt, 0x10 + 2 * i, 1 << 16); // masked
        }
        ioapics.push(IoApic { virt, gsi_base: io.gsi_base, count });
    }
}

// ---------------------------------------------------------------- timer

/// Measure the LAPIC timer against the PIT, then start it at 1000 Hz.
pub fn start_timer() -> u64 {
    lapic_write(REG_TIMER_DIVIDE, 0x3); // divide by 16
    lapic_write(REG_LVT_TIMER, 1 << 16); // masked, one-shot

    let ticks_per_ms = match calibrate_with_pit(10) {
        Some(t) if t > 0 => t,
        _ => {
            kprintln!("[apic] warning: PIT calibration failed, guessing timer frequency");
            100_000
        }
    };
    TICKS_PER_MS.store(ticks_per_ms, Ordering::Relaxed);

    lapic_write(REG_LVT_TIMER, VECTOR_TIMER as u32 | (1 << 17)); // periodic
    lapic_write(REG_TIMER_INIT, ticks_per_ms as u32);
    ticks_per_ms
}

fn calibrate_with_pit(ms: u64) -> Option<u64> {
    const PIT_HZ: u64 = 1_193_182;
    let count = (PIT_HZ * ms / 1000) as u16;
    unsafe {
        // PIT channel 2, gate controlled through port 0x61, speaker off.
        let port61 = inb(0x61) & 0xFC;
        outb(0x61, port61);
        outb(0x43, 0b1011_0000); // channel 2, lo/hi byte, mode 0, binary
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);

        lapic_write(REG_TIMER_INIT, u32::MAX);
        outb(0x61, port61 | 1); // start counting

        let mut spins: u64 = 0;
        while inb(0x61) & 0x20 == 0 {
            spins += 1;
            if spins > 100_000_000 {
                outb(0x61, port61);
                return None;
            }
        }
        let elapsed = u32::MAX - lapic_read(REG_TIMER_CURRENT);
        lapic_write(REG_TIMER_INIT, 0);
        outb(0x61, port61);
        Some(elapsed as u64 / ms)
    }
}

// --------------------------------------------------------------- I/O APIC

struct IoApic {
    virt: u64,
    gsi_base: u32,
    count: u32,
}

static IOAPICS: crate::sync::SpinLock<Vec<IoApic>> = crate::sync::SpinLock::new(Vec::new());

fn ioapic_read(base: u64, reg: u32) -> u32 {
    unsafe {
        core::ptr::write_volatile(base as *mut u32, reg);
        core::ptr::read_volatile((base + 0x10) as *const u32)
    }
}

fn ioapic_write(base: u64, reg: u32, value: u32) {
    unsafe {
        core::ptr::write_volatile(base as *mut u32, reg);
        core::ptr::write_volatile((base + 0x10) as *mut u32, value);
    }
}

/// Route a legacy ISA IRQ (e.g. 1 = keyboard) to `vector` on this CPU,
/// honouring any interrupt source override from the MADT.
pub fn route_isa_irq(madt: &Madt, irq: u8, vector: u8) -> bool {
    let (gsi, flags) = madt
        .overrides
        .iter()
        .find(|o| o.source == irq)
        .map(|o| (o.gsi, o.flags))
        .unwrap_or((irq as u32, 0));

    let mut low = vector as u32;
    if flags & 0b11 == 0b11 {
        low |= 1 << 13; // active low
    }
    if (flags >> 2) & 0b11 == 0b11 {
        low |= 1 << 15; // level triggered
    }
    let high = lapic_id() << 24;

    let ioapics = IOAPICS.lock();
    for io in ioapics.iter() {
        if gsi >= io.gsi_base && gsi < io.gsi_base + io.count {
            let index = 0x10 + 2 * (gsi - io.gsi_base);
            ioapic_write(io.virt, index + 1, high);
            ioapic_write(io.virt, index, low);
            return true;
        }
    }
    false
}
