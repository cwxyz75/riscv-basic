//! The clock's conversions (PL4b) against exact integer arithmetic, at the frequencies real boards have: 10 MHz (QEMU `virt`), 24 MHz,
//! 25 MHz, 100 MHz, 1 GHz, 32.768 kHz (a low-power crystal) and a few odd ones.
mod common;
use common::{Rng, SEEDS};
use kcore::timebase::{Timebase, MAX_HZ};

const RATES: &[u64] = &[
    1,
    32_768,
    1_000_000,
    10_000_000,
    24_000_000,
    25_000_000,
    100_000_000,
    333_333_333,
    1_000_000_000,
    MAX_HZ,
];

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
fn cycles_to_ns_stays_within_a_nanosecond_and_a_relative_two_to_the_minus_thirty() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        for &hz in RATES {
            let t = Timebase::new(hz).unwrap();
            let c = rng.next() >> rng.below(40);
            let want = exact_ns(c, hz);
            let got = u128::from(t.cycles_to_ns(c));
            if want > u128::from(u64::MAX) {
                continue; // saturation is its own test
            }
            let slack = 1 + (want >> 30);
            assert!(
                got.abs_diff(want) <= slack,
                "{hz} Hz, {c} cycles: got {got}, exact {want}"
            );
        }
    }
}

#[test]
fn ns_to_cycles_is_the_exact_ceiling_and_never_early() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        for &hz in RATES {
            let t = Timebase::new(hz).unwrap();
            let ns = rng.next() >> rng.below(36);
            let want = (u128::from(ns) * u128::from(hz)).div_ceil(1_000_000_000);
            if want > u128::from(u64::MAX) {
                continue;
            }
            assert_eq!(u128::from(t.ns_to_cycles(ns)), want, "{hz} Hz, {ns} ns");
            // converting back never lands earlier than the time asked for (the one nanosecond of rounding aside)
            assert!(
                u128::from(t.cycles_to_ns(t.ns_to_cycles(ns))) + 1 >= u128::from(ns)
                    || ns > 1 << 60,
                "{hz} Hz, {ns} ns"
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
}
