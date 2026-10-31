//! Physical memory manager: a free list of 4 KiB frames.
//! Each free frame stores the physical address of the next free frame in its
//! first 8 bytes (accessed through the HHDM), so the allocator needs no
//! extra memory and allocation/free are O(1).

use super::{phys_to_virt, PAGE_SIZE};
use crate::boot::{memmap_type, MemmapResponse};
use crate::sync::SpinLock;

struct FreeList {
    head: u64, // 0 = empty
    free: u64,
    total: u64,
}

static FRAMES: SpinLock<FreeList> = SpinLock::new(FreeList { head: 0, free: 0, total: 0 });

pub fn init(memmap: &MemmapResponse) {
    let mut list = FRAMES.lock();
    for entry in memmap.entries() {
        if entry.kind != memmap_type::USABLE {
            continue;
        }
        let start = super::align_up(entry.base, PAGE_SIZE);
        let end = super::align_down(entry.base + entry.length, PAGE_SIZE);
        let mut frame = start;
        while frame < end {
            if frame != 0 {
                unsafe { *(phys_to_virt(frame) as *mut u64) = list.head };
                list.head = frame;
                list.free += 1;
                list.total += 1;
            }
            frame += PAGE_SIZE;
        }
    }
}

/// Allocate one frame (contents undefined).
pub fn alloc_frame() -> Option<u64> {
    let mut list = FRAMES.lock();
    let frame = list.head;
    if frame == 0 {
        return None;
    }
    list.head = unsafe { *(phys_to_virt(frame) as *const u64) };
    list.free -= 1;
    Some(frame)
}

/// Allocate one frame filled with zeros.
pub fn alloc_zeroed_frame() -> Option<u64> {
    let frame = alloc_frame()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE as usize) };
    Some(frame)
}

pub fn free_frame(frame: u64) {
    debug_assert!(frame % PAGE_SIZE == 0 && frame != 0);
    let mut list = FRAMES.lock();
    unsafe { *(phys_to_virt(frame) as *mut u64) = list.head };
    list.head = frame;
    list.free += 1;
}

/// (free bytes, total bytes)
pub fn stats() -> (u64, u64) {
    let list = FRAMES.lock();
    (list.free * PAGE_SIZE, list.total * PAGE_SIZE)
}
