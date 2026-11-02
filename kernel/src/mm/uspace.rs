//! A process's user address space: its page tables plus the regions it may
//! use.
//!
//! Layout of the user half:
//!
//!   0x0000_0000_0040_0000  program segments (ELF), then the heap (brk),
//!                          which may grow up to MMAP_BASE
//!   MMAP_BASE ..MMAP_TOP   mmap area, handed out top-down
//!   STACK_BOTTOM..STACK_TOP the stack, STACK_SIZE bytes
//!
//! A region says what the process may access. Pages are allocated on first
//! touch (a page fault, or kernel access through `populate`), so reserving
//! address space costs nothing.
//!
//! TLB: a process's page tables are only loaded on the CPU running it, so
//! changes are flushed from this CPU only. (Threads would break this.)

use super::vmm::{self, AddressSpace};
use super::{align_down, align_up, pmm, PAGE_SIZE};
use alloc::collections::BTreeMap;

pub const STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
pub const STACK_SIZE: u64 = 8 << 20;
pub const STACK_BOTTOM: u64 = STACK_TOP - STACK_SIZE;
pub const MMAP_BASE: u64 = 0x0000_1000_0000_0000;
const MMAP_TOP: u64 = STACK_BOTTOM - (1 << 20); // a gap below the stack
/// Bound on kernel memory a process can tie up with tiny mappings.
const MAX_REGIONS: usize = 4096;

/// Access rights, with the Linux PROT_* bit values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prot(u8);

impl Prot {
    pub const NONE: Prot = Prot(0);
    pub const READ: Prot = Prot(1);
    pub const WRITE: Prot = Prot(2);
    pub const EXEC: Prot = Prot(4);
    pub const RW: Prot = Prot(3);

    pub fn from_bits(bits: u64) -> Option<Prot> {
        (bits & !7 == 0).then_some(Prot(bits as u8))
    }

    pub fn union(self, other: Prot) -> Prot {
        Prot(self.0 | other.0)
    }

    pub fn has(self, other: Prot) -> bool {
        self.0 & other.0 == other.0
    }

    /// Page-table flags. x86 can't express write-only or exec-only, so any
    /// access right makes the page readable; PROT_NONE keeps the page (and
    /// its contents) but drops the USER bit, so user access faults.
    fn page_flags(self) -> u64 {
        let mut flags = 0;
        if self != Prot::NONE {
            flags |= vmm::flags::USER;
        }
        if self.has(Prot::WRITE) {
            flags |= vmm::flags::WRITABLE;
        }
        if !self.has(Prot::EXEC) {
            flags |= vmm::flags::NO_EXECUTE;
        }
        flags
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemError {
    /// Out of physical memory or of regions.
    NoMemory,
    /// Bad arguments: misaligned, empty, overlapping, outside user space.
    Invalid,
    /// The address isn't in a region that allows the access.
    Fault,
}

/// What `handle_fault` did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultResult {
    Handled,
    Denied,
    NoMemory,
}

#[derive(Clone, Copy)]
struct Region {
    end: u64,
    prot: Prot,
}

pub struct UserSpace {
    pt: AddressSpace,
    /// start -> region. Page-aligned, non-empty, non-overlapping.
    regions: BTreeMap<u64, Region>,
    heap_start: u64,
    brk: u64,
}

impl UserSpace {
    pub fn new() -> Option<Self> {
        Some(UserSpace { pt: AddressSpace::new()?, regions: BTreeMap::new(), heap_start: 0, brk: 0 })
    }

    pub fn pml4(&self) -> u64 {
        self.pt.pml4
    }

    fn region_at(&self, addr: u64) -> Option<(u64, Region)> {
        let (&start, &r) = self.regions.range(..=addr).next_back()?;
        (addr < r.end).then_some((start, r))
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        // Only the last region starting below `end` can reach past `start`.
        self.regions.range(..end).next_back().map_or(false, |(_, r)| r.end > start)
    }

    /// Add a region over [start, end), which must be free, merging it with
    /// neighbours that have the same rights.
    pub fn add_region(&mut self, start: u64, end: u64, prot: Prot) -> Result<(), MemError> {
        let aligned = start % PAGE_SIZE == 0 && end % PAGE_SIZE == 0;
        if !aligned || start >= end || start < PAGE_SIZE || end > STACK_TOP || self.overlaps(start, end) {
            return Err(MemError::Invalid);
        }
        let mut start = start;
        let mut end = end;
        if let Some((s, left)) = start.checked_sub(1).and_then(|a| self.region_at(a)) {
            if left.prot == prot {
                self.regions.remove(&s);
                start = s;
            }
        }
        if let Some(right) = self.regions.get(&end).copied() {
            if right.prot == prot {
                self.regions.remove(&end);
                end = right.end;
            }
        }
        if self.regions.len() >= MAX_REGIONS {
            return Err(MemError::NoMemory);
        }
        self.regions.insert(start, Region { end, prot });
        Ok(())
    }

    /// Make `at` a region boundary, splitting the region that spans it.
    fn split_at(&mut self, at: u64) {
        if let Some((start, r)) = self.region_at(at) {
            if start < at {
                self.regions.insert(start, Region { end: at, prot: r.prot });
                self.regions.insert(at, r);
            }
        }
    }

    /// Starts of the regions inside [start, end), after splitting at both ends.
    fn isolate(&mut self, start: u64, end: u64) -> alloc::vec::Vec<u64> {
        self.split_at(start);
        self.split_at(end);
        self.regions.range(start..end).map(|(&s, _)| s).collect()
    }

    /// Forget [start, end) and free every page in it.
    fn remove_range(&mut self, start: u64, end: u64) {
        for s in self.isolate(start, end) {
            self.regions.remove(&s);
        }
        vmm::update_range(self.pml4(), start, end, |_, entry| {
            pmm::free_frame(vmm::entry_frame(*entry));
            *entry = 0;
        });
    }

    /// Map the page at `page` if it isn't yet: a fresh zeroed frame.
    fn fault_in(&mut self, page: u64, prot: Prot) -> Result<(), MemError> {
        if vmm::translate(self.pml4(), page).is_some() {
            return Ok(());
        }
        let frame = pmm::alloc_zeroed_frame().ok_or(MemError::NoMemory)?;
        vmm::map(self.pml4(), page, frame, prot.page_flags()).map_err(|_| {
            pmm::free_frame(frame);
            MemError::NoMemory
        })
    }

    /// Map every untouched page of [addr, addr + len); all of it must be in
    /// regions, and `need` must be allowed there unless `need` is None.
    fn populate_with(&mut self, addr: u64, len: u64, need: Option<Prot>) -> Result<(), MemError> {
        if len == 0 {
            return Ok(());
        }
        let end = addr.checked_add(len).filter(|&e| e <= STACK_TOP).ok_or(MemError::Fault)?;
        let mut page = align_down(addr, PAGE_SIZE);
        while page < end {
            let (_, r) = self.region_at(page).ok_or(MemError::Fault)?;
            if need.map_or(false, |n| !r.prot.has(n)) {
                return Err(MemError::Fault);
            }
            let stop = r.end.min(align_up(end, PAGE_SIZE));
            while page < stop {
                self.fault_in(page, r.prot)?;
                page += PAGE_SIZE;
            }
        }
        Ok(())
    }

    /// Before the kernel touches [addr, addr + len) for the process: check
    /// it may be read (and written, if `write`), and map untouched pages.
    pub fn populate(&mut self, addr: u64, len: u64, write: bool) -> Result<(), MemError> {
        self.populate_with(addr, len, Some(if write { Prot::RW } else { Prot::READ }))
    }

    /// Fill in memory while loading a program, whatever the region's rights
    /// (e.g. code). The address space need not be the active one.
    pub fn load(&mut self, addr: u64, data: &[u8]) -> Result<(), MemError> {
        self.populate_with(addr, data.len() as u64, None)?;
        self.pt.write(addr, data).map_err(|_| MemError::NoMemory)
    }

    /// A user-mode page fault on an unmapped page at `addr`.
    pub fn handle_fault(&mut self, addr: u64, write: bool, exec: bool) -> FaultResult {
        let Some((_, r)) = self.region_at(addr) else { return FaultResult::Denied };
        let need = if exec { Prot::EXEC } else if write { Prot::WRITE } else { Prot::READ };
        if !r.prot.has(need) {
            return FaultResult::Denied;
        }
        match self.fault_in(align_down(addr, PAGE_SIZE), r.prot) {
            Ok(()) => FaultResult::Handled,
            Err(_) => FaultResult::NoMemory,
        }
    }

    /// Where the heap starts: just after the program's segments.
    pub fn set_heap_start(&mut self, at: u64) {
        self.heap_start = align_up(at, PAGE_SIZE);
        self.brk = self.heap_start;
    }

    /// Move the end of the heap to `requested` (0: just ask). Returns the new
    /// end, or the unchanged one if the request can't be met (as Linux does).
    pub fn brk(&mut self, requested: u64) -> u64 {
        if requested < self.heap_start || requested > MMAP_BASE {
            return self.brk;
        }
        let (old_top, new_top) = (align_up(self.brk, PAGE_SIZE), align_up(requested, PAGE_SIZE));
        if new_top > old_top {
            if self.add_region(old_top, new_top, Prot::RW).is_err() {
                return self.brk;
            }
        } else if new_top < old_top {
            self.remove_range(new_top, old_top);
        }
        self.brk = requested;
        self.brk
    }

    /// Reserve `len` bytes of fresh zeroed memory with rights `prot`, at
    /// `fixed` (replacing whatever was there) or wherever there is room.
    pub fn mmap(&mut self, fixed: Option<u64>, len: u64, prot: Prot) -> Result<u64, MemError> {
        if len == 0 {
            return Err(MemError::Invalid);
        }
        let len = len.checked_add(PAGE_SIZE - 1).ok_or(MemError::NoMemory)? & !(PAGE_SIZE - 1);
        let start = match fixed {
            Some(addr) => {
                let end = addr.checked_add(len).ok_or(MemError::Invalid)?;
                if addr % PAGE_SIZE != 0 || addr < PAGE_SIZE || end > STACK_TOP {
                    return Err(MemError::Invalid);
                }
                self.remove_range(addr, end);
                addr
            }
            None => self.find_gap(len).ok_or(MemError::NoMemory)?,
        };
        self.add_region(start, start + len, prot)?;
        Ok(start)
    }

    /// Highest free range of `len` bytes in the mmap area.
    fn find_gap(&self, len: u64) -> Option<u64> {
        let mut top = MMAP_TOP;
        for (&start, r) in self.regions.range(..MMAP_TOP).rev() {
            let floor = r.end.max(MMAP_BASE);
            if top >= floor && top - floor >= len {
                return Some(top - len);
            }
            top = top.min(start);
            if top <= MMAP_BASE {
                return None;
            }
        }
        (top >= MMAP_BASE + len).then(|| top - len)
    }

    pub fn munmap(&mut self, addr: u64, len: u64) -> Result<(), MemError> {
        let end = self.check_range(addr, len)?;
        self.remove_range(addr, end);
        Ok(())
    }

    /// Change the rights on [addr, addr + len), which must all be in regions.
    pub fn mprotect(&mut self, addr: u64, len: u64, prot: Prot) -> Result<(), MemError> {
        let end = self.check_range(addr, len)?;
        let mut at = addr;
        while at < end {
            let (_, r) = self.region_at(at).ok_or(MemError::NoMemory)?;
            at = r.end;
        }
        for s in self.isolate(addr, end) {
            self.regions.get_mut(&s).unwrap().prot = prot;
        }
        let flags = vmm::leaf_flags(prot.page_flags());
        vmm::update_range(self.pml4(), addr, end, |_, entry| *entry = vmm::entry_frame(*entry) | flags);
        Ok(())
    }

    /// Validate a munmap/mprotect range; returns its page-aligned end.
    fn check_range(&self, addr: u64, len: u64) -> Result<u64, MemError> {
        // Bound before aligning: STACK_TOP is page-aligned, so this can't overflow.
        let end = addr.checked_add(len).filter(|&e| e <= STACK_TOP).ok_or(MemError::Invalid)?;
        if addr % PAGE_SIZE != 0 || len == 0 {
            return Err(MemError::Invalid);
        }
        Ok(align_up(end, PAGE_SIZE))
    }
}
