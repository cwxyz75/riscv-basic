//! The frame database: which physical frames exist, who holds them, and the free
//! lists. Reference counts exist from day one so sharing and copy-on-write are not
//! a redesign later (OS-ARCHITECTURE.md, 6.1).
//!
//! Frames are identified by frame number (`Pfn`, the physical address >> 12). The
//! database manages one or more contiguous ranges, each a buddy allocator (`buddy`,
//! MM2) over a slice of per-frame metadata that is either on the heap (host tests) or
//! a slice the caller carved out of boot memory (the kernel: never the heap). An
//! allocated frame has a count of at least 1; it returns to the free lists when the
//! count reaches 0. Misuse (freeing a free frame, touching a frame outside the
//! ranges) is an error value, never a panic, because the callers are driven by
//! user-controlled state.

use crate::buddy::{Buddy, Meta, MAX_ORDER};
use alloc::string::String;
use alloc::vec::Vec;

pub const PAGE_SIZE: u64 = 4096;

/// A physical frame number.
pub type Pfn = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The frame is not in the range this database manages.
    NotManaged,
    /// The frame is managed but free.
    NotAllocated,
    /// The reference count would overflow.
    Overflow,
}

/// Where the per-frame metadata lives.
enum Storage {
    Heap(Vec<Meta>),
    Borrowed(&'static mut [Meta]),
}

impl Storage {
    fn get(&self) -> &[Meta] {
        match self {
            Storage::Heap(v) => v,
            Storage::Borrowed(s) => s,
        }
    }
    fn get_mut(&mut self) -> &mut [Meta] {
        match self {
            Storage::Heap(v) => v,
            Storage::Borrowed(s) => s,
        }
    }
}

struct Range {
    buddy: Buddy,
    /// Where this range's metadata starts in the storage.
    off: usize,
}

pub struct FrameDb {
    ranges: Vec<Range>,
    storage: Storage,
}

impl FrameDb {
    /// A database over frames `base .. base + count`, all free, with its metadata on the heap (the host tests).
    pub fn new(base: Pfn, count: usize) -> FrameDb {
        let mut storage = Storage::Heap(alloc::vec![Meta::default(); count]);
        let buddy = Buddy::new(base, count, storage.get_mut());
        FrameDb {
            ranges: alloc::vec![Range { buddy, off: 0 }],
            storage,
        }
    }

    /// A database over several ranges (frame number, count), all free, with the metadata in `storage` (the kernel carves it out of boot
    /// memory before any allocator exists). `None` if `storage` is smaller than [`FrameDb::meta_len`] of the ranges.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: off + count is at most storage.len(), checked first
    pub fn with_ranges(ranges: &[(Pfn, usize)], storage: &'static mut [Meta]) -> Option<FrameDb> {
        if storage.len() < FrameDb::meta_len(ranges) {
            return None;
        }
        // Each range must be a real range of frames: no overflow, links that fit (`u32`, index + 1), and no overlap with another.
        for (k, &(base, count)) in ranges.iter().enumerate() {
            let end = base.checked_add(count as u64)?;
            if count >= u32::MAX as usize {
                return None;
            }
            for &(b2, c2) in ranges.iter().skip(k.saturating_add(1)) {
                let e2 = b2.checked_add(c2 as u64)?;
                if base < e2 && b2 < end {
                    return None;
                }
            }
        }
        let mut out = Vec::new();
        let mut off = 0;
        for &(base, count) in ranges {
            let buddy = Buddy::new(base, count, &mut storage[off..off + count]);
            out.push(Range { buddy, off });
            off += count;
        }
        Some(FrameDb {
            ranges: out,
            storage: Storage::Borrowed(storage),
        })
    }

    /// Entries of metadata the ranges need.
    pub fn meta_len(ranges: &[(Pfn, usize)]) -> usize {
        ranges
            .iter()
            .map(|r| r.1)
            .fold(0usize, usize::saturating_add)
    }

    /// The range holding `pfn` and the frame's index in the storage.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: off + i is below the storage length (i is inside the range)
    fn locate(&self, pfn: Pfn) -> Result<(usize, usize), FrameError> {
        for (r, range) in self.ranges.iter().enumerate() {
            if let Some(i) = range.buddy.index(pfn) {
                return Ok((r, range.off + i));
            }
        }
        Err(FrameError::NotManaged)
    }

    /// The free blocks of every range as `(first frame, order)`, lowest address first within each range (for tests and diagnostics).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: off + count is inside the storage by construction
    pub fn free_blocks(&self) -> Vec<(Pfn, usize)> {
        let mut out = Vec::new();
        for range in &self.ranges {
            let window = &self.storage.get()[range.off..range.off + range.buddy.count()];
            out.extend(range.buddy.free_blocks(window));
        }
        out
    }

    pub fn total(&self) -> usize {
        self.ranges.iter().map(|r| r.buddy.count()).sum()
    }

    pub fn free_count(&self) -> usize {
        self.ranges.iter().map(|r| r.buddy.free_count()).sum()
    }

    /// Does the database manage this frame (as opposed to device memory, which it never sees)?
    pub fn is_managed(&self, pfn: Pfn) -> bool {
        self.locate(pfn).is_ok()
    }

    /// Take a block of `order` from the first range that has one. The caller sets the reference counts of the frames it keeps.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the block came off a free list of the range, so off + index .. + 2^order is inside the storage
    fn take(&mut self, order: usize) -> Option<(usize, Pfn)> {
        let FrameDb { ranges, storage } = self;
        for (r, range) in ranges.iter_mut().enumerate() {
            let window = &mut storage.get_mut()[range.off..range.off + range.buddy.count()];
            if let Some(pfn) = range.buddy.alloc(window, order) {
                return Some((r, pfn));
            }
        }
        None
    }

    /// Take `n` physically contiguous free frames (each with reference count 1); the first frame number. The request is rounded up to a
    /// power of two for the allocation and the unused tail goes straight back. `None` if no block that large is free.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the block and its tail lie inside the range it came from; order is checked against MAX_ORDER first
    pub fn alloc_contiguous(&mut self, n: usize) -> Option<Pfn> {
        if n == 0 {
            return None;
        }
        let order = n.checked_next_power_of_two()?.trailing_zeros() as usize;
        if order > MAX_ORDER {
            return None;
        }
        let (r, pfn) = self.take(order)?;
        let FrameDb { ranges, storage } = self;
        let range = &mut ranges[r];
        let window = &mut storage.get_mut()[range.off..range.off + range.buddy.count()];
        let tail = (1usize << order) - n;
        if tail > 0 {
            range.buddy.free_run(window, pfn + n as u64, tail);
        }
        let first = range.buddy.index(pfn)?;
        for m in &mut window[first..first + n] {
            m.refs = 1;
        }
        Some(pfn)
    }

    /// Take a free frame (reference count 1).
    #[allow(clippy::indexing_slicing)] // INVARIANT: the frame came off a free list of range r, so its index is inside that range's window
    pub fn alloc(&mut self) -> Option<Pfn> {
        let (r, pfn) = self.take(0)?;
        let FrameDb { ranges, storage } = self;
        let range = ranges.get(r)?;
        let i = range.off.saturating_add(range.buddy.index(pfn)?);
        storage.get_mut()[i].refs = 1;
        Some(pfn)
    }

    /// Add a reference to an allocated frame.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the index was checked against the ranges by `locate` just above; a count below its limit cannot overflow
    pub fn inc(&mut self, pfn: Pfn) -> Result<(), FrameError> {
        let (_, i) = self.locate(pfn)?;
        let m = &mut self.storage.get_mut()[i];
        match m.refs {
            0 => Err(FrameError::NotAllocated),
            u32::MAX => Err(FrameError::Overflow),
            n => {
                m.refs = n + 1;
                Ok(())
            }
        }
    }

    /// Drop a reference; `Ok(true)` if that freed the frame.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the index was checked against the ranges just above; the count is at least 1 in the arm that subtracts
    pub fn dec(&mut self, pfn: Pfn) -> Result<bool, FrameError> {
        let (r, i) = self.locate(pfn)?;
        let FrameDb { ranges, storage } = self;
        let range = &mut ranges[r];
        let window = &mut storage.get_mut()[range.off..range.off + range.buddy.count()];
        let m = &mut window[i - range.off];
        match m.refs {
            0 => Err(FrameError::NotAllocated),
            1 => {
                debug_assert_eq!(
                    m.io, 0,
                    "IO3: frame {pfn:#x} is freed while a device mapping names it"
                );
                m.refs = 0;
                range.buddy.free_block(window, pfn, 0);
                Ok(true)
            }
            n => {
                m.refs = n - 1;
                Ok(false)
            }
        }
    }

    /// A device mapping names the frame (IOMMU6, debug builds: the mark is checked by `dec`, which must never free a marked frame).
    /// Taken after the mapping's hold on the frame's VMO and before any leaf names the frame; dropped by `io_unmark` after the
    /// mapping's fence and before its hold is dropped.
    #[cfg(debug_assertions)]
    #[allow(clippy::indexing_slicing)] // INVARIANT: the index was checked by `locate`
    pub fn io_mark(&mut self, pfn: Pfn) {
        if let Ok((_, i)) = self.locate(pfn) {
            let m = &mut self.storage.get_mut()[i];
            m.io = m.io.saturating_add(1);
        }
    }

    /// Drop one `io_mark` (a saturated count stays: it can only under-count, never stop a free that is allowed).
    #[cfg(debug_assertions)]
    #[allow(clippy::indexing_slicing)] // INVARIANT: as io_mark
    pub fn io_unmark(&mut self, pfn: Pfn) {
        if let Ok((_, i)) = self.locate(pfn) {
            let m = &mut self.storage.get_mut()[i];
            if m.io != u16::MAX {
                m.io = m.io.saturating_sub(1);
            }
        }
    }

    /// The reference count of an allocated frame (`None` if free or unmanaged).
    #[allow(clippy::indexing_slicing)] // INVARIANT: the index was checked against the ranges just above
    pub fn refs(&self, pfn: Pfn) -> Option<u32> {
        let (_, i) = self.locate(pfn).ok()?;
        let n = self.storage.get()[i].refs;
        (n != 0).then_some(n)
    }

    /// Check the database's own consistency: each range's free lists and metadata agree (see `Buddy::audit`).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: off + count is inside the storage by construction
    pub fn audit(&self) -> Result<(), String> {
        for range in &self.ranges {
            let window = &self.storage.get()[range.off..range.off + range.buddy.count()];
            range.buddy.audit(window)?;
        }
        Ok(())
    }
}
