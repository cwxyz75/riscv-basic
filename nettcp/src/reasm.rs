//! R, the receive buffer (S9-SPEC 5.6): a ring of `cap` bytes addressed by sequence number from `RCV.NXT`, holding the
//! in-order bytes the application has not read and at most eight out-of-order ranges. First copy wins; nothing is stored
//! beyond the reservation.
use alloc::vec;
use alloc::vec::Vec;

/// The most out-of-order ranges kept; a segment that needs another is dropped.
pub const MAX_RANGES: usize = 8;

pub struct Reasm {
    buf: Vec<u8>,
    /// Ring index of the first unread in-order byte.
    head: usize,
    /// In-order bytes waiting to be read.
    ready: usize,
    /// Out-of-order ranges as `[start, end)` offsets from `RCV.NXT` (so offset 0 is the next byte expected), sorted,
    /// disjoint and not touching (touching ones are merged).
    ranges: Vec<(usize, usize)>,
}

/// What `insert` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inserted {
    /// New in-order bytes (`RCV.NXT` advances by this much).
    pub advanced: usize,
    /// The segment needed a ninth range and was dropped.
    pub dropped_ooo: bool,
    /// It filled (part of) a hole: an ACK is sent at once.
    pub filled_hole: bool,
}

impl Reasm {
    pub fn new(cap: usize) -> Reasm {
        Reasm {
            buf: vec![0u8; cap],
            head: 0,
            ready: 0,
            ranges: Vec::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
    /// In-order bytes the application can read now.
    pub fn readable(&self) -> usize {
        self.ready
    }
    /// Free space measured from `RCV.NXT`: the window we may advertise.
    pub fn free(&self) -> usize {
        self.buf.len() - self.ready
    }
    pub fn ooo_ranges(&self) -> &[(usize, usize)] {
        &self.ranges
    }
    pub fn has_ooo(&self) -> bool {
        !self.ranges.is_empty()
    }

    fn at(&self, off_from_nxt: usize) -> usize {
        (self.head + self.ready + off_from_nxt) % self.buf.len().max(1)
    }

    /// Store `data` that starts `off` bytes after `RCV.NXT` (0 for the next expected byte). Bytes beyond the free space are
    /// cut off; bytes already held (in order or in a range) are not replaced.
    pub fn insert(&mut self, off: usize, data: &[u8]) -> Inserted {
        let mut r = Inserted {
            advanced: 0,
            dropped_ooo: false,
            filled_hole: false,
        };
        let cap = self.free();
        if off >= cap || data.is_empty() {
            return r;
        }
        let data = &data[..data.len().min(cap - off)];
        let (start, end) = (off, off + data.len());
        // work out which parts of [start, end) are new: those not covered by a range
        let mut new_parts: Vec<(usize, usize)> = Vec::new();
        let mut cur = start;
        for &(rs, re) in &self.ranges {
            if re <= cur {
                continue;
            }
            if rs >= end {
                break;
            }
            if rs > cur {
                new_parts.push((cur, rs));
            }
            cur = cur.max(re);
        }
        if cur < end {
            new_parts.push((cur, end));
        }
        if new_parts.is_empty() {
            return r;
        }
        // would this need more ranges than we may keep? (the merged result, counted after the insert)
        let mut merged = self.ranges.clone();
        for &(a, b) in &new_parts {
            merged.push((a, b));
        }
        merged.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(merged.len());
        for (a, b) in merged {
            match out.last_mut() {
                Some(l) if a <= l.1 => l.1 = l.1.max(b),
                _ => out.push((a, b)),
            }
        }
        let first_gap = out.first().is_some_and(|&(a, _)| a == 0);
        let remaining = out.len() - usize::from(first_gap);
        if remaining > MAX_RANGES {
            r.dropped_ooo = true;
            return r;
        }
        for &(a, b) in &new_parts {
            for (i, &byte) in data[a - start..b - start].iter().enumerate() {
                let p = self.at(a + i);
                self.buf[p] = byte;
            }
        }
        let had_ooo = !self.ranges.is_empty();
        self.ranges = out;
        // a range that starts at offset 0 is now in order: it joins the ready bytes
        if let Some(&(0, e)) = self.ranges.first() {
            self.ready += e;
            r.advanced = e;
            self.ranges.remove(0);
            for x in &mut self.ranges {
                x.0 -= e;
                x.1 -= e;
            }
            r.filled_hole = had_ooo;
        }
        r
    }

    /// Copy up to `out.len()` in-order bytes without removing them; returns how many.
    pub fn peek(&self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.ready);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.buf[(self.head + i) % self.buf.len()];
        }
        n
    }

    /// Drop `n` in-order bytes the caller has taken (after `peek`).
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.ready);
        self.head = (self.head + n) % self.buf.len().max(1);
        self.ready -= n;
    }

    /// Copy up to `out.len()` in-order bytes out; returns how many.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.ready);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.buf[(self.head + i) % self.buf.len()];
        }
        self.head = (self.head + n) % self.buf.len().max(1);
        self.ready -= n;
        n
    }
}
