//! The starvation watchdog's pure check (`Sched2::starved`, SCHED-SMP-SPEC 3.9) and the idle loop's question (`idle_has_work`).
//! A thread queued for longer than the limit while a hart that may run it idles is starvation: what a dropped poke leaves behind.
use kcore::sched2::{FixedPrio, SchedAttr, MS};
use kcore::sched_smp::Sched2;

const LIMIT: u64 = 500 * MS;

fn now_of(s: &Sched2<FixedPrio>) -> u64 {
    s.now_on(0)
}

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

/// Two harts; hart 0 runs the boot thread; a thread pinned to hart 1 is spawned from hart 0, which marks hart 1 (one poke owed).
/// Nobody delivers it (the lost IPI): hart 1 idles with the thread queued.
fn lost_poke() -> (Sched2<FixedPrio>, u64) {
    let s = Sched2::new(2, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    let t = s.spawn_on(0, attr(10), Some(1)).unwrap();
    let _lost = s.take_pokes();
    (s, t)
}

#[test]
fn a_thread_left_queued_while_its_hart_idles_is_reported_after_the_limit() {
    let (s, t) = lost_poke();
    assert!(
        s.idle_has_work(1),
        "hart 1 is marked and has a queued thread"
    );
    assert_eq!(
        s.starved(now_of(&s), LIMIT),
        None,
        "not yet: it has not waited"
    );
    for h in 0..2 {
        s.set_clock(h, 600 * MS);
    }
    assert_eq!(s.starved(now_of(&s), LIMIT), Some((t, 1, 1)));
}

#[test]
fn a_thread_that_runs_is_not_starved_and_an_idle_hart_with_nothing_has_no_work() {
    let (s, _t) = lost_poke();
    for h in 0..2 {
        s.set_clock(h, 600 * MS);
    }
    s.reschedule_on(1); // the poke is served after all
    s.switched(1);
    assert_eq!(s.starved(now_of(&s), LIMIT), None, "it is running now");
    assert!(!s.idle_has_work(1));
    assert!(!s.idle_has_work(0));
}

#[test]
fn a_thread_waiting_behind_a_busy_hart_is_not_starvation() {
    let s = Sched2::new(2, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    // pinned to the busy hart 0, lower priority than what runs there: it waits its turn, and hart 1 may not run it
    s.spawn_on(0, attr(10), Some(0)).unwrap();
    for h in 0..2 {
        s.set_clock(h, 600 * MS);
    }
    assert_eq!(s.starved(now_of(&s), LIMIT), None);
}

// --- T266 (T197): the progress rule. A finding counts only when the hart the thread is queued on took an interrupt since the last
// counted sample; a sample where it took none is void (a hart the host did not run), and a hart quiet for STALL_NS is a stalled hart.

use kcore::watchdog::{Verdict, Watchdog, REPORT_AT, STALL_NS};

const PERIOD: u64 = 100 * MS;

/// Run `samples` samples 100 ms apart from 600 ms on (the thread already late), all finding thread 7 queued on hart 1 with hart 2
/// idle; `ticks(i)` is hart 1's progress counter at sample `i`. Returns the verdict of each sample.
fn run(samples: usize, ticks: impl Fn(usize) -> u64) -> Vec<Verdict> {
    let mut w = Watchdog::default();
    (0..samples)
        .map(|i| {
            let now = 600 * MS + i as u64 * PERIOD;
            let p = ticks(i);
            w.sample(now, Some((7, 1, 2)), |h| {
                if h == 1 {
                    p
                } else {
                    1000 + i as u64
                }
            })
        })
        .collect()
}

const STARVED: Verdict = Verdict::Starved {
    tid: 7,
    on: 1,
    idle: 2,
};

#[test]
fn a_thread_on_a_hart_that_did_not_run_is_not_reported() {
    // (i) hart 1's counter never moves over five samples (the host did not run it): no report
    let v = run(5, |_| 42);
    assert!(v.iter().all(|&x| x == Verdict::Quiet), "{v:?}");
}

#[test]
fn a_thread_on_a_hart_that_runs_is_reported_at_the_third_sample() {
    // (ii) hart 1 ticks between every two samples and still leaves the thread queued in front of an idle hart: the third sample reports
    let v = run(5, |i| 10 + i as u64);
    assert_eq!(&v[..3], &[Verdict::Quiet, Verdict::Quiet, STARVED]);
    assert_eq!(REPORT_AT, 3);
}

#[test]
fn a_void_sample_neither_counts_nor_resets() {
    // (iii) progress, no progress, progress: the void sample in the middle neither counts nor resets the two that did
    let ticks = [10, 11, 11, 12, 12];
    let v = run(5, |i| ticks[i]);
    assert_eq!(
        &v[..4],
        &[Verdict::Quiet, Verdict::Quiet, Verdict::Quiet, STARVED],
        "{v:?}"
    );
}

#[test]
fn a_hart_quiet_for_ten_seconds_is_reported_as_stalled() {
    // a hart stuck with interrupts off (or a lost poke on a hart with nothing armed) is still a defect, reported as a stalled hart
    let samples = (STALL_NS / PERIOD) as usize + 1;
    let v = run(samples, |_| 5);
    assert!(v[..samples - 1].iter().all(|&x| x == Verdict::Quiet));
    assert_eq!(
        v[samples - 1],
        Verdict::Stalled {
            tid: 7,
            on: 1,
            quiet_ns: STALL_NS
        }
    );
}

#[test]
fn another_thread_or_hart_starts_the_count_again_and_nothing_found_resets() {
    let mut w = Watchdog::default();
    let mut t = 0u64;
    let mut at = |w: &mut Watchdog, found, p: u64| {
        t += PERIOD;
        w.sample(t, found, |_| p)
    };
    assert_eq!(at(&mut w, Some((7, 1, 2)), 1), Verdict::Quiet);
    assert_eq!(at(&mut w, Some((7, 1, 2)), 2), Verdict::Quiet);
    // another thread: a new count
    assert_eq!(at(&mut w, Some((8, 1, 2)), 3), Verdict::Quiet);
    assert_eq!(at(&mut w, Some((8, 1, 2)), 4), Verdict::Quiet);
    // the same thread on another hart: a new count
    assert_eq!(at(&mut w, Some((8, 0, 2)), 5), Verdict::Quiet);
    assert_eq!(at(&mut w, Some((8, 0, 2)), 6), Verdict::Quiet);
    // nothing found: reset
    assert_eq!(at(&mut w, None, 7), Verdict::Quiet);
    assert_eq!(at(&mut w, Some((8, 0, 2)), 8), Verdict::Quiet);
    assert_eq!(at(&mut w, Some((8, 0, 2)), 9), Verdict::Quiet);
    assert!(matches!(
        at(&mut w, Some((8, 0, 2)), 10),
        Verdict::Starved { tid: 8, .. }
    ));
}

#[test]
fn the_state_survives_its_words() {
    let mut w = Watchdog::default();
    w.sample(5, Some((9, 3, 0)), |_| 77);
    assert_eq!(Watchdog::from_words(w.to_words()), w);
}

/// A property over random progress sequences: the rule reports Starved exactly at the first sample where the hart's counter has
/// changed at REPORT_AT - 1 samples since the first sighting, and never before.
#[test]
fn the_rule_counts_exactly_the_samples_with_progress() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..2000 {
        let n = 1 + (rnd() % 12) as usize;
        let mut ticks = vec![rnd() % 4];
        for _ in 1..n {
            let last = *ticks.last().unwrap();
            ticks.push(last + u64::from(rnd() % 3 == 0));
        }
        let v = run(n, |i| ticks[i]);
        let mut moved = 0;
        for i in 0..n {
            if i > 0 && ticks[i] != ticks[i - 1] {
                moved += 1;
            }
            let want = if moved + 1 >= REPORT_AT {
                STARVED
            } else {
                Verdict::Quiet
            };
            assert_eq!(v[i], want, "ticks {ticks:?}, sample {i}");
            if want != Verdict::Quiet {
                break;
            }
        }
    }
}
