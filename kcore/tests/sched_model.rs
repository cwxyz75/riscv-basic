//! The scheduler core against an independent reference model, for every policy.
//!
//! The model is a different formulation of the same rules, written from the
//! documented behaviour (kcore/src/sched.rs, module comment): one ordered "ready"
//! list with a linear scan instead of per-level queues. It runs the same random
//! operation sequence as the real scheduler and every observable (who is current,
//! every thread's state, the clock) must agree after every step; `audit()` and the
//! properties below are checked as well.
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::sched::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum M {
    Running,
    Ready,
    Blocked,
    Exited,
}

struct MT {
    prio: Prio,
    state: M,
    token: bool,
    deadline: Option<u64>,
    /// slice ticks it will have when it next runs (a preempted thread keeps its rest)
    slice: u32,
}

/// `by_priority` false models the round-robin policy (priorities ignored).
struct Model {
    by_priority: bool,
    threads: BTreeMap<Tid, MT>,
    ready: Vec<Tid>, // service order; the front of a level is its first occurrence
    current: Option<Tid>,
    now: u64,
    slice: u32,
    next: Tid,
}

impl Model {
    fn new(by_priority: bool) -> Model {
        Model {
            by_priority,
            threads: BTreeMap::new(),
            ready: vec![],
            current: None,
            now: 0,
            slice: TIMESLICE_TICKS,
            next: 1,
        }
    }
    fn best_ready(&self) -> Option<usize> {
        if !self.by_priority {
            return if self.ready.is_empty() { None } else { Some(0) };
        }
        let top = self.ready.iter().map(|t| self.threads[t].prio).max()?;
        self.ready.iter().position(|t| self.threads[t].prio == top)
    }
    fn ready_best_prio(&self) -> Option<Prio> {
        self.best_ready().map(|i| self.threads[&self.ready[i]].prio)
    }
    fn dispatch(&mut self) {
        if let Some(i) = self.best_ready() {
            let t = self.ready.remove(i);
            let th = self.threads.get_mut(&t).unwrap();
            th.state = M::Running;
            self.slice = std::mem::replace(&mut th.slice, TIMESLICE_TICKS);
            self.current = Some(t);
        }
    }
    fn resched(&mut self) {
        match self.current {
            None => self.dispatch(),
            Some(c) => {
                let p = self.threads[&c].prio;
                if self.by_priority && self.ready_best_prio().is_some_and(|b| b > p) {
                    let th = self.threads.get_mut(&c).unwrap();
                    th.state = M::Ready;
                    th.slice = self.slice.max(1);
                    self.ready.insert(0, c);
                    self.current = None;
                    self.dispatch();
                }
            }
        }
    }
    fn spawn(&mut self, prio: Prio) -> Tid {
        let t = self.next;
        self.next += 1;
        self.threads.insert(
            t,
            MT {
                prio,
                state: M::Ready,
                token: false,
                deadline: None,
                slice: TIMESLICE_TICKS,
            },
        );
        self.ready.push(t);
        self.resched();
        t
    }
    fn adopt(&mut self, prio: Prio) -> Tid {
        let t = self.next;
        self.next += 1;
        self.threads.insert(
            t,
            MT {
                prio,
                state: M::Running,
                token: false,
                deadline: None,
                slice: TIMESLICE_TICKS,
            },
        );
        self.current = Some(t);
        t
    }
    fn block(&mut self) -> bool {
        let c = self.current.unwrap();
        let t = self.threads.get_mut(&c).unwrap();
        if std::mem::take(&mut t.token) {
            return false;
        }
        t.state = M::Blocked;
        self.current = None;
        self.dispatch();
        true
    }
    fn sleep_until(&mut self, deadline: u64) -> bool {
        let c = self.current.unwrap();
        self.threads.get_mut(&c).unwrap().token = false;
        if deadline <= self.now {
            return false;
        }
        self.threads.get_mut(&c).unwrap().deadline = Some(deadline);
        self.block()
    }
    fn wake(&mut self, tid: Tid) {
        let t = self.threads.get_mut(&tid).unwrap();
        match t.state {
            M::Blocked => {
                t.deadline = None;
                t.state = M::Ready;
                self.ready.push(tid);
                self.resched();
            }
            M::Running | M::Ready => t.token = true,
            M::Exited => {}
        }
    }
    fn yield_now(&mut self) {
        let c = self.current.unwrap();
        self.threads.get_mut(&c).unwrap().state = M::Ready;
        self.ready.push(c);
        self.current = None;
        self.dispatch();
    }
    fn exit(&mut self) {
        let c = self.current.unwrap();
        self.threads.get_mut(&c).unwrap().state = M::Exited;
        self.current = None;
        self.dispatch();
    }
    fn set_priority(&mut self, tid: Tid, prio: Prio) {
        let old = std::mem::replace(&mut self.threads.get_mut(&tid).unwrap().prio, prio);
        match self.threads[&tid].state {
            M::Ready if old != prio => {
                self.ready.retain(|&t| t != tid);
                self.ready.push(tid);
                self.resched();
            }
            M::Running => self.resched(),
            _ => {}
        }
    }
    fn tick(&mut self) {
        self.now += 1;
        let mut due: Vec<(u64, Tid)> = self
            .threads
            .iter()
            .filter(|(_, t)| t.state == M::Blocked)
            .filter_map(|(&id, t)| t.deadline.filter(|&d| d <= self.now).map(|d| (d, id)))
            .collect();
        due.sort();
        for (_, id) in due {
            let t = self.threads.get_mut(&id).unwrap();
            t.deadline = None;
            t.state = M::Ready;
            self.ready.push(id);
        }
        if let Some(c) = self.current {
            self.slice = self.slice.saturating_sub(1);
            if self.slice == 0 {
                let p = self.threads[&c].prio;
                let rotate = if self.by_priority {
                    self.ready_best_prio().is_some_and(|b| b >= p)
                } else {
                    !self.ready.is_empty()
                };
                if rotate {
                    self.threads.get_mut(&c).unwrap().state = M::Ready;
                    self.ready.push(c);
                    self.current = None;
                    self.dispatch();
                } else {
                    self.slice = TIMESLICE_TICKS;
                }
            }
        }
        self.resched();
    }
}

fn agree<C: SchedClass>(s: &Sched<C>, m: &Model, ctx: &str) {
    assert_eq!(s.current(), m.current, "{ctx}: current thread");
    assert_eq!(s.now(), m.now, "{ctx}: clock");
    for (&tid, t) in &m.threads {
        let want = match t.state {
            M::Running => State::Running,
            M::Ready => State::Runnable,
            M::Blocked => State::Blocked,
            M::Exited => State::Exited,
        };
        assert_eq!(s.state(tid), Some(want), "{ctx}: state of thread {tid}");
        assert_eq!(
            s.priority(tid),
            Some(t.prio),
            "{ctx}: priority of thread {tid}"
        );
        if t.state == M::Blocked {
            assert_eq!(s.timer_of(tid), t.deadline, "{ctx}: timer of thread {tid}");
        }
    }
    s.audit().unwrap_or_else(|e| panic!("{ctx}: audit: {e}"));
    assert_eq!(
        s.bad_picks, 0,
        "{ctx}: the policy returned a thread that was not runnable"
    );
}

fn run<C: SchedClass>(make: impl Fn() -> C, by_priority: bool) {
    for seed in 0..SEEDS / 3 {
        let mut rng = Rng::new(seed);
        let mut s = Sched::new(make());
        let mut m = Model::new(by_priority);
        // The idle thread: never blocks or exits, like the kernel's.
        let idle = s.adopt_current(PRIO_IDLE).unwrap();
        assert_eq!(idle, m.adopt(PRIO_IDLE));
        let mut sleepers: BTreeMap<Tid, u64> = BTreeMap::new(); // property: wake exactly at the deadline
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            let cur = s.current().unwrap();
            let can_block = cur != idle;
            let all: Vec<Tid> = m.threads.keys().copied().collect();
            match rng.below(15) {
                0..=2 => {
                    let p = rng.below(PRIO_MAX as u64 + 1) as Prio;
                    let p = if p == PRIO_IDLE { 1 } else { p };
                    assert_eq!(s.spawn(p).unwrap(), m.spawn(p), "{ctx}: tid");
                }
                3 | 4 if can_block => {
                    assert_eq!(s.block_current().unwrap(), m.block(), "{ctx}: block");
                }
                5 | 6 if can_block => {
                    let d = s.now() + rng.below(6);
                    let got = s.sleep_until(d).unwrap();
                    assert_eq!(got, m.sleep_until(d), "{ctx}: sleep");
                    if got {
                        sleepers.insert(cur, d);
                    }
                }
                7..=9 => {
                    let t = all[rng.below(all.len() as u64) as usize];
                    s.wake(t).unwrap();
                    m.wake(t);
                    sleepers.remove(&t); // an early wake cancels the deadline check
                }
                10 => {
                    s.yield_current().unwrap();
                    m.yield_now();
                }
                11 if can_block && rng.chance(30) => {
                    s.exit_current().unwrap();
                    m.exit();
                }
                12 => {
                    let t = all[rng.below(all.len() as u64) as usize];
                    if t != idle {
                        let p = 1 + rng.below(PRIO_MAX as u64) as Prio;
                        s.set_priority(t, p).unwrap();
                        m.set_priority(t, p);
                    }
                }
                13 => {
                    // reap an exited thread, if there is one
                    if let Some(&t) = all.iter().find(|t| m.threads[t].state == M::Exited) {
                        s.reap(t).unwrap();
                        m.threads.remove(&t);
                        sleepers.remove(&t);
                    }
                }
                _ => {
                    s.tick();
                    m.tick();
                    // Property: a sleeper whose deadline is this tick is no longer blocked.
                    for (&t, &d) in &sleepers {
                        if d <= s.now() {
                            assert_ne!(
                                s.state(t),
                                Some(State::Blocked),
                                "{ctx}: sleeper {t} overslept its deadline {d}"
                            );
                        }
                    }
                    sleepers.retain(|_, d| *d > s.now());
                }
            }
            agree(&s, &m, &ctx);
            // Property (priority policy): nobody better than the current thread waits.
            if by_priority {
                let cp = s.priority(s.current().unwrap()).unwrap();
                for (&t, th) in &m.threads {
                    if th.state == M::Ready {
                        assert!(th.prio <= cp, "{ctx}: ready thread {t} (prio {}) outranks the running thread (prio {cp})", th.prio);
                    }
                }
            }
            // Exited threads are never current.
            assert_ne!(m.threads[&s.current().unwrap()].state, M::Exited, "{ctx}");
        }
    }
}

#[test]
fn fixed_priority_agrees_with_the_model() {
    run(FixedPriority::new, true);
}

#[test]
fn round_robin_agrees_with_the_model() {
    run(RoundRobin::new, false);
}

// --- Hand-derived scenarios (expected values worked out from the documented rules). ---

#[test]
fn equal_priority_threads_take_turns_one_slice_each() {
    let mut s = Sched::new(FixedPriority::new());
    let idle = s.adopt_current(PRIO_IDLE).unwrap();
    let (a, b, c) = (
        s.spawn(5).unwrap(),
        s.spawn(5).unwrap(),
        s.spawn(5).unwrap(),
    );
    assert_eq!(
        s.current(),
        Some(a),
        "the first spawned thread preempts idle and runs"
    );
    let mut order = vec![];
    for _ in 0..(3 * TIMESLICE_TICKS * 2) {
        order.push(s.current().unwrap());
        s.tick();
    }
    let slice = TIMESLICE_TICKS as usize;
    let expect: Vec<Tid> = (0..6)
        .flat_map(|i| std::iter::repeat_n([a, b, c][i % 3], slice))
        .collect();
    assert_eq!(
        order, expect,
        "A, B, C, A, B, C, each for exactly one slice"
    );
    assert_ne!(s.current(), Some(idle));
}

#[test]
fn a_higher_priority_thread_preempts_at_once_and_a_lower_one_waits() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let normal = s.spawn(10).unwrap();
    assert_eq!(s.current(), Some(normal));
    let rt = s.spawn(PRIO_RT_MIN).unwrap();
    assert_eq!(
        s.current(),
        Some(rt),
        "preempted immediately, not at the next tick"
    );
    assert_eq!(s.state(normal), Some(State::Runnable));
    let low = s.spawn(3).unwrap();
    assert_eq!(s.current(), Some(rt), "a lower priority never preempts");
    for _ in 0..100 {
        s.tick();
        assert_eq!(
            s.current(),
            Some(rt),
            "an RT thread keeps the CPU past any slice while nobody equal waits"
        );
    }
    s.exit_current().unwrap();
    assert_eq!(
        s.current(),
        Some(normal),
        "then the best of the rest: normal before low"
    );
    s.exit_current().unwrap();
    assert_eq!(s.current(), Some(low));
}

#[test]
fn a_preempted_thread_resumes_before_its_peers() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let a = s.spawn(5).unwrap();
    let b = s.spawn(5).unwrap(); // queued behind a
    let rt = s.spawn(40).unwrap();
    assert_eq!(s.current(), Some(rt));
    s.exit_current().unwrap();
    assert_eq!(
        s.current(),
        Some(a),
        "a was preempted, so it goes back to the front of its level, ahead of b"
    );
    let _ = b;
}

#[test]
fn sleep_wakes_on_exactly_its_deadline() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let t = s.spawn(7).unwrap();
    assert_eq!(s.sleep_until(5), Ok(true));
    assert_eq!(s.current(), Some(1), "idle runs while t sleeps");
    for _ in 0..4 {
        s.tick();
        assert_eq!(s.state(t), Some(State::Blocked));
    }
    s.tick(); // tick 5
    assert_eq!(s.now(), 5);
    assert_eq!(
        s.current(),
        Some(t),
        "it runs on the very tick its deadline arrives (and preempts idle)"
    );
    assert_eq!(
        s.sleep_until(5),
        Ok(false),
        "a deadline that has passed does not block"
    );
}

#[test]
fn a_wake_before_the_block_is_not_lost_and_wakes_do_not_queue() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let t = s.spawn(7).unwrap();
    assert_eq!(s.current(), Some(t));
    s.wake(t).unwrap(); // t is running: leaves a token
    s.wake(t).unwrap(); // a second wake does not queue
    assert_eq!(
        s.block_current(),
        Ok(false),
        "the token lets the block through"
    );
    assert_eq!(s.block_current(), Ok(true), "but only once");
    assert_eq!(s.state(t), Some(State::Blocked));
    s.wake(t).unwrap();
    assert_eq!(s.current(), Some(t));
}

#[test]
fn an_early_wake_cancels_the_sleep_timer() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let t = s.spawn(7).unwrap();
    s.sleep_until(50).unwrap();
    s.tick();
    s.wake(t).unwrap();
    assert_eq!(s.current(), Some(t));
    assert_eq!(s.timer_of(t), None);
    s.block_current().unwrap(); // blocks again, with no timer
    for _ in 0..60 {
        s.tick();
    }
    assert_eq!(
        s.state(t),
        Some(State::Blocked),
        "the cancelled timer must not wake it later"
    );
}

#[test]
fn lowering_the_running_thread_hands_over_to_a_better_one() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let a = s.spawn(20).unwrap();
    let b = s.spawn(10).unwrap();
    assert_eq!(s.current(), Some(a));
    s.set_priority(a, 5).unwrap();
    assert_eq!(s.current(), Some(b));
    s.set_priority(a, 30).unwrap();
    assert_eq!(s.current(), Some(a), "raising it puts it back on top");
    assert_eq!(s.set_priority(a, 64), Err(SchedError::BadPriority));
}

#[test]
fn wait_queue_wakes_in_order_and_never_loses_a_wakeup() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let mut q = WaitQueue::new();
    let (a, b) = (s.spawn(5).unwrap(), s.spawn(5).unwrap());
    assert_eq!(s.current(), Some(a));
    assert_eq!(q.wait(&mut s), Ok(true)); // a waits; b runs
    assert_eq!(s.current(), Some(b));
    assert_eq!(q.wait(&mut s), Ok(true)); // b waits; idle runs
    assert_eq!(q.wake_one(&mut s), Some(a), "first in, first out");
    assert_eq!(s.current(), Some(a));
    assert_eq!(q.wake_all(&mut s), 1);
    assert_eq!(q.wake_one(&mut s), None);
    // A wake that arrives before the wait is remembered (the token), so the wait does not block.
    s.wake(a).unwrap();
    assert_eq!(q.wait(&mut s), Ok(false));
    assert!(
        q.is_empty(),
        "and the non-blocking waiter is not left in the queue"
    );
}

#[test]
fn reaping_and_errors() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let t = s.spawn(5).unwrap();
    assert_eq!(
        s.reap(t),
        Err(SchedError::NotExited),
        "only exited threads can be reaped"
    );
    s.exit_current().unwrap();
    assert_eq!(s.state(t), Some(State::Exited));
    s.wake(t).unwrap();
    assert_eq!(
        s.state(t),
        Some(State::Exited),
        "waking the dead does nothing"
    );
    s.reap(t).unwrap();
    assert_eq!(s.state(t), None);
    assert_eq!(s.wake(t), Err(SchedError::NoSuchThread));
    assert_eq!(s.spawn(64), Err(SchedError::BadPriority));
    let mut empty: Sched<FixedPriority> = Sched::new(FixedPriority::new());
    assert_eq!(empty.block_current(), Err(SchedError::NoCurrent));
}

#[test]
fn a_periodic_higher_priority_thread_cannot_starve_the_next_peer() {
    // A and B share priority 10; R (priority 40) wakes every 3 ticks, preempts, and
    // sleeps again. A's slice must keep draining while R keeps interrupting it, so B
    // gets its turn once A's slice is used up.
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let a = s.spawn(10).unwrap();
    let b = s.spawn(10).unwrap();
    let r = s.spawn(40).unwrap();
    assert_eq!(s.current(), Some(r));
    let mut b_ran_at = None;
    for _ in 0..200 {
        if s.current() == Some(r) {
            s.sleep_until(s.now() + 3).unwrap(); // R's period
        }
        if s.current() == Some(b) && b_ran_at.is_none() {
            b_ran_at = Some(s.now());
        }
        s.tick();
    }
    let at = b_ran_at.expect("B never ran: A's slice kept being reset by the interrupting thread");
    assert!(
        at <= 2 * TIMESLICE_TICKS as u64 + 6,
        "B first ran at tick {at}, far later than A's slice allows"
    );
    let _ = a;
}

#[test]
fn stale_wait_queue_entries_do_not_swallow_wakeups() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let mut q = WaitQueue::new();
    let (a, b) = (s.spawn(5).unwrap(), s.spawn(5).unwrap());
    assert_eq!(s.current(), Some(a));
    assert_eq!(q.wait(&mut s), Ok(true)); // a waits (queue: a); b runs
    assert_eq!(q.wait(&mut s), Ok(true)); // b waits (queue: a, b); idle runs
    s.wake(a).unwrap(); // a is woken some other way: its queue entry is now stale
    assert_eq!(s.current(), Some(a));
    assert_eq!(
        q.wake_one(&mut s),
        Some(b),
        "the stale entry for a is skipped; b is really woken"
    );
    // a waits again: it must not be queued twice.
    assert_eq!(q.wait(&mut s), Ok(true));
    assert_eq!(q.len(), 1);
}

#[test]
fn adopt_and_reap_misuse_are_errors() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    assert_eq!(s.adopt_current(5), Err(SchedError::HasCurrent));
    let t = s.spawn(5).unwrap();
    assert_eq!(s.reap(t), Err(SchedError::NotExited));
}

#[test]
fn a_thread_is_queued_at_most_once() {
    let mut s = Sched::new(FixedPriority::new());
    s.adopt_current(PRIO_IDLE).unwrap();
    let mut q = WaitQueue::new();
    let a = s.spawn(5).unwrap();
    assert_eq!(q.wait(&mut s), Ok(true)); // a waits: queue [a]
    s.wake(a).unwrap(); // woken some other way: the entry is stale
    assert_eq!(q.wait(&mut s), Ok(true)); // a waits again
    assert_eq!(
        q.len(),
        1,
        "the stale entry must be replaced, not duplicated"
    );
}
