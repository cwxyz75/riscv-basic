//! Task 6b: the kernel runs the spec's bands in shared-queue mode, as `sched2::TwoBand` under the multi-hart core
//! `sched::Sched`. In the normal band it must behave exactly like the fixed-priority class it replaces (a
//! differential test across 1 to 4 harts); in the real-time band the rules are the spec's (FIFO within a level, strict
//! precedence over every normal thread, a preempted thread returns to the front of its level).
mod common;
use common::Rng;
use kcore::sched::{FixedPriority, Sched, Tid, PRIO_IDLE, TIMESLICE_TICKS};
use kcore::sched2::TwoBand;

fn pair(harts: usize) -> (Sched<FixedPriority>, Sched<TwoBand>) {
    let mut a = Sched::with_harts(FixedPriority::new(), harts);
    let mut b = Sched::with_harts(TwoBand::new(), harts);
    for h in 0..harts {
        a.adopt_current_on(h, PRIO_IDLE, Some(h)).unwrap();
        b.adopt_current_on(h, PRIO_IDLE, Some(h)).unwrap();
    }
    (a, b)
}

#[test]
fn the_normal_band_is_the_old_fixed_priority_class() {
    for harts in [1usize, 2, 4] {
        for seed in 0..200u64 {
            let mut rng = Rng::new(seed * 7 + harts as u64);
            let (mut a, mut b) = pair(harts);
            let mut live: Vec<Tid> = Vec::new();
            for step in 0..300 {
                let ctx = format!("harts {harts} seed {seed} step {step}");
                let h = rng.below(harts as u64) as usize;
                let cur = a.current_on(h).unwrap();
                match rng.below(10) {
                    0 | 1 => {
                        let prio = 1 + rng.below(31) as u8;
                        let aff = (rng.below(4) == 0).then(|| rng.below(harts as u64) as usize);
                        let t = a.spawn_on(h, prio, aff).unwrap();
                        assert_eq!(b.spawn_on(h, prio, aff).unwrap(), t, "{ctx}: tid");
                        live.push(t);
                    }
                    2 | 3 => {
                        if let Some(&t) = live.get(rng.below(live.len().max(1) as u64) as usize) {
                            a.wake_on(h, t).unwrap();
                            b.wake_on(h, t).unwrap();
                        }
                    }
                    4 if a.priority(cur) != Some(PRIO_IDLE) => {
                        assert_eq!(
                            a.block_current_on(h).unwrap(),
                            b.block_current_on(h).unwrap(),
                            "{ctx}: block"
                        );
                    }
                    5 => {
                        a.yield_current_on(h).unwrap();
                        b.yield_current_on(h).unwrap();
                    }
                    6 => {
                        if let Some(&t) = live.get(rng.below(live.len().max(1) as u64) as usize) {
                            let p = 1 + rng.below(31) as u8;
                            a.set_priority_on(h, t, p).unwrap();
                            b.set_priority_on(h, t, p).unwrap();
                        }
                    }
                    7 => {
                        let to = a.now() + rng.below(3);
                        a.advance_to(to, h);
                        b.advance_to(to, h);
                    }
                    8 => {
                        a.tick_slice(h);
                        b.tick_slice(h);
                    }
                    _ => {
                        // serve the pokes: every poked hart reschedules itself
                        let (pa, pb) = (a.take_pokes(), b.take_pokes());
                        assert_eq!(pa, pb, "{ctx}: pokes");
                        for g in 0..harts {
                            if pa & (1 << g) != 0 {
                                a.reschedule_on(g);
                                b.reschedule_on(g);
                            }
                        }
                    }
                }
                assert_eq!(a.take_pokes(), b.take_pokes(), "{ctx}: pokes");
                for g in 0..harts {
                    assert_eq!(a.current_on(g), b.current_on(g), "{ctx}: hart {g}");
                }
                b.audit().unwrap_or_else(|e| panic!("{ctx}: audit: {e}"));
            }
        }
    }
}

fn one_hart() -> Sched<TwoBand> {
    let mut s = Sched::with_harts(TwoBand::new(), 1);
    s.adopt_current_on(0, PRIO_IDLE, Some(0)).unwrap();
    s
}

#[test]
fn a_realtime_thread_runs_before_every_normal_thread() {
    let mut s = one_hart();
    let n = s.spawn(31).unwrap(); // the top of the normal band
    assert_eq!(s.current(), Some(n));
    let r = s.spawn(32).unwrap(); // the bottom of the real-time band
    assert_eq!(s.current(), Some(r), "preempts the best normal thread");
    s.block_current().unwrap();
    assert_eq!(
        s.current(),
        Some(n),
        "and the normal thread gets the hart back"
    );
}

#[test]
fn realtime_threads_of_one_level_do_not_rotate() {
    let mut s = one_hart();
    let a = s.spawn(40).unwrap();
    let b = s.spawn(40).unwrap();
    assert_eq!(s.current(), Some(a));
    for _ in 0..(TIMESLICE_TICKS * 5) {
        s.tick();
        s.tick_slice(0);
        assert_eq!(
            s.current(),
            Some(a),
            "FIFO: a runs until it blocks or yields"
        );
    }
    s.yield_current().unwrap();
    assert_eq!(s.current(), Some(b), "a yield gives the level to the next");
    s.audit().unwrap();
}

#[test]
fn normal_threads_of_one_level_still_rotate() {
    let mut s = one_hart();
    let a = s.spawn(10).unwrap();
    let b = s.spawn(10).unwrap();
    assert_eq!(s.current(), Some(a));
    let mut changed = false;
    for _ in 0..(TIMESLICE_TICKS * 2) {
        s.tick();
        s.tick_slice(0);
        changed |= s.current() == Some(b);
    }
    assert!(changed, "the slice rotates equal-priority normal threads");
}

#[test]
fn a_preempted_realtime_thread_goes_back_to_the_front_of_its_level() {
    let mut s = one_hart();
    let a = s.spawn(40).unwrap();
    let b = s.spawn(40).unwrap(); // queued behind a
    let hi = s.spawn(50).unwrap(); // preempts a
    assert_eq!(s.current(), Some(hi));
    s.block_current().unwrap();
    assert_eq!(
        s.current(),
        Some(a),
        "a was preempted: it resumes before b, which has not run yet"
    );
    let _ = b;
}

#[test]
fn the_default_class_is_usable() {
    // (the kernel builds its class with `Default`: an empty set of levels once panicked at the first spawn)
    let mut s = Sched::with_harts(TwoBand::default(), 1);
    s.adopt_current_on(0, PRIO_IDLE, Some(0)).unwrap();
    let t = s.spawn(5).unwrap();
    assert_eq!(s.current(), Some(t));
}
