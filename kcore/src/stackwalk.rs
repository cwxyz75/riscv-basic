//! Where the panicking code's stack is (T266, T233-b): the bounds the panic handler's trace may read, from the stack pointer alone.
//!
//! The panic handler prints the words of the live stack that point into the kernel's text (`kernel/src/main.rs`, `Trace`). Before
//! T266 it knew the bounds only for a thread that had entered from user mode (`PerHart.kstack_top`); a panic on a kernel thread, the
//! idle loop, the boot stack or the emergency stack named nothing. [`live_stack`] finds the stack from `sp`: a thread's stack is the
//! mapped upper half of its slot in the kernel-stack window (`abi::layout`), found by arithmetic, and the few other stacks (a hart's
//! boot stack, which its idle thread runs on, and its emergency stack) are given by the caller as ranges. The walk then reads
//! `[sp, top)` and never anything else: never the guard half of a slot (unmapped), never a neighbouring slot.

use abi::layout::{KSTACK_BYTES, KSTACK_SLOT_BYTES, KSTACK_WINDOW_BASE, KSTACK_WINDOW_SIZE};

/// The live part of the stack `sp` is on, `[sp, top)`, or `None` when `sp` is on no stack the caller knows (or in a slot's guard).
/// `others` are the stacks outside the kernel-stack window, each `[low, high)`. `sp` is rounded down to a word first; the answer's
/// `sp` is that word.
pub fn live_stack(sp: u64, others: &[(u64, u64)]) -> Option<(u64, u64)> {
    let sp = sp & !7;
    if in_window(sp) {
        // (a slot's stack or its guard, whatever the caller's ranges say: a range overlapping the window never widens a slot)
        return window_top(sp).map(|top| (sp, top));
    }
    others
        .iter()
        .find(|&&(lo, hi)| lo <= sp && sp < hi)
        .map(|&(_, hi)| (sp, hi & !7))
        .filter(|&(s, t)| s < t)
}

fn in_window(sp: u64) -> bool {
    sp >= KSTACK_WINDOW_BASE && sp.wrapping_sub(KSTACK_WINDOW_BASE) < KSTACK_WINDOW_SIZE
}

/// For an `sp` in the kernel-stack window: the top of its slot, if `sp` is in the slot's mapped upper half (the stack); `None` in
/// the guard half or outside the window.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: sp >= KSTACK_WINDOW_BASE and the window is one slot-aligned GiB below 2^64
pub fn window_top(sp: u64) -> Option<u64> {
    if !in_window(sp) {
        return None;
    }
    let off = sp - KSTACK_WINDOW_BASE;
    let base = KSTACK_WINDOW_BASE + off / KSTACK_SLOT_BYTES * KSTACK_SLOT_BYTES;
    if sp < base + KSTACK_SLOT_BYTES - KSTACK_BYTES {
        return None; // the guard
    }
    Some(base + KSTACK_SLOT_BYTES)
}
