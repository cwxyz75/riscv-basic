//! The delay generator of schedule perturbation (KT7): a per-hart xorshift stream seeded from the boot argument `chaos=<seed>` and
//! the hart's index. At a hook point the kernel asks `next_delay`: one time in eight the answer is a number of `spin_loop`
//! iterations (0 to 2000) to burn, which shifts the harts' relative timing so that races QEMU's regular scheduling hides show up.
//! The same seed and hart give the same sequence of answers (the host test below), so a failing seed is a reproducible hunt, not a
//! one-off.

/// The most spin iterations one hook adds.
pub const MAX_SPINS: u32 = 2000;
/// One hook point in this many delays, at the rare points (the IPI, the switch, the return to user mode).
pub const ONE_IN: u64 = 8;
/// ... and at the lock points, which are hit a hundred times as often: a long critical section has many lock operations in it, and a delay at
/// one in eight of them stretches it past the big lock's 30 million spin watchdog and the harness's stall window (measured, KT7).
pub const ONE_IN_LOCKS: u64 = 64;

/// One hart's generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chaos {
    state: u64,
}

impl Chaos {
    /// A generator for `hart` under `seed`. (The state is never 0, which xorshift cannot leave.)
    pub fn new(seed: u64, hart: usize) -> Chaos {
        // splitmix64 over (seed, hart) so that neighbouring seeds and harts start far apart
        let mut z = seed
            .wrapping_add((hart as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        Chaos {
            state: if z == 0 { 0x2545_f491_4f6c_dd1d } else { z },
        }
    }

    /// The state as one word, for a per-hart slot that is read and written atomically.
    pub fn to_raw(self) -> u64 {
        self.state
    }

    /// A generator resumed from `to_raw`.
    pub fn from_raw(state: u64) -> Chaos {
        Chaos {
            state: if state == 0 {
                0x2545_f491_4f6c_dd1d
            } else {
                state
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// At a hook point: `Some(n)` (0 to `MAX_SPINS`) spin iterations with probability 1/`ONE_IN`, else `None`.
    pub fn next_delay(&mut self) -> Option<u32> {
        self.next_delay_one_in(ONE_IN)
    }

    /// `next_delay` with the chance 1/`one_in`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the modulus is MAX_SPINS + 1, a nonzero constant far below u64::MAX
    pub fn next_delay_one_in(&mut self, one_in: u64) -> Option<u32> {
        let r = self.next();
        if !r.is_multiple_of(one_in) {
            return None;
        }
        Some(((r >> 8) % (u64::from(MAX_SPINS) + 1)) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn run(seed: u64, hart: usize, n: usize) -> Vec<Option<u32>> {
        let mut c = Chaos::new(seed, hart);
        (0..n).map(|_| c.next_delay()).collect()
    }

    #[test]
    fn the_lock_points_delay_about_one_in_sixty_four() {
        let mut c = Chaos::new(5, 0);
        let hits = (0..128_000)
            .filter(|_| c.next_delay_one_in(ONE_IN_LOCKS).is_some())
            .count();
        assert!(
            (1_700..=2_300).contains(&hits),
            "{hits} delays in 128000 lock hooks (expect about 2000)"
        );
    }

    #[test]
    fn the_same_seed_and_hart_give_the_same_sequence() {
        assert_eq!(run(7, 0, 1000), run(7, 0, 1000));
        assert_eq!(run(7, 3, 1000), run(7, 3, 1000));
    }

    #[test]
    fn a_resumed_generator_continues_the_same_sequence() {
        let mut a = Chaos::new(9, 1);
        let first: Vec<_> = (0..50).map(|_| a.next_delay()).collect();
        let mut b = Chaos::from_raw(Chaos::new(9, 1).to_raw());
        let again: Vec<_> = (0..50).map(|_| b.next_delay()).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn another_seed_or_hart_gives_another_sequence() {
        assert_ne!(run(7, 0, 1000), run(8, 0, 1000));
        assert_ne!(run(7, 0, 1000), run(7, 1, 1000));
        assert_ne!(
            run(0, 0, 1000),
            run(1, 0, 1000),
            "seed 0 is a seed like any other"
        );
    }

    #[test]
    fn about_one_in_eight_delays_and_never_past_the_bound() {
        for seed in [0, 1, 2, 99, u64::MAX] {
            let v = run(seed, 2, 80_000);
            let hits = v.iter().flatten().count();
            assert!(
                (9_000..=11_000).contains(&hits),
                "seed {seed}: {hits} delays in 80000 hooks (expect about 10000)"
            );
            assert!(v.iter().flatten().all(|&n| n <= MAX_SPINS));
            let longest = v.iter().flatten().copied().max().unwrap();
            assert!(
                longest > MAX_SPINS * 9 / 10,
                "seed {seed}: the longest delay was only {longest}"
            );
        }
    }
}
