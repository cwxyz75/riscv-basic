use clockmodel::{ClockModel, Quality};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn unset_counts_from_the_epoch() {
    let c = ClockModel::unset(5_000);
    assert_eq!(c.quality(), Quality::Unset);
    assert_eq!(c.now(5_000), 0);
    assert_eq!(c.now(1_005_000), 1_000_000);
}

#[test]
fn a_reading_sets_the_time_and_it_advances_with_the_monotonic_clock() {
    let mut c = ClockModel::unset(0);
    c.sync(1_700_000_000_000_000_000, 10, Quality::FromRtc);
    assert_eq!(c.quality(), Quality::FromRtc);
    assert_eq!(c.now(10), 1_700_000_000_000_000_000);
    assert_eq!(c.now(1_010), 1_700_000_000_000_001_000);
}

#[test]
fn a_forward_correction_is_a_step() {
    let mut c = ClockModel::unset(0);
    c.sync(1_000, 0, Quality::FromRtc);
    c.sync(9_000, 100, Quality::Synced);
    assert_eq!(c.now(100), 9_000);
    assert_eq!(c.now(150), 9_050);
}

#[test]
fn a_backward_correction_is_slewed_at_half_speed() {
    let mut c = ClockModel::unset(0);
    c.sync(10_000, 0, Quality::FromRtc);
    assert_eq!(c.now(1_000), 11_000);
    // the source says it is 10_500 now: we showed 11_000, 500 ahead
    c.sync(10_500, 1_000, Quality::Synced);
    assert_eq!(c.now(1_000), 11_000, "no step back");
    assert_eq!(c.now(1_200), 11_100, "half speed");
    // 500 of surplus is gone after 1000 ns; from then on it tracks the source exactly
    assert_eq!(c.now(2_000), 11_500);
    assert_eq!(c.now(3_000), 12_500);
}

#[test]
fn extreme_values_do_not_overflow() {
    let mut c = ClockModel::unset(u64::MAX - 10);
    c.sync(u64::MAX - 5, u64::MAX - 10, Quality::FromRtc);
    assert_eq!(c.now(u64::MAX), u64::MAX);
    c.sync(0, u64::MAX, Quality::Synced);
    let _ = c.now(u64::MAX);
    c.sync(u64::MAX, 0, Quality::Synced);
    assert_eq!(c.now(u64::MAX), u64::MAX);
}

/// The properties, over random readings: answers never decrease; the error against the latest source is bounded by the
/// surplus at the reading (and gone after twice that long); a source that is right gives the right answer.
#[test]
fn random_readings_keep_the_properties() {
    for seed in 1..=200u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut c = ClockModel::unset(0);
        let mut mono = 0u64;
        let mut last_shown = 0u64;
        // the truth drifts from monotonic by a random offset that changes at each reading
        let mut truth_off: u64 = 1_000_000_000_000;
        c.sync(truth_off, 0, Quality::FromRtc);
        let mut bound_until: Option<(u64, u64)> = None; // (mono when the surplus is gone, truth_off)
        for _ in 0..400 {
            mono += r.next() % 5_000_000;
            if r.next() % 8 == 0 {
                let shown_before = c.now(mono);
                truth_off = (truth_off as i64 + (r.next() % 2_000_000) as i64 - 1_000_000) as u64;
                c.sync(mono + truth_off, mono, Quality::Synced);
                let surplus = shown_before.saturating_sub(mono + truth_off);
                bound_until = Some((mono + surplus * 2, truth_off));
            }
            let shown = c.now(mono);
            assert!(shown >= last_shown, "seed {seed}: went backwards");
            last_shown = shown;
            if let Some((t, off)) = bound_until {
                if mono >= t {
                    assert_eq!(shown, mono + off, "seed {seed}: did not converge");
                }
            }
        }
    }
}
