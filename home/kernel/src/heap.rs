//! The kernel heap: a free-list allocator over a fixed region of `.bss`, so the
//! interpreter's strings, variables and arrays can come and go (decision D10).

use core::ptr::addr_of_mut;
use linked_list_allocator::LockedHeap;

/// Heap size. QEMU's default RAM is 128 MiB, so this is comfortable.
pub const HEAP_SIZE: usize = 16 * 1024 * 1024;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

static mut HEAP_MEMORY: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Hand the heap region to the allocator. Call once, before anything allocates.
pub fn init() {
    // SAFETY: `HEAP_MEMORY` is a private static that nothing else touches; after
    // this call it is accessed only through the allocator. `init` is called once
    // at boot, before the first allocation, as `Heap::init` requires.
    unsafe {
        ALLOCATOR
            .lock()
            .init(addr_of_mut!(HEAP_MEMORY).cast::<u8>(), HEAP_SIZE);
    }
}
