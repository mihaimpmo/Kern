//! Minimal ACPI table parsing: finds the MADT to learn about the interrupt
//! controllers and CPUs. (Full ACPI needs an AML interpreter, e.g. uACPI.)

use crate::mm::phys_to_virt;
use alloc::vec::Vec;
use core::ptr::read_unaligned;

pub struct IoApicInfo {
    pub phys: u64,
    pub gsi_base: u32,
}

pub struct IrqOverride {
    pub source: u8,
    pub gsi: u32,
    pub flags: u16,
}

pub struct Madt {
    pub lapic_phys: u64,
    pub cpu_count: usize,
    pub ioapics: Vec<IoApicInfo>,
    pub overrides: Vec<IrqOverride>,
}

unsafe fn read<T: Copy>(addr: u64) -> T {
    read_unaligned(addr as *const T)
}

unsafe fn find_table(rsdp: u64, signature: &[u8; 4]) -> Option<u64> {
    if &read::<[u8; 8]>(rsdp) != b"RSD PTR " {
        return None;
    }
    let revision: u8 = read(rsdp + 15);
    let (root, entry_size) = if revision >= 2 {
        (read::<u64>(rsdp + 24), 8) // XSDT
    } else {
        (read::<u32>(rsdp + 16) as u64, 4) // RSDT
    };
    let root = phys_to_virt(root);
    let length: u32 = read(root + 4);
    let count = (length as u64 - 36) / entry_size;
    for i in 0..count {
        let addr = root + 36 + i * entry_size;
        let table = if entry_size == 8 { read::<u64>(addr) } else { read::<u32>(addr) as u64 };
        let table = phys_to_virt(table);
        if &read::<[u8; 4]>(table) == signature {
            return Some(table);
        }
    }
    None
}

/// `rsdp` is the (HHDM-virtual) address handed over by Limine.
pub fn parse_madt(rsdp: u64) -> Option<Madt> {
    unsafe {
        let madt = find_table(rsdp, b"APIC")?;
        let length: u32 = read(madt + 4);
        let mut info = Madt {
            lapic_phys: read::<u32>(madt + 36) as u64,
            cpu_count: 0,
            ioapics: Vec::new(),
            overrides: Vec::new(),
        };

        let mut p = madt + 44;
        let end = madt + length as u64;
        while p + 2 <= end {
            let kind: u8 = read(p);
            let len: u8 = read(p + 1);
            if len < 2 {
                break;
            }
            match kind {
                0 => {
                    // Processor local APIC: flags bit 0 = enabled, bit 1 = online capable
                    let flags: u32 = read(p + 4);
                    if flags & 3 != 0 {
                        info.cpu_count += 1;
                    }
                }
                1 => info.ioapics.push(IoApicInfo { phys: read::<u32>(p + 4) as u64, gsi_base: read(p + 8) }),
                2 => info.overrides.push(IrqOverride { source: read(p + 3), gsi: read(p + 4), flags: read(p + 8) }),
                5 => info.lapic_phys = read(p + 4),
                9 => {
                    let flags: u32 = read(p + 8);
                    if flags & 3 != 0 {
                        info.cpu_count += 1;
                    }
                }
                _ => {}
            }
            p += len as u64;
        }
        Some(info)
    }
}
