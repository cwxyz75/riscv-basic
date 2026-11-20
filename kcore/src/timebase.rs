//! The clock's rate (PL4b, KERNEL-V1-SPEC 2.3): the `time` counter runs at the frequency the device tree states (`timebase-frequency`;
//! 10 MHz on QEMU `virt`, 24 MHz or 32.768 kHz elsewhere), and nothing in the kernel assumes one. A [`Timebase`] is made once from the
//! frequency and converts counter cycles to nanoseconds with one multiplication and one shift (the clock is read on every deadline check,
//! so no division), and nanoseconds to cycles exactly, rounded up so that a timer is never programmed early.

/// Fixed-point shift of the cycles-to-nanoseconds multiplier.
const SHIFT: u32 = 32;
/// The fastest counter supported (`ns_to_cycles` multiplies a remainder below 10^9 by the rate, which must fit a `u64`).
pub const MAX_HZ: u64 = 4_000_000_000;

/// A counter frequency and the fixed-point multiplier derived from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timebase {
    hz: u64,
    /// `ceil(10^9 * 2^SHIFT / hz)`: nanoseconds per cycle in 32.32 fixed point, rounded up so that the clock never reads earlier than the
    /// exact time (a timer armed by `ns_to_cycles` then never fires before `cycles_to_ns` reaches its deadline).
    mult: u64,
}

impl Timebase {
    /// The timebase of a counter running at `hz`; `None` for 0 or a rate above [`MAX_HZ`].
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: hz is 1 to 4e9, so 10^9 << 32 (about 4.3e18) fits a u64
    pub fn new(hz: u64) -> Option<Timebase> {
        if hz == 0 || hz > MAX_HZ {
            return None;
        }
        let mult = (1_000_000_000u64 << SHIFT).div_ceil(hz);
        Some(Timebase { hz, mult })
    }

    /// The counter frequency in hertz.
    #[inline]
    pub fn hz(&self) -> u64 {
        self.hz
    }

    /// The fixed-point multiplier, for a caller that keeps it in an atomic (see [`Timebase::from_parts`]).
    #[inline]
    pub fn mult(&self) -> u64 {
        self.mult
    }

    /// Rebuild a timebase from the two numbers it is made of (the kernel stores them in atomics).
    #[inline]
    pub fn from_parts(hz: u64, mult: u64) -> Timebase {
        Timebase { hz, mult }
    }

    /// Nanoseconds in `cycles` counter cycles: never less than the floor of `cycles * 10^9 / hz`, and at most one nanosecond plus a relative
    /// `hz / (10^9 * 2^32)` (under 2^-30 at the fastest rate) above it; exact when the rate divides 10^9, as 10 MHz does. Saturates at `u64::MAX`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the product of a u64 and a u64 fits a u128; the shift is below 128
    #[inline]
    pub fn cycles_to_ns(&self, cycles: u64) -> u64 {
        let ns = (u128::from(cycles) * u128::from(self.mult)) >> SHIFT;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// Cycles in `ns` nanoseconds: exactly `ceil(ns * hz / 10^9)`, so a timer armed for the result never fires before the time asked for.
    /// Saturates at `u64::MAX`. (64-bit divisions only: seconds and the remainder are scaled apart.)
    #[allow(clippy::arithmetic_side_effects)]
    // INVARIANT: the remainder is below 10^9 and hz is at most 4e9, so the product is below 4e18; the whole seconds times hz saturates
    #[inline]
    pub fn ns_to_cycles(&self, ns: u64) -> u64 {
        let secs = ns / 1_000_000_000;
        let rem = ns % 1_000_000_000;
        secs.saturating_mul(self.hz)
            .saturating_add((rem * self.hz).div_ceil(1_000_000_000))
    }
}
