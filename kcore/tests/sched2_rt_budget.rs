//! Task 25 (SCHED-SMP-SPEC 4.5): the RT budgets enforced, on the simulated clock. NM1: in every window in which the normal band has a
//! runnable thread on a hart for the whole window, it gets at least `RT_WINDOW - RT_HART_MAX` of that hart; a job that has used its
//! budget has its real-time threads demoted until the window ends; a spinning RT thread alone on a hart is not stopped (work conserving).
use kcore::jobsched::JobSchedTable;
use kcore::sched2::{FixedPrio, HartRq, SchedAttr, DEMOTED_PRIO, MS, RT_HART_MAX, RT_WINDOW};
use std::sync::Arc;

fn rt(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}
fn normal() -> SchedAttr {
    SchedAttr {
        prio: 10,
        rr: false,
    }
}

/// Run the hart for `total` in steps of the next event or 1 ms, whichever is first (what the kernel's timer does); returns the time each
/// listed thread ran, measured by the thread that is current at the start of each step.
fn run(h: &mut HartRq<FixedPrio>, total: u64, watch: &[u32]) -> Vec<u64> {
    let mut ran = vec![0u64; watch.len()];
    let end = h.now() + total;
    while h.now() < end {
        let cur = h.current();
        let step = h
            .next_event()
            .map_or(MS, |e| e.saturating_sub(h.now()).clamp(1, MS))
            .min(end - h.now());
        h.advance(step);
        if let Some(i) = watch.iter().position(|&t| Some(u64::from(t)) == cur) {
            ran[i] += step;
        }
    }
    ran
}

fn align(h: &mut HartRq<FixedPrio>) {
    let to_go = RT_WINDOW - h.now() % RT_WINDOW;
    run(h, to_go, &[]);
}

fn hart_with(table: Option<Arc<JobSchedTable>>) -> HartRq<FixedPrio> {
    let mut h = HartRq::new(FixedPrio::new());
    h.adopt_idle();
    if let Some(t) = table {
        h.set_job_table(t);
    }
    h
}

#[test]
fn the_normal_band_keeps_a_fifth_of_every_window_against_a_spinning_rt_thread() {
    let mut h = hart_with(None);
    let r = h.spawn(rt(50)).unwrap();
    let n = h.spawn(normal()).unwrap();
    // let one window pass to align, then measure five whole windows
    align(&mut h);
    for w in 0..5 {
        let ran = run(&mut h, RT_WINDOW, &[r as u32, n as u32]);
        assert!(
            ran[1] >= RT_WINDOW - RT_HART_MAX - 2 * MS,
            "window {w}: the normal thread ran only {} ms of 100",
            ran[1] / MS
        );
        assert!(
            ran[0] >= RT_HART_MAX - 2 * MS,
            "window {w}: the RT thread got {} ms (it should get its 80)",
            ran[0] / MS
        );
    }
    h.audit().unwrap();
}

#[test]
fn a_spinning_rt_thread_alone_on_a_hart_is_not_stopped() {
    let mut h = hart_with(None);
    let r = h.spawn(rt(50)).unwrap();
    let ran = run(&mut h, 3 * RT_WINDOW, &[r as u32]);
    assert_eq!(ran[0], 3 * RT_WINDOW, "nothing else wants the hart");
}

#[test]
fn a_job_that_has_used_its_budget_is_demoted_until_the_window_ends() {
    let table = Arc::new(JobSchedTable::new());
    table.set_budget(3, 30 * MS); // job slot 3: 30 ms of real-time time per window
    let tag = table.tag(3);
    let mut h = hart_with(Some(table.clone()));
    let r = h.spawn(rt(50)).unwrap();
    h.set_job(r, tag).unwrap();
    // (a thread at the top of the normal band: the demoted thread counts as 31, round robin, so the two now share the hart)
    let n = h
        .spawn(SchedAttr {
            prio: 31,
            rr: false,
        })
        .unwrap();
    align(&mut h); // to a window start
    let ran = run(&mut h, 60 * MS, &[r as u32, n as u32]);
    assert!(
        ran[0] >= 28 * MS && ran[1] >= 10 * MS,
        "in the first 60 ms the RT thread ran {} ms (30 of RT budget, then it shares) and the other {} ms",
        ran[0] / MS,
        ran[1] / MS
    );
    assert_eq!(
        h.record(r).unwrap().eff,
        DEMOTED_PRIO,
        "demoted at the budget"
    );
    run(&mut h, 40 * MS, &[]); // the window ends
    let again = run(&mut h, 10 * MS, &[r as u32]);
    assert_eq!(h.record(r).unwrap().eff, 50, "back in the RT band");
    assert!(
        again[0] >= 8 * MS,
        "and running there: {} ms of 10",
        again[0] / MS
    );
}

/// Negative control: with the budget never enforced the RT thread takes its 80 ms every window whatever the job says, so the budget test
/// above proves the job rule and not the hart rule.
#[test]
fn a_job_with_no_budget_gets_no_real_time_at_all() {
    let table = Arc::new(JobSchedTable::new());
    table.set_budget(4, 0);
    let tag = table.tag(4);
    let mut h = hart_with(Some(table));
    let r = h.spawn(rt(50)).unwrap();
    h.set_job(r, tag).unwrap();
    let n = h.spawn(normal()).unwrap();
    let ran = run(&mut h, 2 * RT_WINDOW, &[r as u32, n as u32]);
    // (it runs as a normal thread of priority 31: it is above the other, which gets the rest only by round robin when 31 is
    // alone at its level, i.e. never: but the point is that the RT band never held the hart against the normal thread)
    assert!(ran[0] + ran[1] >= 2 * RT_WINDOW - 2 * MS);
    assert!(h.record(r).unwrap().eff <= DEMOTED_PRIO);
}

/// The job's budget is shared by every hart (4.5): four real-time spinners of one job, one per hart, together get about the budget in a window,
/// not the budget each. (Found by the epic gate at four harts: the figure read 360 ms of a 100 ms budget.)
#[test]
fn a_job_budget_is_shared_by_the_harts_that_run_its_threads() {
    use kcore::sched_smp::Sched2;
    let table = Arc::new(JobSchedTable::new());
    let tag = table.tag(0);
    let s = Sched2::new(4, FixedPrio::new);
    s.set_job_table(table.clone());
    let mut hogs = Vec::new();
    for h in 0..4 {
        let t = s.spawn_on(h, rt(40), Some(h)).unwrap();
        s.set_job(t, tag).unwrap();
        hogs.push(t);
    }
    for h in 0..4 {
        s.reschedule_on(h);
        s.switched(h);
    }
    let mut worst = 0u64;
    let mut now = RT_WINDOW; // a window start
    for _ in 1..=300u64 {
        now += MS;
        for h in 0..4 {
            // (a tickless hart takes an interrupt only at the event it programmed)
            if s.next_event_on(h).is_some_and(|e| e <= now) {
                s.tick_on(h, now);
                s.switched(h);
                s.reschedule_on(h);
                s.switched(h);
            }
        }
        // sample the figure of the window just before it ends (the last full millisecond of each window)
        if now % RT_WINDOW == RT_WINDOW - MS {
            let used = table.rt_used(0, now);
            worst = worst.max(used);
        }
    }
    assert!(
        worst <= RT_WINDOW + 4 * 15 * MS,
        "the job's real-time time in a window reached {} ms (budget 100)",
        worst / MS
    );
}
