//! Shared helpers for the model tests: a fake physical memory and a tiny
//! deterministic random number generator (seeds are printed on failure so any
//! failing run can be replayed).
#![allow(dead_code)]

use kcore::frame::Pfn;
use kcore::pagetable::PhysMem;
use std::collections::HashMap;

/// Physical memory as a map from frame number to 512 words, created on demand.
#[derive(Default)]
pub struct FakeMem {
    frames: HashMap<Pfn, Box<[u64; 512]>>,
}

impl PhysMem for FakeMem {
    fn read_u64(&self, pa: u64) -> u64 {
        self.frames
            .get(&(pa >> 12))
            .map_or(0xdead_beef_dead_beef, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write_u64(&mut self, pa: u64, value: u64) {
        let frame = self
            .frames
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0xa5a5_a5a5_a5a5_a5a5; 512]));
        frame[((pa & 0xfff) / 8) as usize] = value;
    }
}

/// xorshift64*: deterministic, good enough for choosing operations.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// How many random sequences to run (fewer under Miri, which is slow).
pub const SEEDS: u64 = if cfg!(miri) { 5 } else { 300 };
pub const STEPS: u64 = if cfg!(miri) { 60 } else { 400 };

/// A second page-table format, modelled on AArch64's descriptors, that exists only to prove the page-table and address-space code
/// is generic (AR1c): four levels, 48-bit addresses, the "table or page" bit at bit 1 (so a leaf above level 0 is a block with
/// that bit clear), read-only and user bits that mean the opposite of RISC-V's, an execute-never bit, and the frame number at
/// bit 12. A software bit (5) carries "readable" so execute-only mappings round-trip.
pub struct TestFmt48;

impl kcore::pagetable::PageTableFormat for TestFmt48 {
    const LEVELS: u32 = 4;
    fn is_valid(pte: u64) -> bool {
        pte & 1 != 0
    }
    fn is_leaf(pte: u64, level: u32) -> bool {
        if level == 0 {
            pte & 2 != 0
        } else {
            pte & 2 == 0
        }
    }
    fn pfn_of(pte: u64) -> Pfn {
        (pte >> 12) & ((1 << 36) - 1)
    }
    fn table_entry(pfn: Pfn) -> u64 {
        (pfn << 12) | 3
    }
    fn leaf_entry(pfn: Pfn, p: kcore::pagetable::Perms) -> u64 {
        let mut pte = (pfn << 12) | 3 | (1 << 10);
        if p.read {
            pte |= 1 << 5;
        }
        if !p.write {
            pte |= 1 << 7;
        }
        if p.user {
            pte |= 1 << 6;
        }
        if !p.exec {
            pte |= 1 << 54;
        }
        pte
    }
    fn block_entry(pfn: Pfn, p: kcore::pagetable::Perms, _level: u32) -> u64 {
        // a block has the "table" bit clear
        Self::leaf_entry(pfn, p) & !2
    }
    fn perms_of(pte: u64) -> kcore::pagetable::Perms {
        kcore::pagetable::Perms {
            read: pte & (1 << 5) != 0,
            write: pte & (1 << 7) == 0,
            exec: pte & (1 << 54) == 0,
            user: pte & (1 << 6) != 0,
        }
    }
    fn is_canonical(va: u64) -> bool {
        let top = (va as i64) >> 47;
        top == 0 || top == -1
    }
    fn extend(va: u64) -> u64 {
        if va & (1 << 47) != 0 {
            va | 0xffff_8000_0000_0000
        } else {
            va
        }
    }
}

/// A tiny flattened-device-tree writer.
pub struct Dt {
    pub strings: Vec<u8>,
    pub structure: Vec<u8>,
    pub rsv: Vec<(u64, u64)>,
}

impl Dt {
    pub fn new() -> Dt {
        Dt {
            strings: vec![],
            structure: vec![],
            rsv: vec![],
        }
    }
    pub fn pad(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
    }
    pub fn begin(&mut self, name: &str) {
        self.structure.extend(1u32.to_be_bytes());
        self.structure.extend(name.as_bytes());
        self.structure.push(0);
        Self::pad(&mut self.structure);
    }
    pub fn end(&mut self) {
        self.structure.extend(2u32.to_be_bytes());
    }
    pub fn prop(&mut self, name: &str, val: &[u8]) {
        let off = self.strings.len() as u32;
        self.strings.extend(name.as_bytes());
        self.strings.push(0);
        self.structure.extend(3u32.to_be_bytes());
        self.structure.extend((val.len() as u32).to_be_bytes());
        self.structure.extend(off.to_be_bytes());
        self.structure.extend(val);
        Self::pad(&mut self.structure);
    }
    pub fn u32s(&mut self, name: &str, v: &[u32]) {
        let b: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
        self.prop(name, &b);
    }
    /// `reg` of one (address, size) pair in one-cell form.
    pub fn reg_1cell(&mut self, a: u32, s: u32) {
        let mut b = vec![];
        b.extend(a.to_be_bytes());
        b.extend(s.to_be_bytes());
        self.prop("reg", &b);
    }
    /// `reg` bytes as given.
    pub fn raw_reg(&mut self, b: &[u8]) {
        self.prop("reg", b);
    }
    /// `reg` of (address, size) pairs in two-cell form.
    pub fn reg(&mut self, ranges: &[(u64, u64)]) {
        let mut b = vec![];
        for &(a, s) in ranges {
            b.extend(a.to_be_bytes());
            b.extend(s.to_be_bytes());
        }
        self.prop("reg", &b);
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.structure.extend(9u32.to_be_bytes());
        let mut rsv = vec![];
        for &(a, s) in &self.rsv {
            rsv.extend(a.to_be_bytes());
            rsv.extend(s.to_be_bytes());
        }
        rsv.extend([0u8; 16]);
        let off_rsv = 40;
        let off_struct = off_rsv + rsv.len();
        let off_strings = off_struct + self.structure.len();
        let total = off_strings + self.strings.len();
        let mut b = vec![];
        for x in [
            0xd00d_feedu32,
            total as u32,
            off_struct as u32,
            off_strings as u32,
            off_rsv as u32,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            b.extend(x.to_be_bytes());
        }
        b.extend(rsv);
        b.extend(self.structure);
        b.extend(self.strings);
        b
    }
}
