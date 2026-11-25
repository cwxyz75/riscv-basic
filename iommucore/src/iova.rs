//! The I/O address allocator of a domain (IOMMU-SPEC 4.6): runs of pages in a fixed window, lowest fit, never page 0, and a
//! **pending** state for a range that was unmapped but whose fence has not completed, which is never handed out again. A range
//! whose fence was abandoned stays pending for the life of the domain (6.4).
//!
//! Two bitmaps (live, pending) are sized when the allocator is created; nothing after that allocates (the T177 rule:
//! `release` and `complete` cannot fail for want of memory). The allocator does not bound the number of mappings: a mapping's
//! own record lives in `kcore` (IOMMU3).

use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IovaError {
    /// The window is empty, starts at page 0, or ends past the last page number.
    BadWindow,
    /// The range is not inside the window.
    OutOfRange,
    /// The range is not entirely live (for `release`) or not entirely pending (for `complete`).
    WrongState,
}

/// The state of one page of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Free,
    Live,
    Pending,
}

/// The allocator of one domain's I/O addresses, in page numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iova {
    first: u64,
    pages: u64,
    live: Vec<u64>,
    pending: Vec<u64>,
}

impl Iova {
    /// An allocator for the pages `first_page .. first_page + pages`; `first_page` is never 0.
    pub fn new(first_page: u64, pages: u64) -> Result<Iova, IovaError> {
        if first_page == 0
            || pages == 0
            || first_page
                .checked_add(pages)
                .is_none_or(|end| end > 1 << 52)
        {
            return Err(IovaError::BadWindow);
        }
        let words = usize::try_from(pages.div_ceil(64)).map_err(|_| IovaError::BadWindow)?;
        Ok(Iova {
            first: first_page,
            pages,
            live: vec![0; words],
            pending: vec![0; words],
        })
    }

    pub const fn first_page(&self) -> u64 {
        self.first
    }
    pub const fn pages(&self) -> u64 {
        self.pages
    }

    /// The window-relative index of `page`, if the range `page .. page + n` is inside the window and not empty.
    fn rel(&self, page: u64, n: u64) -> Result<u64, IovaError> {
        let i = page.checked_sub(self.first).ok_or(IovaError::OutOfRange)?;
        match i.checked_add(n) {
            Some(end) if n > 0 && end <= self.pages => Ok(i),
            _ => Err(IovaError::OutOfRange),
        }
    }

    #[allow(clippy::indexing_slicing)] // INVARIANT: i < pages, so i / 64 < words
    fn bit(map: &[u64], i: u64) -> bool {
        (map[(i / 64) as usize] >> (i % 64)) & 1 != 0
    }

    /// The state of `page`, or `None` outside the window.
    pub fn state(&self, page: u64) -> Option<PageState> {
        let i = self.rel(page, 1).ok()?;
        Some(if Self::bit(&self.live, i) {
            PageState::Live
        } else if Self::bit(&self.pending, i) {
            PageState::Pending
        } else {
            PageState::Free
        })
    }

    /// Word `w` of the occupied map (live or pending), with the bits past the window's end set.
    #[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: w < words; the tail shift is 1 to 63
    fn occupied(&self, w: usize) -> u64 {
        let mut word = self.live[w] | self.pending[w];
        let end = (w as u64 + 1) * 64;
        if end > self.pages {
            word |= !0u64 << (self.pages % 64);
        }
        word
    }

    /// The first index at or after `i` whose occupied bit is `want`, or `pages` if there is none.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: i < pages before each step; the result is at most pages
    fn scan(&self, mut i: u64, want: bool) -> u64 {
        while i < self.pages {
            let w = (i / 64) as usize;
            let word = self.occupied(w);
            let word = if want { word } else { !word };
            let rest = word >> (i % 64);
            if rest != 0 {
                return (i + u64::from(rest.trailing_zeros())).min(self.pages);
            }
            i = (i / 64 + 1) * 64;
        }
        self.pages
    }

    /// The lowest run of `n` free pages (neither live nor pending), made live; its first page number.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: start <= end <= pages; first + pages fits (checked in new)
    pub fn alloc(&mut self, n: u64) -> Option<u64> {
        if n == 0 || n > self.pages {
            return None;
        }
        let mut i = 0;
        loop {
            let start = self.scan(i, false);
            if start + n > self.pages {
                return None;
            }
            let end = self.scan(start, true);
            if end - start >= n {
                set_bits(&mut self.live, start, n, true);
                return Some(self.first + start);
            }
            i = end;
        }
    }

    /// The range was unmapped: live to pending. Every page must be live.
    pub fn release(&mut self, page: u64, n: u64) -> Result<(), IovaError> {
        let i = self.rel(page, n)?;
        if !all_bits(&self.live, i, n) {
            return Err(IovaError::WrongState);
        }
        set_bits(&mut self.live, i, n, false);
        set_bits(&mut self.pending, i, n, true);
        Ok(())
    }

    /// The fence after the range's invalidation completed: pending to free. Every page must be pending.
    pub fn complete(&mut self, page: u64, n: u64) -> Result<(), IovaError> {
        let i = self.rel(page, n)?;
        if !all_bits(&self.pending, i, n) {
            return Err(IovaError::WrongState);
        }
        set_bits(&mut self.pending, i, n, false);
        Ok(())
    }

    /// Pages that are neither live nor pending.
    pub fn free_pages(&self) -> u64 {
        let used: u64 = self
            .live
            .iter()
            .zip(&self.pending)
            .map(|(l, p)| u64::from((l | p).count_ones()))
            .sum();
        self.pages.saturating_sub(used)
    }
}

#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: callers pass i + n <= pages, so every index is below words * 64
fn set_bits(map: &mut [u64], i: u64, n: u64, value: bool) {
    for b in i..i + n {
        let word = &mut map[(b / 64) as usize];
        if value {
            *word |= 1 << (b % 64);
        } else {
            *word &= !(1 << (b % 64));
        }
    }
}

#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // INVARIANT: as set_bits
fn all_bits(map: &[u64], i: u64, n: u64) -> bool {
    (i..i + n).all(|b| (map[(b / 64) as usize] >> (b % 64)) & 1 != 0)
}
