//! `kcore::jobsched`: per-job CPU and real-time accounting (task 8) against a straight-line reference, and under threads.

use kcore::jobsched::JobSchedTable;
use kcore::sched2::{MS, RT_WINDOW};
use std::sync::Arc;

/// The reference: every interval is split at window boundaries and summed per window.
fn reference_rt(intervals: &[(u64, u64)], window: u64) -> u64 {
    intervals
        .iter()
        .map(|&(end, ran)| {
            let start = end - ran;
            let lo = start.max(window * RT_WINDOW);
            end.saturating_sub(lo)
                .min(end.saturating_sub(window * RT_WINDOW))
        })
        .sum()
}

#[test]
fn cpu_is_the_sum_of_what_ran_and_rt_is_the_part_in_the_window() {
    let t = JobSchedTable::new();
    // a seeded walk of intervals, all of one job, with RT and normal time mixed
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut now = 5 * MS;
    let mut all = Vec::new();
    let (mut total, mut rt_total_ns) = (0u64, 0u64);
    for _ in 0..5000 {
        let ran = 1 + next() % (30 * MS);
        now += ran + next() % (5 * MS);
        let rt = next() % 3 != 0;
        t.charge(7, ran, rt, now);
        total += ran;
        if rt {
            all.push((now, ran));
            rt_total_ns += ran;
        }
        assert_eq!(t.cpu(7), total);
        let w = now / RT_WINDOW;
        assert_eq!(
            t.rt_used(7, now),
            reference_rt(&all, w),
            "rt used in window {w} after interval ending {now}"
        );
    }
    assert!(rt_total_ns > 0);
    assert_eq!(t.cpu(8), 0, "another job is untouched");
    // a window later with no charge reads zero
    assert_eq!(t.rt_used(7, now + 2 * RT_WINDOW), 0);
}

#[test]
fn a_reset_slot_starts_from_nothing() {
    let t = JobSchedTable::new();
    t.charge(3, 4 * MS, true, 10 * MS);
    t.reset(3);
    assert_eq!((t.cpu(3), t.rt_used(3, 10 * MS)), (0, 0));
}

#[test]
fn an_out_of_range_job_is_ignored() {
    let t = JobSchedTable::new();
    t.charge(u32::MAX, MS, true, 5 * MS);
    assert_eq!(t.cpu(u32::MAX), 0);
}

#[test]
fn concurrent_charges_lose_nothing_across_a_window_boundary() {
    for _ in 0..50 {
        let t = Arc::new(JobSchedTable::new());
        // four "harts" charge one job: each interval ends just after the boundary at 2 * RT_WINDOW and lies wholly in the new window
        let end = 2 * RT_WINDOW + 10 * MS;
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let t = t.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        t.charge(1, MS, true, end);
                    }
                })
            })
            .collect();
        // (and a charge in the old window racing them: it must not resurrect the old window's figure)
        t.charge(1, MS, true, 2 * RT_WINDOW - MS);
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(t.cpu(1), 4000 * MS + MS);
        let used = t.rt_used(1, end);
        // the old-window charge may land first (then the new window starts from zero) or last (then it is dropped): either way the new window
        // holds all 4000 intervals unless the racer's CAS replaced the word after some of them, which the loop rules out
        assert!(used == 4000 * MS, "new window: {used}");
    }
}
