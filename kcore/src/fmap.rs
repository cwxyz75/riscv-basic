//! An ordered map with **fallible growth** (KT9, T177): the `PageMap` idea with keys. A `BTreeMap` allocates a node inside `insert` and cannot
//! say no; this map allocates only in `reserve`, which can, so the kernel decides whether it has the room *before* it commits to anything, and
//! the step that follows (`insert_reserved`) allocates nothing.
//!
//! Layout: an outer `Vec` of chunks, each a sorted `Vec<(K, V)>` made once with room for `CHUNK` entries and never grown. A lookup is two binary
//! searches. An insert into a full chunk splits it into a spare chunk; a removal that empties a chunk, or leaves two neighbours that fit in
//! one, gives a chunk back to the spare list, so **removal never allocates**. Chunks are never empty. `forbid(unsafe_code)`.

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unreachable
)] // INVARIANT: chunk and entry indices come from the map's own searches over non-empty chunks; counts are bounded by memory; a one-element vector is a one-element box

use alloc::vec::Vec;
use core::ops::{Bound, RangeBounds};

/// Entries per chunk.
pub const CHUNK: usize = 16;
/// The most inserts one `reserve` can promise room for.
pub const MAX_RESERVE: usize = 4;

/// An allocation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMem;

/// An ordered map from `K` to `V` whose allocations are all in `reserve`.
#[derive(Debug)]
pub struct FMap<K, V> {
    chunks: Vec<Vec<(K, V)>>,
    /// Empty chunks with room for `CHUNK` entries, for splits and the first insert.
    spare: Vec<Vec<(K, V)>>,
    len: usize,
    /// Inserts that found no reserved room (a bug in the caller: the audits read it).
    short: u32,
}

impl<K, V> Default for FMap<K, V> {
    fn default() -> Self {
        FMap::new()
    }
}

/// A position: chunk and index in it. `(chunks.len(), 0)` is the end.
type Pos = (usize, usize);

impl<K, V> FMap<K, V> {
    pub const fn new() -> FMap<K, V> {
        FMap {
            chunks: Vec::new(),
            spare: Vec::new(),
            len: 0,
            short: 0,
        }
    }
}

impl<K: Ord + Copy, V> FMap<K, V> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts that went on without reserved room (zero in a correct kernel).
    pub fn short_inserts(&self) -> u32 {
        self.short
    }

    /// Chunks in use (a test reads it: the bound is `4 * len / CHUNK + 1`).
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Make sure the next `n` (at most `MAX_RESERVE`) inserts cannot fail and allocate nothing.
    pub fn reserve(&mut self, n: usize) -> Result<(), NoMem> {
        let n = n.min(MAX_RESERVE);
        // (the outer vector and the spare list have room for a split of each of the `n` inserts)
        self.chunks.try_reserve(n).map_err(|_| NoMem)?;
        self.spare
            .try_reserve(MAX_RESERVE.max(n))
            .map_err(|_| NoMem)?;
        while self.spare.len() < n {
            let mut c: Vec<(K, V)> = Vec::new();
            c.try_reserve_exact(CHUNK).map_err(|_| NoMem)?;
            self.spare.push(c); // (room was reserved above)
        }
        Ok(())
    }

    /// `reserve(1)` and then the insert. Returns the value the key had.
    pub fn try_insert(&mut self, k: K, v: V) -> Result<Option<V>, NoMem> {
        self.reserve(1)?;
        Ok(self.insert_reserved(k, v))
    }

    /// Insert using room `reserve` promised. Without it the call still works when it can (`short_inserts` counts the miss) and otherwise the
    /// entry goes into a chunk grown past `CHUNK`, which allocates: only a caller that skipped `reserve` can see that, and the audits flag it.
    pub fn insert_reserved(&mut self, k: K, v: V) -> Option<V> {
        let Some(ci) = self.chunk_for(&k) else {
            // no chunks yet: the first one
            let mut c = self.take_spare();
            c.push((k, v));
            self.chunks.push(c);
            self.len += 1;
            return None;
        };
        let chunk = &mut self.chunks[ci];
        match chunk.binary_search_by(|e| e.0.cmp(&k)) {
            Ok(i) => Some(core::mem::replace(&mut chunk[i].1, v)),
            Err(i) => {
                self.len += 1;
                if chunk.len() < CHUNK {
                    chunk.insert(i, (k, v));
                    return None;
                }
                // split: the upper half goes to a spare chunk placed after this one
                let mut upper = self.take_spare();
                let chunk = &mut self.chunks[ci];
                let half = CHUNK / 2;
                upper.extend(chunk.drain(half..));
                if i <= half {
                    chunk.insert(i, (k, v));
                } else {
                    upper.insert(i - half, (k, v));
                }
                self.chunks.insert(ci + 1, upper);
                None
            }
        }
    }

    fn take_spare(&mut self) -> Vec<(K, V)> {
        self.spare.pop().unwrap_or_else(|| {
            self.short = self.short.saturating_add(1);
            Vec::with_capacity(CHUNK)
        })
    }

    /// The chunk whose range holds or should hold `k`: the first whose last key is at least `k`, else the last chunk.
    fn chunk_for(&self, k: &K) -> Option<usize> {
        if self.chunks.is_empty() {
            return None;
        }
        let ci = self
            .chunks
            .partition_point(|c| c.last().is_some_and(|e| e.0 < *k));
        Some(ci.min(self.chunks.len() - 1))
    }

    /// First position whose key is `>= k` (`strict`: `> k`).
    fn bound_pos(&self, k: &K, strict: bool) -> Pos {
        let ci = self.chunks.partition_point(|c| {
            c.last()
                .is_some_and(|e| if strict { e.0 <= *k } else { e.0 < *k })
        });
        match self.chunks.get(ci) {
            None => (self.chunks.len(), 0),
            Some(c) => (
                ci,
                c.partition_point(|e| if strict { e.0 <= *k } else { e.0 < *k }),
            ),
        }
    }

    fn find(&self, k: &K) -> Option<(usize, usize)> {
        let ci = self.chunk_for(k)?;
        let i = self.chunks[ci].binary_search_by(|e| e.0.cmp(k)).ok()?;
        Some((ci, i))
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        let (c, i) = self.find(k)?;
        Some(&self.chunks[c][i].1)
    }

    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let (c, i) = self.find(k)?;
        Some(&mut self.chunks[c][i].1)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.find(k).is_some()
    }

    /// Remove `k`. Never allocates.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let (ci, i) = self.find(k)?;
        let (_, v) = self.chunks[ci].remove(i);
        self.len -= 1;
        if self.chunks[ci].is_empty() {
            let c = self.chunks.remove(ci);
            self.give_back(c);
        } else {
            // merge with the next chunk when both fit in half a chunk (keeps the chunk count within 4n / CHUNK + 1)
            let half = CHUNK / 2;
            if ci + 1 < self.chunks.len()
                && self.chunks[ci].len() + self.chunks[ci + 1].len() <= half
            {
                let mut next = self.chunks.remove(ci + 1);
                self.chunks[ci].append(&mut next);
                self.give_back(next);
            } else if ci > 0 && self.chunks[ci - 1].len() + self.chunks[ci].len() <= half {
                let mut cur = self.chunks.remove(ci);
                self.chunks[ci - 1].append(&mut cur);
                self.give_back(cur);
            }
        }
        Some(v)
    }

    fn give_back(&mut self, mut c: Vec<(K, V)>) {
        c.clear();
        // (a full spare list drops the chunk: freeing allocates nothing)
        if self.spare.len() < self.spare.capacity() {
            self.spare.push(c);
        }
    }

    /// Change the key of an entry in place (never allocates). `new` must keep the order: it lies between the keys on either side of `old`, as when
    /// a region's start moves up inside its own gap. Returns false (nothing changed) if `old` is absent or `new` would break the order.
    pub fn rekey(&mut self, old: &K, new: K) -> bool {
        let Some((ci, i)) = self.find(old) else {
            return false;
        };
        let prev = if i > 0 {
            self.chunks[ci].get(i - 1).map(|e| e.0)
        } else if ci > 0 {
            self.chunks[ci - 1].last().map(|e| e.0)
        } else {
            None
        };
        let next = self.chunks[ci]
            .get(i + 1)
            .map(|e| e.0)
            .or_else(|| self.chunks.get(ci + 1).and_then(|c| c.first()).map(|e| e.0));
        if prev.is_some_and(|p| p >= new) || next.is_some_and(|n| n <= new) {
            return false;
        }
        self.chunks[ci][i].0 = new;
        true
    }

    /// Every entry, mutably, in key order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.chunks
            .iter_mut()
            .flat_map(|c| c.iter_mut().map(|e| (&e.0, &mut e.1)))
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.iter_mut().map(|(_, v)| v)
    }

    pub fn clear(&mut self) {
        while let Some(c) = self.chunks.pop() {
            self.give_back(c);
        }
        self.len = 0;
    }

    pub fn iter(&self) -> Range<'_, K, V> {
        self.range(..)
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.chunks.first()?.first().map(|e| (&e.0, &e.1))
    }

    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.chunks.last()?.last().map(|e| (&e.0, &e.1))
    }

    /// Entries whose keys lie in `r`, in key order (double-ended).
    pub fn range<R: RangeBounds<K>>(&self, r: R) -> Range<'_, K, V> {
        let front = match r.start_bound() {
            Bound::Unbounded => (0, 0),
            Bound::Included(k) => self.bound_pos(k, false),
            Bound::Excluded(k) => self.bound_pos(k, true),
        };
        let back = match r.end_bound() {
            Bound::Unbounded => (self.chunks.len(), 0),
            Bound::Included(k) => self.bound_pos(k, true),
            Bound::Excluded(k) => self.bound_pos(k, false),
        };
        // (an inverted range is empty)
        let back = if back < front { front } else { back };
        Range {
            map: self,
            front,
            back,
        }
    }
}

/// The iterator of `range` and `iter`.
pub struct Range<'a, K, V> {
    map: &'a FMap<K, V>,
    front: Pos,
    back: Pos,
}

impl<'a, K, V> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        let (ci, i) = self.front;
        let e = &self.map.chunks[ci][i];
        self.front = if i + 1 < self.map.chunks[ci].len() {
            (ci, i + 1)
        } else {
            (ci + 1, 0)
        };
        Some((&e.0, &e.1))
    }
}

impl<K, V> DoubleEndedIterator for Range<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        let (ci, i) = self.back;
        self.back = if i > 0 {
            (ci, i - 1)
        } else {
            (ci - 1, self.map.chunks[ci - 1].len() - 1)
        };
        let e = &self.map.chunks[self.back.0][self.back.1];
        Some((&e.0, &e.1))
    }
}

/// An ordered set with fallible growth.
#[derive(Debug, Default)]
pub struct FSet<K>(FMap<K, ()>);

impl<K: Ord + Copy> FSet<K> {
    pub const fn new() -> FSet<K> {
        FSet(FMap::new())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn contains(&self, k: &K) -> bool {
        self.0.contains_key(k)
    }
    pub fn reserve(&mut self, n: usize) -> Result<(), NoMem> {
        self.0.reserve(n)
    }
    pub fn try_insert(&mut self, k: K) -> Result<bool, NoMem> {
        Ok(self.0.try_insert(k, ())?.is_none())
    }
    pub fn insert_reserved(&mut self, k: K) -> bool {
        self.0.insert_reserved(k, ()).is_none()
    }
    pub fn remove(&mut self, k: &K) -> bool {
        self.0.remove(k).is_some()
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &K> {
        self.0.keys()
    }
    pub fn clear(&mut self) {
        self.0.clear()
    }
}

impl<K: Ord + Copy, V> core::ops::Index<&K> for FMap<K, V> {
    type Output = V;
    /// The value for `k`; panics if there is none (as `BTreeMap`'s index does).
    #[allow(clippy::expect_used)] // INVARIANT: the documented contract of an index
    fn index(&self, k: &K) -> &V {
        self.get(k).expect("no entry for the key")
    }
}
