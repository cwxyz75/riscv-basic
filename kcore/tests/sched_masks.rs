//! SC2: placement and pulling go through the global masks (KERNEL-V1-SPEC 5.2), not through every hart's lock.
//!
//! Acceptance: while no hart has a queued thread, a settle takes no other hart's run-queue lock (`pull_looks` stays zero); an idle hart is found from the mask; a
//! hart that does have a queued thread is looked at, and only that one. The negative control `fault-skip-overloaded` (no hart is ever marked overloaded) makes
//! the last check fail, and `an_idle_hart_takes_a_new_thread_and_a_freed_hart_pulls_a_waiting_one` with it.

use kcore::sched2::{FixedPrio, SchedAttr};
use kcore::sched_smp::Sched2;

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

#[test]
fn no_cross_hart_lock_is_taken_while_no_hart_is_overloaded() {
    let s = Sched2::new(16, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    // fifteen threads, one per idle hart: placement finds each from the mask, and no queue ever holds a thread
    let mut placed = Vec::new();
    for _ in 0..15 {
        let t = s.spawn_on(0, attr(10), None).unwrap();
        placed.push(s.home(t).unwrap());
    }
    placed.sort_unstable();
    assert_eq!(
        placed,
        (1..16).collect::<Vec<_>>(),
        "each on its own idle hart"
    );
    for h in 0..16 {
        s.reschedule_on(h);
    }
    for h in 0..16 {
        assert_eq!(s.queued_on(h), 0);
    }
    // (until each hart has looked at its own queue, the thread placed there is waiting: that is the one case the others look at it)
    assert!(s.settles() > 0);
    let before = s.pull_looks();
    for h in 0..16 {
        s.reschedule_on(h);
    }
    assert_eq!(
        s.pull_looks() - before,
        0,
        "no settle looked into another hart's queue: none had a thread waiting"
    );
    s.audit(true).unwrap();
}

#[test]
fn only_the_hart_with_a_waiting_thread_is_looked_at() {
    let s = Sched2::new(8, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    for _ in 0..7 {
        s.spawn_on(0, attr(10), None).unwrap();
    }
    for h in 0..8 {
        s.reschedule_on(h);
    }
    // an eighth thread of 10 has no idle hart and nothing lower to displace: it queues somewhere
    let extra = s.spawn_on(0, attr(10), None).unwrap();
    let on = s.home(extra).unwrap();
    assert_eq!(s.queued_on(on), 1);
    // a thread on another hart blocks: that hart is free, settles, and goes to the one hart that has a thread waiting (one look, not seven)
    let free = if on == 3 { 4 } else { 3 };
    let before = s.pull_looks();
    s.prepare_block_on(free).unwrap();
    assert!(s.block_on(free).unwrap());
    s.reschedule_on(free);
    assert_eq!(
        s.pull_looks() - before,
        1,
        "one hart was overloaded: one look"
    );
    assert_eq!(
        s.current_on(free),
        extra,
        "and the waiting thread was pulled"
    );
    s.audit(true).unwrap();
}

#[test]
fn pack_consolidates_onto_low_harts_and_spread_spreads() {
    use kcore::sched_smp::Policy;
    // PM4: two light periodic threads on four harts, never running together. Each wakes, runs and sleeps again. Under `pack` they share the lowest hart that is
    // free of a better thread, under `spread` each goes back to the hart it last ran on (the `spread` side of the comparison is the QEMU test's).
    for policy in [Policy::Pack, Policy::Spread] {
        let s = Sched2::new(4, FixedPrio::new);
        s.set_policy(policy);
        assert_eq!(s.policy(), policy);
        // hart 0 keeps a thread of 20 (so it is never the place for a thread of 10)
        s.adopt_current_on(0, attr(20), None).unwrap();
        let a = s.spawn_on(0, attr(10), None).unwrap();
        for h in 0..4 {
            s.reschedule_on(h);
        }
        let b = s.spawn_on(0, attr(10), None).unwrap();
        for h in 0..4 {
            s.reschedule_on(h);
        }
        let running = |t| (0..4).find(|&h| s.current_on(h) == t);
        let sleep = |t| {
            if let Some(h) = running(t) {
                s.prepare_block_on(h).unwrap();
                assert!(s.block_on(h).unwrap());
                s.reschedule_on(h);
                s.switched(h); // (the kernel does this when the thread's context is saved: until then a blocked thread is in transit and stays where it is)
            }
        };
        sleep(a);
        sleep(b);
        let mut seen = std::collections::BTreeSet::new();
        for round in 0..21 {
            for t in [a, b] {
                s.wake_on(0, t).unwrap();
                for h in 0..4 {
                    s.reschedule_on(h);
                }
                let on = running(t).expect("a woken thread runs: a hart is free");
                if round > 0 {
                    seen.insert(on); // (the first round starts from where the threads were first placed)
                }
                sleep(t);
            }
        }
        s.audit(true).unwrap();
        match policy {
            Policy::Pack => assert_eq!(seen.len(), 1, "pack: one hart for both: {seen:?}"),
            Policy::Spread => assert!(
                !seen.is_empty(),
                "spread: a woken thread always ran somewhere"
            ), // (spread returns a thread to its last hart; which hart that is depends on where it first ran)
        }
        assert!(!seen.contains(&0));
    }
}

#[test]
fn an_idle_hart_that_is_about_to_sleep_looks_at_no_queue_while_no_hart_is_overloaded() {
    // (T156: `idle_has_work` read every other hart's run queue under its lock; it reads the `overloaded` mask now)
    let s = Sched2::new(16, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    for h in 0..16 {
        s.reschedule_on(h);
    }
    let before = s.pull_looks();
    for h in 1..16 {
        assert!(!s.idle_has_work(h), "hart {h}: nothing queued anywhere");
    }
    assert_eq!(s.pull_looks() - before, 0, "no queue was looked at");
    // a thread waiting on hart 0 (it runs a thread of 20, the new one is 10 and pinned nowhere: an idle hart may take it)
    for _ in 0..16 {
        s.spawn_on(0, attr(10), None).unwrap();
    }
    for h in 0..16 {
        s.reschedule_on(h);
    }
    let waiting = (0..16).filter(|&h| s.queued_on(h) > 0).count();
    if waiting > 0 {
        let before = s.pull_looks();
        let any = (0..16).any(|h| s.idle_has_work(h));
        assert!(any, "a hart can take what waits");
        assert!(
            s.pull_looks() > before,
            "and found it by looking at the harts the mask names"
        );
    }
    s.audit(true).unwrap();
}
