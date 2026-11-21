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
