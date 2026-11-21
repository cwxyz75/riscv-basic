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
//! **Small objects (MM4)** (up to 2048 bytes, aligned to at most 2048) come from a slab allocator (`kcore::slab`) behind per-hart
//! magazines: a hart's own allocation and free take no shared lock, a magazine refills from and drains to the slabs in batches under the
//! `HEAP` lock. The slabs' frames come from a **frame pool** that `service` keeps stocked at the safe points (under the big lock, the
//! frame database's lock; the allocator itself cannot take it, it runs from interrupts). A small request that cannot be served (the pool is
//! empty, or early boot before frames exist) falls back to the free-list heaps above, so the allocator never fails where it used to
//! succeed; `dealloc` tells the two apart by address (the slabs live in the direct map). Large requests stay on the free-list heaps
//! until the frame database has a lock of its own (L16): then the allocator can take frame runs inline and `linked_list_allocator` goes.
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
use abi::layout::{DIRECT_BASE, DIRECT_MAP_PHYS_LIMIT, HEAP_WINDOW_BASE, HEAP_WINDOW_SIZE};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kcore::frame::PAGE_SIZE;
use kcore::pagetable::{PageTable, Perms};
use kcore::slab::{self, FrameSource, Slab, SlabMem};
use linked_list_allocator::Heap;

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;
/// Grow when fewer than this many bytes are free in all.
pub const LOW_WATER: usize = 4 * 1024 * 1024;
/// How much one growth adds (and the first growth, at boot, which makes the window's page-table slot exist).
pub const GROW_STEP: usize = 2 * 1024 * 1024;

/// Frames the pool is stocked to at a safe point, below which it asks for a safe point, and above which it gives frames back.
pub const POOL_TARGET: usize = 64;
const POOL_LOW: usize = 16;
const POOL_HIGH: usize = 192;
/// Objects a magazine moves to or from the slabs at a time.
const MAGAZINE_BATCH: usize = 16;

struct Heaps {
    boot: Heap,
    grown: Heap,
    /// The slab core: the lists and counters; the slabs' headers are in the frames themselves.
    slab: Slab,
    /// The frame pool: free frames as a stack threaded through the frames themselves (the first word of each holds the next), by direct-map
    /// address.
    pool_head: usize,
    pool_count: usize,
}

/// The slab's view of memory: words in the direct map.
struct DirectMem;

impl SlabMem for DirectMem {
    fn read(&self, addr: u64) -> u64 {
        // SAFETY: the slab core reads only the headers of slab frames and the pool's link words, at addresses in frames this module owns in
        // the direct map (always mapped), 8-aligned.
        unsafe { core::ptr::read_volatile(addr as *const u64) }
    }
    fn write(&mut self, addr: u64, value: u64) {
        // SAFETY: as in `read`; nothing else uses the header and link words.
        unsafe { core::ptr::write_volatile(addr as *mut u64, value) }
    }
}

/// The slab's frames: the pool, taken under the `HEAP` lock.
struct Pool<'a> {
    head: &'a mut usize,
    count: &'a mut usize,
}

impl Pool<'_> {
    fn push(&mut self, frame: usize) {
        let mut m = DirectMem;
        m.write(frame as u64, *self.head as u64);
        *self.head = frame;
        *self.count = self.count.saturating_add(1);
    }
    fn pop(&mut self) -> Option<usize> {
        if *self.head == 0 {
            return None;
        }
        let frame = *self.head;
        *self.head = DirectMem.read(frame as u64) as usize;
        *self.count = self.count.saturating_sub(1);
        Some(frame)
    }
}

impl FrameSource for Pool<'_> {
    fn alloc_frame(&mut self) -> Option<u64> {
        self.pop().map(|f| f as u64)
    }
    fn free_frame(&mut self, addr: u64) {
        self.push(addr as usize);
    }
}

/// Is `addr` in the direct map, where the slabs live?
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the direct map's base plus its size is a kernel virtual address, far below usize::MAX
fn in_direct_map(addr: usize) -> bool {
    let base = DIRECT_BASE as usize;
    addr >= base && addr < base + DIRECT_MAP_PHYS_LIMIT as usize
}

/// The heap lock: a `klock` spinlock of class `Heap` (interrupts off while held; the order checker sees every take).
static HEAPS: klock::Spin<Heaps, crate::bkl::Machine> = klock::Spin::new(
    klock::Class::Heap,
    0,
    Heaps {
        boot: Heap::empty(),
        grown: Heap::empty(),
        slab: Slab::new(),
        pool_head: 0,
        pool_count: 0,
    },
);

fn with_heaps<R>(f: impl FnOnce(&mut Heaps) -> R) -> R {
    f(&mut HEAPS.lock())
}

/// Set when the free bytes in both heaps are below `LOW_WATER`: `service` reads it before taking any lock, so the
/// safe points cost one load when the heap is healthy.
static LOW: AtomicBool = AtomicBool::new(false);

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: free() of both heaps is at most their sizes (a few MiB)
/// Set once the frame database exists (`init_window`): before it the pool cannot be stocked, and its being empty is no reason to ask for a
/// safe point.
static POOL_READY: AtomicBool = AtomicBool::new(false);

fn note_free(h: &Heaps) {
    LOW.store(
        h.boot.free().saturating_add(h.grown.free()) < LOW_WATER
            || (h.pool_count < POOL_LOW && POOL_READY.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
}

/// Bytes of the window mapped so far (the grown heap's size).
static GROWN_BYTES: AtomicUsize = AtomicUsize::new(0);

struct IrqSafeHeap;

/// A small object from this hart's magazine, refilling it from the slabs when it is empty; `None` if the slabs cannot serve (no frame in
/// the pool): the caller falls back to the free-list heaps.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the batch is at most MAGAZINE_SIZE
fn small_alloc(class: usize) -> Option<*mut u8> {
    let was = irq::save_and_disable();
    let mag = &crate::percpu::this().magazines;
    // SAFETY: this hart's own magazines, interrupts off (the allocator is reached from interrupt handlers too, and they are masked here).
    let mut got = unsafe { mag.pop(class) };
    if got.is_none() {
        // refill: a batch from the slabs under the lock (the first object is for the caller)
        let mut first = None;
        with_heaps(|h| {
            let Heaps {
                slab,
                pool_head,
                pool_count,
                ..
            } = h;
            let mut pool = Pool {
                head: pool_head,
                count: pool_count,
            };
            for i in 0..MAGAZINE_BATCH {
                let Some(a) = slab.alloc(&mut DirectMem, &mut pool, class) else {
                    break;
                };
                if i == 0 {
                    first = Some(a as usize);
                } else {
                    // SAFETY: as above.
                    if !unsafe { mag.push(class, a as usize) } {
                        // (cannot happen: the magazine was empty) give it back rather than lose it
                        let _ = slab.free(&mut DirectMem, &mut pool, a);
                        break;
                    }
                }
            }
            note_free(h);
        });
        got = first;
    }
    irq::restore(was);
    got.map(|a| a as *mut u8)
}

/// Return a small object: into this hart's magazine, draining half of it to the slabs when it is full.
fn small_free(addr: usize) {
    let was = irq::save_and_disable();
    let mag = &crate::percpu::this().magazines;
    let class = slab::class_of_addr(&DirectMem, addr as u64);
    let Some(class) = class else {
        // (not a slab object: a stray free; the heap cannot do better than leak it)
        irq::restore(was);
        return;
    };
    // SAFETY: this hart's own magazines, interrupts off.
    if !unsafe { mag.push(class, addr) } {
        with_heaps(|h| {
            let Heaps {
                slab,
                pool_head,
                pool_count,
                ..
            } = h;
            let mut pool = Pool {
                head: pool_head,
                count: pool_count,
            };
            // drain a batch, and the object being freed with it
            for _ in 0..MAGAZINE_BATCH {
                // SAFETY: as above.
                let Some(a) = (unsafe { mag.pop(class) }) else {
                    break;
                };
                let _ = slab.free(&mut DirectMem, &mut pool, a as u64);
            }
            let _ = slab.free(&mut DirectMem, &mut pool, addr as u64);
            note_free(h);
        });
    }
    irq::restore(was);
}

// SAFETY: forwards to `linked_list_allocator::Heap`, a correct allocator, under a spinlock with interrupts disabled
// (so the timer interrupt cannot re-enter it); `dealloc` returns a block to the heap it came from, told apart by address. Small
// objects come from the slab allocator behind this hart's magazines (interrupts off), and go back there.
unsafe impl GlobalAlloc for IrqSafeHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(class) = slab::class_for(layout.size(), layout.align()) {
            if let Some(p) = small_alloc(class) {
                return p;
            }
        }
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
        if in_direct_map(ptr as usize) {
            small_free(ptr as usize);
            return;
        }
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
    let grown = if pages == 0 {
        0
    } else {
        pages + 1 + pages.div_ceil(512)
    };
    // (the slabs' frames and the pool's are the heap's too: counting them keeps `free_frames_accounted` constant while the heap caches)
    grown + irq::without(|| with_heaps(|h| h.slab.frames() + h.pool_count))
}

/// Bytes of small objects in use: the slabs' objects less those sitting in the harts' magazines (free, but cached). The leak oracles subtract
/// this too, so a small object that is never freed shows as a leak as the free-list heap's blocks do.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: class < CLASSES; the magazines hold only objects the slabs counted as in use, so the difference does not underflow; the sums are bounded by the memory the slabs hold
pub fn small_bytes_in_use() -> usize {
    irq::without(|| {
        with_heaps(|h| {
            let mut bytes = 0usize;
            for class in 0..slab::CLASSES {
                let cached: usize = (0..crate::percpu::MAX_HARTS)
                    .map(|i| crate::percpu::hart(i).magazines.len(class))
                    .sum();
                bytes += h.slab.objects_of(class).saturating_sub(cached) * slab::CLASS_SIZES[class];
            }
            bytes
        })
    })
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
    service_pool();
}

/// Stock the slabs' frame pool to `POOL_TARGET` frames from the frame database, and give frames back above `POOL_HIGH`. Needs the big kernel
/// lock (the frame database's).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a frame number is below 2^52, so its address in the direct map (a kernel virtual address) does not overflow, and a pool frame is a direct-map address, so subtracting the base cannot underflow
fn service_pool() {
    if !POOL_READY.load(Ordering::Relaxed) {
        return;
    }
    debug_assert!(crate::bkl::held());
    let k = crate::kmem::get();
    let count = || irq::without(|| with_heaps(|h| h.pool_count));
    while count() < POOL_TARGET {
        let Some(pfn) = k.mm.db.alloc() else { break };
        let frame = (DIRECT_BASE + (pfn << 12)) as usize;
        irq::without(|| {
            with_heaps(|h| {
                Pool {
                    head: &mut h.pool_head,
                    count: &mut h.pool_count,
                }
                .push(frame);
                note_free(h);
            })
        });
    }
    while count() > POOL_HIGH {
        let frame = irq::without(|| {
            with_heaps(|h| {
                let f = Pool {
                    head: &mut h.pool_head,
                    count: &mut h.pool_count,
                }
                .pop();
                note_free(h);
                f
            })
        });
        let Some(frame) = frame else { break };
        let pfn = (frame as u64 - DIRECT_BASE) >> 12;
        let _ = k.mm.db.dec(pfn);
    }
}

/// Make the window's first mapping at boot (so its top-level page-table entry exists before the kernel half is
/// snapshotted, OD41) and give the grown heap its first bytes.
pub fn init_window() {
    irq::without(|| {
        let added = grow(GROW_STEP);
        assert!(added > 0, "memory for the first heap growth");
        POOL_READY.store(true, Ordering::Relaxed);
        service_pool(); // (the slabs' first frames)
    });
}
