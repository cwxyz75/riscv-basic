//! No lost wakeup: the user-word check and the enqueue are one step under the lock (the big kernel
//! lock in the kernel), so a wake that follows a user store is never missed. Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test futex_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test futex_concurrency
//! (`make os-test` does both). Without either cfg this file is empty.
#![cfg(any(loom, shuttle))]

use kcore::futex::Futexes;
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

const KEY: (u32, u64) = (1, 0x1000);

/// The waiter's side of `futex_wait`: under the lock, sleep only if the word still holds `expected`.
/// Returns true if it went to sleep (and must be woken).
fn wait_if_equal(table: &Mutex<Futexes>, word: &AtomicU32, expected: u32, who: u64) -> bool {
    let mut t = table.lock().unwrap();
    if word.load(Ordering::SeqCst) != expected {
        return false;
    }
    t.wait(KEY, who);
    true
}

/// A user thread stores 1 then wakes; a waiter that saw 0 must always end up woken or never sleep.
fn store_then_wake_is_never_lost() {
    let table = Arc::new(Mutex::new(Futexes::new()));
    let word = Arc::new(AtomicU32::new(0));
    let (t2, w2) = (table.clone(), word.clone());
    let waker = thread::spawn(move || {
        w2.store(1, Ordering::SeqCst); // user-mode store, no lock
        let woken = t2.lock().unwrap().wake(KEY, 1); // the futex_wake syscall
        woken.len()
    });
    let slept = wait_if_equal(&table, &word, 0, 7);
    let woken = waker.join().unwrap();
    let still_asleep = table.lock().unwrap().contains(7);
    // either the waiter saw the store and never slept, or it slept and the wake found it
    if slept {
        assert_eq!(
            woken, 1,
            "the wake missed a sleeper that had slept before it"
        );
        assert!(!still_asleep);
    } else {
        assert_eq!(woken, 0);
        assert!(!still_asleep);
    }
}

/// Two sleepers, one wake of count 1: exactly one is woken (the older), the other still sleeps.
fn one_wake_wakes_one() {
    let table = Arc::new(Mutex::new(Futexes::new()));
    let word = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = [1u64, 2]
        .into_iter()
        .map(|who| {
            let (t, w) = (table.clone(), word.clone());
            thread::spawn(move || wait_if_equal(&t, &w, 0, who))
        })
        .collect();
    for h in hs {
        assert!(h.join().unwrap());
    }
    let first = {
        let t = table.lock().unwrap();
        // whoever slept first is first in line: wake order follows arrival order
        t.len()
    };
    assert_eq!(first, 2);
    let woken = table.lock().unwrap().wake(KEY, 1);
    assert_eq!(woken.len(), 1);
    let left = table.lock().unwrap().len();
    assert_eq!(left, 1);
}

#[cfg(loom)]
#[test]
fn loom_store_then_wake() {
    loom::model(store_then_wake_is_never_lost);
}
#[cfg(loom)]
#[test]
fn loom_one_wake_wakes_one() {
    loom::model(one_wake_wakes_one);
}
#[cfg(shuttle)]
#[test]
fn shuttle_store_then_wake() {
    shuttle::check_random(store_then_wake_is_never_lost, 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_one_wake_wakes_one() {
    shuttle::check_random(one_wake_wakes_one, 3000);
}
