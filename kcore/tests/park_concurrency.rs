//! The park protocol (SCHED-SMP-SPEC 3.7, PK1) under the concurrency checkers, **without the big kernel lock**: two waiters, each
//! waiting twice on one condition (a wait structure and its condition under one lock, as a futex bucket keeps them), and two wakers
//! (a condition waker that makes the condition true twice and wakes everyone registered each time, and a timer that times out the
//! first waiter's wait, whichever round it is in), with one run-queue lock (T85 (6)). PK1: a waiter that blocks after its
//! registration is woken, so no waiter is left blocked once the wakers have finished; every wait ends. Three negative controls, each
//! of which the checkers must catch: prepare *after* registering; read `park` outside the run-queue lock; look at the thread's
//! state without the run-queue lock.
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test park_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test park_concurrency
#![cfg(any(loom, shuttle))]

use kcore::park::Park;
use kcore::sync::{AtomicU32, Ordering};

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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Variant {
    Right,
    /// Negative control 1: the waiter registers, then prepares (the waker's CAS can find Idle and do nothing).
    PrepareAfterRegister,
    /// Negative control 2: the waiter reads `park` before taking the run-queue lock, and blocks under it.
    CheckParkOutsideLock,
    /// Negative control 3: the waker looks at the thread's state without the run-queue lock.
    WakerSkipsLock,
}

const RUNNING: u32 = 0;
const BLOCKED: u32 = 1;
const RUNNABLE: u32 = 2;

const WAITERS: usize = 2;
/// Each waiter waits this many times; the condition waker makes the condition true this many times.
const ROUNDS: u32 = 2;
const WAKERS: u32 = 2;

struct Waiter {
    park: Park,
    /// The thread's scheduler state, written under `rq` (an atomic so a variant can read it without the lock).
    state: AtomicU32,
}

/// The wait structure: the condition (how many times it has been made true) and each waiter's registration (the round it waits
/// in, 0 for none), under one lock.
struct Queue {
    cond: u32,
    reg: [u32; WAITERS],
}

struct World {
    w: [Waiter; WAITERS],
    rq: Mutex<()>,
    /// A blocked thread sleeps on this (with `rq`) until a step d makes it runnable or the last waker finishes.
    switched: Condvar,
    q: Mutex<Queue>,
    /// Wakers that have finished (after their last step d).
    wakers_done: AtomicU32,
    v: Variant,
}

/// Steps 2 and 3 under the wait structure's lock: register for round `r`, unless the condition already holds for it. True:
/// registered (the waiter goes on to block).
fn register(w: &World, me: usize, r: u32) -> bool {
    let mut q = w.q.lock().unwrap();
    if q.cond >= r {
        return false;
    }
    q.reg[me] = r;
    true
}

fn waiter(w: Arc<World>, me: usize) {
    let t = &w.w[me];
    for r in 1..=ROUNDS {
        // steps 1 to 3
        let registered = if w.v == Variant::PrepareAfterRegister {
            let reg = register(&w, me, r);
            thread::yield_now();
            t.park.prepare();
            reg
        } else {
            t.park.prepare();
            register(&w, me, r)
        };
        if !registered {
            t.park.cancel(); // (step 3: the condition was already true)
            continue;
        }
        // steps 6 to 8
        let blocked = if w.v == Variant::CheckParkOutsideLock {
            if t.park.take_notified() {
                false
            } else {
                let _g = w.rq.lock().unwrap();
                t.state.store(BLOCKED, Ordering::SeqCst);
                true
            }
        } else {
            let _g = w.rq.lock().unwrap();
            if t.park.take_notified() {
                false
            } else {
                thread::yield_now(); // (inside the critical section: the right waker holds the lock, so no waker is between its check and its enqueue)
                t.state.store(BLOCKED, Ordering::SeqCst);
                true
            }
        };
        // switched away until a waker makes the thread runnable
        if blocked {
            let mut g = w.rq.lock().unwrap();
            while t.state.load(Ordering::SeqCst) == BLOCKED {
                // (under the run-queue lock, after every waker's step d: nobody can make it runnable any more)
                assert!(
                    w.wakers_done.load(Ordering::SeqCst) < WAKERS,
                    "PK1: a wake after the registration was lost (waiter {me}, round {r})"
                );
                g = w.switched.wait(g).unwrap();
            }
            t.state.store(RUNNING, Ordering::SeqCst);
        }
        t.park.cancel(); // step 9: the wait is over
    }
}

/// Steps c and d for a waiter the waker took out of the wait structure.
fn wake(w: &World, i: usize) {
    let t = &w.w[i];
    if !t.park.notify() {
        return;
    }
    if w.v == Variant::WakerSkipsLock {
        if t.state.load(Ordering::SeqCst) == BLOCKED {
            let _g = w.rq.lock().unwrap();
            t.state.store(RUNNABLE, Ordering::SeqCst);
            w.switched.notify_all();
        }
        return;
    }
    let _g = w.rq.lock().unwrap();
    if t.state.load(Ordering::SeqCst) == BLOCKED {
        t.state.store(RUNNABLE, Ordering::SeqCst);
        w.switched.notify_all();
    }
}

/// A waker has finished: under the run-queue lock, so a blocked waiter that looks next sees it (and one that sleeps is told).
fn waker_done(w: &World) {
    let _g = w.rq.lock().unwrap();
    w.wakers_done.fetch_add(1, Ordering::SeqCst);
    w.switched.notify_all();
}

/// The condition waker: makes the condition true for round `k` (step a) and takes every waiter registered for it or earlier out of
/// the wait structure (step b), under its lock; then wakes them.
fn condition_waker(w: Arc<World>) {
    for k in 1..=ROUNDS {
        let mut woken = [false; WAITERS];
        {
            let mut q = w.q.lock().unwrap();
            q.cond = k;
            for (i, reg) in q.reg.iter_mut().enumerate() {
                if *reg != 0 && *reg <= k {
                    *reg = 0;
                    woken[i] = true;
                }
            }
        }
        for (i, &b) in woken.iter().enumerate() {
            if b {
                wake(&w, i);
            }
        }
    }
    waker_done(&w);
}

/// The timer: waiter 0's timeout fires once, in whichever round it is waiting (or finds it not waiting and does nothing).
fn timer(w: Arc<World>) {
    let fired = {
        let mut q = w.q.lock().unwrap();
        let r = q.reg[0];
        q.reg[0] = 0;
        r != 0
    };
    if fired {
        wake(&w, 0);
    }
    waker_done(&w);
}

fn model(v: Variant) {
    let w = Arc::new(World {
        w: [
            Waiter {
                park: Park::new(),
                state: AtomicU32::new(RUNNING),
            },
            Waiter {
                park: Park::new(),
                state: AtomicU32::new(RUNNING),
            },
        ],
        rq: Mutex::new(()),
        switched: Condvar::new(),
        q: Mutex::new(Queue {
            cond: 0,
            reg: [0; WAITERS],
        }),
        wakers_done: AtomicU32::new(0),
        v,
    });
    let ts: Vec<_> = (0..WAITERS)
        .map(|i| {
            let w = w.clone();
            thread::spawn(move || waiter(w, i))
        })
        .collect();
    let c = {
        let w = w.clone();
        thread::spawn(move || condition_waker(w))
    };
    let k = {
        let w = w.clone();
        thread::spawn(move || timer(w))
    };
    for t in ts {
        t.join().unwrap();
    }
    c.join().unwrap();
    k.join().unwrap();
    // every wait ended: nobody is left blocked, nobody is still registered
    for (i, t) in w.w.iter().enumerate() {
        assert_eq!(
            t.state.load(Ordering::SeqCst),
            RUNNING,
            "PK1: waiter {i} was left blocked"
        );
    }
}

fn explore(v: Variant) {
    #[cfg(loom)]
    {
        // (four threads: exhaustive up to two preemptions, which every negative control needs at most one of)
        let mut b = loom::model::Builder::new();
        b.preemption_bound = Some(2);
        b.check(move || model(v));
    }
    #[cfg(shuttle)]
    shuttle::check_random(move || model(v), 5000);
}

/// The variant is caught **for the reason claimed**: the panic is the PK1 assertion, not some accident.
fn is_caught(v: Variant) -> bool {
    match std::panic::catch_unwind(move || explore(v)) {
        Ok(()) => false,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            // (loom and shuttle re-raise the model's panic, sometimes wrapped: look for the assertion's own words)
            msg.contains("PK1")
        }
    }
}

#[test]
fn the_protocol_loses_no_wake() {
    explore(Variant::Right);
}

#[test]
fn preparing_after_registering_is_caught() {
    assert!(is_caught(Variant::PrepareAfterRegister));
}

#[test]
fn reading_park_outside_the_run_queue_lock_is_caught() {
    assert!(is_caught(Variant::CheckParkOutsideLock));
}

#[test]
fn a_waker_that_skips_the_run_queue_lock_is_caught() {
    assert!(is_caught(Variant::WakerSkipsLock));
}
