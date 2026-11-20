//! The kernel heap (OD29, grown by slice T step 2, OD82): a free-list allocator over a fixed region of `.bss` (the
//! **boot heap**, 8 MiB: usable from the first instruction, before the frame database exists) plus a **grown
//! heap** in a window of virtual address space whose pages come from the frame database as the heap fills.
//!
//! Allocation tries the boot heap, then the grown one. It never grows by itself: the allocator runs with
//! interrupts off, from the timer interrupt and with or without the big kernel lock, and the frame database is
//! under the big lock. Growth happens at **safe points** (`service`, called with the big lock held at the tail
//! of every trap that takes the big lock and by the idle loop): when the free bytes fall below `LOW_WATER` the heap is extended
//! by `GROW_STEP`. A single syscall allocates far less than `LOW_WATER`, so an allocation cannot find both heaps
//! empty unless something allocates without bound (which the caps of OD50 and the job quotas forbid). The grown
//! heap does not shrink (the frames stay; T-SPEC revision).
//!
//! Frames come from the frame database like any kernel frame; leak checks add `frames_held` to the free count.
//! Interrupts are disabled around every operation: an interrupt that allocated while a thread held the lock would
//! spin for ever.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use crate::irq;
use abi::layout::{HEAP_WINDOW_BASE, HEAP_WINDOW_SIZE};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kcore::frame::PAGE_SIZE;
use kcore::pagetable::{PageTable, Perms};
use linked_list_allocator::Heap;

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;
/// Grow when fewer than this many bytes are free in all.
pub const LOW_WATER: usize = 4 * 1024 * 1024;
/// How much one growth adds (and the first growth, at boot, which makes the window's page-table slot exist).
pub const GROW_STEP: usize = 2 * 1024 * 1024;

struct Heaps {
    boot: Heap,
    grown: Heap,
}

/// The heap lock: a `klock` spinlock of class `Heap` (interrupts off while held; the order checker sees every take).
static HEAPS: klock::Spin<Heaps, crate::bkl::Machine> = klock::Spin::new(
    klock::Class::Heap,
    0,
    Heaps {
        boot: Heap::empty(),
        grown: Heap::empty(),
    },
);

fn with_heaps<R>(f: impl FnOnce(&mut Heaps) -> R) -> R {
    f(&mut HEAPS.lock())
}

/// Set when the free bytes in both heaps are below `LOW_WATER`: `service` reads it before taking any lock, so the
/// safe points cost one load when the heap is healthy.
static LOW: AtomicBool = AtomicBool::new(false);

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: free() of both heaps is at most their sizes (a few MiB)
fn note_free(h: &Heaps) {
    LOW.store(
        h.boot.free() + h.grown.free() < LOW_WATER,
        Ordering::Relaxed,
    );
}

/// Bytes of the window mapped so far (the grown heap's size).
static GROWN_BYTES: AtomicUsize = AtomicUsize::new(0);

struct IrqSafeHeap;

// SAFETY: forwards to `linked_list_allocator::Heap`, a correct allocator, under a spinlock with interrupts disabled
// (so the timer interrupt cannot re-enter it); `dealloc` returns a block to the heap it came from, told apart by address.
unsafe impl GlobalAlloc for IrqSafeHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        with_heaps(|h| {
            let p = match h.boot.allocate_first_fit(layout) {
                Ok(p) => p.as_ptr(),
                Err(()) => h
                    .grown
                    .allocate_first_fit(layout)
                    .map_or(core::ptr::null_mut(), |p| p.as_ptr()),
            };
            note_free(h);
            p
        })
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the window base plus its size is a kernel virtual address range, far below usize::MAX
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        with_heaps(|h| {
            let a = ptr as usize;
            let base = HEAP_WINDOW_BASE as usize;
            // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract; the block came from the heap whose
            // address range holds it.
            unsafe {
                if a >= base && a < base + HEAP_WINDOW_SIZE as usize {
                    h.grown
                        .deallocate(core::ptr::NonNull::new_unchecked(ptr), layout);
                } else {
                    h.boot
                        .deallocate(core::ptr::NonNull::new_unchecked(ptr), layout);
                }
            }
            note_free(h);
        });
    }
}

#[global_allocator]
static ALLOCATOR: IrqSafeHeap = IrqSafeHeap;

static mut HEAP_MEMORY: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Hand the boot heap region to the allocator. Call once, before anything allocates.
pub fn init() {
    with_heaps(|h| {
        // SAFETY: `HEAP_MEMORY` is a private static that nothing else touches; after this call it is used only
        // through the allocator. `init` is called once at boot, before the first allocation, as `Heap::init`
        // requires.
        unsafe {
            h.boot
                .init(addr_of_mut!(HEAP_MEMORY).cast::<u8>(), HEAP_SIZE)
        };
    });
}

/// Free bytes in both heaps (for leak checks in the self-tests).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: free() of both heaps is at most their sizes
pub fn free_bytes() -> usize {
    irq::without(|| with_heaps(|h| h.boot.free() + h.grown.free()))
}

/// Bytes of the grown heap (zero until the first growth).
pub fn grown_bytes() -> usize {
    GROWN_BYTES.load(Ordering::Relaxed)
}

/// Frames the grown heap holds (stack pages for leak checks): its pages plus the page-table frames under them (one
/// level-1 table for the window and one level-0 table per 2 MiB).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: page counts of a window of HEAP_WINDOW_SIZE bytes
pub fn frames_held() -> usize {
    let pages = GROWN_BYTES.load(Ordering::Relaxed) / PAGE_SIZE as usize;
    if pages == 0 {
        0
    } else {
        pages + 1 + pages.div_ceil(512)
    }
}

/// Add `bytes` (rounded up to pages) to the grown heap: take frames from the frame database, map them at the end
/// of the window, extend the heap. Needs the big kernel lock (frames and the kernel page table). Returns how many
/// bytes were added (0 when the window is full or no frames are left; nothing is left half done).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: `have` is at most HEAP_WINDOW_SIZE (checked into `room`), mapped <= room; the sums are inside the heap window
pub fn grow(bytes: usize) -> usize {
    debug_assert!(crate::bkl::held());
    let want = bytes.div_ceil(PAGE_SIZE as usize);
    let have = GROWN_BYTES.load(Ordering::Relaxed);
    let room = (HEAP_WINDOW_SIZE as usize - have) / PAGE_SIZE as usize;
    let pages = want.min(room);
    let k = crate::kmem::get();
    let mut pt = PageTable::from_root(k.aspace.table().root());
    let mut mapped = 0;
    while mapped < pages {
        let Some(f) = k.mm.db.alloc() else { break };
        let va = HEAP_WINDOW_BASE + (have + mapped * PAGE_SIZE as usize) as u64;
        if pt.map(&mut k.mm, va, f, Perms::RW).is_err() {
            let _ = k.mm.db.dec(f);
            break;
        }
        mapped += 1;
    }
    if mapped == 0 {
        return 0;
    }
    // (a mapping that was invalid is now valid: no remote flush is needed; this hart is the only one that can touch
    // the new pages until the allocator hands them out)
    crate::arch::tlb_flush_all();
    let added = mapped * PAGE_SIZE as usize;
    irq::without(|| {
        with_heaps(|h| {
            // SAFETY: the pages `[window + have, window + have + added)` were just mapped read-write, are
            // contiguous with the grown heap (or start it), and nothing else uses them.
            unsafe {
                if have == 0 {
                    h.grown.init(HEAP_WINDOW_BASE as *mut u8, added);
                } else {
                    h.grown.extend(added);
                }
            }
            note_free(h);
        });
    });
    GROWN_BYTES.store(have + added, Ordering::Relaxed);
    added
}

/// Does the heap want a safe point (is it below its low-water mark)? A lock-free check, so an idle hart can look without
/// taking the big kernel lock for nothing.
pub fn needs_service() -> bool {
    LOW.load(Ordering::Relaxed)
}

/// The safe point: with the big kernel lock held, make sure the heap has headroom (see the module comment).
pub fn service() {
    // (until the heap is back above the low-water mark, or nothing more can be added)
    while LOW.load(Ordering::Relaxed) && free_bytes() < LOW_WATER {
        if grow(GROW_STEP) == 0 {
            break;
        }
    }
}

/// Make the window's first mapping at boot (so its top-level page-table entry exists before the kernel half is
/// snapshotted, OD41) and give the grown heap its first bytes.
pub fn init_window() {
    irq::without(|| {
        let added = grow(GROW_STEP);
        assert!(added > 0, "memory for the first heap growth");
    });
}
