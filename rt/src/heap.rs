//! The user heap (S4-SPEC section 9): a `#[global_allocator]` over `linked_list_allocator` that grows by mapping **arenas**, each its own
//! VMO and its own `Heap`, into one reserved range, and gives an arena back (`process_unmap`, which frees its pages and credits the job) when
//! nothing in it is live any more (T207 PR 5, MM6). Memory is committed when an arena is made, so exhaustion (the job's page limit, the frames, the
//! cap) is a null return from the allocator and never a fault on first touch; and what a trim gave back is simply made again by the next growth,
//! committed before it is handed out. The arena that empties last is kept (up to [`KEEP_MAX`] bytes) so that a loop of allocate and free does not
//! make and unmake a VMO every time. Growth is capped at [`HEAP_LIMIT`], and failure is a null return (the caller sees an allocation error),
//! never a hang.

extern crate alloc;

use crate::sys;
use alloc::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use linked_list_allocator::Heap;

/// Start of the reserved range (below `STACK_TOP`, away from the program image).
pub const HEAP_BASE: u64 = 0x08_0000_0000;
/// Largest heap: 128 MiB of address space (committed only as touched, and charged to the job). Lua's
/// stack-overflow test alone needs about 70 MiB here (S7e).
pub const HEAP_LIMIT: usize = 128 * 1024 * 1024;
/// Growth granule: 256 KiB (64 pages, committed lazily).
const CHUNK: usize = 256 * 1024;

const PROT_RW: u64 = 1 | 2;

/// An arena: `len` bytes at `HEAP_BASE`-relative offset `at`, with its own allocator.
struct Arena {
    at: usize,
    len: usize,
    heap: Heap,
}

/// Most arenas at once (the range over the smallest arena).
const MAX_ARENAS: usize = HEAP_LIMIT / CHUNK;
/// An emptied arena up to this size is kept, one at a time, for the next allocation; a bigger one goes back at once.
const KEEP_MAX: usize = 4 * CHUNK;

struct State {
    arenas: [Option<Arena>; MAX_ARENAS],
    /// Bytes mapped now, in all arenas.
    mapped: usize,
    /// Where the range starts (0 until the first growth): `HEAP_BASE` for a program linked at a fixed address, else a place in the 4 GiB window that
    /// starts there, derived from where the kernel put this process's stack and image (HD2).
    base: u64,
    /// Our `process_self` handle, once fetched (0: not yet).
    me: u64,
    /// The arena that served the last allocation (tried first).
    hint: usize,
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
                arenas: [const { None }; MAX_ARENAS],
                mapped: 0,
                base: 0,
                me: 0,
                hint: 0,
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        while self
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Give the CPU to the holder (it may have been preempted inside `grow`).
            sys::thread_yield();
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

/// The heap's start (HD2). A fixed-address program (its stack is the kernel's default one, just below `STACK_TOP`) keeps `HEAP_BASE`. A position-independent one has
/// a stack and an image the kernel placed at random, so the heap starts at a page of the 4 GiB window above `HEAP_BASE` chosen by a hash of those two addresses:
/// the window lies below the mapping arena, so nothing the kernel places ANYWHERE can fall into the range the heap grows through.
fn pick_base() -> u64 {
    let probe = 0u8;
    let sp = core::ptr::addr_of!(probe) as u64;
    let top = abi::user::STACK_TOP;
    if sp <= top && sp >= top - (abi::user::STACK_PAGES + 1) * 4096 {
        return HEAP_BASE;
    }
    let image = core::ptr::addr_of!(HEAP) as u64;
    let mut x = (sp >> 12) ^ (image >> 12).rotate_left(23);
    x = (x ^ (x >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 32)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let slots = (0x1_0000_0000u64 - HEAP_LIMIT as u64) / 4096;
    HEAP_BASE + (x % slots) * 4096
}

impl State {
    /// The offset of the first gap of `by` bytes in the range, if there is one.
    fn find_gap(&self, by: usize) -> Option<usize> {
        let fits = |at: usize| {
            at + by <= HEAP_LIMIT
                && self
                    .arenas
                    .iter()
                    .flatten()
                    .all(|a| at + by <= a.at || a.at + a.len <= at)
        };
        if fits(0) {
            return Some(0);
        }
        self.arenas
            .iter()
            .flatten()
            .map(|a| a.at + a.len)
            .filter(|&at| fits(at))
            .min()
    }

    /// Make an arena of at least `want` bytes; false if the limit is reached or the kernel refuses. Its pages are committed before it is used.
    fn grow(&mut self, want: usize) -> bool {
        let by = want.max(CHUNK).div_ceil(4096) * 4096;
        let Some(slot) = self.arenas.iter().position(|a| a.is_none()) else {
            return false;
        };
        let Some(off) = self.find_gap(by) else {
            return false;
        };
        if self.me == 0 {
            let r = sys::process_self();
            if r.status != 0 {
                return false;
            }
            self.me = r.a1 as u64;
        }
        let v = sys::vmo_create((by / 4096) as u64);
        if v.status != 0 {
            return false;
        }
        let vmo = v.a1 as u64;
        // Commit the arena now, so that exhaustion (the job's page limit, or the frames) is a failed
        // allocation here and not a fault on first touch that ends the program (slice V).
        // (in slices: one call commits at most MAX_COMMIT_PAGES pages)
        let slice = abi::limits::MAX_COMMIT_PAGES * 4096;
        let mut done = 0u64;
        while done < by as u64 {
            let n = slice.min(by as u64 - done);
            if sys::vmo_commit(vmo, done, n).status != 0 {
                sys::handle_close(vmo); // (destroying it gives back what was committed)
                return false;
            }
            done += n;
        }
        if self.base == 0 {
            self.base = pick_base();
        }
        let at = self.base + off as u64;
        let m = sys::process_map(self.me, vmo, at, 0, by as u64, PROT_RW);
        // The mapping holds the memory; our handle is not needed any more.
        sys::handle_close(vmo);
        if m.status != 0 {
            return false;
        }
        let mut heap = Heap::empty();
        // SAFETY: the range [at, at + by) was just mapped read-write for this arena alone, and no other arena overlaps it.
        unsafe { heap.init(at as *mut u8, by) };
        self.arenas[slot] = Some(Arena {
            at: off,
            len: by,
            heap,
        });
        self.mapped += by;
        self.hint = slot;
        true
    }

    /// Give an arena back: unmap it (its VMO, held by the mapping alone, goes with it, and the job is credited).
    fn release(&mut self, slot: usize) {
        let Some(a) = self.arenas[slot].take() else {
            return;
        };
        if sys::process_unmap(self.me, self.base + a.at as u64).status != 0 {
            // (cannot happen: the range is ours; keep the arena rather than hand its memory out as free)
            self.arenas[slot] = Some(a);
            return;
        }
        self.mapped -= a.len;
    }

    /// The slot of the arena holding `p`.
    fn slot_of(&self, p: u64) -> Option<usize> {
        let off = p.checked_sub(self.base)? as usize;
        let near = |i: usize| {
            self.arenas[i]
                .as_ref()
                .is_some_and(|a| off >= a.at && off < a.at + a.len)
        };
        if near(self.hint) {
            return Some(self.hint);
        }
        (0..MAX_ARENAS).find(|&i| near(i))
    }

    fn alloc_in(&mut self, layout: Layout) -> Option<*mut u8> {
        let start = self.hint;
        for k in 0..MAX_ARENAS {
            let i = (start + k) % MAX_ARENAS;
            if let Some(a) = self.arenas[i].as_mut() {
                if a.heap.free() < layout.size() {
                    continue;
                }
                if let Ok(p) = a.heap.allocate_first_fit(layout) {
                    self.hint = i;
                    return Some(p.as_ptr());
                }
            }
        }
        None
    }

    /// After a free: an arena with nothing live goes back, except one small spare.
    fn trim(&mut self, slot: usize) {
        let Some(a) = self.arenas[slot].as_ref() else {
            return;
        };
        if a.heap.used() != 0 {
            return;
        }
        let spare = self
            .arenas
            .iter()
            .enumerate()
            .any(|(i, o)| i != slot && o.as_ref().is_some_and(|o| o.heap.used() == 0));
        if spare || a.len > KEEP_MAX {
            self.release(slot);
        }
    }
}

// SAFETY: allocations come from one linked-list heap guarded by a lock; memory is mapped
// before it is handed out; `dealloc` returns blocks to the same heap.
unsafe impl GlobalAlloc for UserHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|s| {
            if let Some(p) = s.alloc_in(layout) {
                return p;
            }
            // Headroom for the allocator's own bookkeeping and alignment padding.
            let need = layout.size() + layout.align() + 64;
            if !s.grow(need) {
                return core::ptr::null_mut();
            }
            s.alloc_in(layout).unwrap_or(core::ptr::null_mut())
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.with(|s| {
            if let Some(p) = core::ptr::NonNull::new(ptr) {
                if let Some(slot) = s.slot_of(ptr as u64) {
                    if let Some(a) = s.arenas[slot].as_mut() {
                        // SAFETY: `ptr` came from `alloc` with this layout (the GlobalAlloc contract), from this arena.
                        unsafe { a.heap.deallocate(p, layout) };
                    }
                    s.hint = slot;
                    s.trim(slot);
                }
            }
        })
    }
}

#[global_allocator]
pub static HEAP: UserHeap = UserHeap::new();
