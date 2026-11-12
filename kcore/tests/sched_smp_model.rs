//! The multi-hart scheduler core (S5b): random operations across 1 to 8 harts for each policy,
//! with the per-hart rules checked after every step, plus the hand-written scenarios of the
//! protocol "only hart h changes `current[h]`".
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::sched::*;

/// A scheduler with one pinned idle thread per hart already running on it.
fn world<C: SchedClass>(class: C, harts: usize) -> (Sched<C>, Vec<Tid>) {
    let mut s = Sched::with_harts(class, harts);
    let mut idle = Vec::new();
    for h in 0..harts {
        idle.push(s.adopt_current_on(h, PRIO_IDLE, Some(h)).unwrap());
    }
    (s, idle)
}

/// What a hart would do on the IPI: reschedule itself.
fn serve_pokes<C: SchedClass>(s: &mut Sched<C>) {
    // (serving one hart can queue a thread another hart should take: go on until nothing is left)
    for _ in 0..100 {
        let pokes = s.take_pokes();
        let mut any = pokes != 0;
        for h in 0..s.harts() {
            if pokes & (1 << h) != 0 || s.resched_pending(h) {
                s.reschedule_on(h);
                any = true;
            }
        }
        if !any {
            return;
        }
    }
    panic!("reschedules never settled");
}

#[test]
fn a_wake_on_one_hart_never_changes_another_harts_current() {
    let (mut s, idle) = world(FixedPriority::new(), 2);
    let a = s.spawn_on(0, 5, None).unwrap(); // hart 0 preempts its idle at once
    assert_eq!(s.current_on(0), Some(a));
    s.block_current_on(0).unwrap(); // a blocks; hart 0 back to idle
    assert_eq!(s.current_on(0), Some(idle[0]));
    // hart 1 runs a low-priority worker
    let w = s.spawn_on(1, 3, None).unwrap();
    assert_eq!(s.current_on(1), Some(w));
    s.take_pokes();
    // hart 0 wakes `a` (priority 5 > 3): hart 0 is idle, so a lands on hart 0; hart 1 is not
    // disturbed, because hart 0 can take it
    s.wake_on(0, a).unwrap();
    assert_eq!(s.current_on(0), Some(a));
    assert_eq!(s.current_on(1), Some(w));
    s.audit().unwrap();
    // now a higher-priority thread, woken by hart 0, while hart 0 runs `a` (5) and hart 1 `w` (3):
    // hart 1 should take it, but only hart 1 may change its own designation
    let b = s.spawn_on(0, 9, Some(1)).unwrap(); // pinned to hart 1
    assert_eq!(
        s.current_on(1),
        Some(w),
        "hart 1's current is untouched by hart 0's spawn"
    );
    assert!(s.resched_pending(1));
    assert_eq!(s.take_pokes(), 0b10, "hart 1 is poked, hart 0 is not");
    s.audit().unwrap();
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), Some(b));
    assert_eq!(
        s.state(w),
        Some(State::Runnable),
        "the preempted worker waits in the queue"
    );
    assert!(!s.resched_pending(1));
    s.audit().unwrap();
}

#[test]
fn a_pinned_thread_runs_only_on_its_hart() {
    let (mut s, _idle) = world(FixedPriority::new(), 3);
    let p = s.spawn_on(0, 7, Some(2)).unwrap();
    assert_ne!(s.current_on(0), Some(p));
    assert_ne!(s.current_on(1), Some(p));
    serve_pokes(&mut s);
    assert_eq!(s.current_on(2), Some(p));
    // when hart 2 yields it, no other hart takes it
    s.yield_current_on(2).unwrap();
    assert_eq!(
        s.current_on(2),
        Some(p),
        "the only thing hart 2 may run above idle"
    );
    s.audit().unwrap();
}

#[test]
fn equal_priority_threads_spread_over_idle_harts_and_a_thread_runs_on_one_hart_only() {
    let (mut s, _idle) = world(FixedPriority::new(), 4);
    let ts: Vec<Tid> = (0..4).map(|_| s.spawn_on(0, 4, None).unwrap()).collect();
    serve_pokes(&mut s);
    let mut on: Vec<Tid> = (0..4).map(|h| s.current_on(h).unwrap()).collect();
    on.sort();
    let mut want = ts.clone();
    want.sort();
    assert_eq!(on, want, "four threads on four harts");
    s.audit().unwrap();
}

fn random_run<C: SchedClass>(make: impl Fn() -> C, strict_priority: bool) {
    for harts in [1usize, 2, 4, 8] {
        for seed in 0..SEEDS {
            let mut rng = Rng::new(seed * 8 + harts as u64);
            let (mut s, idle) = world(make(), harts);
            let mut live: Vec<Tid> = Vec::new(); // non-idle, not exited
            for step in 0..STEPS {
                let ctx = format!(
                    "class {} harts {harts} seed {seed} step {step}",
                    s.class().name()
                );
                let h = rng.below(harts as u64) as usize;
                match rng.below(11) {
                    0 | 1 => {
                        let aff = (rng.below(4) == 0).then(|| rng.below(harts as u64) as usize);
                        let prio = 1 + rng.below(8) as u8;
                        live.push(s.spawn_on(h, prio, aff).unwrap());
                    }
                    2 => {
                        // a blocked thread is woken (by hart h)
                        if let Some(&t) = live.get(rng.below(live.len().max(1) as u64) as usize) {
                            s.wake_on(h, t).unwrap();
                        }
                    }
                    3 => {
                        let c = s.current_on(h).unwrap();
                        if !idle.contains(&c) {
                            let _ = s.block_current_on(h);
                        }
                    }
                    4 => {
                        let c = s.current_on(h).unwrap();
                        if !idle.contains(&c) {
                            let _ = s.sleep_until_on(h, s.now() + 1 + rng.below(6));
                        }
                    }
                    5 => s.yield_current_on(h).unwrap(),
                    6 => {
                        let c = s.current_on(h).unwrap();
                        if !idle.contains(&c) {
                            s.exit_current_on(h).unwrap();
                            live.retain(|&t| t != c);
                            let _ = s.reap(c);
                        }
                    }
                    7 => {
                        if let Some(&t) = live.get(rng.below(live.len().max(1) as u64) as usize) {
                            s.set_priority_on(h, t, 1 + rng.below(8) as u8).unwrap();
                        }
                    }
                    8 => {
                        let to = s.now() + rng.below(3);
                        s.advance_to(to, h);
                    }
                    9 => s.tick_slice(h),
                    _ => serve_pokes(&mut s),
                }
                s.audit().unwrap_or_else(|e| panic!("{ctx}: audit: {e}"));
                // every hart always has a thread to run (its idle thread at worst)
                for g in 0..harts {
                    assert!(
                        s.current_on(g).is_some(),
                        "{ctx}: hart {g} has nothing to run"
                    );
                }
                // a thread is on at most one hart (also in audit), and an idle thread only on its own
                for (g, &i) in idle.iter().enumerate() {
                    for k in 0..harts {
                        if k != g {
                            assert_ne!(s.current_on(k), Some(i), "{ctx}: idle {g} on hart {k}");
                        }
                    }
                }
                if strict_priority && step % 7 == 0 {
                    // once every hart has served its pending reschedule, each runs the best thread
                    // that may run there: nothing queued and allowed is strictly better
                    let mut t = world_copy_check(&mut s, &ctx);
                    t.take();
                }
            }
        }
    }
}

/// Serve every pending reschedule, then check the strong property for the priority policy.
fn world_copy_check<C: SchedClass>(s: &mut Sched<C>, ctx: &str) -> Option<()> {
    serve_pokes(s);
    // `audit` now has no pending excuse left: it fails if a better thread waits for any hart
    for h in 0..s.harts() {
        assert!(!s.resched_pending(h), "{ctx}: pending after serving");
    }
    s.audit()
        .unwrap_or_else(|e| panic!("{ctx}: after serving pokes: {e}"));
    Some(())
}

#[test]
fn random_operations_keep_every_per_hart_rule_fixed_priority() {
    random_run(FixedPriority::new, true);
}

#[test]
fn random_operations_keep_every_per_hart_rule_round_robin() {
    random_run(RoundRobin::new, false);
}

#[test]
fn a_lone_hart_behaves_exactly_like_the_single_hart_core() {
    // the legacy interface and the hart-0 interface are the same thing
    let mut a = Sched::new(FixedPriority::new());
    let mut b = Sched::with_harts(FixedPriority::new(), 1);
    let ta = a.adopt_current(1).unwrap();
    let tb = b.adopt_current_on(0, 1, None).unwrap();
    assert_eq!(ta, tb);
    for p in [3u8, 2, 5, 1] {
        assert_eq!(a.spawn(p).unwrap(), b.spawn_on(0, p, None).unwrap());
        assert_eq!(a.current(), b.current_on(0));
    }
    for _ in 0..40 {
        a.tick();
        b.advance_to(b.now() + 1, 0);
        b.tick_slice(0);
        assert_eq!(a.current(), b.current_on(0));
    }
    a.audit().unwrap();
    b.audit().unwrap();
}
