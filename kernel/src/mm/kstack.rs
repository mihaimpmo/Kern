//! Kernel stacks for tasks. Each stack sits in its own 64 KiB slot with the
//! bottom of the slot left unmapped, so a stack overflow hits a guard page and
//! causes a clean double fault instead of silently corrupting memory.

use super::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use alloc::vec::Vec;

const BASE: u64 = 0xffff_e000_0000_0000;
const SLOT_SIZE: u64 = 64 * 1024;
pub const STACK_SIZE: u64 = 32 * 1024;

struct Slots {
    next: u64,
    free: Vec<u64>,
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots { next: 0, free: Vec::new() });

pub struct KernelStack {
    slot: u64,
}

impl KernelStack {
    pub fn new() -> Option<Self> {
        let slot = {
            let mut s = SLOTS.lock();
            match s.free.pop() {
                Some(slot) => slot,
                None => {
                    s.next += 1;
                    s.next - 1
                }
            }
        };
        let stack = KernelStack { slot };
        let bottom = stack.top() - STACK_SIZE;
        let mut page = bottom;
        while page < stack.top() {
            let frame = pmm::alloc_frame()?; // on failure, Drop unmaps what we mapped
            let f = vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE | vmm::flags::GLOBAL;
            if vmm::map(vmm::kernel_pml4(), page, frame, f).is_err() {
                pmm::free_frame(frame);
                return None;
            }
            page += PAGE_SIZE;
        }
        Some(stack)
    }

    pub fn top(&self) -> u64 {
        BASE + (self.slot + 1) * SLOT_SIZE
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let mut page = self.top() - STACK_SIZE;
        while page < self.top() {
            if let Some(frame) = vmm::unmap(vmm::kernel_pml4(), page) {
                pmm::free_frame(frame);
            }
            page += PAGE_SIZE;
        }
        SLOTS.lock().free.push(self.slot);
    }
}
