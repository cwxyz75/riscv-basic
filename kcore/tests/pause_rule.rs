//! T284: the pause detector's rule (`kcore::pause`), property-tested against a reference model, and the `##PAUSE` line.
//!
//! The model: a hart's ring is the newest `RING` samples at least `FLOOR_US` late, in the order they were recorded; a sample covers a
//! window when the span `[arrival - late, arrival]` meets it; a window is taken again when a pause later than the test's bound covered
//! it, and the third such window in a row fails the test; a window with no such pause is always kept, however late its own sample.

use kcore::pause::{
    covering, covers, decode, encode, overlap, pack, parse_stall_arg, paused_total, rearm, unpack,
    worst_of, Cover, HartRing, Resampler, Verdict, FLOOR_US, MAX_COVERED_IN_A_ROW, RING,
};

/// A small deterministic generator (xorshift64*), so a failing case can be replayed from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn a_sample_packs_into_one_word_and_back() {
    let mut r = Rng(1);
    for _ in 0..10_000 {
        let (at, late) = (r.below(1 << 40), r.below(1 << 24));
        assert_eq!(unpack(pack(at, late)), (at, late));
    }
    // (a lateness past 24 bits saturates rather than wrapping into the arrival)
    assert_eq!(unpack(pack(5, u64::MAX)), (5, (1 << 24) - 1));
}

#[test]
fn covering_is_overlap_of_the_unrun_span_with_the_window() {
    let mut r = Rng(2);
    for _ in 0..100_000 {
        let (at, late, s) = (r.below(10_000) + 1, r.below(3_000), r.below(10_000));
        let e = s + r.below(3_000);
        // the reference: some instant t lies in both [at - late, at] and [s, e]
        let want = (at.saturating_sub(late)..=at).any(|t| (s..=e).contains(&t));
        assert_eq!(
            covers(at, late, s, e),
            want,
            "at {at} late {late} window [{s}, {e}]"
        );
        // the overlap's length agrees with the same model, as a length
        let len = overlap(at.saturating_sub(late), at, s, e);
        assert!(len <= late && len <= e - s);
        if !want {
            assert_eq!(len, 0);
        }
    }
}

#[test]
fn the_ring_keeps_the_newest_samples_above_the_floor() {
    let mut r = Rng(3);
    for round in 0..200 {
        let ring = HartRing::new();
        let mut model: Vec<(u64, u64)> = Vec::new();
        let mut at = 1_000;
        for _ in 0..r.below(3 * RING as u64) {
            at += 1 + r.below(2_000);
            let late = r.below(2 * FLOOR_US + 50_000);
            ring.record(at, late);
            if late >= FLOOR_US {
                model.push((at, late));
            }
        }
        let kept: Vec<_> = model.iter().rev().take(RING).collect();
        // every window's worst is the model's worst over the kept samples that cover it
        for _ in 0..50 {
            let s = r.below(at + 10_000);
            let e = s + r.below(50_000);
            let want = kept
                .iter()
                .filter(|&&&(a, l)| covers(a, l, s, e))
                .map(|&&(_, l)| l)
                .max()
                .unwrap_or(0);
            assert_eq!(
                ring.worst_in(s, e),
                want,
                "round {round}, window [{s}, {e}]"
            );
            let total: u64 = kept
                .iter()
                .map(|&&(a, l)| overlap(a.saturating_sub(l), a, s, e))
                .sum();
            assert_eq!(
                ring.paused_in(s, e, 0),
                total,
                "round {round}, window [{s}, {e}]"
            );
        }
    }
}

#[test]
fn a_lateness_below_the_floor_is_not_a_pause() {
    let ring = HartRing::new();
    ring.record(10_000, FLOOR_US - 1);
    assert_eq!(ring.worst_in(0, 20_000), 0);
    ring.record(10_000, FLOOR_US);
    assert_eq!(ring.worst_in(0, 20_000), FLOOR_US);
}

#[test]
fn the_worst_of_several_harts_names_the_hart() {
    let rings: Vec<HartRing> = (0..4).map(|_| HartRing::new()).collect();
    assert_eq!(worst_of(&rings, 0, 100_000), None);
    rings[1].record(60_000, 3_000); // (span [57_000, 60_000])
    rings[3].record(52_000, 21_000); // (span [31_000, 52_000])
    rings[2].record(90_000, 30_000); // (span [60_000, 90_000])
    assert_eq!(worst_of(&rings, 40_000, 55_000), Some((3, 21_000)));
    assert_eq!(worst_of(&rings, 51_000, 51_500), Some((3, 21_000)));
    assert_eq!(worst_of(&rings, 58_000, 58_500), Some((1, 3_000)));
    assert_eq!(worst_of(&rings, 59_000, 61_000), Some((2, 30_000)));
    assert_eq!(worst_of(&rings, 53_000, 56_000), None);
    // the time the harts were not run, summed: hart 3's [40_000, 52_000] and hart 1's [57_000, 58_000] inside [40_000, 58_000]
    assert_eq!(paused_total(&rings, 40_000, 58_000, 0), 12_000 + 1_000);
}

#[test]
fn the_answer_of_debug_stat_216_round_trips() {
    assert_eq!(encode(None), 0);
    assert_eq!(decode(0), None);
    for (h, us) in [
        (0usize, 1u64),
        (0, 21_648),
        (3, 5_000_000),
        (63, (1 << 48) - 1),
    ] {
        assert_eq!(decode(encode(Some((h, us)))), Some((h, us)));
    }
    // (a pause of 0 us on hart 0 must not read as "none")
    assert_eq!(decode(encode(Some((0, 0)))), Some((0, 1)));
}

/// The reference model of the re-sampling rule over a sequence of windows: Some(kept samples) or None (the test fails: three covered
/// in a row).
fn model(windows: &[(u64, Option<u64>)], bound: u64, want: usize) -> Option<Vec<u64>> {
    let (mut kept, mut row) = (Vec::new(), 0);
    for &(sample, pause) in windows {
        if kept.len() == want {
            break;
        }
        if pause.is_some_and(|p| p > bound) {
            row += 1;
            if row == 3 {
                return None;
            }
        } else {
            row = 0;
            kept.push(sample);
        }
    }
    Some(kept)
}

#[test]
fn the_resampler_follows_the_model() {
    let mut r = Rng(4);
    let bound = 4_000;
    for case in 0..20_000 {
        let n = 1 + r.below(80) as usize;
        let rate = r.below(100);
        let windows: Vec<(u64, Option<u64>)> = (0..n)
            .map(|_| {
                let sample = r.below(10_000);
                let pause = (r.below(100) < rate).then(|| r.below(30_000));
                (sample, pause)
            })
            .collect();
        let want = 1 + r.below(50) as usize;
        let mut rs = Resampler::new();
        let mut kept = Vec::new();
        let mut gave_up = false;
        for &(sample, pause) in &windows {
            if kept.len() == want {
                break;
            }
            match rs.judge(pause.map(|p| (1, p)), bound) {
                Verdict::Keep => kept.push(sample),
                Verdict::Retake { late_us, .. } => assert!(late_us > bound),
                Verdict::GiveUp { late_us, .. } => {
                    assert!(late_us > bound);
                    gave_up = true;
                    break;
                }
            }
        }
        match model(&windows, bound, want) {
            None => assert!(
                gave_up,
                "case {case}: the model fails, the resampler did not"
            ),
            Some(m) => {
                assert!(
                    !gave_up,
                    "case {case}: the resampler gave up, the model did not"
                );
                assert_eq!(kept, m, "case {case}");
            }
        }
    }
}

#[test]
fn a_late_window_with_no_pause_is_kept_so_a_slow_scheduler_still_fails() {
    // (the planted-delay control's shape: every wake 5 ms late, the interrupts on time; nothing is taken again, the bound fails it)
    let mut rs = Resampler::new();
    for _ in 0..50 {
        assert_eq!(rs.judge(None, 4_000), Verdict::Keep);
        assert_eq!(rs.judge(Some((0, 4_000)), 4_000), Verdict::Keep); // (a pause no later than the bound cannot have failed it)
    }
    assert_eq!(rs.retaken, 0);
}

#[test]
fn three_covered_windows_in_a_row_give_up_and_a_kept_one_starts_again() {
    let mut rs = Resampler::new();
    let p = Some((2, 20_000));
    assert_eq!(
        rs.judge(p, 4_000),
        Verdict::Retake {
            hart: 2,
            late_us: 20_000
        }
    );
    assert_eq!(
        rs.judge(p, 4_000),
        Verdict::Retake {
            hart: 2,
            late_us: 20_000
        }
    );
    assert_eq!(rs.judge(None, 4_000), Verdict::Keep);
    for _ in 1..MAX_COVERED_IN_A_ROW {
        assert!(matches!(rs.judge(p, 4_000), Verdict::Retake { .. }));
    }
    assert_eq!(
        rs.judge(Some((1, 25_000)), 4_000),
        Verdict::GiveUp {
            hart: 1,
            late_us: 25_000
        }
    );
    assert_eq!(rs.worst, Some((1, 25_000)));
    assert_eq!(rs.retaken, 4);
}

#[test]
fn the_kernel_line_word_is_read_exactly() {
    assert_eq!(parse_stall_arg("20:200"), Some((20, 200)));
    for bad in [
        "", "20", "20:", ":200", "0:200", "20:0", "x:1", "20:200:3", "-1:5",
    ] {
        assert_eq!(parse_stall_arg(bad), None, "{bad:?}");
    }
}

#[test]
fn the_pause_line_is_written_and_recognised() {
    let mut out = String::new();
    kcore::testlog::pause(
        &mut out,
        "realtime_thread_stays_responsive_under_load",
        2,
        21_648,
    )
    .unwrap();
    assert_eq!(
        out,
        "##PAUSE realtime_thread_stays_responsive_under_load hart 2 late 21648 us\n"
    );
    assert!(kcore::testlog::is_pause_line(out.trim_end()));
    for bad in [
        "##PAUSE x hart 2 late 5 us extra",
        "##PAUSE x hart two late 5 us",
        "##PAUSE  hart 2 late 5 us",
        "##PAUSE x-y hart 2 late 5 us",
        "##PAUSE x hart 2 late 5 ms",
        "##PAUSE x hart 2 late  us",
        "##DONE pass=1 fail=0",
        "##PAUSE x hart 2 late 5 us\n##DONE pass=1 fail=0",
    ] {
        assert!(!kcore::testlog::is_pause_line(bad), "{bad:?}");
    }
}

#[test]
fn a_re_arm_does_not_hide_an_interrupt_already_due() {
    const NONE: u64 = u64::MAX;
    // a future value is due when it says; nothing was pending
    assert_eq!(rearm(NONE, 500, 100), (500, 0));
    assert_eq!(rearm(300, 500, 100), (500, 0));
    // a past value written when nothing was pending: due from the write (the scheduler's lagging clock is not the hardware's lateness)
    assert_eq!(rearm(NONE, 40, 100), (100, 0));
    assert_eq!(rearm(300, 40, 100), (100, 0));
    // a past value written while an interrupt was already pending: still due from then (a hart back from a pause re-arms its deadline)
    assert_eq!(rearm(60, 60, 100), (60, 0));
    assert_eq!(rearm(60, 90, 100), (60, 0));
    // a write that moves a pending interrupt to the future, or turns the timer off, cancels it: overdue by now - old
    assert_eq!(rearm(60, 500, 100), (500, 40));
    assert_eq!(rearm(60, NONE, 100), (NONE, 40));
    assert_eq!(rearm(100, NONE, 100), (NONE, 0));
    // the model, over random cases
    let mut r = Rng(5);
    for _ in 0..100_000 {
        let now = 1_000 + r.below(1_000);
        let old = if r.below(4) == 0 {
            NONE
        } else {
            r.below(3_000)
        };
        let new = if r.below(8) == 0 {
            NONE
        } else {
            r.below(3_000)
        };
        let (due, missed) = rearm(old, new, now);
        let pending = old != NONE && old <= now;
        if new == NONE || new > now {
            assert_eq!(due, new);
            assert_eq!(missed, if pending { now - old } else { 0 });
        } else {
            assert_eq!(missed, 0);
            assert_eq!(due, if pending { old } else { now });
        }
    }
}

#[test]
fn a_pause_covers_a_window_only_when_two_harts_saw_it() {
    let rings: Vec<HartRing> = (0..4).map(|_| HartRing::new()).collect();
    assert_eq!(covering(&rings, 0, 100_000, 4_000), Cover::None);
    // one hart late: a hart that ran with interrupts off looks the same, so the window is not excused
    rings[0].record(50_000, 9_000);
    assert_eq!(
        covering(&rings, 45_000, 50_000, 4_000),
        Cover::OneHart {
            hart: 0,
            late_us: 9_000
        }
    );
    // a second hart late below the bound does not corroborate it
    rings[2].record(50_100, 3_000);
    assert_eq!(
        covering(&rings, 45_000, 50_000, 4_000),
        Cover::OneHart {
            hart: 0,
            late_us: 9_000
        }
    );
    // a second hart late above it does: the emulator stopped
    rings[3].record(50_200, 12_000);
    assert_eq!(
        covering(&rings, 45_000, 50_000, 4_000),
        Cover::Pause {
            hart: 3,
            late_us: 12_000
        }
    );
    // on a machine of one hart, the one hart is enough
    let one = [HartRing::new()];
    one[0].record(50_000, 9_000);
    assert_eq!(
        covering(&one, 45_000, 50_000, 4_000),
        Cover::Pause {
            hart: 0,
            late_us: 9_000
        }
    );
    // the billing allowance counts only samples late enough to be a stopped hart
    // (hart 3's span [38_200, 50_200] is clipped to the window's start)
    assert_eq!(paused_total(&rings, 40_000, 60_000, 5_000), 9_000 + 10_200);
    assert_eq!(
        paused_total(&rings, 40_000, 60_000, 0),
        9_000 + 3_000 + 10_200
    );
}
