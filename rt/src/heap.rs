//! The user heap (S4-SPEC section 9): a `#[global_allocator]` over `linked_list_allocator`
//! that grows by mapping fresh VMOs into one reserved, contiguous virtual range, so the
//! allocator always sees a single region. Growth is lazy (VMO pages are committed on first
//! touch), capped at [`HEAP_LIMIT`], and failure is a null return (the caller sees an
//! allocation error), never a hang.

extern crate alloc;

use crate::sys;
use alloc::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use linked_list_allocator::Heap;

/// Start of the reserved range (below `STACK_TOP`, away from the program image).
pub const HEAP_BASE: u64 = 0x08_0000_0000;
/// Largest heap: 16 MiB.
pub const HEAP_LIMIT: usize = 16 * 1024 * 1024;
/// Growth granule: 256 KiB (64 pages, committed lazily).
const CHUNK: usize = 256 * 1024;

const PROT_RW: u64 = 1 | 2;

struct State {
    heap: Heap,
    /// Bytes mapped so far (from `HEAP_BASE`).
    mapped: usize,
    /// Our `process_self` handle, once fetched (0: not yet).
    me: u32,
}

pub struct UserHeap {
    lock: AtomicBool,
    state: UnsafeCell<State>,
}

// SAFETY: every access to `state` holds `lock` (a spin lock below).
unsafe impl Sync for UserHeap {}

impl UserHeap {
    pub const fn new() -> UserHeap {
        UserHeap {
            lock: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                heap: Heap::empty(),
                mapped: 0,
                me: 0,
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        while self
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held, so no other reference to the state exists.
        let r = f(unsafe { &mut *self.state.get() });
        self.lock.store(false, Ordering::Release);
        r
    }

    /// Bytes the heap has mapped (for tests).
    pub fn mapped(&self) -> usize {
        self.with(|s| s.mapped)
    }
}

impl Default for UserHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// Map at least `want` more bytes at the top of the heap; false if the limit is reached
    /// or the kernel refuses.
    fn grow(&mut self, want: usize) -> bool {
        let by = want.max(CHUNK).div_ceil(4096) * 4096;
        if self.mapped + by > HEAP_LIMIT {
            return false;
        }
        if self.me == 0 {
            let r = sys::process_self();
            if r.status != 0 {
                return false;
            }
            self.me = r.a1 as u32;
        }
        let v = sys::vmo_create((by / 4096) as u64);
        if v.status != 0 {
            return false;
        }
        let vmo = v.a1 as u32;
        let at = HEAP_BASE + self.mapped as u64;
        let m = sys::process_map(self.me, vmo, at, 0, by as u64, PROT_RW);
        // The mapping holds the memory; our handle is not needed any more.
        sys::handle_close(vmo);
        if m.status != 0 {
            return false;
        }
        // SAFETY: the range [at, at + by) was just mapped read-write for this heap alone and
        // is contiguous with the heap's current top (or, the first time, its start).
        unsafe {
            if self.mapped == 0 {
                self.heap.init(at as *mut u8, by);
            } else {
                self.heap.extend(by);
            }
        }
        self.mapped += by;
        true
    }
}

// SAFETY: allocations come from one linked-list heap guarded by a lock; memory is mapped
// before it is handed out; `dealloc` returns blocks to the same heap.
unsafe impl GlobalAlloc for UserHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|s| {
            if let Ok(p) = s.heap.allocate_first_fit(layout) {
                return p.as_ptr();
            }
            // Headroom for the allocator's own bookkeeping and alignment padding.
            let need = layout.size() + layout.align() + 64;
            if !s.grow(need) {
                return core::ptr::null_mut();
            }
            match s.heap.allocate_first_fit(layout) {
                Ok(p) => p.as_ptr(),
                Err(()) => core::ptr::null_mut(),
            }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.with(|s| {
            if let Some(p) = core::ptr::NonNull::new(ptr) {
                // SAFETY: `ptr` came from `alloc` with this layout (the GlobalAlloc contract).
                unsafe { s.heap.deallocate(p, layout) };
            }
        })
    }
}

#[global_allocator]
pub static HEAP: UserHeap = UserHeap::new();
