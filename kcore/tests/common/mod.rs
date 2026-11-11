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
