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
use kcore::pagetable::{PageTable, Perms};

const PAGES: u64 = KSTACK_BYTES / PAGE_SIZE;
/// How many slots the window holds.
pub const MAX_SLOTS: u32 = (KSTACK_WINDOW_SIZE / KSTACK_SLOT_BYTES) as u32;

struct Slots {
    /// The next slot never used.
    next: u32,
    /// Slots whose stacks are free (their frames stay mapped).
    free: Vec<u32>,
}

static mut SLOTS: Slots = Slots {
    next: 0,
    free: Vec::new(),
};

/// The slot state, under the big kernel lock.
fn slots() -> &'static mut Slots {
    debug_assert!(
        crate::bkl::held(),
        "kernel stack slots touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (asserted), so there is no other reference.
    unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) }
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
        // SAFETY: the slot's four stack pages are mapped read-write for as long as the kernel runs
        // (slots are never unmapped), and this `KStack` is the only owner of the slot's contents.
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
        // (the lock is re-entrant per thread, so this is fine from a path that already holds it)
        crate::irq::without(|| slots().free.push(slot));
    }
}

/// Make a slot with frames: map the four stack pages of the next unused slot. `None` if the window is
/// full or no frames (or page-table frames) are left; nothing is changed then.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i < PAGES indexes the frames array of PAGES entries; the addresses are inside the kernel-stack window; the slot counter is far below u32::MAX
fn make_slot() -> Option<u32> {
    let s = slots();
    if s.next >= MAX_SLOTS {
        return None;
    }
    let slot = s.next;
    let k = crate::kmem::get();
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
    let mut pt = PageTable::from_root(k.aspace.table().root());
    let top = slot_base(slot) + KSTACK_SLOT_BYTES;
    for (i, &f) in frames.iter().enumerate() {
        let va = top - (PAGES - i as u64) * PAGE_SIZE;
        if pt.map(&mut k.mm, va, f, Perms::RW).is_err() {
            // undo this slot's mappings so far, and give every frame back
            for j in 0..i {
                let _ = pt.unmap(&mut k.mm, top - (PAGES - j as u64) * PAGE_SIZE);
            }
            for &f in &frames {
                let _ = k.mm.db.dec(f);
            }
            return None;
        }
    }
    // (a mapping that was invalid is now valid: no remote flush is needed for that, and this hart is the
    // only one that can touch the slot until the stack is handed out)
    // SAFETY: `sfence.vma` only drops cached translations.
    unsafe { core::arch::asm!("sfence.vma zero, zero") };
    s.next += 1;
    Some(slot)
}

/// A zeroed kernel stack, or `None` if there is no slot or no memory.
pub fn alloc() -> Option<KStack> {
    crate::irq::without(|| {
        let slot = match slots().free.pop() {
            Some(s) => s,
            None => make_slot()?,
        };
        let mut stack = KStack { slot };
        stack.fill(0); // a recycled stack starts clean (and `thread::stack_high_water` relies on it)
        Some(stack)
    })
}

/// Make the first slot now (boot, before the kernel half is snapshotted), so the window's top-level
/// page-table entry exists; the slot goes on the free list.
#[allow(clippy::expect_used)] // INVARIANT: boot code: the first slot must exist for the boot thread to get a stack
pub fn init() {
    crate::irq::without(|| {
        let slot = make_slot().expect("memory for the first kernel stack slot");
        slots().free.push(slot);
    });
}

/// Frames held by slots (made and kept): leak checks add this to the free count.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: made is the slot count, at most the window's slots
pub fn frames_held() -> usize {
    let made = slots().next as usize;
    // the stack pages, plus the page-table frames under them: one level-1 table for the window and one
    // level-0 table per 64 slots (2 MiB of window)
    made * PAGES as usize + if made == 0 { 0 } else { 1 + made.div_ceil(64) }
}
