//! The frame database: which physical frames exist, who holds them, and the free
//! list. Reference counts exist from day one so sharing and copy-on-write are not
//! a redesign later (OS-ARCHITECTURE.md, 6.1).
//!
//! Frames are identified by frame number (`Pfn`, the physical address >> 12). The
//! database manages one contiguous range. An allocated frame has a count of at
//! least 1; it returns to the free list when the count reaches 0. Misuse (freeing
//! a free frame, touching a frame outside the range) is an error value, never a
//! panic, because the callers are driven by user-controlled state.

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

pub struct FrameDb {
    base: Pfn,
    refs: Vec<u32>,
    /// A stack of free frames; the next allocation takes the top.
    free: Vec<Pfn>,
}

impl FrameDb {
    /// A database over frames `base .. base + count`, all free.
    pub fn new(base: Pfn, count: usize) -> FrameDb {
        // Pushed in descending order so allocation hands out ascending frames.
        let free = (base..base + count as u64).rev().collect();
        FrameDb {
            base,
            refs: alloc::vec![0; count],
            free,
        }
    }

    fn index(&self, pfn: Pfn) -> Result<usize, FrameError> {
        pfn.checked_sub(self.base)
            .filter(|&i| i < self.refs.len() as u64)
            .map(|i| i as usize)
            .ok_or(FrameError::NotManaged)
    }

    pub fn total(&self) -> usize {
        self.refs.len()
    }

    pub fn free_count(&self) -> usize {
        self.free.len()
    }

    /// Does the database manage this frame (as opposed to device memory, which it never sees)?
    pub fn is_managed(&self, pfn: Pfn) -> bool {
        self.index(pfn).is_ok()
    }

    /// Take `n` physically contiguous free frames (each with reference count 1); the first
    /// frame number. `None` if no run of that length is free.
    pub fn alloc_contiguous(&mut self, n: usize) -> Option<Pfn> {
        if n == 0 || n > self.free.len() {
            return None;
        }
        let mut run = 0usize;
        let mut start = 0usize;
        for i in 0..self.refs.len() {
            if self.refs[i] == 0 {
                if run == 0 {
                    start = i;
                }
                run += 1;
                if run == n {
                    let first = self.base + start as u64;
                    let taken: alloc::collections::BTreeSet<Pfn> =
                        (first..first + n as u64).collect();
                    self.free.retain(|p| !taken.contains(p));
                    for i in start..start + n {
                        self.refs[i] = 1;
                    }
                    return Some(first);
                }
            } else {
                run = 0;
            }
        }
        None
    }

    /// Take a free frame (reference count 1).
    pub fn alloc(&mut self) -> Option<Pfn> {
        let pfn = self.free.pop()?;
        let i = (pfn - self.base) as usize;
        debug_assert_eq!(self.refs[i], 0);
        self.refs[i] = 1;
        Some(pfn)
    }

    /// Add a reference to an allocated frame.
    pub fn inc(&mut self, pfn: Pfn) -> Result<(), FrameError> {
        let i = self.index(pfn)?;
        match self.refs[i] {
            0 => Err(FrameError::NotAllocated),
            u32::MAX => Err(FrameError::Overflow),
            n => {
                self.refs[i] = n + 1;
                Ok(())
            }
        }
    }

    /// Drop a reference; `Ok(true)` if that freed the frame.
    pub fn dec(&mut self, pfn: Pfn) -> Result<bool, FrameError> {
        let i = self.index(pfn)?;
        match self.refs[i] {
            0 => Err(FrameError::NotAllocated),
            1 => {
                self.refs[i] = 0;
                self.free.push(pfn);
                Ok(true)
            }
            n => {
                self.refs[i] = n - 1;
                Ok(false)
            }
        }
    }

    /// The reference count of an allocated frame (`None` if free or unmanaged).
    pub fn refs(&self, pfn: Pfn) -> Option<u32> {
        let i = self.index(pfn).ok()?;
        (self.refs[i] != 0).then_some(self.refs[i])
    }

    /// Check the database's own consistency: the free list holds exactly the
    /// frames with a count of 0, each once.
    pub fn audit(&self) -> Result<(), String> {
        let mut seen = alloc::vec![false; self.refs.len()];
        for &pfn in &self.free {
            let i = self
                .index(pfn)
                .map_err(|_| alloc::format!("free list holds unmanaged frame {pfn}"))?;
            if self.refs[i] != 0 {
                return Err(alloc::format!(
                    "frame {pfn} is free-listed but has {} references",
                    self.refs[i]
                ));
            }
            if core::mem::replace(&mut seen[i], true) {
                return Err(alloc::format!("frame {pfn} is on the free list twice"));
            }
        }
        for (i, &n) in self.refs.iter().enumerate() {
            if n == 0 && !seen[i] {
                return Err(alloc::format!(
                    "frame {} has no references and is not on the free list",
                    self.base + i as u64
                ));
            }
        }
        Ok(())
    }
}
