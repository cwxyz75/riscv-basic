//! Task 11, review findings 1 to 3: `Sched2` driven the way the kernel drives it.
//!
//! Differences from `sched2_smp_model.rs`: harts 1.. start **offline** and join at random moments (`set_clock`,
//! `set_online`, `reschedule_on`, as `join_scheduler` does); threads may be pinned to harts that have not joined; marked
//! harts are served in a **random order and within a hard bound** (a mark storm fails the test instead of passing
//! silently); the **poke contract** is checked after every operation (a mark that turns on has its poke bit, a hart is
//! never poked while offline, the poke set names exactly the marked harts); a thread that has prepared to wait is woken
//! before it blocks (the notified path); and the walk runs over both normal-band classes with each class's own form of
//! GP1 (priority order for fixed priority; work conservation for round robin, plus strict real-time precedence).

// (the walk indexes per-hart vectors by hart number and names each arm's operation for the failure message)
#![allow(clippy::needless_range_loop, clippy::needless_late_init)]

use kcore::sched::{State, Tid};
use kcore::sched2::{Class, FixedPrio, RoundRobin, SchedAttr, MS};
use kcore::sched_smp::{Sched2, Waiters2};
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
    Blocked,
    Exited,
}

struct Ref {
    state: R,
    prio: u8,
    pin: Option<usize>,
}

struct World<N: Class> {
    s: Sched2<N>,
    t: BTreeMap<Tid, Ref>,
    harts: usize,
    joined: Vec<bool>,
    rr: bool,
    /// Marks set by `finish` since the last poke check (it checked them itself; the operation's check must not count them
    /// as its own).
    carry: Vec<bool>,
    /// The thread whose context is live on each hart: what the hart last switched to (`finish`), not what the scheduler has
    /// designated since. No thread may be designated on one hart while it is executing on another.
    executing: Vec<Option<Tid>>,
    /// Harts whose open switch came from a tick or a served mark (no big lock held yet). Every other switch is made under
    /// the big kernel lock, which the hart keeps until the switch is done, so no other hart's big-lock operation can run
    /// while it is open (`bkl_barrier`).
    fast: Vec<bool>,
}

impl<N: Class> World<N> {
    fn cur(&self, h: usize) -> Option<Tid> {
        let c = self.s.current_on(h);
        (c != self.s.idle(h)).then_some(c)
    }

    /// Hart `h` finishes the context switch it was in the middle of: what every kernel path does before the hart does
    /// anything else. The thread it left becomes available to the other harts, and the marks owed for it are poked.
    fn finish(&mut self, h: usize) {
        self.fast[h] = false;
        // the hart's switch is complete (or there was none): its context is the one it is designated to run
        self.executing[h] = self.cur(h);
        if self.s.in_transit(h).is_some() {
            let before = self.pending();
            let _ = self.s.take_pokes();
            self.s.switched(h);
            let pokes = self.s.take_pokes();
            for x in 0..self.harts {
                let new_mark = self.s.resched_pending(x) && !before[x];
                if new_mark {
                    self.carry[x] = true;
                }
                assert_eq!(
                    pokes & (1 << x) != 0,
                    new_mark,
                    "finishing hart {h}'s switch: marks and pokes disagree for hart {x}"
                );
            }
        }
    }

    /// An operation that takes the big kernel lock starts: every switch made under that lock has finished (its hart held the
    /// lock until then); only switches begun by a tick or a served mark, which take it later, may still be open.
    fn bkl_barrier(&mut self) {
        for h in 0..self.harts {
            if !self.fast[h] {
                self.finish(h);
            }
        }
    }

    fn pending(&self) -> Vec<bool> {
        (0..self.harts).map(|h| self.s.resched_pending(h)).collect()
    }

    /// After an operation that started with no mark pending: the poke set is exactly the marked harts, none offline.
    fn check_pokes(&mut self, before: &[bool], what: &str, seed: u64, step: usize) {
        let pokes = self.s.take_pokes();
        for h in 0..self.harts {
            let marked = self.s.resched_pending(h);
            let bit = pokes & (1 << h) != 0;
            if marked && !before[h] && !self.carry[h] && !bit {
                panic!(
                    "seed {seed} step {step} after {what}: hart {h} was marked with no poke owed"
                );
            }
            if bit && !marked {
                panic!(
                    "seed {seed} step {step} after {what}: a poke owed to hart {h}, which is not marked"
                );
            }
            if (marked || bit) && !self.joined[h] {
                panic!(
                    "seed {seed} step {step} after {what}: hart {h} is marked or poked while offline"
                );
            }
        }
        for h in 0..self.harts {
            self.carry[h] = false;
        }
    }

    /// Serve the marked harts in random order, within a bound: a mark that does not die out is a failure.
    fn serve(&mut self, r: &mut Rng, what: &str, seed: u64, step: usize) {
        let bound = 4 * self.harts + 4;
        for round in 0..=bound {
            let marked: Vec<usize> = (0..self.harts)
                .filter(|&h| self.s.resched_pending(h))
                .collect();
            if marked.is_empty() {
                return;
            }
            if round == bound {
                panic!("seed {seed} step {step} after {what}: marks did not die out in {bound} rounds ({marked:?})");
            }
            let h = marked[r.below(marked.len() as u64) as usize];
            self.finish(h);
            let before = self.pending();
            self.s.reschedule_on(h);
            self.fast[h] = self.s.in_transit(h).is_some();
            self.check_pokes(&before, what, seed, step);
        }
    }

    fn check(&self, what: &str, seed: u64, step: usize) {
        let ctx = |m: String| panic!("seed {seed} step {step} after {what}: {m}");
        if let Err(e) = self.s.audit(true) {
            ctx(format!("audit: {e}"));
        }
        let mut running: BTreeMap<Tid, usize> = BTreeMap::new();
        for h in 0..self.harts {
            if let Some(c) = self.cur(h) {
                if !self.joined[h] {
                    ctx(format!("offline hart {h} runs thread {c}"));
                }
                if running.insert(c, h).is_some() {
                    ctx(format!("thread {c} runs on two harts"));
                }
                let r = &self.t[&c];
                if r.state != R::Runnable || r.pin.is_some_and(|p| p != h) {
                    ctx(format!(
                        "hart {h} runs thread {c}: {:?}, pin {:?}",
                        r.state, r.pin
                    ));
                }
                if self.s.state(c) != Some(State::Running) {
                    ctx(format!(
                        "thread {c} is current on {h} but {:?}",
                        self.s.state(c)
                    ));
                }
            }
        }
        for x in 0..self.harts {
            if let Some(c) = self.cur(x) {
                for y in 0..self.harts {
                    if y != x && self.executing[y] == Some(c) {
                        ctx(format!("double dispatch: thread {c} is designated on hart {x} while it executes on hart {y}"));
                    }
                }
            }
        }
        for (&tid, r) in &self.t {
            match r.state {
                R::Blocked => assert_eq!(
                    self.s.state(tid),
                    Some(State::Blocked),
                    "seed {seed} step {step}"
                ),
                R::Exited => assert_eq!(
                    self.s.state(tid),
                    Some(State::Exited),
                    "seed {seed} step {step}"
                ),
                R::Runnable if !running.contains_key(&tid) => {
                    if self.s.state(tid) != Some(State::Runnable) {
                        ctx(format!(
                            "thread {tid} should wait, is {:?}",
                            self.s.state(tid)
                        ));
                    }
                    if (0..self.harts).any(|h| self.s.in_transit(h) == Some(tid)) {
                        continue; // mid-switch: not available to the other harts yet
                    }
                    for h in 0..self.harts {
                        if !self.joined[h] || r.pin.is_some_and(|p| p != h) {
                            continue;
                        }
                        let c = self.cur(h).map_or(0, |c| self.t[&c].prio);
                        // GP1 per class, recomputed from the reference
                        let bad = if self.rr && r.prio < 32 {
                            self.cur(h).is_none() // round robin: a hart must not idle while a normal thread it may run waits
                        } else {
                            c < r.prio // fixed priority, and real-time strict precedence
                        };
                        if bad {
                            ctx(format!(
                                "GP1: hart {h} runs priority {c}, thread {tid} (priority {}) waits",
                                r.prio
                            ));
                        }
                    }
                }
                R::Runnable => {}
            }
        }
    }
}

fn walk<N: Class>(make: fn() -> N, rr: bool, harts: usize, seed: u64, steps: usize) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut w = World {
        s: Sched2::new(harts, make),
        t: BTreeMap::new(),
        harts,
        joined: (0..harts).map(|h| h == 0).collect(),
        rr,
        carry: vec![false; harts],
        executing: vec![None; harts],
        fast: vec![false; harts],
    };
    for h in 1..harts {
        w.s.set_online(h, false);
    }
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
    let mut now: u64 = 0;
    let mut clock = vec![0u64; harts];
    for step in 1..=steps {
        let caller = loop {
            let c = r.below(harts as u64) as usize;
            if w.joined[c] {
                break c;
            }
        };
        w.finish(caller);
        let before = w.pending();
        let what;
        let op = r.below(100);
        if !(86..=99).contains(&op) {
            w.bkl_barrier();
        }
        match op {
            // a hart joins
            0..=4 => {
                if let Some(h) = (1..harts).find(|&h| !w.joined[h]) {
                    w.s.set_clock(h, now);
                    clock[h] = now;
                    w.s.set_online(h, true);
                    w.joined[h] = true;
                    w.finish(h);
                    w.s.reschedule_on(h);
                }
                what = "join";
            }
            // spawn (maybe pinned to a hart that has not joined)
            5..=24 => {
                let prio = match r.below(4) {
                    0 => 1 + r.below(31) as u8,
                    1 => 32 + r.below(32) as u8,
                    _ => 1 + r.below(8) as u8,
                };
                let pin = (r.below(4) == 0).then(|| r.below(harts as u64) as usize);
                let tid =
                    w.s.spawn_on(caller, SchedAttr { prio, rr: false }, pin)
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
            // block (untimed) the thread on some hart
            25..=38 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if let Some(c) = w.cur(h) {
                    w.s.prepare_block_on(h).unwrap();
                    assert!(w.s.block_on(h).unwrap());
                    w.t.get_mut(&c).unwrap().state = R::Blocked;
                }
                what = "block";
            }
            // prepare, then a wake before the block: the notified path: it must not block
            39..=44 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if let Some(c) = w.cur(h) {
                    w.s.prepare_block_on(h).unwrap();
                    assert!(
                        w.s.wake_counted_on(caller, c).unwrap(),
                        "a wake of a prepared thread counts"
                    );
                    assert!(
                        !w.s.block_on(h).unwrap(),
                        "a notified thread does not block"
                    );
                    assert_eq!(w.s.state(c), Some(State::Running));
                }
                what = "notified";
            }
            // wake any thread: blocked, prepared-not-blocked, running, queued or exited
            45..=62 => {
                let live: Vec<Tid> =
                    w.t.iter()
                        .filter(|(_, x)| x.state != R::Exited)
                        .map(|(&t, _)| t)
                        .collect();
                if !live.is_empty() {
                    let t = live[r.below(live.len() as u64) as usize];
                    let blocked = w.t[&t].state == R::Blocked;
                    let counted = w.s.wake_counted_on(caller, t).unwrap();
                    assert_eq!(counted, blocked, "only a blocked thread's wake counts here");
                    if blocked {
                        w.t.get_mut(&t).unwrap().state = R::Runnable;
                    }
                }
                what = "wake";
            }
            // yield
            63..=70 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if w.cur(h).is_some() {
                    w.s.yield_on(h).unwrap();
                }
                what = "yield";
            }
            // exit
            71..=75 => {
                let h = r.below(harts as u64) as usize;
                w.finish(h);
                if let Some(c) = w.cur(h) {
                    let runnable = w.t.values().filter(|x| x.state == R::Runnable).count();
                    if runnable > harts + 2 {
                        w.s.exit_on(h).unwrap();
                        w.s.reap(c).unwrap();
                        w.t.remove(&c);
                    }
                }
                what = "exit";
            }
            // priority change (never 0)
            76..=85 => {
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
            // the clock: hart 0 moves it, each joined hart ticks alone and may lag by up to a millisecond
            _ => {
                now += (1 + r.below(20)) * MS;
                for h in 0..harts {
                    if w.joined[h] {
                        let lag = if h == 0 { 0 } else { r.below(2) * MS };
                        let t = now.saturating_sub(lag).max(clock[h]);
                        clock[h] = t;
                        w.finish(h);
                        let before = w.pending();
                        w.s.tick_on(h, t);
                        w.fast[h] = w.s.in_transit(h).is_some();
                        w.check_pokes(&before, "tick", seed, step);
                        w.serve(&mut r, "tick", seed, step);
                    }
                }
                what = "tick";
            }
        }
        w.check_pokes(&before, what, seed, step);
        w.serve(&mut r, what, seed, step);
        w.check(what, seed, step);
    }
    assert_eq!(w.s.bad_picks(), 0);
}

#[test]
fn kernel_style_walk_fixed_priority() {
    for harts in 1..=8usize {
        for seed in 1..=25u64 {
            walk(FixedPrio::new, false, harts, seed * 17 + harts as u64, 300);
        }
    }
}

#[test]
fn kernel_style_walk_round_robin() {
    for harts in 1..=8usize {
        for seed in 1..=25u64 {
            walk(RoundRobin::new, true, harts, seed * 29 + harts as u64, 300);
        }
    }
}

#[test]
fn a_hart_that_has_not_joined_is_never_marked_and_picks_its_pinned_thread_up_when_it_joins() {
    let s = Sched2::new(2, FixedPrio::new);
    s.set_online(1, false);
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
            Some(1),
        )
        .unwrap();
    assert_eq!(s.home(p), Some(1));
    assert!(
        !s.resched_pending(1) && s.take_pokes() == 0,
        "an offline hart is not marked"
    );
    s.set_priority_on(0, p, 12).unwrap();
    assert!(
        !s.resched_pending(1) && s.take_pokes() == 0,
        "not by a priority change either"
    );
    let free = s
        .spawn_on(0, SchedAttr { prio: 5, rr: false }, None)
        .unwrap();
    assert_eq!(
        s.home(free),
        Some(0),
        "a free thread is never placed on a hart that has not joined"
    );
    assert_eq!(
        s.thread_count(),
        4,
        "the offline hart's idle thread is not counted"
    );
    s.set_clock(1, 0);
    s.set_online(1, true);
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), p);
    assert_eq!(s.thread_count(), 5);
    let _ = boot;
}

#[test]
fn a_sleeper_wakes_on_its_home_harts_tick_and_never_before_its_deadline() {
    let s = Sched2::new(2, FixedPrio::new);
    // hart 1 runs the sleeper (pinned there), hart 0 the boot thread
    let _boot = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 20,
                rr: false,
            },
            None,
        )
        .unwrap();
    let sl = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            Some(1),
        )
        .unwrap();
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), sl);
    s.prepare_block_on(1).unwrap();
    assert!(s.sleep_until_on(1, 10 * MS).unwrap());
    // hart 0's clock is ahead, hart 1's lags: the sleeper is home on hart 1 and wakes when hart 1's clock reaches 10 ms
    s.tick_on(0, 12 * MS);
    assert_eq!(
        s.state(sl),
        Some(State::Blocked),
        "hart 0's tick does not fire hart 1's timers"
    );
    s.tick_on(1, 9 * MS);
    assert_eq!(
        s.state(sl),
        Some(State::Blocked),
        "never before the deadline"
    );
    s.tick_on(1, 10 * MS);
    assert_ne!(
        s.state(sl),
        Some(State::Blocked),
        "at the deadline on its home hart's clock"
    );
}

#[test]
fn waiters_wake_in_fifo_order_and_a_stale_entry_does_not_swallow_a_wake() {
    let s = Sched2::new(1, FixedPrio::new);
    let a = s
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
    let mut q = Waiters2::new();
    // a waits and blocks; b runs, waits and blocks; c runs, waits and blocks: the queue is a, b, c
    assert_eq!(s.current_on(0), a);
    assert!(q.wait_on(&s, 0).unwrap());
    assert_eq!(s.current_on(0), b);
    assert!(q.wait_on(&s, 0).unwrap());
    assert_eq!(s.current_on(0), c);
    assert!(q.wait_on(&s, 0).unwrap());
    assert_eq!(q.len(), 3);
    // first in, first woken
    assert_eq!(q.wake_one_on(&s, 0), Some(a));
    // b is woken some other way (not through the queue): its entry is now stale
    s.wake_on(0, b).unwrap();
    // the next wake skips the stale entry and takes c, whose wake counts
    assert_eq!(q.wake_one_on(&s, 0), Some(c));
    assert!(q.is_empty());
    assert_eq!(q.wake_one_on(&s, 0), None, "nothing left to wake");
    // wake_all counts what it woke
    let d = s.current_on(0);
    assert!(q.wait_on(&s, 0).unwrap());
    let e = s.current_on(0);
    assert_ne!(d, e);
    assert_eq!(q.wake_all_on(&s, 0), 1);
}

#[test]
fn priority_zero_is_refused() {
    let s = Sched2::new(1, FixedPrio::new);
    let t = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    assert!(s
        .spawn_on(0, SchedAttr { prio: 0, rr: false }, None)
        .is_err());
    assert!(s.set_priority_on(0, t, 0).is_err());
}

#[test]
fn round_robin_does_not_ping_pong_a_higher_priority_thread_between_harts() {
    // review finding 1: a higher-priority normal thread is not pulled to, or marked for, a hart that would only queue it
    let s = Sched2::new(2, RoundRobin::new);
    let _a = s
        .adopt_current_on(0, SchedAttr { prio: 5, rr: false }, None)
        .unwrap();
    let b = s
        .spawn_on(0, SchedAttr { prio: 7, rr: false }, None)
        .unwrap();
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), b);
    let _t = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    for _ in 0..50 {
        for h in 0..2 {
            if s.resched_pending(h) {
                s.reschedule_on(h);
            }
        }
    }
    assert!(
        !s.resched_pending(0) && !s.resched_pending(1),
        "no mark is left pending"
    );
    assert!(
        s.pulls() <= 1,
        "at most one pull, not a ping-pong: {}",
        s.pulls()
    );
    s.audit(true).unwrap();
}

#[test]
fn one_thread_marks_one_hart() {
    // review finding 2: eight harts, seven idle: a spawn owes one poke, not seven
    let s = Sched2::new(8, FixedPrio::new);
    let _boot = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 30,
                rr: false,
            },
            None,
        )
        .unwrap();
    let t = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    let pokes = s.take_pokes();
    assert_eq!(
        pokes.count_ones(),
        1,
        "one poke for one thread, got {pokes:#b}"
    );
    let h = s.home(t).unwrap();
    assert_eq!(pokes, 1 << h, "and it is for the hart placement chose");
}

#[test]
fn a_hart_that_joins_late_starts_its_clock_at_the_join_not_at_zero() {
    // review finding 3 (M9): without `set_clock`, the first tick of a hart that joined at 500 ms would account 501 ms to the
    // thread running there and rotate it at once
    let s = Sched2::new(2, FixedPrio::new);
    s.set_online(1, false);
    let _boot = s
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
            Some(1),
        )
        .unwrap();
    let _b = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            Some(1),
        )
        .unwrap();
    s.set_clock(1, 500 * MS);
    s.set_online(1, true);
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), a);
    s.tick_on(1, 501 * MS);
    assert_eq!(
        s.current_on(1),
        a,
        "one millisecond into a ten millisecond slice: no rotation"
    );
    s.tick_on(1, 511 * MS);
    assert_ne!(
        s.current_on(1),
        a,
        "after the slice, the equal peer gets its turn"
    );
}

#[test]
fn a_lopsided_queue_of_equals_is_shared_out_at_the_ticks_and_then_left_alone() {
    // all harts busy, one with a long queue of equal-priority threads: equals are not pulled by priority, so the tick moves
    // them by queue length, one per tick per hart, until the queues are within one of each other
    let s = Sched2::new(2, FixedPrio::new);
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
    for _ in 0..8 {
        // both harts run 10: each new equal queues on its creator's hart
        s.spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    }
    assert_eq!((s.queued_on(0), s.queued_on(1)), (8, 0));
    let mut now = 0;
    for _ in 0..20 {
        now += MS;
        s.tick_on(0, now);
        s.tick_on(1, now);
        for h in 0..2 {
            if s.resched_pending(h) {
                s.reschedule_on(h);
            }
        }
    }
    let (a, c) = (s.queued_on(0), s.queued_on(1));
    assert!(
        a.abs_diff(c) <= 1,
        "queues of {a} and {c}: within one of each other"
    );
    let pulls = s.pulls();
    for _ in 0..20 {
        now += MS;
        s.tick_on(0, now);
        s.tick_on(1, now);
    }
    assert_eq!(s.pulls(), pulls, "balanced queues do not churn");
    s.audit(true).unwrap();
}

#[test]
fn a_thread_switched_away_is_not_available_to_other_harts_until_its_switch_completes() {
    // the race a kernel without a lock across the switch would have: hart 0's tick rotates A out (queued, still executing); an
    // idle hart 1 must not take A and run it while hart 0 is still saving its context
    let s = Sched2::new(2, FixedPrio::new);
    let a = s
        .adopt_current_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            None,
        )
        .unwrap();
    s.set_online(1, false); // (keep hart 1 out of it while the situation is set up)
    let b = s
        .spawn_on(
            0,
            SchedAttr {
                prio: 10,
                rr: false,
            },
            Some(0),
        )
        .unwrap();
    s.tick_on(0, 11 * MS); // the slice is over and an equal waits: A is rotated out
    assert_eq!(s.current_on(0), b);
    assert_eq!(
        s.in_transit(0),
        Some(a),
        "A is still executing on hart 0, queued but in transit"
    );
    s.set_online(1, true);
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), s.idle(1), "hart 1 cannot take A yet");
    assert_eq!(s.home(a), Some(0));
    s.switched(0);
    assert_eq!(s.in_transit(0), None);
    s.reschedule_on(1);
    assert_eq!(s.current_on(1), a, "now it can");
    s.audit(true).unwrap();
}

#[test]
fn a_tick_that_rotates_and_then_pulls_still_protects_the_thread_that_is_executing() {
    // the review of task 12 (its reproduction): hart 0's tick rotates A out, and the same settle pulls C from hart 1 over B;
    // the transit marker must stay on A, which is still executing, so that hart 2 cannot take it
    let at = |prio: u8| SchedAttr { prio, rr: false };
    let s = Sched2::new(3, FixedPrio::new);
    s.set_online(2, false);
    let a = s.adopt_current_on(0, at(10), None).unwrap(); // A runs on hart 0, free
    let c = s.adopt_current_on(1, at(20), None).unwrap(); // C runs on hart 1, free
    let _b = s.spawn_on(0, at(10), Some(0)).unwrap(); // B: A's equal peer, pinned to hart 0, queued
    assert_eq!(s.current_on(0), a);
    let x = s.spawn_on(1, at(30), Some(1)).unwrap(); // X (30, pinned to 1) preempts C
    assert_eq!(s.current_on(1), x);
    assert_eq!(s.in_transit(1), Some(c));
    s.switched(1); // hart 1 finished: C is available, hart 0 is marked for it
    s.tick_on(0, 11 * MS); // the slice is over: A rotates out, and the settle pulls C over B
    assert_eq!(
        s.in_transit(0),
        Some(a),
        "A is the thread still executing on hart 0"
    );
    s.set_online(2, true);
    s.reschedule_on(2);
    assert_ne!(
        s.current_on(2),
        a,
        "hart 2 took A while hart 0 has not completed its switch"
    );
    s.audit(false).unwrap();
    s.switched(0);
}
