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
fn lost_poke() -> (Sched2<FixedPrio>, u32) {
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
