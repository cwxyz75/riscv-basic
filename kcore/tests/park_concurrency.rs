//! The park protocol (SCHED-SMP-SPEC 3.7, PK1) under the concurrency checkers, **without the big kernel lock**: one
//! waiter, two wakers (a condition wake and a timer), the run-queue lock modelled as a mutex. PK1: after a wake that
//! follows the waiter's registration has run, the waiter is not left Blocked. Three negative controls, each of which
//! the checkers must catch: prepare *after* registering; read `park` outside the run-queue lock; look at the
//! thread's state without the run-queue lock.
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test park_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test park_concurrency
#![cfg(any(loom, shuttle))]

use kcore::park::Park;
use kcore::sync::{AtomicU32, Ordering};

#[cfg(loom)]
use loom::{
    sync::{Arc, Mutex},
    thread,
};
#[cfg(shuttle)]
use shuttle::{
    sync::{Arc, Mutex},
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

struct World {
    park: Park,
    /// The thread's scheduler state, written under `rq` (an atomic so a variant can read it without the lock).
    state: AtomicU32,
    rq: Mutex<()>,
    registered: AtomicU32,
    /// How many wakers ran their wake after seeing the registration.
    woke_after: AtomicU32,
    v: Variant,
}

fn waiter(w: Arc<World>) {
    if w.v == Variant::PrepareAfterRegister {
        w.registered.store(1, Ordering::SeqCst);
        thread::yield_now();
        w.park.prepare();
    } else {
        w.park.prepare();
        w.registered.store(1, Ordering::SeqCst);
    }
    // steps 6 to 8
    if w.v == Variant::CheckParkOutsideLock {
        if w.park.take_notified() {
            return;
        }
        let _g = w.rq.lock().unwrap();
        w.state.store(BLOCKED, Ordering::SeqCst);
        return;
    }
    let _g = w.rq.lock().unwrap();
    if w.park.take_notified() {
        return;
    }
    thread::yield_now(); // (inside the critical section: the right waker holds the lock, so no waker is between its check and its enqueue)
    w.state.store(BLOCKED, Ordering::SeqCst);
}

fn waker(w: Arc<World>) {
    if w.registered.load(Ordering::SeqCst) == 0 {
        return; // before the registration: a wake the protocol does not promise to deliver
    }
    w.woke_after.fetch_add(1, Ordering::SeqCst);
    if !w.park.notify() {
        return;
    }
    if w.v == Variant::WakerSkipsLock {
        if w.state.load(Ordering::SeqCst) == BLOCKED {
            let _g = w.rq.lock().unwrap();
            w.state.store(RUNNABLE, Ordering::SeqCst);
        }
        return;
    }
    let _g = w.rq.lock().unwrap();
    if w.state.load(Ordering::SeqCst) == BLOCKED {
        w.state.store(RUNNABLE, Ordering::SeqCst);
    }
}

fn model(v: Variant) {
    let w = Arc::new(World {
        park: Park::new(),
        state: AtomicU32::new(RUNNING),
        rq: Mutex::new(()),
        registered: AtomicU32::new(0),
        woke_after: AtomicU32::new(0),
        v,
    });
    let t = {
        let w = w.clone();
        thread::spawn(move || waiter(w))
    };
    let k1 = {
        let w = w.clone();
        thread::spawn(move || waker(w))
    };
    let k2 = {
        let w = w.clone();
        thread::spawn(move || waker(w))
    };
    t.join().unwrap();
    k1.join().unwrap();
    k2.join().unwrap();
    // PK1: a wake followed the registration, so the thread must not be left Blocked
    if w.woke_after.load(Ordering::SeqCst) > 0 {
        assert_ne!(
            w.state.load(Ordering::SeqCst),
            BLOCKED,
            "PK1: a wake after the registration was lost"
        );
    }
}

fn explore(v: Variant) {
    #[cfg(loom)]
    loom::model(move || model(v));
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
            msg.contains("PK1") || msg.contains("lost")
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
