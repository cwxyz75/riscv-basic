//! L11a: the refactored `kcore::sched_smp::Sched2` (shared state in atomics, `home` per thread, published hints) against the
//! frozen pre-refactor copy `sched_smp_v0::Sched2`, call for call. Both are driven with the same random sequence of operations,
//! valid or not (an invalid call must fail the same way in both), and after every call everything either can show is compared:
//! the result, each hart's current thread, mark, transit, queue length and clock, the pokes owed, the counters, every thread's
//! state, home and priority, and the audit. The refactor is exact, not approximately right.

use kcore::sched::Tid;
use kcore::sched2::{Class, FixedPrio, RoundRobin, SchedAttr, MS};
use kcore::sched_smp::Sched2 as New;
use kcore::sched_smp_v0::Sched2 as Old;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn compare<N: Class>(
    o: &mut Old<N>,
    n: &mut New<N>,
    harts: usize,
    tids: &[Tid],
    what: &str,
    seed: u64,
    step: usize,
) {
    let ctx = || format!("seed {seed} step {step} after {what}");
    for h in 0..harts {
        assert_eq!(o.current_on(h), n.current_on(h), "{} current on {h}", ctx());
        assert_eq!(
            o.resched_pending(h),
            n.resched_pending(h),
            "{} mark on {h}",
            ctx()
        );
        assert_eq!(o.in_transit(h), n.in_transit(h), "{} transit on {h}", ctx());
        assert_eq!(o.queued_on(h), n.queued_on(h), "{} queue on {h}", ctx());
        assert_eq!(o.now_on(h), n.now_on(h), "{} clock on {h}", ctx());
    }
    assert_eq!(
        (o.pulls, o.marks, o.settles),
        (n.pulls, n.marks, n.settles),
        "{} counters",
        ctx()
    );
    assert_eq!(o.thread_count(), n.thread_count(), "{} thread count", ctx());
    for &t in tids {
        assert_eq!(o.state(t), n.state(t), "{} state of {t}", ctx());
        assert_eq!(o.home(t), n.home(t), "{} home of {t}", ctx());
        assert_eq!(o.priority(t), n.priority(t), "{} priority of {t}", ctx());
    }
    assert_eq!(o.audit(false), n.audit(false), "{} audit", ctx());
    assert_eq!(o.take_pokes(), n.take_pokes(), "{} pokes", ctx());
}

fn run<N: Class>(make: impl Fn() -> N + Copy, harts: usize, seed: u64, steps: usize) {
    let mut o = Old::new(harts, make);
    let mut n = New::new(harts, make);
    let mut r = Rng(seed | 1);
    let mut tids: Vec<Tid> = (0..harts).map(|h| o.idle(h)).collect();
    let mut now = vec![0u64; harts];
    for step in 0..steps {
        let h = r.below(harts as u64) as usize;
        let t = tids[r.below(tids.len() as u64) as usize];
        let mut what;
        macro_rules! both {
            ($e:ident ( $($a:expr),* )) => {{
                let (a, b) = (o.$e($($a),*), n.$e($($a),*));
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "seed {seed} step {step}: {} result", stringify!($e));
                a
            }};
        }
        match r.below(16) {
            0..=2 => {
                what = "spawn";
                let attr = SchedAttr {
                    prio: 1 + r.below(40) as u8,
                    rr: r.below(4) == 0,
                };
                let aff = (r.below(3) == 0).then(|| r.below(harts as u64) as usize);
                if let Ok(x) = both!(spawn_on(h, attr, aff)) {
                    tids.push(x);
                }
            }
            3 => {
                what = "prepare";
                let _ = both!(prepare_block_on(h));
            }
            4 | 5 => {
                what = "block";
                let _ = both!(block_on(h));
            }
            6 => {
                what = "sleep";
                let d = (1 + r.below(5)) * MS;
                let _ = both!(sleep_until_on(h, now[h] + d));
            }
            7 | 8 => {
                what = "wake";
                let _ = both!(wake_on(h, t));
            }
            9 => {
                what = "wake_counted";
                let _ = both!(wake_counted_on(h, t));
            }
            10 => {
                what = "yield";
                let _ = both!(yield_on(h));
            }
            11 => {
                what = "exit";
                if r.below(3) == 0 {
                    let _ = both!(exit_on(h));
                } else {
                    what = "nop";
                }
            }
            12 => {
                what = "reap";
                let _ = both!(reap(t));
            }
            13 => {
                what = "set_priority";
                let p = r.below(45) as u8;
                let _ = both!(set_priority_on(h, t, p));
            }
            14 => {
                what = "tick";
                now[h] += MS;
                both!(tick_on(h, now[h]));
            }
            _ => {
                what = if r.below(2) == 0 {
                    "reschedule"
                } else {
                    "switched"
                };
                if what == "reschedule" {
                    both!(reschedule_on(h));
                } else {
                    both!(switched(h));
                }
            }
        }
        compare(&mut o, &mut n, harts, &tids, what, seed, step);
    }
}

#[test]
fn the_refactor_matches_the_frozen_scheduler_fixed_priority() {
    for seed in 1..=60u64 {
        for harts in [1usize, 2, 3, 4, 8] {
            run(FixedPrio::new, harts, seed * 7919 + harts as u64, 600);
        }
    }
}

#[test]
fn the_refactor_matches_the_frozen_scheduler_round_robin() {
    for seed in 1..=40u64 {
        for harts in [2usize, 4] {
            run(RoundRobin::new, harts, seed * 104729 + harts as u64, 600);
        }
    }
}
