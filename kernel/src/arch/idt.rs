//! Interrupt Descriptor Table. Every vector points to a generated stub
//! (see build.rs) which funnels into `interrupts::interrupt_dispatch`.

use super::gdt;
use crate::sync::Racy;
use core::arch::asm;
use core::mem::size_of;

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_mid: u16,
    offset_high: u32,
    _zero: u32,
}

impl Entry {
    const EMPTY: Entry =
        Entry { offset_low: 0, selector: 0, ist: 0, attributes: 0, offset_mid: 0, offset_high: 0, _zero: 0 };

    fn new(handler: u64, ist: u8) -> Entry {
        Entry {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CS,
            ist,
            attributes: 0x8E, // present, DPL 0, 64-bit interrupt gate (IF cleared on entry)
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _zero: 0,
        }
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static IDT: Racy<[Entry; 256]> = Racy::new([Entry::EMPTY; 256]);

extern "C" {
    static isr_stub_table: [u64; 256];
}

pub fn init() {
    unsafe {
        let idt = &mut *IDT.get();
        for (vector, entry) in idt.iter_mut().enumerate() {
            let ist = match vector {
                2 => gdt::IST_NMI,
                8 => gdt::IST_DOUBLE_FAULT,
                18 => gdt::IST_MACHINE_CHECK,
                _ => 0,
            };
            *entry = Entry::new(isr_stub_table[vector], ist);
        }
        let ptr = IdtPointer { limit: (size_of::<[Entry; 256]>() - 1) as u16, base: idt.as_ptr() as u64 };
        asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
    }
}
