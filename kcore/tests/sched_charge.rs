//! Tickless accounting (L13): with a clock source every settle charges the running thread, and a voluntary switch charges the thread
//! that ran, not the one that runs next (PR 273 review: the designation changes first, so the charge must come first).
use kcore::sched2::{FixedPrio, SchedAttr, MS};
use kcore::sched_smp::Sched2;
use std::sync::atomic::{AtomicU64, Ordering};

static NOW: AtomicU64 = AtomicU64::new(0);
/// (one clock for the whole file: the tests take turns)
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn clock() -> u64 {
    NOW.load(Ordering::SeqCst)
}

fn attr() -> SchedAttr {
    SchedAttr {
        prio: 10,
        rr: false,
    }
}

#[test]
fn a_thread_that_yields_is_charged_for_its_own_run() {
    let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
    NOW.store(0, Ordering::SeqCst);
    let s = Sched2::new(1, FixedPrio::new).with_clock(clock);
    let a = s.spawn_on(0, attr(), Some(0)).unwrap();
    let b = s.spawn_on(0, attr(), Some(0)).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    assert_eq!(s.current_on(0), a);
    NOW.store(9 * MS, Ordering::SeqCst);
    s.yield_on(0).unwrap();
    s.switched(0);
    assert_eq!(s.current_on(0), b);
    assert_eq!(
        s.record(a).unwrap().cpu,
        9 * MS,
        "A ran 9 ms and is charged for it"
    );
    assert_eq!(s.record(b).unwrap().cpu, 0, "B has not run yet");
}

#[test]
fn a_thread_that_sleeps_is_charged_for_its_own_run() {
    let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
    NOW.store(0, Ordering::SeqCst);
    let s = Sched2::new(1, FixedPrio::new).with_clock(clock);
    let a = s.spawn_on(0, attr(), Some(0)).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    NOW.store(500 * MS, Ordering::SeqCst);
    s.prepare_block_on(0).unwrap();
    assert!(s.sleep_until_on(0, 600 * MS).unwrap());
    s.switched(0);
    assert_eq!(s.record(a).unwrap().cpu, 500 * MS);
}
