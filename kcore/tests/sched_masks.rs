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
