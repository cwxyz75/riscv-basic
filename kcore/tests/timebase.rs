//! The clock's conversions (PL4b) against exact integer arithmetic, at the frequencies real boards have: 10 MHz (QEMU `virt`), 24 MHz,
//! 25 MHz, 100 MHz, 1 GHz, 32.768 kHz (a low-power crystal) and a few odd ones.
mod common;
use common::{Rng, SEEDS};
use kcore::timebase::{Timebase, MAX_HZ};

/// Rates real boards have, including the ones whose multiplier rounds down when rounded to nearest (12, 19.2, 26 and 48 MHz, 3 GHz).
const RATES: &[u64] = &[
    1,
    32_768,
    1_000_000,
    10_000_000,
    12_000_000,
    19_200_000,
    24_000_000,
    25_000_000,
    26_000_000,
    48_000_000,
    100_000_000,
    333_333_333,
    1_000_000_000,
    3_000_000_000,
    MAX_HZ,
];

/// The rates for one seed: the fixed list and a few random ones.
fn rates(rng: &mut Rng) -> Vec<u64> {
    let mut v = RATES.to_vec();
    for _ in 0..4 {
        v.push(1 + rng.below(MAX_HZ));
    }
    v
}

fn exact_ns(cycles: u64, hz: u64) -> u128 {
    u128::from(cycles) * 1_000_000_000 / u128::from(hz)
}

#[test]
fn rates_outside_the_supported_range_are_refused() {
    assert!(Timebase::new(0).is_none());
    assert!(Timebase::new(MAX_HZ + 1).is_none());
    assert!(Timebase::new(MAX_HZ).is_some());
    assert_eq!(Timebase::new(10_000_000).unwrap().hz(), 10_000_000);
}

#[test]
fn a_rate_that_divides_a_billion_converts_exactly() {
    for hz in [
        1u64,
        1_000,
        1_000_000,
        10_000_000,
        25_000_000,
        100_000_000,
        1_000_000_000,
    ] {
        let t = Timebase::new(hz).unwrap();
        let ns_per = 1_000_000_000 / hz;
        for c in [0u64, 1, 2, 7, 12_345, 10_000_000_000, 1 << 40] {
            let want = u128::from(c) * u128::from(ns_per);
            if want <= u128::from(u64::MAX) {
                assert_eq!(u128::from(t.cycles_to_ns(c)), want, "{hz} Hz, {c} cycles");
            }
        }
        assert_eq!(t.ns_to_cycles(ns_per * 1234), 1234);
    }
}

#[test]
fn cycles_to_ns_is_never_below_the_exact_time_and_close_above_it() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        for hz in rates(&mut rng) {
            let t = Timebase::new(hz).unwrap();
            let c = rng.next() >> rng.below(40);
            let want = exact_ns(c, hz);
            if want > u128::from(u64::MAX) {
                continue; // saturation is its own test
            }
            let got = u128::from(t.cycles_to_ns(c));
            assert!(
                got >= want,
                "{hz} Hz, {c} cycles: got {got} is below the exact {want}"
            );
            assert!(
                got <= want + 1 + (want >> 30),
                "{hz} Hz, {c} cycles: got {got}, exact {want}"
            );
        }
    }
}

#[test]
fn ns_to_cycles_is_the_exact_ceiling_and_a_timer_armed_by_it_is_never_early() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        for hz in rates(&mut rng) {
            let t = Timebase::new(hz).unwrap();
            // times up to years of uptime: the error of a rounded-down multiplier grows with the time, so reach for large values too
            let ns = rng.next() >> (rng.below(40) + 2);
            let want = (u128::from(ns) * u128::from(hz)).div_ceil(1_000_000_000);
            if want > u128::from(u64::MAX) {
                continue;
            }
            let cycles = t.ns_to_cycles(ns);
            assert_eq!(u128::from(cycles), want, "{hz} Hz, {ns} ns");
            // the clock has reached the deadline by the time the counter reaches the cycles the timer was armed for: no early interrupt
            assert!(
                t.cycles_to_ns(cycles) >= ns,
                "{hz} Hz, {ns} ns: the clock reads {} at the timer's cycle",
                t.cycles_to_ns(cycles)
            );
        }
    }
}

#[test]
fn the_clock_is_monotonic_and_a_second_is_a_second() {
    for &hz in RATES {
        let t = Timebase::new(hz).unwrap();
        assert!(
            t.cycles_to_ns(hz) >= 999_999_999 && t.cycles_to_ns(hz) <= 1_000_000_000,
            "{hz} Hz: one second of cycles"
        );
        let mut rng = Rng::new(hz);
        let mut c = 0u64;
        let mut last = 0u64;
        for _ in 0..2000 {
            c = c.saturating_add(rng.below(1 << 20));
            let ns = t.cycles_to_ns(c);
            assert!(ns >= last, "{hz} Hz: time went backwards at {c}");
            last = ns;
        }
    }
}

#[test]
fn huge_values_saturate_instead_of_wrapping() {
    let t = Timebase::new(1).unwrap(); // a 1 Hz counter: a cycle is a second
    assert_eq!(t.cycles_to_ns(u64::MAX), u64::MAX);
    assert_eq!(t.ns_to_cycles(u64::MAX), u64::MAX / 1_000_000_000 + 1);
    let f = Timebase::new(MAX_HZ).unwrap();
    assert_eq!(f.ns_to_cycles(u64::MAX), u64::MAX);
    assert_eq!(Timebase::from_parts(f.hz(), f.mult()), f);
    // from_parts takes the frequency first and the multiplier second: a swap would turn a second into nonsense
    let t = Timebase::new(24_000_000).unwrap();
    let r = Timebase::from_parts(t.hz(), t.mult());
    assert_eq!(r.cycles_to_ns(24_000_000), t.cycles_to_ns(24_000_000));
    assert_eq!(r.ns_to_cycles(1_000_000_000), 24_000_000);
}
