//! Shared helpers for the model tests: a tiny deterministic random number generator (the seed is in every assertion message, so
//! a failing run can be replayed) and the run sizes.
#![allow(dead_code)]

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

/// How many random sequences to run, and how long each is (fewer under Miri, which is slow).
pub const SEEDS: u64 = if cfg!(miri) { 4 } else { 200 };
pub const STEPS: u64 = if cfg!(miri) { 40 } else { 300 };
