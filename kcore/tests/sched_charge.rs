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

#[test]
fn what_ran_before_a_priority_change_is_billed_at_the_old_band() {
    use kcore::jobsched::JobSchedTable;
    use std::sync::Arc;
    let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
    NOW.store(0, Ordering::SeqCst);
    let s = Sched2::new(1, FixedPrio::new).with_clock(clock);
    let table = Arc::new(JobSchedTable::new());
    s.set_job_table(table.clone());
    let a = s.spawn_on(0, attr(), Some(0)).unwrap();
    s.set_job(a, 3).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    NOW.store(90 * MS, Ordering::SeqCst);
    s.set_priority_on(0, a, 40).unwrap(); // 90 ms at priority 10, then it turns real-time
    NOW.store(95 * MS, Ordering::SeqCst);
    s.yield_on(0).ok();
    s.switched(0);
    assert_eq!(table.cpu(3), 95 * MS);
    assert_eq!(
        table.rt_used(3, 95 * MS),
        5 * MS,
        "only the 5 ms after the change are real-time"
    );
}

#[test]
fn a_threads_run_is_charged_to_its_job_and_its_real_time_part_to_the_window() {
    use kcore::jobsched::JobSchedTable;
    use std::sync::Arc;
    let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
    NOW.store(0, Ordering::SeqCst);
    let s = Sched2::new(1, FixedPrio::new).with_clock(clock);
    let table = Arc::new(JobSchedTable::new());
    s.set_job_table(table.clone());
    let rt = SchedAttr {
        prio: 40,
        rr: false,
    };
    let a = s.spawn_on(0, rt, Some(0)).unwrap();
    s.set_job(a, 3).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    NOW.store(12 * MS, Ordering::SeqCst);
    s.yield_on(0).unwrap();
    s.switched(0);
    assert_eq!(table.cpu(3), 12 * MS, "A ran 12 ms: the job pays");
    assert_eq!(
        table.rt_used(3, 12 * MS),
        12 * MS,
        "its base priority is real-time"
    );
    assert_eq!(table.cpu(0), 0, "the root's slot is untouched");
    // a normal-band thread of another job is charged cpu time and no RT time
    let b = s.spawn_on(0, attr(), Some(0)).unwrap();
    s.set_job(b, 4).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    NOW.store(20 * MS, Ordering::SeqCst);
    s.yield_on(0).ok();
    s.switched(0);
    assert_eq!(table.rt_used(4, 20 * MS), 0);
    assert!(table.cpu(4) + table.cpu(3) <= 20 * MS);
}
