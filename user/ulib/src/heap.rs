//! The global allocator: a linked-list heap on memory from `brk`, grown on
//! demand (the kernel maps the pages when they are first touched).
//!
//! Processes are single-threaded, so the heap needs no lock; threads would
//! need one here. When memory runs out, allocation fails and Rust's default
//! handler panics ("memory allocation of N bytes failed").

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{null_mut, NonNull};
use linked_list_allocator::Heap;

/// Grow by at least this much at a time, so brk isn't called per allocation.
const MIN_GROWTH: usize = 256 * 1024;

struct State {
    heap: Heap,
    /// End of the memory the heap manages (0 until first use).
    top: usize,
}

pub struct UserHeap(UnsafeCell<State>);

// Single-threaded process: see the module comment.
unsafe impl Sync for UserHeap {}

#[global_allocator]
static HEAP: UserHeap = UserHeap(UnsafeCell::new(State { heap: Heap::empty(), top: 0 }));

impl State {
    /// Ask the kernel for at least `at_least` more bytes after `top`.
    fn grow(&mut self, at_least: usize) -> bool {
        let size = at_least.max(MIN_GROWTH).next_multiple_of(4096);
        let start = if self.top == 0 { crate::brk(0) as usize } else { self.top };
        let Some(end) = start.checked_add(size) else { return false };
        if crate::brk(end as u64) as usize != end {
            return false;
        }
        unsafe {
            if self.top == 0 {
                self.heap.init(start as *mut u8, size);
            } else {
                self.heap.extend(size);
            }
        }
        self.top = end;
        true
    }
}

unsafe impl GlobalAlloc for UserHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let st = &mut *self.0.get();
        if let Ok(p) = st.heap.allocate_first_fit(layout) {
            return p.as_ptr();
        }
        // Room for the block, its alignment and the allocator's bookkeeping.
        if st.grow(layout.size() + layout.align() + 64) {
            if let Ok(p) = st.heap.allocate_first_fit(layout) {
                return p.as_ptr();
            }
        }
        null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        (*self.0.get()).heap.deallocate(NonNull::new_unchecked(ptr), layout);
    }
}
