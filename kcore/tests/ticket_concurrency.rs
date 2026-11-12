//! The big kernel lock's protocol (ticket lock, reentrancy, depth hand-over) under the
//! concurrency checkers (T38). Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test ticket_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test ticket_concurrency
//! (`make os-test` does both). Without either cfg this file is empty.
#![cfg(any(loom, shuttle))]

use kcore::sync::{AtomicU32, Ordering};
use kcore::ticket::{Depth, Ticket};

#[cfg(loom)]
use loom::{sync::Arc, thread};
#[cfg(shuttle)]
use shuttle::{sync::Arc, thread};

fn relax() {
    thread::yield_now();
}

/// A read-modify-write that is only correct if the lock really excludes: a plain load then
/// store with relaxed ordering loses updates when two harts are inside at once, and reads a stale
/// value when the lock does not publish the previous holder's write.
fn bump(cell: &AtomicU32) {
    let v = cell.load(Ordering::Relaxed);
    thread::yield_now(); // let the checker interleave inside the critical section
    cell.store(v + 1, Ordering::Relaxed);
}

/// Mutual exclusion and visibility, with a reentrant take inside.
fn exclusion(harts: u32, rounds: u32) {
    let lock = Arc::new(Ticket::new());
    let cell = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = (0..harts)
        .map(|h| {
            let (l, c) = (lock.clone(), cell.clone());
            thread::spawn(move || {
                let d = Depth::new();
                for _ in 0..rounds {
                    l.lock(h, &d, relax);
                    l.lock(h, &d, relax); // nested
                    bump(&c);
                    assert!(!l.unlock(h, &d));
                    assert!(l.held_by(h));
                    assert!(l.unlock(h, &d));
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(cell.load(Ordering::SeqCst), harts * rounds);
    assert!(lock.is_free());
}

/// try_lock never succeeds while another hart holds or waits, and never breaks exclusion.
fn try_lock_excludes() {
    let lock = Arc::new(Ticket::new());
    let cell = Arc::new(AtomicU32::new(0));
    let wins = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = (0..2u32)
        .map(|h| {
            let (l, c, w) = (lock.clone(), cell.clone(), wins.clone());
            thread::spawn(move || {
                let d = Depth::new();
                if l.try_lock(h, &d) {
                    bump(&c);
                    w.fetch_add(1, Ordering::SeqCst);
                    l.unlock(h, &d);
                } else {
                    l.lock(h, &d, relax);
                    bump(&c);
                    l.unlock(h, &d);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(cell.load(Ordering::SeqCst), 2);
    assert!(lock.is_free());
}

/// The hand-over (K3): hart 0 holds the lock at depth 2 in thread A and switches to thread B
/// (saved depth 1) without releasing; B's single unlock releases it. Meanwhile hart 1 contends:
/// it must never be inside while either thread of hart 0 is, and the lock must be released
/// exactly when B's depth reaches zero.
fn handover() {
    let lock = Arc::new(Ticket::new());
    let cell = Arc::new(AtomicU32::new(0));
    let (l0, c0) = (lock.clone(), cell.clone());
    let hart0 = thread::spawn(move || {
        let d = Depth::new();
        l0.lock(0, &d, relax);
        l0.lock(0, &d, relax); // thread A at depth 2
        bump(&c0);
        let saved_a = l0.swap_depth(0, &d, 1); // switch to B, which holds one level
        assert_eq!(saved_a, 2);
        bump(&c0);
        assert!(l0.held_by(0));
        assert!(l0.unlock(0, &d)); // B leaves the kernel: really released
                                   // later A is switched back in (it must re-take: the lock was released)
        l0.lock(0, &d, relax);
        let _ = l0.swap_depth(0, &d, saved_a);
        bump(&c0);
        assert!(!l0.unlock(0, &d));
        assert!(l0.unlock(0, &d));
    });
    let (l1, c1) = (lock.clone(), cell.clone());
    let hart1 = thread::spawn(move || {
        let d = Depth::new();
        for _ in 0..2 {
            l1.lock(1, &d, relax);
            bump(&c1);
            assert!(l1.unlock(1, &d));
        }
    });
    hart0.join().unwrap();
    hart1.join().unwrap();
    assert_eq!(cell.load(Ordering::SeqCst), 5);
    assert!(lock.is_free());
}

#[cfg(loom)]
#[test]
fn loom_exclusion_two() {
    loom::model(|| exclusion(2, 1));
}
#[cfg(loom)]
#[test]
fn loom_try_lock() {
    loom::model(try_lock_excludes);
}
#[cfg(loom)]
#[test]
fn loom_handover() {
    loom::model(handover);
}

#[cfg(shuttle)]
#[test]
fn shuttle_exclusion_three() {
    shuttle::check_random(|| exclusion(3, 2), 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_try_lock() {
    shuttle::check_random(try_lock_excludes, 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_handover() {
    shuttle::check_random(handover, 5000);
}
