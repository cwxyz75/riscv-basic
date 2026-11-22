//! G4 (the big lock is released at the switch): a thread that has blocked, slept or exited is still executing its last instructions on
//! its hart until `switched`, so no other hart may run it (a wake that queued it, then a settle on another hart) and nothing may
//! reap it before then. Each case is deterministic: the switch of hart 0 is left unfinished while hart 1 acts.
use kcore::sched::State;
use kcore::sched2::{Error, FixedPrio, SchedAttr, MS};
use kcore::sched_smp::Sched2;

fn two_harts() -> (Sched2<FixedPrio>, u64) {
    let s = Sched2::new(2, FixedPrio::new);
    let a = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 20,
                rr: false,
            },
            None,
        )
        .unwrap();
    (s, a)
}

#[test]
fn a_thread_that_blocked_is_not_run_elsewhere_before_its_switch_is_done() {
    let (s, a) = two_harts();
    s.prepare_block_on(0).unwrap();
    assert!(s.block_on(0).unwrap());
    // hart 0 has not finished switching away from `a`; hart 1 wakes it and looks at its queues
    s.wake_on(1, a).unwrap();
    s.reschedule_on(1);
    assert_ne!(
        s.current_on(1),
        a,
        "run on hart 1 while hart 0 still executes it"
    );
    s.switched(0);
    // the switch is over: now it may run (on hart 0 or 1)
    s.reschedule_on(0);
    s.reschedule_on(1);
    assert!(
        s.current_on(0) == a || s.current_on(1) == a,
        "it never runs after the switch"
    );
    s.audit(true).unwrap();
}

#[test]
fn a_thread_that_slept_is_not_run_elsewhere_before_its_switch_is_done() {
    let (s, a) = two_harts();
    s.set_clock(0, MS);
    s.prepare_block_on(0).unwrap();
    assert!(s.sleep_until_on(0, 3 * MS).unwrap());
    s.wake_on(1, a).unwrap(); // woken early from another hart
    s.reschedule_on(1);
    assert_ne!(
        s.current_on(1),
        a,
        "run on hart 1 while hart 0 still executes it"
    );
    s.switched(0);
}

#[test]
fn a_thread_that_exited_is_not_reaped_before_its_switch_is_done() {
    let (s, a) = two_harts();
    s.exit_on(0).unwrap();
    assert_eq!(s.state(a), Some(State::Exited));
    assert!(
        s.is_in_transit(a),
        "an exited thread is in transit until its switch is done"
    );
    s.switched(0);
    assert!(!s.is_in_transit(a));
    s.reap(a).unwrap();
}

#[test]
fn reap_frees_an_exited_thread_and_refuses_anything_else() {
    // (KT10: the mutant "reap gives Ok(())" survived the first mutation run)
    let (s, a) = two_harts();
    assert_eq!(
        s.reap(a),
        Err(Error::NotExited),
        "a running thread is not reaped"
    );
    assert_eq!(
        s.reap(9_999),
        Err(Error::NoSuchThread),
        "a thread that never was"
    );
    assert!(s.state(a).is_some());
    s.exit_on(0).unwrap();
    assert_eq!(s.state(a), Some(State::Exited));
    assert_eq!(s.reap(a), Ok(()), "an exited thread is reaped");
    assert_eq!(s.state(a), None, "and is gone");
    assert_eq!(
        s.reap(a),
        Err(Error::NoSuchThread),
        "a second reap finds nothing"
    );
}

/// L24, direct hand-off (SCHED-SMP-SPEC 4.4 rule 2 and 5.5): a wake by a thread that is about to block (`wake_sync_on`) places the woken thread on the
/// waker's hart; an ordinary wake of the same thread goes back to the hart it last ran on.
#[test]
fn a_sync_wake_places_the_thread_on_the_wakers_hart() {
    let attr = SchedAttr {
        prio: 20,
        rr: false,
    };
    let place = |sync: bool, pin: Option<usize>| {
        let s = Sched2::new(2, FixedPrio::new);
        // `a` runs on hart 0 and blocks there (its switch done); the waker `w` runs on hart 1
        let a = s.adopt_current_on(0, attr, pin).unwrap();
        let _w = s.adopt_current_on(1, attr, None).unwrap();
        s.prepare_block_on(0).unwrap();
        assert!(s.block_on(0).unwrap());
        s.switched(0);
        if sync {
            s.wake_sync_on(1, a).unwrap();
        } else {
            s.wake_on(1, a).unwrap();
        }
        (
            s.queued_on(0),
            s.queued_on(1),
            s.current_on(0) == a || s.current_on(1) == a,
        )
    };
    // an ordinary wake: the thread's own hart (it last ran there, which is idle now, so it runs there at once)
    let (q0, q1, ran) = place(false, None);
    assert!(
        ran || q0 == 1,
        "an ordinary wake went to hart 0 ({q0},{q1})"
    );
    assert_eq!(
        q1, 0,
        "an ordinary wake stacked the thread on the waker's hart"
    );
    // a sync wake: the waker's hart (hart 1 runs the waker at the same priority, so the thread queues behind it until the waker blocks)
    let (q0, q1, _) = place(true, None);
    assert_eq!(
        (q0, q1),
        (0, 1),
        "a sync wake did not place the thread on the waker's hart"
    );
    // a thread pinned to hart 0 is placed there whatever the hint says
    let (q0, q1, ran) = place(true, Some(0));
    assert!(ran || q0 == 1, "a pinned thread left its hart ({q0},{q1})");
    assert_eq!(q1, 0);
}
