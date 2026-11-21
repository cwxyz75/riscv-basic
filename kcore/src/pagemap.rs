//! The committed pages of a VMO: page number to frame, in two levels (chunks of 64 pages), with **fallible** growth (KT9). A `BTreeMap` would do the
//! same job, but its node allocations cannot report failure: a failed allocation while committing a page would abort the kernel, where this
//! returns `Err` and the commit gives its frame and charge back.

use crate::frame::Pfn;
use alloc::vec::Vec;

const CHUNK: u64 = 64;
const NONE: u64 = u64::MAX;
/// A page that is explicitly empty: it reads as zeros whatever an ancestor holds, and has no frame (MM8: a decommit in a VMO that inherits).
const HOLE: u64 = u64::MAX - 1;

/// What the map says about a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Nothing: an ancestor may show a page.
    None,
    /// Explicitly zero, no frame, shadows the ancestors.
    Hole,
    Frame(Pfn),
}

#[derive(Debug, Default)]
pub struct PageMap {
    chunks: Vec<Option<Vec<u64>>>,
    len: usize,
}

impl PageMap {
    pub const fn new() -> PageMap {
        PageMap {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// The pages `0 .. pages` mapped to the frames `first ..` (the device and DMA VMOs, which are made once, with privilege).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: pages is below MAX_VMO_PAGES and first + pages is a frame range the caller checked
    pub fn from_run(pages: u64, first: Pfn) -> PageMap {
        let mut m = PageMap::new();
        for i in 0..pages {
            // (an infallible allocation, on a privileged path: T177)
            m.insert_growing(i, first + i);
        }
        m
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the chunk index is below chunks.len() after the resize, and the slot index is below CHUNK
    fn insert_growing(&mut self, page: u64, pfn: Pfn) {
        let c = (page / CHUNK) as usize;
        if self.chunks.len() <= c {
            self.chunks.resize_with(c + 1, || None);
        }
        let chunk = self.chunks[c].get_or_insert_with(|| alloc::vec![NONE; CHUNK as usize]);
        if chunk[(page % CHUNK) as usize] == NONE {
            self.len += 1;
        }
        chunk[(page % CHUNK) as usize] = pfn;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK, the chunk's length
    pub fn get(&self, page: u64) -> Option<Pfn> {
        let chunk = self.chunks.get((page / CHUNK) as usize)?.as_ref()?;
        let v = chunk[(page % CHUNK) as usize];
        (v != NONE && v != HOLE).then_some(v)
    }

    /// What the map says about `page`.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK, the chunk's length
    pub fn entry(&self, page: u64) -> Entry {
        match self
            .chunks
            .get((page / CHUNK) as usize)
            .and_then(|c| c.as_ref())
        {
            Some(chunk) => Self::decode(chunk[(page % CHUNK) as usize]),
            None => Entry::None,
        }
    }

    fn decode(v: u64) -> Entry {
        match v {
            NONE => Entry::None,
            HOLE => Entry::Hole,
            pfn => Entry::Frame(pfn),
        }
    }

    pub fn contains(&self, page: u64) -> bool {
        self.get(page).is_some()
    }

    /// Record that `page` is backed by `pfn`. `Err` if the memory for the bookkeeping cannot be had (nothing changed).
    pub fn insert(&mut self, page: u64, pfn: Pfn) -> Result<(), abi::status::Status> {
        self.put(page, pfn)
    }

    /// Record that `page` is explicitly empty (a frame it had is the caller's to free: `remove` or `get` it first). `Err` as `insert`.
    pub fn insert_hole(&mut self, page: u64) -> Result<(), abi::status::Status> {
        self.put(page, HOLE)
    }

    /// Turn a page that has a slot already (a frame or a hole) into a hole. Cannot fail; returns the frame it had.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; len is at least 1 when a frame is replaced
    pub fn make_hole(&mut self, page: u64) -> Option<Pfn> {
        let chunk = self.chunks.get_mut((page / CHUNK) as usize)?.as_mut()?;
        let old = core::mem::replace(&mut chunk[(page % CHUNK) as usize], HOLE);
        match Self::decode(old) {
            Entry::Frame(pfn) => {
                self.len -= 1;
                Some(pfn)
            }
            _ => None,
        }
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the chunk index is below chunks.len() after the reservation, and the slot index is below CHUNK; len counts frames
    fn put(&mut self, page: u64, raw: u64) -> Result<(), abi::status::Status> {
        let c = (page / CHUNK) as usize;
        if self.chunks.len() <= c {
            self.chunks
                .try_reserve(c + 1 - self.chunks.len())
                .map_err(|_| abi::status::Status::NoMemory)?;
            self.chunks.resize_with(c + 1, || None);
        }
        if self.chunks[c].is_none() {
            let mut v: Vec<u64> = Vec::new();
            v.try_reserve_exact(CHUNK as usize)
                .map_err(|_| abi::status::Status::NoMemory)?;
            v.resize(CHUNK as usize, NONE);
            self.chunks[c] = Some(v);
        }
        if let Some(chunk) = self.chunks[c].as_mut() {
            let slot = &mut chunk[(page % CHUNK) as usize];
            let was_frame = matches!(Self::decode(*slot), Entry::Frame(_));
            let is_frame = matches!(Self::decode(raw), Entry::Frame(_));
            *slot = raw;
            if is_frame && !was_frame {
                self.len += 1;
            } else if was_frame && !is_frame {
                self.len -= 1;
            }
        }
        Ok(())
    }

    /// The first page at or after `from` with an entry (a frame or a hole), and what it is (an allocation-free walk over the map).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the chunk index is below chunks.len() and the slot index below CHUNK
    pub fn next_entry_at_or_after(&self, from: u64) -> Option<(u64, Entry)> {
        let mut c = (from / CHUNK) as usize;
        let mut slot = (from % CHUNK) as usize;
        while c < self.chunks.len() {
            if let Some(chunk) = self.chunks[c].as_ref() {
                while slot < CHUNK as usize {
                    if chunk[slot] != NONE {
                        return Some((c as u64 * CHUNK + slot as u64, Self::decode(chunk[slot])));
                    }
                    slot += 1;
                }
            }
            c += 1;
            slot = 0;
        }
        None
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; len is at least 1 when a slot is occupied
    pub fn remove(&mut self, page: u64) -> Option<Pfn> {
        let chunk = self.chunks.get_mut((page / CHUNK) as usize)?.as_mut()?;
        let v = core::mem::replace(&mut chunk[(page % CHUNK) as usize], NONE);
        if v == NONE || v == HOLE {
            return None;
        }
        self.len -= 1;
        Some(v)
    }

    /// Call `f` with every frame, and empty the map.
    pub fn drain_pfns(&mut self, mut f: impl FnMut(Pfn)) {
        for chunk in self.chunks.drain(..).flatten() {
            for v in chunk {
                if v != NONE && v != HOLE {
                    f(v);
                }
            }
        }
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    #[test]
    fn it_behaves_like_a_map_of_page_to_frame() {
        let mut m = PageMap::new();
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut x = 12345u64;
        for _ in 0..5000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let page = x % 700;
            match (x >> 20) % 3 {
                0 | 1 => {
                    assert_eq!(m.insert(page, x >> 30), Ok(()));
                    model.insert(page, x >> 30);
                }
                _ => assert_eq!(m.remove(page), model.remove(&page)),
            }
            assert_eq!(m.len(), model.len());
            assert_eq!(m.get(page), model.get(&page).copied());
            assert_eq!(m.contains(page), model.contains_key(&page));
        }
        let mut seen = Vec::new();
        m.drain_pfns(|p| seen.push(p));
        seen.sort_unstable();
        let mut want: Vec<u64> = model.values().copied().collect();
        want.sort_unstable();
        assert_eq!(seen, want);
        assert!(m.is_empty());
    }

    #[test]
    fn a_run_maps_consecutive_frames() {
        let m = PageMap::from_run(130, 1000);
        assert_eq!(
            (m.len(), m.get(0), m.get(129), m.get(130)),
            (130, Some(1000), Some(1129), None)
        );
    }
}
