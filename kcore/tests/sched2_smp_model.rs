//! Task 11 (pure part): the multi-hart scheduler `kcore::sched_smp::Sched2` against an independent reference.
//!
//! The reference is a plain table of threads (state, base priority, pinning, wake time) driven by the same operations
//! with none of the scheduler's queue logic. After every operation and after the marked harts have looked (the
//! quiescent point of GP1), the test checks, from the reference's own table and the harts' observable current
//! threads only: every runnable thread is running on exactly one hart or waiting, nothing blocked or exited runs, a
//! pinned thread runs only on its hart, and **GP1**: no hart runs a lower priority than a waiting thread that may run
//! there. `Sched2::audit` (GP2 and the structure) runs alongside.

use kcore::sched::{State, Tid};
use kcore::sched2::{FixedPrio, SchedAttr, MS};
use kcore::sched_smp::Sched2;
use std::collections::BTreeMap;

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

#[derive(Clone, Copy, PartialEq, Debug)]
enum R {
    Runnable,
    Blocked(Option<u64>),
    Exited,
}

struct Ref {
    state: R,
    prio: u8,
    pin: Option<usize>,
}

struct World {
    s: Sched2<FixedPrio>,
    t: BTreeMap<Tid, Ref>,
    now: u64,
    harts: usize,
}

impl World {
    fn new(harts: usize) -> World {
        World {
            s: Sched2::new(harts, FixedPrio::new),
            t: BTreeMap::new(),
            now: 0,
            harts,
        }
    }

    fn cur(&self, h: usize) -> Option<Tid> {
        let c = self.s.current_on(h);
        (c != self.s.idle(h)).then_some(c)
    }

    /// Hart `h` finishes the context switch it was in the middle of (every kernel path does this before the hart does anything
    /// else); the thread it left becomes available to the other harts.
    fn finish(&mut self, h: usize) {
        if self.s.in_transit(h).is_some() {
            self.s.switched(h);
        }
    }

    /// The marked harts look at their queues until nobody is marked (the IPIs of a real kernel).
    fn drain(&mut self) {
        for _ in 0..1000 {
            let mut any = false;
            for h in 0..self.harts {
                if self.s.resched_pending(h) {
                    self.finish(h);
                    self.s.reschedule_on(h);
                    any = true;
                }
            }
            if !any {
                break;
            }
        }
        let _ = self.s.take_pokes();
    }

    /// The reference's checks, from its own table and `current_on` alone.
    fn check(&self, what: &str, seed: u64, step: usize) {
        let ctx = |m: String| panic!("seed {seed} step {step} after {what}: {m}");
        if let Err(e) = self.s.audit(true) {
            ctx(format!("audit: {e}"));
        }
        let mut running: BTreeMap<Tid, usize> = BTreeMap::new();
        for h in 0..self.harts {
            if let Some(c) = self.cur(h) {
                if running.insert(c, h).is_some() {
                    ctx(format!("thread {c} runs on two harts"));
                }
                let r = &self.t[&c];
                if r.state != R::Runnable {
                    ctx(format!("hart {h} runs thread {c} which is {:?}", r.state));
                }
                if r.pin.is_some_and(|p| p != h) {
                    ctx(format!("pinned thread {c} runs on hart {h}"));
                }
                if self.s.state(c) != Some(State::Running) {
                    ctx(format!(
                        "thread {c} is current on hart {h} but {:?}",
                        self.s.state(c)
                    ));
                }
            }
        }
        for (&tid, r) in &self.t {
            match r.state {
                R::Blocked(_) => {
                    if self.s.state(tid) != Some(State::Blocked) {
                        ctx(format!(
                            "thread {tid} should be blocked, is {:?}",
                            self.s.state(tid)
                        ));
                    }
                }
                R::Exited => {
                    if self.s.state(tid) != Some(State::Exited) {
                        ctx(format!(
                            "thread {tid} should be exited, is {:?}",
                            self.s.state(tid)
                        ));
                    }
                }
                R::Runnable => {
                    let st = self.s.state(tid);
                    if !running.contains_key(&tid) {
                        if st != Some(State::Runnable) {
                            ctx(format!("thread {tid} should wait in a queue, is {st:?}"));
                        }
                        if (0..self.harts).any(|h| self.s.in_transit(h) == Some(tid)) {
                            continue; // mid-switch: not available to the other harts yet
                        }
                        // GP1, recomputed from the reference: every hart that may run it runs at least this priority
                        for h in 0..self.harts {
                            if r.pin.is_none_or(|p| p == h) {
                                let c = self.cur(h).map_or(0, |c| self.t[&c].prio);
                                if c < r.prio {
                                    ctx(format!(
                                        "GP1: hart {h} runs priority {c}, thread {tid} (priority {}) waits",
                                        r.prio
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn live_runnable(&self) -> Vec<Tid> {
        self.t
            .iter()
            .filter(|(_, r)| r.state == R::Runnable)
            .map(|(&t, _)| t)
            .collect()
    }
}

#[allow(clippy::needless_late_init)] // (each arm names the operation it did, for the failure message)
fn walk(harts: usize, seed: u64, steps: usize) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut w = World::new(harts);
    // thread: the boot code on hart 0
    let boot =
        w.s.adopt_current_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    w.t.insert(
        boot,
        Ref {
            state: R::Runnable,
            prio: 10,
            pin: None,
        },
    );
    w.check("adopt", seed, 0);
    for step in 1..=steps {
        let caller = r.below(harts as u64) as usize;
        w.finish(caller);
        let what;
        match r.below(100) {
            // spawn
            0..=19 => {
                let prio = match r.below(4) {
                    0 => 1 + r.below(31) as u8,
                    1 => 32 + r.below(32) as u8,
                    _ => 1 + r.below(8) as u8,
                };
                let pin = (r.below(4) == 0).then(|| r.below(harts as u64) as usize);
                let tid =
                    w.s.spawn_on(
                        caller,
                        SchedAttr {
                            prio,
                            rr: r.below(3) == 0,
                        },
                        pin,
                    )
                    .unwrap();
                w.t.insert(
                    tid,
                    Ref {
                        state: R::Runnable,
                        prio,
                        pin,
                    },
                );
                what = "spawn";
            }
            // block or sleep: the thread running on some hart
            20..=39 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if let Some(c) = w.cur(h) {
                    let timed = r.below(2) == 0;
                    w.s.prepare_block_on(h).unwrap();
                    if timed {
                        let d = w.now + 1 + r.below(30) * MS;
                        let blocked = w.s.sleep_until_on(h, d).unwrap();
                        assert!(blocked, "a prepared thread with no notification blocks");
                        w.t.get_mut(&c).unwrap().state = R::Blocked(Some(d));
                    } else {
                        let blocked = w.s.block_on(h).unwrap();
                        assert!(blocked);
                        w.t.get_mut(&c).unwrap().state = R::Blocked(None);
                    }
                }
                what = "block";
            }
            // wake
            40..=59 => {
                let blocked: Vec<Tid> =
                    w.t.iter()
                        .filter(|(_, x)| matches!(x.state, R::Blocked(_)))
                        .map(|(&t, _)| t)
                        .collect();
                if !blocked.is_empty() {
                    let t = blocked[r.below(blocked.len() as u64) as usize];
                    w.s.wake_on(caller, t).unwrap();
                    w.t.get_mut(&t).unwrap().state = R::Runnable;
                }
                what = "wake";
            }
            // yield
            60..=67 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if w.cur(h).is_some() {
                    w.s.yield_on(h).unwrap();
                }
                what = "yield";
            }
            // exit (keep a supply of threads: only when several are runnable)
            68..=73 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if let Some(c) = w.cur(h) {
                    if w.live_runnable().len() > harts + 2 {
                        w.s.exit_on(h).unwrap();
                        w.t.get_mut(&c).unwrap().state = R::Exited;
                        w.s.reap(c).unwrap();
                        w.t.remove(&c);
                    }
                }
                what = "exit";
            }
            // priority change
            74..=83 => {
                let live: Vec<Tid> =
                    w.t.iter()
                        .filter(|(_, x)| x.state != R::Exited)
                        .map(|(&t, _)| t)
                        .collect();
                if !live.is_empty() {
                    let t = live[r.below(live.len() as u64) as usize];
                    let prio = if r.below(2) == 0 {
                        1 + r.below(31) as u8
                    } else {
                        32 + r.below(32) as u8
                    };
                    w.s.set_priority_on(caller, t, prio).unwrap();
                    w.t.get_mut(&t).unwrap().prio = prio;
                }
                what = "set_priority";
            }
            // the clock: every hart ticks to the new time, so every due timer fires
            _ => {
                w.now += r.below(25) * MS + MS;
                for h in 0..harts {
                    w.finish(h);
                    w.s.tick_on(h, w.now);
                }
                let now = w.now;
                for x in w.t.values_mut() {
                    if let R::Blocked(Some(d)) = x.state {
                        if d <= now {
                            x.state = R::Runnable;
                        }
                    }
                }
                what = "tick";
            }
        }
        w.drain();
        w.check(what, seed, step);
    }
    assert_eq!(w.s.bad_picks(), 0);
}

#[test]
fn gp1_and_gp2_hold_on_random_walks_over_one_to_eight_harts() {
    for harts in 1..=8usize {
        for seed in 1..=60u64 {
            walk(harts, seed * 31 + harts as u64, 400);
        }
    }
}

#[test]
fn an_idle_hart_takes_a_new_thread_and_a_freed_hart_pulls_a_waiting_one() {
    let mut s = Sched2::new(2, FixedPrio::new);
    // hart 0 runs the boot thread at 20; a thread of 10 is placed on idle hart 1 at once
    let boot = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 20,
                rr: false,
            },
            None,
        )
        .unwrap();
    let a = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    assert_eq!(s.home(a), Some(1), "placed on the idle hart");
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), a);
    // a second thread of 10 has nowhere better (both harts run at least 10), so it queues
    let b = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    assert_eq!(s.state(b), Some(State::Runnable));
    // the boot thread blocks: hart 0 goes idle and takes the waiting thread
    s.prepare_block_on(0).unwrap();
    assert!(s.block_on(0).unwrap());
    for h in 0..2 {
        if s.resched_pending(h) {
            s.reschedule_on(h);
        }
    }
    assert_eq!(s.current_on(0), b, "hart 0 took the waiting thread");
    s.audit(true).unwrap();
    let _ = boot;
}

#[test]
fn a_pinned_thread_is_never_moved() {
    let mut s = Sched2::new(2, FixedPrio::new);
    let boot = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 20,
                rr: false,
            },
            None,
        )
        .unwrap();
    let p = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            Some(0),
        )
        .unwrap();
    assert_eq!(s.home(p), Some(0), "pinned to hart 0 though hart 1 is idle");
    for h in 0..2 {
        s.reschedule_on(h);
    }
    assert_eq!(
        s.current_on(1),
        s.idle(1),
        "hart 1 stays idle: the thread may not run there"
    );
    assert_eq!(s.state(p), Some(State::Runnable));
    s.audit(true).unwrap();
    let _ = boot;
}

#[test]
fn equal_priority_threads_are_not_pulled_back_and_forth() {
    // a pull needs a strictly better thread: an equal one stays where it is (no migration churn between equals)
    let mut s = Sched2::new(2, FixedPrio::new);
    let _boot = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    let b = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), b);
    let c = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    assert_eq!(
        s.home(c),
        Some(0),
        "both harts run 10: it queues on its creator's hart"
    );
    for _ in 0..10 {
        s.yield_on(1).unwrap();
        s.switched(1); // (each hart finishes its switch before it does anything else)
        s.yield_on(0).unwrap();
        s.switched(0);
    }
    assert_eq!(s.pulls, 0, "equals never migrate");
    s.audit(true).unwrap();
}
