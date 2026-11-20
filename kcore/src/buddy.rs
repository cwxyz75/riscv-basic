//! The buddy allocator over one range of frames (MM2, KERNEL-V1-SPEC 4.1): orders 0 (one frame) to 18 (1 GiB), a free list per order, split
//! and merge by buddy address. Its per-frame metadata is a slice the caller provides, so the kernel can carve it out of boot memory before
//! any allocator exists. The module is pure: it never touches memory but that slice, and it knows nothing of reference counts beyond
//! the one field `FrameDb` keeps in each [`Meta`].
//!
//! A block of order `k` starts at a frame number that is a multiple of `1 << k` (alignment is of the physical frame number, not of the
//! offset into the range, so a big block is a big aligned physical page: MM3 maps them as huge pages). Its buddy is `pfn ^ (1 << k)`.
//! Only the first frame of a free block is a *head*: it carries the block's order and the list links; the other frames of a free block
//! carry nothing. An allocated frame has a reference count of at least 1 and is never a head.

use crate::frame::Pfn;
use alloc::string::String;

/// The largest order: `1 << 18` frames, 1 GiB.
pub const MAX_ORDER: usize = 18;
const ORDERS: usize = MAX_ORDER + 1;
/// "No frame" in a link (links hold `index + 1`).
const NIL: u32 = 0;

/// Per-frame metadata: a reference count, the list links of a free head and the head's order. All-zero is a valid, empty value (an
/// allocated-or-not frame that is not a free head), so boot memory needs no initialisation but a clear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Meta {
    /// Holders of the frame; 0 means free.
    pub refs: u32,
    next: u32,
    prev: u32,
    /// 0: not a free head; `k + 1`: the head of a free block of order `k`.
    head: u8,
}

/// Bytes of metadata per frame (the size of [`Meta`]).
pub const META_BYTES: usize = core::mem::size_of::<Meta>();

/// The free lists of one range. The metadata lives in the slice passed to each call (index = frame number - `base`).
#[derive(Debug)]
pub struct Buddy {
    base: Pfn,
    count: usize,
    heads: [u32; ORDERS],
    free: usize,
}

/// `floor(log2(n))` for `n >= 1`.
fn floor_log2(n: u64) -> usize {
    63usize.saturating_sub(n.leading_zeros() as usize)
}

impl Buddy {
    /// Manage frames `base .. base + count`, all free. `meta` (at least `count` entries) is cleared and becomes this range's metadata.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: p stays below end = base + count (a range the boot code derived from the memory map); order is at most MAX_ORDER, below ORDERS
    pub fn new(base: Pfn, count: usize, meta: &mut [Meta]) -> Buddy {
        for m in meta.iter_mut().take(count) {
            *m = Meta::default();
        }
        let mut b = Buddy {
            base,
            count,
            heads: [NIL; ORDERS],
            free: 0,
        };
        // Cut the range into the largest aligned blocks, low addresses first, so that lists hold what is aligned best.
        let end = base + count as u64;
        let mut p = base;
        while p < end {
            let align = if p == 0 {
                MAX_ORDER
            } else {
                (p.trailing_zeros() as usize).min(MAX_ORDER)
            };
            let fit = floor_log2(end - p);
            let order = align.min(fit);
            b.push(meta, p, order);
            b.free += 1usize << order;
            p += 1u64 << order;
        }
        b
    }

    pub fn base(&self) -> Pfn {
        self.base
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Frames in free blocks.
    pub fn free_count(&self) -> usize {
        self.free
    }

    /// The index of `pfn` in this range's metadata, if it is in the range.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the subtraction is checked by checked_sub
    pub fn index(&self, pfn: Pfn) -> Option<usize> {
        pfn.checked_sub(self.base)
            .filter(|&i| i < self.count as u64)
            .map(|i| i as usize)
    }

    /// Put the block at `pfn` (order `order`) on the free list. Does not touch `free`.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: pfn is in the range (checked by every caller) and order is at most MAX_ORDER; links hold index + 1 of frames of the range
    fn push(&mut self, meta: &mut [Meta], pfn: Pfn, order: usize) {
        let Some(i) = self.index(pfn) else { return };
        let old = self.heads[order];
        meta[i].head = order as u8 + 1;
        meta[i].prev = NIL;
        meta[i].next = old;
        if old != NIL {
            meta[(old - 1) as usize].prev = i as u32 + 1;
        }
        self.heads[order] = i as u32 + 1;
    }

    /// Take the block whose head is frame index `i` (order `order`) off its list.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i is the index of a free head of that order, so its links are indexes of frames of the range
    fn unlink(&mut self, meta: &mut [Meta], i: usize, order: usize) {
        let (prev, next) = (meta[i].prev, meta[i].next);
        if prev == NIL {
            self.heads[order] = next;
        } else {
            meta[(prev - 1) as usize].next = next;
        }
        if next != NIL {
            meta[(next - 1) as usize].prev = prev;
        }
        meta[i].head = 0;
        meta[i].prev = NIL;
        meta[i].next = NIL;
    }

    /// Take a block of `order` (split from a larger one if needed). The frames' reference counts are left to the caller.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: order <= MAX_ORDER; the head index came off a free list, so it is a frame of the range
    pub fn alloc(&mut self, meta: &mut [Meta], order: usize) -> Option<Pfn> {
        if order > MAX_ORDER {
            return None;
        }
        let mut k = (order..ORDERS).find(|&k| self.heads[k] != NIL)?;
        let i = (self.heads[k] - 1) as usize;
        self.unlink(meta, i, k);
        let pfn = self.base + i as u64;
        // Split: keep the low half, put the high half on the next list down.
        while k > order {
            k -= 1;
            self.push(meta, pfn + (1u64 << k), k);
        }
        self.free -= 1usize << order;
        Some(pfn)
    }

    /// Return a block to the free lists, merging with its buddy while the buddy is a free head of the same order.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: pfn is a frame of the range and the block lies inside it; order is at most MAX_ORDER
    pub fn free_block(&mut self, meta: &mut [Meta], pfn: Pfn, order: usize) {
        self.free += 1usize << order;
        let (mut pfn, mut order) = (pfn, order);
        while order < MAX_ORDER {
            let buddy = pfn ^ (1u64 << order);
            let Some(bi) = self.index(buddy) else { break };
            if meta[bi].head != order as u8 + 1 {
                break;
            }
            self.unlink(meta, bi, order);
            pfn = pfn.min(buddy);
            order += 1;
        }
        self.push(meta, pfn, order);
    }

    /// Return `n` frames starting at `pfn` (the unused tail of a rounded-up allocation) as the largest aligned blocks that fit.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: p stays at most end, a frame number inside the range
    pub fn free_run(&mut self, meta: &mut [Meta], pfn: Pfn, n: usize) {
        let end = pfn + n as u64;
        let mut p = pfn;
        while p < end {
            let align = if p == 0 {
                MAX_ORDER
            } else {
                (p.trailing_zeros() as usize).min(MAX_ORDER)
            };
            let order = align.min(floor_log2(end - p));
            self.free_block(meta, p, order);
            p += 1u64 << order;
        }
    }

    /// The free blocks as `(first frame, order)`, lowest address first.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: list links are indexes of frames of the range, as `audit` checks
    pub fn free_blocks(&self, meta: &[Meta]) -> alloc::vec::Vec<(Pfn, usize)> {
        let mut out = alloc::vec::Vec::new();
        for order in 0..ORDERS {
            let mut at = self.heads[order];
            while at != NIL {
                let i = (at - 1) as usize;
                out.push((self.base + i as u64, order));
                at = meta[i].next;
            }
        }
        out.sort();
        out
    }

    /// Check the free lists and the metadata against each other: every list entry is a head of that order, aligned, inside the range, with
    /// every frame of its block unreferenced; no frame is in two blocks; no two free buddies of the same order sit unmerged; every
    /// unreferenced frame is in a block; `free_count` is the total of the blocks.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every index is checked against the range before use
    pub fn audit(&self, meta: &[Meta]) -> Result<(), String> {
        let mut covered = alloc::vec![false; self.count];
        let mut total = 0usize;
        for order in 0..ORDERS {
            let mut at = self.heads[order];
            let mut prev = NIL;
            let mut guard = 0usize;
            while at != NIL {
                guard += 1;
                if guard > self.count {
                    return Err(alloc::format!("order {order}: the free list loops"));
                }
                let i = (at - 1) as usize;
                if i >= self.count {
                    return Err(alloc::format!(
                        "order {order}: a link points outside the range"
                    ));
                }
                let m = &meta[i];
                let pfn = self.base + i as u64;
                if m.head != order as u8 + 1 {
                    return Err(alloc::format!(
                        "frame {pfn} is on the order-{order} list but is not its head"
                    ));
                }
                if m.prev != prev {
                    return Err(alloc::format!("frame {pfn}: the back link is wrong"));
                }
                if !pfn.is_multiple_of(1u64 << order) {
                    return Err(alloc::format!(
                        "frame {pfn} heads an order-{order} block but is not aligned to it"
                    ));
                }
                if i + (1usize << order) > self.count {
                    return Err(alloc::format!(
                        "frame {pfn}: an order-{order} block runs past the range"
                    ));
                }
                for j in i..i + (1usize << order) {
                    if meta[j].refs != 0 {
                        return Err(alloc::format!(
                            "frame {} is in a free block but has {} references",
                            self.base + j as u64,
                            meta[j].refs
                        ));
                    }
                    if core::mem::replace(&mut covered[j], true) {
                        return Err(alloc::format!(
                            "frame {} is in two free blocks",
                            self.base + j as u64
                        ));
                    }
                    if j != i && meta[j].head != 0 {
                        return Err(alloc::format!(
                            "frame {} is inside a free block and is a head",
                            self.base + j as u64
                        ));
                    }
                }
                if order < MAX_ORDER {
                    let buddy = pfn ^ (1u64 << order);
                    if let Some(bi) = self.index(buddy) {
                        if meta[bi].head == order as u8 + 1 {
                            return Err(alloc::format!("frames {pfn} and {buddy} are free buddies of order {order} and were not merged"));
                        }
                    }
                }
                total += 1usize << order;
                prev = at;
                at = m.next;
            }
        }
        if total != self.free {
            return Err(alloc::format!(
                "the free blocks hold {total} frames but free_count says {}",
                self.free
            ));
        }
        for (j, m) in meta.iter().take(self.count).enumerate() {
            if m.refs == 0 && !covered[j] {
                return Err(alloc::format!(
                    "frame {} has no references and is in no free block",
                    self.base + j as u64
                ));
            }
            if m.head != 0 && !covered[j] {
                return Err(alloc::format!(
                    "frame {} claims to head a free block but is on no list",
                    self.base + j as u64
                ));
            }
        }
        Ok(())
    }
}
