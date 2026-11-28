//! The committed pages of a VMO: page number to frame, **sparse** (T207 PR 1). The map is an `FMap` from a chunk number (`page / 64`) to a leaf
//! of 64 slots, so the cost is one 528-byte leaf per *touched* chunk and nothing per untouched page: a page at 2 GiB costs one leaf, not an
//! outer vector of 196 KiB. Growth is **fallible** (KT9: class R): an insert that needs a new leaf reserves its room first and returns
//! `NoMemory` with nothing changed; removal never allocates (a leaf whose last entry goes is freed).

use crate::fmap::FMap;
use crate::frame::Pfn;

const CHUNK: u64 = 64;
const NONE: u64 = u64::MAX;
/// A page that is explicitly empty: it reads as zeros whatever an ancestor holds, and has no frame (MM8: a decommit in a VMO that inherits).
const HOLE: u64 = u64::MAX - 1;
/// What one leaf costs the kernel heap: the 64 slots (the key and the count live in the map's own entry).
pub const LEAF_BYTES: u64 = CHUNK * 8 + 16;

/// What the map says about a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Nothing: an ancestor may show a page.
    None,
    /// Explicitly zero, no frame, shadows the ancestors.
    Hole,
    Frame(Pfn),
}

impl Entry {
    /// The frame, if the entry is one.
    pub fn frame(self) -> Option<Pfn> {
        match self {
            Entry::Frame(p) => Some(p),
            _ => None,
        }
    }
}

/// A leaf's 64 slots, all `NONE`, or `None` when there is no memory (nothing is allocated then). The room is reserved exactly, so the box is
/// the vector's own allocation (no shrink, which could not fail softly).
fn new_slots() -> Option<alloc::boxed::Box<[u64]>> {
    let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    v.try_reserve_exact(CHUNK as usize).ok()?;
    v.resize(CHUNK as usize, NONE);
    Some(v.into_boxed_slice())
}

/// One chunk: 64 slots and how many of them are not `NONE` (a frame or a hole). A leaf with `used == 0` does not exist.
#[derive(Debug)]
struct Leaf {
    /// (a boxed slice filled in place, not a boxed array: the array would be built on the stack first, half a KiB, KT13)
    slots: alloc::boxed::Box<[u64]>,
    used: u8,
}

#[derive(Debug, Default)]
pub struct PageMap {
    leaves: FMap<u64, Leaf>,
    /// Frames (not holes).
    len: usize,
}

impl PageMap {
    pub const fn new() -> PageMap {
        PageMap {
            leaves: FMap::new(),
            len: 0,
        }
    }

    /// The pages `0 .. pages` mapped to the frames `first ..` (the device and DMA VMOs, which are made once, with privilege).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: first + pages is a frame range the caller checked
    pub fn from_run(pages: u64, first: Pfn) -> PageMap {
        let mut m = PageMap::new();
        for i in 0..pages {
            // (a privileged path that has always been infallible: a failure here aborts, as `Box::new` would; T177)
            assert!(
                m.put(i, first + i).is_ok(),
                "out of memory making a device or DMA VMO"
            );
        }
        m
    }

    /// Frames in the map.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Leaves held: the kernel bytes the index costs are `leaf_count() * LEAF_BYTES`.
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK
    fn raw(&self, page: u64) -> u64 {
        match self.leaves.get(&(page / CHUNK)) {
            Some(l) => l.slots[(page % CHUNK) as usize],
            None => NONE,
        }
    }

    pub fn get(&self, page: u64) -> Option<Pfn> {
        let v = self.raw(page);
        (v != NONE && v != HOLE).then_some(v)
    }

    /// What the map says about `page`.
    pub fn entry(&self, page: u64) -> Entry {
        Self::decode(self.raw(page))
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
        let leaf = self.leaves.get_mut(&(page / CHUNK))?;
        let slot = &mut leaf.slots[(page % CHUNK) as usize];
        if *slot == NONE {
            return None;
        }
        let old = core::mem::replace(slot, HOLE);
        match Self::decode(old) {
            Entry::Frame(pfn) => {
                self.len -= 1;
                Some(pfn)
            }
            _ => None,
        }
    }

    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; len counts frames; used counts non-NONE slots (at most 64)
    fn put(&mut self, page: u64, raw: u64) -> Result<(), abi::status::Status> {
        let key = page / CHUNK;
        if !self.leaves.contains_key(&key) {
            self.leaves
                .reserve(1)
                .map_err(|_| abi::status::Status::NoMemory)?;
            let slots = new_slots().ok_or(abi::status::Status::NoMemory)?;
            self.leaves.insert_reserved(key, Leaf { slots, used: 0 });
        }
        if let Some(leaf) = self.leaves.get_mut(&key) {
            let slot = &mut leaf.slots[(page % CHUNK) as usize];
            let was_frame = matches!(Self::decode(*slot), Entry::Frame(_));
            let was_none = *slot == NONE;
            let is_frame = matches!(Self::decode(raw), Entry::Frame(_));
            *slot = raw;
            if was_none {
                leaf.used += 1;
            }
            if is_frame && !was_frame {
                self.len += 1;
            } else if was_frame && !is_frame {
                self.len -= 1;
            }
        }
        Ok(())
    }

    /// The first page at or after `from` with an entry (a frame or a hole), and what it is (an allocation-free walk over the map).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; the key times CHUNK is a page number below 2^64 / 64 * 64
    pub fn next_entry_at_or_after(&self, from: u64) -> Option<(u64, Entry)> {
        let first_key = from / CHUNK;
        for (&key, leaf) in self.leaves.range(first_key..) {
            let start = if key == first_key {
                (from % CHUNK) as usize
            } else {
                0
            };
            for slot in start..CHUNK as usize {
                let v = leaf.slots[slot];
                if v != NONE {
                    return Some((key * CHUNK + slot as u64, Self::decode(v)));
                }
            }
        }
        None
    }

    /// Frames (not holes) in the pages `first .. end`, without allocating: the cost is the leaves the range touches, not its length.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; lo and hi are inside the range, so hi - lo is at most 64
    pub fn count_frames_in(&self, first: u64, end: u64) -> u64 {
        if first >= end {
            return 0;
        }
        let mut n = 0;
        for (&key, leaf) in self.leaves.range(first / CHUNK..=(end - 1) / CHUNK) {
            let lo = first.max(key * CHUNK) - key * CHUNK;
            let hi = end.min(key * CHUNK + CHUNK) - key * CHUNK;
            n += leaf.slots[lo as usize..hi as usize]
                .iter()
                .filter(|&&v| v != NONE && v != HOLE)
                .count() as u64;
        }
        n
    }

    /// Remove the frames of `first .. end`, up to `out.len()` of them, into `out` (in page order); returns how many, and the page to continue from
    /// (`end` when the range is done). Holes in the range stay. Never allocates: the batch is the caller's (a chunked decommit takes 64 at a time).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: n < out.len() when a frame is stored; the page numbers are below end
    pub fn take_frames_in(&mut self, first: u64, end: u64, out: &mut [Pfn]) -> (usize, u64) {
        let mut n = 0;
        let mut at = first;
        while at < end {
            let Some((pg, e)) = self.next_entry_at_or_after(at) else {
                return (n, end);
            };
            if pg >= end {
                return (n, end);
            }
            if let Entry::Frame(_) = e {
                if n == out.len() {
                    return (n, pg);
                }
                if let Some(pfn) = self.remove(pg) {
                    out[n] = pfn;
                    n += 1;
                }
            }
            at = pg + 1;
        }
        (n, end)
    }

    /// Does the leaf that holds `page` exist (an insert there needs no new leaf)?
    pub fn has_leaf(&self, page: u64) -> bool {
        self.leaves.contains_key(&(page / CHUNK))
    }

    /// Remove whatever the map says about `page`, a frame or a hole, and return it. A leaf left empty is freed (`leaf_count` falls). Never
    /// allocates.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; len is at least 1 when a frame is removed; used is at least 1 in a leaf
    pub fn clear(&mut self, page: u64) -> Entry {
        let key = page / CHUNK;
        let Some(leaf) = self.leaves.get_mut(&key) else {
            return Entry::None;
        };
        let slot = &mut leaf.slots[(page % CHUNK) as usize];
        let v = *slot;
        if v == NONE {
            return Entry::None;
        }
        *slot = NONE;
        leaf.used -= 1;
        let gone = leaf.used == 0;
        if v != HOLE {
            self.len -= 1;
        }
        if gone {
            self.leaves.remove(&key);
        }
        Self::decode(v)
    }

    /// Remove the frame at `page` (a hole stays). Never allocates.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: the slot index is below CHUNK; len is at least 1 when a frame is removed; used is at least 1 in a leaf
    pub fn remove(&mut self, page: u64) -> Option<Pfn> {
        let key = page / CHUNK;
        let leaf = self.leaves.get_mut(&key)?;
        let slot = &mut leaf.slots[(page % CHUNK) as usize];
        let v = *slot;
        if v == NONE || v == HOLE {
            return None;
        }
        *slot = NONE;
        leaf.used -= 1;
        let gone = leaf.used == 0;
        self.len -= 1;
        if gone {
            self.leaves.remove(&key);
        }
        Some(v)
    }

    /// Call `f` with every frame, and empty the map.
    pub fn drain_pfns(&mut self, mut f: impl FnMut(Pfn)) {
        for leaf in self.leaves.values() {
            for &v in leaf.slots.iter() {
                if v != NONE && v != HOLE {
                    f(v);
                }
            }
        }
        self.leaves.clear();
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;

    #[test]
    fn it_behaves_like_a_map_of_page_to_frame() {
        let mut m = PageMap::new();
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut x = 12345u64;
        // (Miri runs this about forty times slower: the same operations, fewer of them)
        for _ in 0..if cfg!(miri) { 300 } else { 5000 } {
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
