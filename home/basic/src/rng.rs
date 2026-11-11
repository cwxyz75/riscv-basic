//! The pseudo-random generator behind `RND`: xorshift64*, with seeds scrambled
//! through splitmix64 so that nearby seeds give unrelated sequences. Only the
//! properties in `BASIC.md` are promised, not the actual numbers.

const INITIAL_STATE: u64 = 0x2545_F491_4F6C_DD1D;
/// 2^53, so that `bits >> 11` maps onto [0, 1) exactly.
const TWO_POW_53: f64 = 9_007_199_254_740_992.0;

#[derive(Debug, Clone)]
pub(crate) struct Rng {
    state: u64,
    last: f64,
}

impl Rng {
    pub(crate) fn new() -> Self {
        Rng {
            state: INITIAL_STATE,
            last: 0.0,
        }
    }

    /// Restart the sequence from `seed`: the same seed always gives the same
    /// sequence.
    pub(crate) fn reseed(&mut self, seed: f64) {
        let mut z = seed.to_bits().wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // xorshift must never be all zeros.
        self.state = if z == 0 { INITIAL_STATE } else { z };
    }

    /// The next number of the sequence, from 0 up to but not including 1.
    pub(crate) fn generate(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let bits = x.wrapping_mul(INITIAL_STATE);
        self.last = (bits >> 11) as f64 / TWO_POW_53;
        self.last
    }

    /// The number most recently returned.
    pub(crate) fn last(&self) -> f64 {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_in_range_and_varies() {
        let mut rng = Rng::new();
        let values: [f64; 200] = core::array::from_fn(|_| rng.generate());
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        assert_ne!(values[0], values[1]);
    }

    #[test]
    fn same_seed_same_sequence_different_seed_different() {
        let (mut a, mut b, mut c) = (Rng::new(), Rng::new(), Rng::new());
        a.reseed(-5.0);
        b.reseed(-5.0);
        c.reseed(-6.0);
        assert_eq!(a.generate(), b.generate());
        assert_ne!(a.generate(), c.generate());
    }

    #[test]
    fn last_repeats_the_previous_number() {
        let mut rng = Rng::new();
        let v = rng.generate();
        assert_eq!(rng.last(), v);
    }

    #[test]
    fn zero_seed_does_not_stick() {
        let mut rng = Rng::new();
        rng.reseed(f64::from_bits(0u64.wrapping_sub(0x9E37_79B9_7F4A_7C15)));
        assert_ne!(rng.generate(), rng.generate());
    }
}
