//! What memory the frame allocator is given (PL4, KERNEL-V1-SPEC 2.3): the RAM ranges of the device tree minus everything that is
//! not free for the taking: the reserved ranges (firmware), the loader's reservation block, the tree's own extent, the kernel image and
//! whatever else the boot code names. Pure: ranges in bytes in, ranges of whole frames out.

use crate::frame::{Pfn, PAGE_SIZE};
use alloc::vec::Vec;

/// A `(first frame, number of frames)` range.
pub type FrameRange = (Pfn, usize);

/// The frames fully inside the byte range `(address, size)`: the first frame at or after the start and the last whole one before the end.
fn inward(addr: u64, size: u64) -> Option<(u64, u64)> {
    let end = addr.saturating_add(size); // a range that runs past the top of the address space is cut there, like `outward`
    let first = addr.div_ceil(PAGE_SIZE);
    let last = end / PAGE_SIZE; // exclusive
    (first < last).then_some((first, last))
}

/// The frames touched by the byte range (a reservation takes every frame it overlaps).
fn outward(addr: u64, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let end = addr.saturating_add(size);
    Some((addr / PAGE_SIZE, end.div_ceil(PAGE_SIZE)))
}

/// The usable frames: each `memory` range cut to whole frames, minus every `reserved` byte range widened to whole frames, as sorted,
/// non-overlapping, non-adjacent `(first frame, count)` ranges. Memory ranges that overlap one another are merged (a bad tree does not hand
/// a frame out twice).
pub fn usable_frames(memory: &[(u64, u64)], reserved: &[(u64, u64)]) -> Vec<FrameRange> {
    let mut free: Vec<(u64, u64)> = memory.iter().filter_map(|&(a, s)| inward(a, s)).collect();
    free.sort_unstable();
    free = merge(free);
    let mut cut: Vec<(u64, u64)> = reserved
        .iter()
        .filter_map(|&(a, s)| outward(a, s))
        .collect();
    cut.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (mut a, b) in free {
        for &(ca, cb) in &cut {
            if cb <= a || ca >= b {
                continue;
            }
            if ca > a {
                out.push((a, ca));
            }
            a = a.max(cb);
            if a >= b {
                break;
            }
        }
        if a < b {
            out.push((a, b));
        }
    }
    merge(out)
        .into_iter()
        .map(|(a, b)| (a, (b.saturating_sub(a)) as usize))
        .collect()
}

/// Sort-merge: overlapping or touching ranges become one.
fn merge(sorted: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (a, b) in sorted {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}
