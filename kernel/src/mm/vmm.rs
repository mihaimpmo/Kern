//! Virtual memory: 4-level x86-64 page tables.
//!
//! All address spaces share the kernel half (PML4 entries 256..512). Those
//! entries are created once at boot, so kernel mappings added later (heap
//! growth, new kernel stacks) are automatically visible in every process.

use super::{phys_to_virt, pmm, PAGE_SIZE};
use crate::arch::cpu;
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub mod flags {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    pub const WRITE_THROUGH: u64 = 1 << 3;
    pub const NO_CACHE: u64 = 1 << 4;
    pub const HUGE: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    pub const NO_EXECUTE: u64 = 1 << 63;
}
use flags::*;

const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
const EFER_NXE: u64 = 1 << 11;
pub const USER_END: u64 = 0x0000_8000_0000_0000;

static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);
static NX_SUPPORTED: AtomicBool = AtomicBool::new(false);
static MMIO_NEXT: SpinLock<u64> = SpinLock::new(0xffff_f000_0000_0000);

/// Serialises all page-table modifications.
static PT_LOCK: SpinLock<()> = SpinLock::new(());

#[derive(Debug)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
    NotMapped,
}

fn table(phys: u64) -> &'static mut [u64; 512] {
    unsafe { &mut *(phys_to_virt(phys) as *mut [u64; 512]) }
}

fn fix_flags(f: u64) -> u64 {
    if NX_SUPPORTED.load(Ordering::Relaxed) {
        f
    } else {
        f & !NO_EXECUTE
    }
}

pub fn init() {
    let pml4 = cpu::read_cr3() & ADDR_MASK;
    KERNEL_PML4.store(pml4, Ordering::Relaxed);
    let efer = unsafe { cpu::rdmsr(cpu::msr::EFER) };
    NX_SUPPORTED.store(efer & EFER_NXE != 0, Ordering::Relaxed);

    // Pre-create every kernel-half PML4 entry so all address spaces share them.
    let t = table(pml4);
    for entry in t.iter_mut().skip(256) {
        if *entry & PRESENT == 0 {
            let frame = pmm::alloc_zeroed_frame().expect("out of memory creating kernel page tables");
            *entry = frame | PRESENT | WRITABLE;
        }
    }
}

/// Per-CPU part of `init`, for the other CPUs: turn on NX if the boot CPU
/// had it, since the shared page tables use the NX bit.
pub fn init_cpu() {
    if NX_SUPPORTED.load(Ordering::Relaxed) {
        unsafe { cpu::wrmsr(cpu::msr::EFER, cpu::rdmsr(cpu::msr::EFER) | EFER_NXE) };
    }
}

pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

/// Returns a pointer to the level-1 entry for `virt`, creating tables if asked.
fn walk(pml4: u64, virt: u64, create: bool, user: bool) -> Result<Option<&'static mut u64>, MapError> {
    let mut table_phys = pml4;
    for level in (1..=3).rev() {
        let index = ((virt >> (12 + 9 * level)) & 0x1ff) as usize;
        let entry = &mut table(table_phys)[index];
        if *entry & PRESENT == 0 {
            if !create {
                return Ok(None);
            }
            let frame = pmm::alloc_zeroed_frame().ok_or(MapError::OutOfMemory)?;
            *entry = frame | PRESENT | WRITABLE | if user { USER } else { 0 };
        } else if *entry & HUGE != 0 {
            return Ok(None);
        } else if user {
            *entry |= USER;
        }
        table_phys = *entry & ADDR_MASK;
    }
    let index = ((virt >> 12) & 0x1ff) as usize;
    Ok(Some(&mut table(table_phys)[index]))
}

pub fn map(pml4: u64, virt: u64, phys: u64, page_flags: u64) -> Result<(), MapError> {
    let _lock = PT_LOCK.lock();
    let user = page_flags & USER != 0;
    let pte = walk(pml4, virt, true, user)?.ok_or(MapError::AlreadyMapped)?;
    if *pte & PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }
    *pte = (phys & ADDR_MASK) | fix_flags(page_flags | PRESENT);
    Ok(())
}

/// Remove a mapping, returning the physical frame that was mapped.
pub fn unmap(pml4: u64, virt: u64) -> Option<u64> {
    let _lock = PT_LOCK.lock();
    let pte = walk(pml4, virt, false, false).ok()??;
    if *pte & PRESENT == 0 {
        return None;
    }
    let phys = *pte & ADDR_MASK;
    *pte = 0;
    unsafe { cpu::invlpg(virt) };
    Some(phys)
}

/// Call `f(virt, entry)` for every present 4 KiB mapping in [start, end)
/// (page-aligned, user half), skipping page tables that don't exist, so a
/// huge sparse range costs only as much as what is mapped in it. `f` may
/// rewrite or clear the entry; changed entries are flushed from this CPU's
/// TLB. (Only this CPU: user page tables are loaded only on the CPU running
/// their process.)
pub fn update_range(pml4: u64, start: u64, end: u64, mut f: impl FnMut(u64, &mut u64)) {
    debug_assert!(start % PAGE_SIZE == 0 && end % PAGE_SIZE == 0 && end <= USER_END);
    let _lock = PT_LOCK.lock();
    visit(pml4, 4, 0, start, end, &mut f);
}

fn visit(table_phys: u64, level: u32, base: u64, start: u64, end: u64, f: &mut dyn FnMut(u64, &mut u64)) {
    let span = 1u64 << (12 + 9 * (level - 1)); // bytes covered by one entry
    let first = start.saturating_sub(base) / span;
    let last = ((end - 1).saturating_sub(base) / span).min(511);
    for i in first..=last {
        let lo = base + i * span;
        let entry = &mut table(table_phys)[i as usize];
        if *entry & PRESENT == 0 {
            continue;
        }
        if level == 1 {
            let old = *entry;
            f(lo, entry);
            if *entry != old {
                unsafe { cpu::invlpg(lo) };
            }
        } else if *entry & HUGE == 0 {
            visit(*entry & ADDR_MASK, level - 1, lo, start, end, f);
        }
    }
}

/// Final page-table flags for a leaf entry (drops NX if the CPU lacks it).
pub fn leaf_flags(page_flags: u64) -> u64 {
    fix_flags(page_flags | PRESENT)
}

pub fn entry_frame(entry: u64) -> u64 {
    entry & ADDR_MASK
}

/// Translate a virtual address. Returns (physical address, entry flags).
pub fn translate(pml4: u64, virt: u64) -> Option<(u64, u64)> {
    let _lock = PT_LOCK.lock();
    let mut table_phys = pml4;
    for level in (1..=3).rev() {
        let index = ((virt >> (12 + 9 * level)) & 0x1ff) as usize;
        let entry = table(table_phys)[index];
        if entry & PRESENT == 0 {
            return None;
        }
        if entry & HUGE != 0 {
            let size = 1u64 << (12 + 9 * level);
            return Some(((entry & ADDR_MASK & !(size - 1)) + (virt & (size - 1)), entry));
        }
        table_phys = entry & ADDR_MASK;
    }
    let entry = table(table_phys)[((virt >> 12) & 0x1ff) as usize];
    if entry & PRESENT == 0 {
        return None;
    }
    Some(((entry & ADDR_MASK) + (virt & 0xfff), entry))
}

/// Map a device's registers into kernel space (uncached).
pub fn map_mmio(phys: u64, size: u64) -> u64 {
    let offset = phys & 0xfff;
    let pages = super::align_up(size + offset, PAGE_SIZE) / PAGE_SIZE;
    let base = {
        let mut next = MMIO_NEXT.lock();
        let base = *next;
        *next += pages * PAGE_SIZE;
        base
    };
    for i in 0..pages {
        map(
            kernel_pml4(),
            base + i * PAGE_SIZE,
            (phys & !0xfff) + i * PAGE_SIZE,
            WRITABLE | NO_CACHE | WRITE_THROUGH | NO_EXECUTE | GLOBAL,
        )
        .expect("map_mmio failed");
    }
    base + offset
}

// ------------------------------------------------------------- address spaces

/// A user process address space. The kernel half is shared.
pub struct AddressSpace {
    pub pml4: u64,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let pml4 = pmm::alloc_zeroed_frame()?;
        let kernel = table(kernel_pml4());
        table(pml4)[256..].copy_from_slice(&kernel[256..]);
        Some(Self { pml4 })
    }

    /// Copy `data` to `virt` in this address space, which need not be the
    /// active one: the copy goes through the HHDM. The pages must be mapped.
    pub fn write(&self, virt: u64, data: &[u8]) -> Result<(), MapError> {
        let mut done = 0;
        while done < data.len() {
            let va = virt + done as u64;
            let in_page = ((PAGE_SIZE - va % PAGE_SIZE) as usize).min(data.len() - done);
            let (pa, _) = translate(self.pml4, va).ok_or(MapError::NotMapped)?;
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr().add(done), phys_to_virt(pa) as *mut u8, in_page);
            }
            done += in_page;
        }
        Ok(())
    }
}

impl Drop for AddressSpace {
    /// Frees every user page, every user page table, then the PML4 itself.
    fn drop(&mut self) {
        let _lock = PT_LOCK.lock();
        assert!(cpu::read_cr3() & ADDR_MASK != self.pml4, "destroying the active address space");
        fn free_level(table_phys: u64, level: u32) {
            for &entry in table(table_phys).iter() {
                if entry & PRESENT == 0 {
                    continue;
                }
                let phys = entry & ADDR_MASK;
                if level > 1 && entry & HUGE == 0 {
                    free_level(phys, level - 1);
                }
                if level == 1 || entry & HUGE == 0 {
                    pmm::free_frame(phys);
                }
            }
        }
        let pml4 = table(self.pml4);
        for &entry in pml4[..256].iter() {
            if entry & PRESENT != 0 {
                let phys = entry & ADDR_MASK;
                free_level(phys, 3);
                pmm::free_frame(phys);
            }
        }
        pmm::free_frame(self.pml4);
    }
}
