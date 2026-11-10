//! The generator behind `random`: xorshift64*, seeded from a fixed constant so
//! runs are repeatable.

#[derive(Debug, Clone)]
pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    pub(crate) fn new() -> Self {
        Rng {
            state: 0x2545_F491_4F6C_DD1D,
        }
    }

    /// A whole number from 0 up to but not including `n` (`n >= 1`).
    pub(crate) fn below(&mut self, n: f64) -> f64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let unit = (bits >> 11) as f64 / 9_007_199_254_740_992.0;
        libm::floor(unit * n)
    }
}
