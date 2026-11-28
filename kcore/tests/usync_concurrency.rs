//! T287 (T222): the user library's semaphore (`kcore::usync`, which `rt::sync::Semaphore` runs over the kernel's futex) loses no
//! wake, over a model futex built on the kernel's own futex table (`kcore::futex::Futexes`: compare and enqueue under one lock, a
//! wake dequeues, a timeout `forget`s a sleeper still in the table). An independent oracle for the user half of T222's question
//! ("two posts, two taken: got 1": a late sleeper or a lost wake); the kernel half is `futex_concurrency.rs`.
//!
//! The properties: with no deadline every taker owed a permit gets one (a lost wake leaves a taker asleep forever, which loom and
//! shuttle report as a deadlock); with deadlines that pass at any moment, no permit is lost or made (takes plus what is left equal the
//! posts), and a taker that gives up leaves no permit behind it that it could have taken.
//! Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test usync_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test usync_concurrency
//! (`make os-test` does both). Without either cfg this file is empty.
#![cfg(any(loom, shuttle))]

use kcore::futex::Futexes;
use kcore::sync::{AtomicBool, AtomicU32, Ordering};
use kcore::usync::{self, Futex, Took, Wait};

#[cfg(loom)]
use loom::{
    sync::{Arc, Condvar, Mutex},
    thread,
};
#[cfg(shuttle)]
use shuttle::{
    sync::{Arc, Condvar, Mutex},
    thread,
};

const KEY: (u64, u64) = (1, 0x1000);

/// The kernel's futex, modelled: the table under one lock (the bucket lock), sleepers parked on a condition variable until a wake or a
/// timeout has taken them out of the table.
struct Model {
    table: Mutex<Futexes>,
    cv: Condvar,
}

/// One taker's view of the model futex: its sleeper id, and its deadline (`passed`: the timer has fired; set by a timer thread at any
/// moment the checker picks, or never).
struct Taker {
    m: Arc<Model>,
    who: u64,
    passed: Arc<AtomicBool>,
    timed_out: AtomicBool,
}

impl Futex for Taker {
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: u64) -> Wait {
        let mut t = self.m.table.lock().unwrap();
        if word.load(Ordering::SeqCst) != expected {
            return Wait::Differed;
        }
        if deadline != u64::MAX && self.passed.load(Ordering::SeqCst) {
            return Wait::TimedOut; // (a deadline already past: the kernel does not sleep)
        }
        t.wait(KEY, self.who).unwrap();
        while t.contains(self.who) {
            t = self.m.cv.wait(t).unwrap();
        }
        if self.timed_out.swap(false, Ordering::SeqCst) {
            Wait::TimedOut
        } else {
            Wait::Woken
        }
    }
    fn wake(&self, _word: &AtomicU32, count: u32) -> usize {
        let n = self
            .m
            .table
            .lock()
            .unwrap()
            .wake(KEY, u64::from(count))
            .unwrap()
            .len();
        self.m.cv.notify_all();
        n
    }
    fn expired(&self, deadline: u64) -> bool {
        deadline != u64::MAX && self.passed.load(Ordering::SeqCst)
    }
}

/// The poster's handle (it never waits).
fn poster(m: &Arc<Model>) -> Taker {
    Taker {
        m: m.clone(),
        who: 0,
        passed: Arc::new(AtomicBool::new(false)),
        timed_out: AtomicBool::new(false),
    }
}

fn taker(m: &Arc<Model>, who: u64) -> Arc<Taker> {
    Arc::new(Taker {
        m: m.clone(),
        who,
        passed: Arc::new(AtomicBool::new(false)),
        timed_out: AtomicBool::new(false),
    })
}

/// The timer of `t`'s deadline: the deadline passes, and a sleeper still in the table is taken out with TIMED_OUT (`forget`).
fn fire(t: &Taker) {
    let mut table = t.m.table.lock().unwrap();
    t.passed.store(true, Ordering::SeqCst);
    if table.forget(t.who) {
        t.timed_out.store(true, Ordering::SeqCst);
    }
    drop(table);
    t.m.cv.notify_all();
}

fn model() -> Arc<Model> {
    Arc::new(Model {
        table: Mutex::new(Futexes::new()),
        cv: Condvar::new(),
    })
}

/// One sleeper, one post: the sleeper takes it (a lost wake leaves it asleep: a deadlock).
fn a_post_wakes_a_sleeper() {
    let m = model();
    let count = Arc::new(AtomicU32::new(0));
    let t = taker(&m, 1);
    let (c2, t2) = (count.clone(), t.clone());
    let h = thread::spawn(move || usync::wait_until(&c2, u64::MAX, &*t2));
    usync::post(&count, &poster(&m));
    assert!(h.join().unwrap().is_some(), "the sleeper took the post");
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// T222's shape: two sleepers, two posts from one thread (back to back, so the second comes before the first woken taker has taken
/// its permit): both take one and none is left.
fn two_posts_release_two_sleepers() {
    let m = model();
    let count = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = [1u64, 2]
        .into_iter()
        .map(|who| {
            let (c, t) = (count.clone(), taker(&m, who));
            thread::spawn(move || usync::wait_until(&c, u64::MAX, &*t))
        })
        .collect();
    let p = poster(&m);
    usync::post(&count, &p);
    usync::post(&count, &p);
    for h in hs {
        assert!(
            h.join().unwrap().is_some(),
            "every sleeper owed a permit took one"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0, "two posts, two taken");
}

/// A deadline that passes at any moment races the post: the permit is taken by the taker or left in the count, never lost or made, and
/// a taker that gave up saw none (it looks once more after a timeout).
fn a_deadline_races_a_post() {
    let m = model();
    let count = Arc::new(AtomicU32::new(0));
    let t = taker(&m, 1);
    let (c2, t2) = (count.clone(), t.clone());
    let h = thread::spawn(move || usync::wait_until(&c2, 1_000, &*t2));
    let t3 = t.clone();
    let timer = thread::spawn(move || fire(&t3));
    usync::post(&count, &poster(&m));
    let took = h.join().unwrap();
    timer.join().unwrap();
    let left = count.load(Ordering::SeqCst);
    assert_eq!(
        u32::from(took.is_some()) + left,
        1,
        "the one permit is taken or left: {took:?}, {left} left"
    );
    assert!(
        !m.table.lock().unwrap().contains(1),
        "the taker left the table"
    );
}

/// Two takers with deadlines, two posts: whatever the deadlines do, takes plus what is left is two; a taker that took after its
/// deadline passed says so (`AfterTimeout`), which the suite's test counts as a lost wake only when every post came long before it.
fn deadlines_and_posts_conserve_permits() {
    let m = model();
    let count = Arc::new(AtomicU32::new(0));
    let ts = [taker(&m, 1), taker(&m, 2)];
    let hs: Vec<_> = ts
        .iter()
        .map(|t| {
            let (c, t) = (count.clone(), t.clone());
            thread::spawn(move || usync::wait_until(&c, 1_000, &*t))
        })
        .collect();
    let t1 = ts[0].clone();
    let timer = thread::spawn(move || fire(&t1));
    let p = poster(&m);
    usync::post(&count, &p);
    usync::post(&count, &p);
    let took: Vec<Option<Took>> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    timer.join().unwrap();
    let taken = took.iter().filter(|t| t.is_some()).count() as u32;
    assert_eq!(
        taken + count.load(Ordering::SeqCst),
        2,
        "permits conserved: {took:?}"
    );
    // the second taker's deadline never passes: it must have taken one
    assert!(
        took[1].is_some(),
        "a taker with no deadline passing took a permit: {took:?}"
    );
    assert_ne!(
        took[1],
        Some(Took::AfterTimeout),
        "only a passed deadline reports a timeout"
    );
}

#[cfg(loom)]
fn bounded(preemptions: usize) -> loom::model::Builder {
    let mut b = loom::model::Builder::new();
    b.preemption_bound = Some(preemptions);
    b
}

#[cfg(loom)]
#[test]
fn loom_a_post_wakes_a_sleeper() {
    loom::model(a_post_wakes_a_sleeper);
}
#[cfg(loom)]
#[test]
fn loom_two_posts_release_two_sleepers() {
    // (three threads over a condition variable: bounded, as the other three-thread models here are)
    bounded(3).check(two_posts_release_two_sleepers);
}
#[cfg(loom)]
#[test]
fn loom_a_deadline_races_a_post() {
    loom::model(a_deadline_races_a_post);
}
#[cfg(loom)]
#[test]
fn loom_deadlines_and_posts_conserve_permits() {
    bounded(3).check(deadlines_and_posts_conserve_permits);
}

#[cfg(shuttle)]
const ITERS: usize = 20_000;
#[cfg(shuttle)]
#[test]
fn shuttle_a_post_wakes_a_sleeper() {
    shuttle::check_random(a_post_wakes_a_sleeper, ITERS);
    shuttle::check_pct(a_post_wakes_a_sleeper, ITERS, 3);
}
#[cfg(shuttle)]
#[test]
fn shuttle_two_posts_release_two_sleepers() {
    shuttle::check_random(two_posts_release_two_sleepers, ITERS);
    shuttle::check_pct(two_posts_release_two_sleepers, ITERS, 3);
}
#[cfg(shuttle)]
#[test]
fn shuttle_a_deadline_races_a_post() {
    shuttle::check_random(a_deadline_races_a_post, ITERS);
    shuttle::check_pct(a_deadline_races_a_post, ITERS, 3);
}
#[cfg(shuttle)]
#[test]
fn shuttle_deadlines_and_posts_conserve_permits() {
    shuttle::check_random(deadlines_and_posts_conserve_permits, ITERS);
    shuttle::check_pct(deadlines_and_posts_conserve_permits, ITERS, 3);
}
