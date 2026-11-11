//! The latch under the concurrency checkers. Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test latch_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test latch_concurrency
//! (`make os-test` does both). Without either cfg this file is empty.
#![cfg(any(loom, shuttle))]

use kcore::latch::Latch;
use kcore::sync::{AtomicU32, Ordering};

#[cfg(loom)]
use loom::{sync::Arc, thread};
#[cfg(shuttle)]
use shuttle::{sync::Arc, thread};

/// One set, two takers racing: exactly one of them gets the token.
fn one_set_two_takers() {
    let latch = Arc::new(Latch::new());
    let taken = Arc::new(AtomicU32::new(0));
    latch.set();
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (l, t) = (latch.clone(), taken.clone());
            thread::spawn(move || {
                if l.take() {
                    t.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(taken.load(Ordering::SeqCst), 1);
}

/// A waker racing a waiter: whatever the interleaving, the wakeup is either seen
/// by the racing take or still there afterwards. Never lost, never doubled.
fn set_races_take() {
    let latch = Arc::new(Latch::new());
    let l = latch.clone();
    let waker = thread::spawn(move || l.set());
    let seen_early = latch.take();
    waker.join().unwrap();
    let seen_late = latch.take();
    assert!(
        seen_early ^ seen_late,
        "early={seen_early} late={seen_late}"
    );
    assert!(!latch.take());
}

#[cfg(loom)]
#[test]
fn loom_one_set_two_takers() {
    loom::model(one_set_two_takers);
}

#[cfg(loom)]
#[test]
fn loom_set_races_take() {
    loom::model(set_races_take);
}

#[cfg(shuttle)]
#[test]
fn shuttle_one_set_two_takers() {
    shuttle::check_random(one_set_two_takers, 2000);
}

#[cfg(shuttle)]
#[test]
fn shuttle_set_races_take() {
    shuttle::check_random(set_races_take, 2000);
}
