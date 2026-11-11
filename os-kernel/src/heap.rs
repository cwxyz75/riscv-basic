//! The kernel heap: a free-list allocator over a fixed region of `.bss` (OD29).
//! Frames for user memory come from `kcore::frame`, not from here.

use core::ptr::addr_of_mut;
use linked_list_allocator::LockedHeap;

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

static mut HEAP_MEMORY: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Hand the heap region to the allocator. Call once, before anything allocates.
pub fn init() {
    // SAFETY: `HEAP_MEMORY` is a private static that nothing else touches; after
    // this call it is used only through the allocator. `init` is called once at
    // boot, before the first allocation, as `Heap::init` requires.
    unsafe {
        ALLOCATOR
            .lock()
            .init(addr_of_mut!(HEAP_MEMORY).cast::<u8>(), HEAP_SIZE);
    }
}
