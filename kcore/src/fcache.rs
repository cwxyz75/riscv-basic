//! The per-hart frame cache (L16, SCHED-SMP-SPEC 3.10): a small stack of frames one hart takes from and gives back to without the `FRAME`
//! lock. A cache holds up to [`CACHE_MAX`] frames; an empty cache refills [`BATCH`] frames from the shared database in one take of the lock,
//! a full one drains [`BATCH`] back in one take.
//!
//! A cached frame stays allocated in the database (reference count 1, held by the cache), so the database never counts it free (a stray `dec` of it
//! would, so the cache's own drain checks that each frame was still held) and the leak oracles count it as free by adding the caches' lengths (`FrameCache::len`). Only a frame whose **sole** reference the caller holds
//! may be given back to a cache (`free_sole`): the kernel's own frames (stacks, page tables, slabs, the pool), not a user page that a
//! mapping and a VMO share.
//!
//! The cache is plain data; the kernel keeps one per hart and touches it with interrupts off. The shared side is the [`Shared`] trait,
//! which the kernel implements by taking `FRAME`, and the tests by a mutex or a plain database.

use crate::frame::{FrameDb, Pfn};

/// Frames a cache holds at most.
pub const CACHE_MAX: usize = 32;
/// Frames moved between a cache and the shared database in one take of the lock.
pub const BATCH: usize = 16;

/// The shared database as a cache sees it: each call is one take of the lock.
pub trait Shared {
    /// Allocate up to `out.len()` frames (each with reference count 1) into `out`; returns how many.
    fn refill(&mut self, out: &mut [Pfn]) -> usize;
    /// Give back frames the cache held (each with reference count 1).
    fn drain(&mut self, frames: &[Pfn]);
}

impl Shared for FrameDb {
    fn refill(&mut self, out: &mut [Pfn]) -> usize {
        let mut n = 0usize;
        for slot in out.iter_mut() {
            match self.alloc() {
                Some(p) => {
                    *slot = p;
                    n = n.saturating_add(1);
                }
                None => break,
            }
        }
        n
    }
    fn drain(&mut self, frames: &[Pfn]) {
        for &p in frames {
            let r = self.dec(p);
            // (a cached frame has exactly one reference, the cache's: anything else is a double free or a free of a shared frame)
            debug_assert!(
                r == Ok(true),
                "frame {p} came back from a cache with a bad count: {r:?}"
            );
        }
    }
}

/// One hart's cache.
#[derive(Debug)]
pub struct FrameCache {
    frames: [Pfn; CACHE_MAX],
    len: usize,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCache {
    pub const fn new() -> FrameCache {
        FrameCache {
            frames: [0; CACHE_MAX],
            len: 0,
        }
    }

    /// Frames held now.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A frame with reference count 1: from the cache, or (when empty) after a refill from `shared`. `None` when the database is out of
    /// frames too.
    pub fn alloc(&mut self, shared: &mut impl Shared) -> Option<Pfn> {
        if self.len == 0 {
            self.len = shared.refill(self.frames.get_mut(..BATCH)?);
        }
        self.len = self.len.checked_sub(1)?;
        self.frames.get(self.len).copied()
    }

    /// Give back a frame whose only reference the caller holds (it stays allocated, held by the cache). A full cache drains a batch first.
    pub fn free_sole(&mut self, pfn: Pfn, shared: &mut impl Shared) {
        if self.len >= CACHE_MAX {
            let keep = CACHE_MAX.saturating_sub(BATCH);
            if let Some(out) = self.frames.get(keep..CACHE_MAX) {
                shared.drain(out);
            }
            self.len = keep;
        }
        if let Some(slot) = self.frames.get_mut(self.len) {
            *slot = pfn;
            self.len = self.len.saturating_add(1); // (below CACHE_MAX: just made room)
        }
    }

    /// Return every cached frame to the shared database (a hart going offline, a test's end).
    pub fn flush(&mut self, shared: &mut impl Shared) {
        if let Some(out) = self.frames.get(..self.len) {
            shared.drain(out);
        }
        self.len = 0;
    }
}
