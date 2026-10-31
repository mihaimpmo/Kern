//! Kernel heap. Backed by `linked_list_allocator`; when it runs out, more
//! pages are mapped at the end of the heap region and the heap is extended.

use super::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use linked_list_allocator::Heap;

const HEAP_START: u64 = 0xffff_d000_0000_0000;
const HEAP_LIMIT: u64 = 0xffff_d040_0000_0000; // 256 GiB of address space
const INITIAL_SIZE: u64 = 1024 * 1024;
const MIN_GROWTH: u64 = 256 * 1024;

struct State {
    heap: Heap,
    top: u64,
}

pub struct KernelHeap(SpinLock<State>);

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap(SpinLock::new(State { heap: Heap::empty(), top: HEAP_START }));

fn map_range(start: u64, size: u64) -> bool {
    let mut page = start;
    while page < start + size {
        let Some(frame) = pmm::alloc_frame() else { return false };
        let f = vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE | vmm::flags::GLOBAL;
        if vmm::map(vmm::kernel_pml4(), page, frame, f).is_err() {
            pmm::free_frame(frame);
            return false;
        }
        page += PAGE_SIZE;
    }
    true
}

pub fn init() {
    let mut st = ALLOCATOR.0.lock();
    assert!(map_range(HEAP_START, INITIAL_SIZE), "cannot map initial kernel heap");
    unsafe { st.heap.init(HEAP_START as *mut u8, INITIAL_SIZE as usize) };
    st.top = HEAP_START + INITIAL_SIZE;
}

fn grow(st: &mut State, at_least: u64) -> bool {
    let size = super::align_up(at_least.max(MIN_GROWTH), PAGE_SIZE);
    if st.top + size > HEAP_LIMIT || !map_range(st.top, size) {
        return false;
    }
    unsafe { st.heap.extend(size as usize) };
    st.top += size;
    true
}

/// (used bytes, heap size)
#[allow(dead_code)]
pub fn stats() -> (usize, usize) {
    let st = ALLOCATOR.0.lock();
    (st.heap.used(), st.heap.size())
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut st = self.0.lock();
        if let Ok(p) = st.heap.allocate_first_fit(layout) {
            return p.as_ptr();
        }
        let needed = (layout.size() + layout.align() + 64) as u64;
        if grow(&mut st, needed) {
            if let Ok(p) = st.heap.allocate_first_fit(layout) {
                return p.as_ptr();
            }
        }
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock().heap.deallocate(NonNull::new_unchecked(ptr), layout);
    }
}
