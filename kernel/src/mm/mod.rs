//! Memory management.
//!
//! Virtual address space layout (4-level paging):
//!
//!   0x0000_0000_0040_0000  user program code (ELF)
//!   0x0000_7fff_ffff_f000  top of user stack
//!   0xffff_8000_0000_0000  HHDM: all physical RAM, mapped by Limine
//!   0xffff_d000_0000_0000  kernel heap (grows on demand)
//!   0xffff_e000_0000_0000  kernel stacks (with unmapped guard pages)
//!   0xffff_f000_0000_0000  MMIO mappings (APIC, ...)
//!   0xffff_ffff_8000_0000  kernel image

pub mod heap;
pub mod kstack;
pub mod pmm;
pub mod vmm;

use core::sync::atomic::{AtomicU64, Ordering};

pub const PAGE_SIZE: u64 = 4096;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn set_hhdm_offset(offset: u64) {
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
}

#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}

pub const fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

pub const fn align_down(v: u64, align: u64) -> u64 {
    v & !(align - 1)
}
