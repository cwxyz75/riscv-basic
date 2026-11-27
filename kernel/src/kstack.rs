//! Kernel stacks from frames (slice T, T-SPEC.md section 4). A kernel thread's stack is four frames
//! mapped into a window of its own (`abi::layout::KSTACK_WINDOW_BASE`), at the top of a 32 KiB slot whose
//! lower half stays **unmapped**: a stack that overflows runs into the guard and faults, and the trap
//! entry (`trap.rs`) recognises that and stops with a report on a per-hart emergency stack instead of
//! corrupting a neighbour. Stacks no longer come out of the kernel heap.
//!
//! A slot keeps its frames and its mapping for ever once made: freeing a stack puts the slot on a free
//! list, so a stale translation on another hart can never point a reused slot at the wrong frame, and
//! creating a thread after the first few costs no frame traffic. (Returning frames needs a shootdown;
//! a later step can add it.) The first slot is made at boot, so the window's top-level page-table entry
//! exists before the kernel half is snapshotted (OD41).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use abi::layout::{KSTACK_BYTES, KSTACK_SLOT_BYTES, KSTACK_WINDOW_BASE, KSTACK_WINDOW_SIZE};
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use kcore::frame::PAGE_SIZE;
use kcore::pagetable::Perms;

const PAGES: u64 = KSTACK_BYTES / PAGE_SIZE;
/// How many slots the window holds.
pub const MAX_SLOTS: u32 = (KSTACK_WINDOW_SIZE / KSTACK_SLOT_BYTES) as u32;

struct Slots {
    /// The next slot never used.
    next: u32,
    /// Slots whose stacks are free (their frames stay mapped).
    free: Vec<u32>,
}

/// The slot allocator, under its own lock (class `KSTACK`, instance 0; the kernel page table's lock, `kmem::KPT`, is instance 1 and comes after it).
static SLOTS: klock::Spin<Slots, crate::bkl::Machine> = klock::Spin::new(
    klock::Class::Kstack,
    0,
    Slots {
        next: 0,
        free: Vec::new(),
    },
);

/// The slot state.
fn slots() -> klock::Guard<'static, Slots, crate::bkl::Machine> {
    SLOTS.lock()
}

/// One kernel stack: a slot of the window. Dereferences to the stack's words (lowest address first).
pub struct KStack {
    slot: u32,
}

impl KStack {
    /// The lowest address of the usable stack (just above the guard).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: slot_base is inside the kernel-stack window, so adding a stack's size stays a kernel address
    pub fn bottom(&self) -> usize {
        slot_base(self.slot) as usize + KSTACK_BYTES as usize
    }

    /// One past the highest address of the stack: where the stack pointer starts.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: slot_base is inside the kernel-stack window, so adding a stack's size stays a kernel address
    pub fn top(&self) -> usize {
        self.bottom() + KSTACK_BYTES as usize
    }
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: slot < the window's slot count (the allocator hands out indices below it)
fn slot_base(slot: u32) -> u64 {
    KSTACK_WINDOW_BASE + u64::from(slot) * KSTACK_SLOT_BYTES
}

impl Deref for KStack {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        // SAFETY: the slot's four stack pages are mapped read-write for as long as the kernel runs (slots are never unmapped) and the pointer is page-aligned, so the
        // reads are valid. Exclusive ownership of the contents does NOT hold while a thread runs on this stack: `stacks_intact`, the switch path and `current_stack_use`
        // form this slice while the thread (or another hart's thread) writes the live frames; only the guard words and the words below the live frames are
        // read in those uses, which is harmless in practice and not sound by the letter of the aliasing rules (T242).
        unsafe {
            core::slice::from_raw_parts(self.bottom() as *const u64, KSTACK_BYTES as usize / 8)
        }
    }
}

impl DerefMut for KStack {
    fn deref_mut(&mut self) -> &mut [u64] {
        // SAFETY: as in `deref`; `&mut self` makes the access exclusive.
        unsafe {
            core::slice::from_raw_parts_mut(self.bottom() as *mut u64, KSTACK_BYTES as usize / 8)
        }
    }
}

impl Drop for KStack {
    fn drop(&mut self) {
        let slot = self.slot;
        LIVE.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        slots().free.push(slot);
    }
}

/// Make a slot with frames: map the four stack pages of the next unused slot. `None` if the window is
/// full or no frames (or page-table frames) are left; nothing is changed then.
#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// INVARIANT: i < PAGES indexes the frames array of PAGES entries; the addresses are inside the kernel-stack window; the slot counter is far below u32::MAX
fn make_slot(s: &mut Slots) -> Option<u32> {
    if s.next >= MAX_SLOTS {
        return None;
    }
    let slot = s.next;
    let _kpt = crate::kmem::KPT.lock();
    let k = crate::kmem::frames();
    let mut frames = [0u64; PAGES as usize];
    for i in 0..frames.len() {
        match k.mm.db.alloc() {
            Some(f) => frames[i] = f,
            None => {
                for &f in &frames[..i] {
                    let _ = k.mm.db.dec(f);
                }
                return None;
            }
        }
    }
    let mut pt = crate::kmem::kernel_table();
    let top = slot_base(slot) + KSTACK_SLOT_BYTES;
    for (i, &f) in frames.iter().enumerate() {
        let va = top - (PAGES - i as u64) * PAGE_SIZE;
        if pt.map(k.mm, va, f, Perms::RW).is_err() {
            // undo this slot's mappings so far, and give every frame back
            for j in 0..i {
                let _ = pt.unmap(k.mm, top - (PAGES - j as u64) * PAGE_SIZE);
            }
            for &f in &frames {
                let _ = k.mm.db.dec(f);
            }
            return None;
        }
    }
    // (a mapping that was invalid is now valid: no remote flush is needed for that, and this hart is the
    // only one that can touch the slot until the stack is handed out)
    // (a hart that cached the entry while it was invalid and is handed the memory later takes a fault the kernel's trap handler answers with a local flush: T241)
    crate::arch::tlb_flush_all();
    s.next += 1;
    Some(slot)
}

/// How many kernel stacks are in use (one per thread): the allocator's reserve for the allocations that cannot be refused grows with it (KT9, `heap::reserve`).
pub static LIVE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A zeroed kernel stack, or `None` if there is no slot or no memory.
pub fn alloc() -> Option<KStack> {
    let slot = {
        let mut s = slots();
        match s.free.pop() {
            Some(x) => x,
            None => make_slot(&mut s)?,
        }
    };
    LIVE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut stack = KStack { slot };
    stack.fill(0); // a recycled stack starts clean (and `thread::stack_high_water` relies on it)
    Some(stack)
}

/// Make the first slot now (boot, before the kernel half is snapshotted), so the window's top-level
/// page-table entry exists; the slot goes on the free list.
#[allow(clippy::expect_used)] // INVARIANT: boot code: the first slot must exist for the boot thread to get a stack
pub fn init() {
    let mut s = slots();
    let slot = make_slot(&mut s).expect("memory for the first kernel stack slot");
    s.free.push(slot);
}

/// Frames held by slots (made and kept): leak checks add this to the free count.
pub fn frames_held() -> usize {
    formula(slots().next as usize)
}

/// The frames `made` slots hold: the stack pages, plus the page-table frames under them: one level-1 table for the window and one level-0
/// table per 64 slots (2 MiB of window). The arithmetic is `kcore::kwindow::stack_window`, proved by Kani against a page-by-page count.
fn formula(made: usize) -> usize {
    usize::try_from(kcore::kwindow::stack_window(
        made as u64,
        PAGES,
        KSTACK_SLOT_BYTES,
    ))
    .unwrap_or(usize::MAX)
}

/// T227's walk oracle for the stack window: `(walked, formula)`, the frames the kernel's table holds under the window (its tables and its
/// leaves, `PageTable::frames_in_range`) and what [`frames_held`] derives from the slot counter, read under the slot lock and the kernel
/// table's lock, so no slot is half made. Equal unless a path maps or leaves a frame there that the formula does not count.
#[cfg(any(not(feature = "release"), feature = "release-suite"))] // (the leak checks of `user_tests` and the boot self-tests)
pub fn frames_walked() -> (usize, usize) {
    let s = slots();
    let _kpt = crate::kmem::KPT.lock();
    let k = crate::kmem::frames();
    let walked = crate::kmem::kernel_table()
        .frames_in_range(
            k.mm,
            KSTACK_WINDOW_BASE,
            KSTACK_WINDOW_BASE.wrapping_add(KSTACK_WINDOW_SIZE),
        )
        .total();
    (
        usize::try_from(walked).unwrap_or(usize::MAX),
        formula(s.next as usize),
    )
}
