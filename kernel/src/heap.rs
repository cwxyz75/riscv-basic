//! The kernel heap: a free-list allocator over a fixed region of `.bss` (OD29).
//! Frames for user memory come from `kcore::frame`, not from here.
//!
//! Allocation can happen in the timer interrupt (the scheduler's tick), so every
//! operation runs with interrupts disabled: otherwise an interrupt arriving while a
//! thread holds the heap lock would try to take it again and spin forever.

use crate::irq;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::addr_of_mut;
use linked_list_allocator::LockedHeap;

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

struct IrqSafeHeap(LockedHeap);

// SAFETY: forwards to `LockedHeap`, which is a correct allocator; disabling
// interrupts around each call only removes re-entrancy from the timer interrupt.
unsafe impl GlobalAlloc for IrqSafeHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let was = irq::save_and_disable();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.0.alloc(layout) };
        irq::restore(was);
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let was = irq::save_and_disable();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { self.0.dealloc(ptr, layout) };
        irq::restore(was);
    }
}

#[global_allocator]
static ALLOCATOR: IrqSafeHeap = IrqSafeHeap(LockedHeap::empty());

static mut HEAP_MEMORY: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Hand the heap region to the allocator. Call once, before anything allocates.
pub fn init() {
    // SAFETY: `HEAP_MEMORY` is a private static that nothing else touches; after
    // this call it is used only through the allocator. `init` is called once at
    // boot, before the first allocation, as `Heap::init` requires.
    unsafe {
        ALLOCATOR
            .0
            .lock()
            .init(addr_of_mut!(HEAP_MEMORY).cast::<u8>(), HEAP_SIZE);
    }
}

/// Free bytes in the heap (for leak checks in the self-tests).
pub fn free_bytes() -> usize {
    irq::without(|| ALLOCATOR.0.lock().free())
}
