//! Kernel stacks for tasks. Each stack sits in its own 64 KiB slot with the
//! bottom of the slot left unmapped, so a stack overflow hits a guard page and
//! causes a clean double fault instead of silently corrupting memory.
//!
//! Freed stacks go back to a pool with their pages still mapped. Unmapping
//! them would need a TLB shootdown: another CPU that ran the task may still
//! cache the old translation, and would write through it once the slot is
//! reused with fresh frames. The pool never shrinks, so its size is the
//! largest number of tasks that existed at once.

use super::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use alloc::vec::Vec;

const BASE: u64 = 0xffff_e000_0000_0000;
const SLOT_SIZE: u64 = 64 * 1024;
pub const STACK_SIZE: u64 = 32 * 1024;

struct Slots {
    next: u64,      // first slot never used
    free: Vec<u64>, // slots whose stack is mapped and unused
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots { next: 0, free: Vec::new() });

pub struct KernelStack {
    slot: u64,
}

impl KernelStack {
    pub fn new() -> Option<Self> {
        let fresh = {
            let mut slots = SLOTS.lock();
            match slots.free.pop() {
                Some(slot) => return Some(KernelStack { slot }),
                None => {
                    slots.next += 1;
                    slots.next - 1
                }
            }
        };
        let top = slot_top(fresh);
        let mut page = top - STACK_SIZE;
        while page < top {
            let mapped = pmm::alloc_frame().and_then(|frame| {
                let f = vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE | vmm::flags::GLOBAL;
                vmm::map(vmm::kernel_pml4(), page, frame, f).map_err(|_| pmm::free_frame(frame)).ok()
            });
            if mapped.is_none() {
                // Give back what was mapped so far; the slot itself stays used.
                let mut p = top - STACK_SIZE;
                while p < page {
                    pmm::free_frame(vmm::unmap(vmm::kernel_pml4(), p).expect("stack page vanished"));
                    p += PAGE_SIZE;
                }
                return None;
            }
            page += PAGE_SIZE;
        }
        Some(KernelStack { slot: fresh })
    }

    pub fn top(&self) -> u64 {
        slot_top(self.slot)
    }
}

fn slot_top(slot: u64) -> u64 {
    BASE + (slot + 1) * SLOT_SIZE
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        SLOTS.lock().free.push(self.slot);
    }
}
