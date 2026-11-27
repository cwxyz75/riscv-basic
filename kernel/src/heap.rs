//! The kernel heap (OD29, grown by slice T step 2, OD82): a free-list allocator over a fixed region of `.bss` (the
//! **boot heap**, 8 MiB: usable from the first instruction, before the frame database exists) plus a **grown
//! heap** in a window of virtual address space whose pages come from the frame database as the heap fills.
//!
//! Allocation tries the boot heap, then the grown one. It never grows by itself: the allocator runs with
//! interrupts off, from the timer interrupt and with or without the big kernel lock, and the frame database is
//! under the big lock and its own `FRAME` lock (L16). Growth happens at **safe points** (`service`, called with the big lock held at the tail
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
use abi::layout::{DIRECT_MAP_PHYS_LIMIT, HEAP_WINDOW_BASE, HEAP_WINDOW_SIZE};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kcore::frame::PAGE_SIZE;
use kcore::pagetable::Perms;
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
/// Objects a magazine moves to or from the slabs at a time: 16, fewer for the classes whose slab holds fewer (a 2048-byte object is half a
/// frame).
fn magazine_batch(class: usize) -> usize {
    slab::objects_per_slab(class).min(16)
}

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
    let base = crate::kmem::direct_base() as usize;
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

/// Set once the frame database exists (`init_window`): before it the pool cannot be stocked, and its being empty is no reason to ask for a
/// safe point.
static POOL_READY: AtomicBool = AtomicBool::new(false);

/// The pool's size, mirrored from the heap lock so a safe point can look without taking it (`service` runs at the tail of every trap).
static POOL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Set when a safe point found the frame database empty and could not stock the pool: the pool being low is then no reason to ask for another
/// safe point until a frame comes back to it.
static POOL_STARVED: AtomicBool = AtomicBool::new(false);

/// Frees the slab refused: a double free, a stray address, a misaligned one (each is a kernel bug; counted so a test can say so).
static SLAB_VIOLATIONS: AtomicUsize = AtomicUsize::new(0);

fn note_free(h: &Heaps) {
    POOL_COUNT.store(h.pool_count, Ordering::Relaxed);
    if h.pool_count >= POOL_LOW {
        POOL_STARVED.store(false, Ordering::Relaxed);
    }
    LOW.store(
        h.boot.free().saturating_add(h.grown.free()) < low_water()
            || (h.pool_count < POOL_LOW
                && POOL_READY.load(Ordering::Relaxed)
                && !POOL_STARVED.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
}

/// KT9, T177 class N: the free bytes class R (an allocation a call can be refused) may not take: a base plus a share for every thread that exists, since
/// what the allocations that cannot fail (the scheduler's queues, teardown) need grows with the population, and the population cannot grow once class R
/// is refused. An estimate, not a proof: a fragmented free list can still fail a large class-N request (a tidy item of T177).
const RESERVE_BASE: usize = 1024 * 1024;
const RESERVE_PER_THREAD: usize = 256;

/// Set when the machine is up (the harts have their records and the frame database exists): before it nothing is refused.
static RESERVE_ON: AtomicBool = AtomicBool::new(false);

/// Start refusing class R below the reserve (called once, after boot).
pub fn enable_reserve() {
    RESERVE_ON.store(true, Ordering::Release);
}

/// The reserve now.
pub fn reserve() -> usize {
    RESERVE_BASE.saturating_add(
        RESERVE_PER_THREAD.saturating_mul(crate::kstack::LIVE.load(Ordering::Relaxed)),
    )
}

/// The heap grows when fewer than this are free: `LOW_WATER` above the reserve, so growth still leads.
fn low_water() -> usize {
    LOW_WATER.saturating_add(reserve())
}

/// Is the allocation on this hart one that cannot be refused (class N)? Inside `nofail`, or while a scheduler lock is held (the order checker's stack
/// sees every holder; the scheduler needs no call-site marker). Interrupts are off.
fn is_class_n() -> bool {
    let me = crate::percpu::this();
    me.nofail_depth.load(Ordering::Relaxed) != 0
        || me
            .held
            .with(|h| h.holds(klock::Class::Tt) || h.holds(klock::Class::Rq))
}

/// Bytes of the window mapped so far (the grown heap's size).
static GROWN_BYTES: AtomicUsize = AtomicUsize::new(0);

struct IrqSafeHeap;

/// A small object from this hart's magazine, refilling it from the slabs when it is empty; `None` if the slabs cannot serve (no frame in
/// the pool): the caller falls back to the free-list heaps.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the batch is at most MAGAZINE_SIZE
fn small_alloc(class: usize) -> Option<*mut u8> {
    let was = irq::save_and_disable();
    crate::bkl::check_use(klock::Class::Heap); // (the order checker sees every allocation, not only the ones that reach the lock)
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
            for i in 0..magazine_batch(class) {
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

/// Give every object in this hart's magazines back to the slabs (a hart going offline, PM1).
pub fn return_magazines() {
    let was = irq::save_and_disable();
    crate::bkl::check_use(klock::Class::Heap);
    let mag = &crate::percpu::this().magazines;
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
        for class in 0..slab::CLASSES {
            // SAFETY: this hart's own magazines, interrupts off.
            while let Some(a) = unsafe { mag.pop(class) } {
                if slab.free(&mut DirectMem, &mut pool, a as u64).is_err() {
                    SLAB_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        note_free(h);
    });
    irq::restore(was);
}

/// Return a small object: into this hart's magazine, draining half of it to the slabs when it is full.
fn small_free(addr: usize) {
    let was = irq::save_and_disable();
    crate::bkl::check_use(klock::Class::Heap);
    let mag = &crate::percpu::this().magazines;
    let class = slab::class_of_addr(&DirectMem, addr as u64);
    let Some(class) = class else {
        // (not a slab object: a stray free; the heap cannot do better than leak it, and says so)
        SLAB_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
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
            for _ in 0..magazine_batch(class) {
                // SAFETY: as above.
                let Some(a) = (unsafe { mag.pop(class) }) else {
                    break;
                };
                if slab.free(&mut DirectMem, &mut pool, a as u64).is_err() {
                    SLAB_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                }
            }
            if slab.free(&mut DirectMem, &mut pool, addr as u64).is_err() {
                SLAB_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
            }
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
        #[cfg(feature = "fault-alloc")]
        if faultalloc::should_fail() {
            return core::ptr::null_mut();
        }
        if let Some(class) = slab::class_for(layout.size(), layout.align()) {
            if let Some(p) = small_alloc(class) {
                return p;
            }
        }
        let gated = RESERVE_ON.load(Ordering::Acquire) && {
            let was = irq::save_and_disable();
            let n = is_class_n();
            irq::restore(was);
            !n
        };
        with_heaps(|h| {
            if gated
                && h.boot
                    .free()
                    .saturating_add(h.grown.free())
                    .saturating_sub(layout.size())
                    < reserve()
            {
                return core::ptr::null_mut(); // (class R below the reserve: refused, the caller reports NoMemory)
            }
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

/// KT9, the guest half: fail the Nth allocation that one armed thread makes (once), so a program can sweep N over a scenario. An infallible allocation
/// on the path ends in the allocation-error handler, a panic the harness reports with the scenario that was running.
#[cfg(feature = "fault-alloc")]
pub mod faultalloc {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    static ARMED: AtomicU64 = AtomicU64::new(0); // the armed thread's id plus one
    static FAIL_AT: AtomicU64 = AtomicU64::new(0);
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static FIRED: AtomicBool = AtomicBool::new(false);
    /// Counting and failing are suspended (a scenario sets up and tears down with memory, and arms only the call under test: `debug_stat` 207).
    static PAUSED: AtomicBool = AtomicBool::new(false);
    /// Every class-R allocation from the Nth on fails, not just the Nth (what exhaustion with the reserve in force looks like, KT9).
    static PERSIST: AtomicBool = AtomicBool::new(false);

    /// `arm`, persistent.
    pub fn arm_persist(n: u64) {
        arm(n);
        PERSIST.store(true, Relaxed);
    }

    /// Arm for the calling thread: its `n`th allocation from now fails.
    pub fn arm(n: u64) {
        COUNT.store(0, Relaxed);
        FIRED.store(false, Relaxed);
        FAIL_AT.store(n, Relaxed);
        ARMED.store(crate::thread::current().saturating_add(1), Relaxed);
    }

    /// Suspend (`true`) or resume (`false`) the armed thread's counting and failing.
    pub fn pause(on: bool) {
        PAUSED.store(on, Relaxed);
    }

    /// Disarm; whether the failure was injected.
    pub fn disarm() -> bool {
        ARMED.store(0, Relaxed);
        PAUSED.store(false, Relaxed);
        PERSIST.store(false, Relaxed);
        FIRED.load(Relaxed)
    }

    /// Allocations the armed thread has made since `arm`.
    pub fn count() -> u64 {
        COUNT.load(Relaxed)
    }

    pub(super) fn should_fail() -> bool {
        let armed = ARMED.load(Relaxed);
        if armed == 0 || PAUSED.load(Relaxed) || crate::thread::current().saturating_add(1) != armed
        {
            return false;
        }
        // (class N is not refused by the reserve, so it is not failed here either: inside `nofail`, or with a scheduler lock held; not counted either, so
        // that N does not move with what the scheduler does)
        {
            let was = crate::irq::save_and_disable();
            let n = super::is_class_n();
            crate::irq::restore(was);
            if n {
                return false;
            }
        }
        let c = COUNT.fetch_add(1, Relaxed).saturating_add(1);
        if PERSIST.load(Relaxed) && c >= FAIL_AT.load(Relaxed) {
            FIRED.store(true, Relaxed);
            return true;
        }
        if c == FAIL_AT.load(Relaxed) {
            FIRED.store(true, Relaxed);
            // (where the failure was injected: `scripts/resolve_trace.py` turns the words into the call chain, so a panic that follows can be placed)
            {
                use core::fmt::Write as _;
                let _ = writeln!(
                    crate::console::PanicConsole::new(),
                    "alloc fail: n={c}{}",
                    crate::Trace
                );
            }
            return true;
        }
        false
    }
}

/// Run `f`, whose allocations cannot be refused because nobody could be told (KT9, T177 class N: the scheduler core, which allocates on every spawn,
/// sleep and wake, and teardown): they may use the allocator's reserve, which class R cannot (`reserve`), and the `fault-alloc` sweep does not fail them.
/// Each use is counted by the docs check
/// (`scripts/check_docs.py`), so the class cannot grow silently.
pub fn nofail<R>(f: impl FnOnce() -> R) -> R {
    // (the hart is the one the section starts on, and the count is put back on the same record: every use of `nofail` but the boot's is with interrupts
    // already off, under the object lock or a scheduler lock, so the thread does not move; where it could, the count only stays a little off on the hart
    // it started on, which is the lenient side)
    let me = crate::percpu::this();
    me.nofail_depth.fetch_add(1, Ordering::Relaxed);
    let r = f();
    me.nofail_depth.fetch_sub(1, Ordering::Relaxed);
    r
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
    with_heaps(|h| h.boot.free() + h.grown.free())
}

/// Bytes of the grown heap (zero until the first growth).
pub fn grown_bytes() -> usize {
    GROWN_BYTES.load(Ordering::Relaxed)
}

/// Frames the grown heap holds (stack pages for leak checks): its pages plus the page-table frames under them (one
/// level-1 table for the window and one level-0 table per 2 MiB).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: page counts of a window of HEAP_WINDOW_SIZE bytes
pub fn frames_held() -> usize {
    let grown = window_formula(GROWN_BYTES.load(Ordering::Relaxed));
    // (the slabs' frames and the pool's are the heap's too: counting them keeps `free_frames_accounted` constant while the heap caches;
    // a frame in hand between the pool and this hart's frame cache is counted by `IN_TRANSIT`, T227)
    grown + with_heaps(|h| h.slab.frames() + h.pool_count) + IN_TRANSIT.load(Ordering::Acquire)
}

/// The frames a grown heap of `bytes` holds in its window: its pages, plus the page-table frames under them (one level-1 table for the
/// window and one level-0 table per 2 MiB). The arithmetic is `kcore::kwindow::heap_window`, proved by Kani against a page-by-page count.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a division by the page size, a non-zero constant
fn window_formula(bytes: usize) -> usize {
    usize::try_from(kcore::kwindow::heap_window(
        (bytes / PAGE_SIZE as usize) as u64,
    ))
    .unwrap_or(usize::MAX)
}

/// T227's walk oracle for the heap window: `(walked, formula)`, the frames the kernel's table holds under the window (tables and leaves)
/// and what [`frames_held`] derives from the grown size, read under the kernel table's lock (which `grow` holds while it maps and sets the
/// size). Equal unless a path maps or leaves a frame there that the formula does not count.
#[cfg(any(not(feature = "release"), feature = "release-suite"))] // (the leak checks of `user_tests` and the boot self-tests)
pub fn window_frames_walked() -> (usize, usize) {
    let _kpt = crate::kmem::KPT.lock();
    let k = crate::kmem::frames();
    let walked = crate::kmem::kernel_table()
        .frames_in_range(
            k.mm,
            HEAP_WINDOW_BASE,
            HEAP_WINDOW_BASE.wrapping_add(HEAP_WINDOW_SIZE),
        )
        .total();
    (
        usize::try_from(walked).unwrap_or(usize::MAX),
        window_formula(GROWN_BYTES.load(Ordering::Relaxed)),
    )
}

/// Frames the pool's rebalance holds in hand (taken from this hart's frame cache and not yet pushed, or popped and not yet given back):
/// a term of `frames_held`, so the leak oracle's sum does not dip while one is in flight; 0 at a quiet time (T227).
static IN_TRANSIT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// [`IN_TRANSIT`], for a leak report.
#[cfg(any(not(feature = "release"), feature = "release-suite"))] // (the leak checks of `user_tests` and the boot self-tests)
pub fn in_transit() -> usize {
    IN_TRANSIT.load(Ordering::Acquire)
}

/// Add `bytes` (rounded up to pages) to the grown heap: take frames from the frame database, map them at the end
/// of the window, extend the heap. Takes the kernel page table's lock (`kmem::KPT`) and the frame lock, in that order (T224). Returns how many
/// bytes were added (0 when the window is full or no frames are left; nothing is left half done).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: `have` is at most HEAP_WINDOW_SIZE (checked into `room`), mapped <= room; the sums are inside the heap window
pub fn grow(bytes: usize) -> usize {
    // (the kernel page table's edits are one at a time, and so is the growth of the window: `have` is read and written under the lock; T224)
    let _kpt = crate::kmem::KPT.lock();
    let want = bytes.div_ceil(PAGE_SIZE as usize);
    let have = GROWN_BYTES.load(Ordering::Relaxed);
    let room = (HEAP_WINDOW_SIZE as usize - have) / PAGE_SIZE as usize;
    let pages = want.min(room);
    let k = crate::kmem::frames();
    let mut pt = crate::kmem::kernel_table();
    let mut mapped = 0;
    while mapped < pages {
        let Some(f) = k.mm.db.alloc() else { break };
        let va = HEAP_WINDOW_BASE + (have + mapped * PAGE_SIZE as usize) as u64;
        if pt.map(k.mm, va, f, Perms::RW).is_err() {
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
    // (a hart that cached the entry while it was invalid and is handed the memory later takes a fault the kernel's trap handler answers with a local flush: T241)
    crate::arch::tlb_flush_all();
    let added = mapped * PAGE_SIZE as usize;
    // (the big kernel lock is held and interrupts are off; `irq::without` would take the BKL again, after the memory locks `k` holds)
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
    while LOW.load(Ordering::Relaxed) && free_bytes() < low_water() {
        if grow(GROW_STEP) == 0 {
            break;
        }
    }
    service_pool();
}

/// Stock the slabs' frame pool to `POOL_TARGET` frames from the frame database, and give frames back above `POOL_HIGH`. Needs the big kernel
/// lock (the frame database's).
fn service_pool() {
    if !POOL_READY.load(Ordering::Relaxed) {
        return;
    }
    // (one load when the pool is between its marks: the safe points are on every trap's tail)
    let c = POOL_COUNT.load(Ordering::Relaxed);
    if (POOL_LOW..=POOL_HIGH).contains(&c) {
        return;
    }
    rebalance_pool();
}

/// Bring the pool to `POOL_TARGET`: stock it from this hart's frame cache, or give the surplus back to it.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a frame number is below 2^52, so its address in the direct map (a kernel virtual address) does not overflow, and a pool frame is a direct-map address, so subtracting the base cannot underflow
fn rebalance_pool() {
    // (frames come from this hart's cache, which takes `FRAME` only to refill or drain a batch)
    let count = || with_heaps(|h| h.pool_count);
    // (T227: a frame in hand is counted by `IN_TRANSIT` from before it leaves one count to after it enters the other: the increment comes
    // before the take and the decrement after the put, so the oracle's sum may read one frame high for an instant, never one low)
    while count() < POOL_TARGET {
        IN_TRANSIT.fetch_add(1, Ordering::AcqRel);
        let Some(pfn) = crate::kmem::frame_alloc() else {
            IN_TRANSIT.fetch_sub(1, Ordering::AcqRel);
            POOL_STARVED.store(true, Ordering::Relaxed);
            break;
        };
        let frame = (crate::kmem::direct_base() + (pfn << 12)) as usize;
        with_heaps(|h| {
            Pool {
                head: &mut h.pool_head,
                count: &mut h.pool_count,
            }
            .push(frame);
            note_free(h);
        });
        IN_TRANSIT.fetch_sub(1, Ordering::AcqRel);
    }
    while count() > POOL_TARGET {
        IN_TRANSIT.fetch_add(1, Ordering::AcqRel);
        let frame = with_heaps(|h| {
            let f = Pool {
                head: &mut h.pool_head,
                count: &mut h.pool_count,
            }
            .pop();
            note_free(h);
            f
        });
        let Some(frame) = frame else {
            IN_TRANSIT.fetch_sub(1, Ordering::AcqRel);
            break;
        };
        let pfn = (frame as u64 - crate::kmem::direct_base()) >> 12;
        crate::kmem::frame_free_sole(pfn);
        IN_TRANSIT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Make the window's first mapping at boot (so its top-level page-table entry exists before the kernel half is
/// snapshotted, OD41) and give the grown heap its first bytes.
pub fn init_window() {
    irq::without(|| {
        let added = grow(GROW_STEP);
        assert!(added > 0, "memory for the first heap growth");
        POOL_READY.store(true, Ordering::Relaxed);
        rebalance_pool(); // (the slabs' first frames)
    });
}

/// The leak oracle in one take of the heap lock: the free-list heaps' free bytes, less the grown heap's size (growth at a safe point adds free
/// bytes that are not a leak), less the small objects in use. Wrapping, so the value is only for comparing with another taken at a quiet time
/// (an equal pair means nothing was kept).
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: class < CLASSES; the sums are bounded by the memory the heaps hold, and the arithmetic that could wrap is wrapping on purpose
pub fn leak_oracle() -> usize {
    {
        with_heaps(|h| {
            let mut small = 0usize;
            for class in 0..slab::CLASSES {
                let cached: usize = (0..crate::percpu::MAX_HARTS)
                    .map(|i| crate::percpu::hart(i).magazines.len(class))
                    .sum();
                small += h.slab.objects_of(class).saturating_sub(cached) * slab::CLASS_SIZES[class];
            }
            h.boot
                .free()
                .wrapping_add(h.grown.free())
                .wrapping_sub(GROWN_BYTES.load(Ordering::Relaxed))
                .wrapping_sub(small)
        })
    }
}

/// How many frees the slab refused so far (a double free, a stray or misaligned address): zero unless the kernel has a bug.
pub fn slab_violations() -> usize {
    SLAB_VIOLATIONS.load(Ordering::Relaxed)
}
