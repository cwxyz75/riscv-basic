//! MM9 (PG1): no lost wake for a thread that waits for a pager. The check "is the page there?" and the registration as a waiter are one step under
//! the lock (the big kernel lock in the kernel), so a supply that follows is never missed. Run with
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test pager_concurrency
//! (`make os-models` does). Without the cfg this file is empty. The negative control registers **after** releasing the lock: the same test body
//! must then fail (a lost wake), which shows the check can fail.
#![cfg(shuttle)]

use kcore::pager::Pagers;
use shuttle::{
    sync::{Arc, Mutex},
    thread,
};

struct Shared {
    pagers: Pagers,
    present: bool,
}

const PAGER: u64 = 1;
const VMO: u64 = 10;

/// The waiter's side of a fault: under the lock, register only if the page is still absent. `buggy` releases the lock between the check and
/// the registration. Returns true when it registered.
fn wait_for_page(s: &Mutex<Shared>, tid: u64, buggy: bool) -> bool {
    if buggy {
        let absent = !s.lock().unwrap().present;
        if !absent {
            return false;
        }
        // (the lock was released here: a supply can come in now)
        shuttle::thread::yield_now();
        s.lock().unwrap().pagers.wait(tid, PAGER, VMO, 0).unwrap();
        return true;
    }
    let mut g = s.lock().unwrap();
    if g.present {
        return false;
    }
    g.pagers.wait(tid, PAGER, VMO, 0).unwrap();
    true
}

fn supply_is_never_lost(buggy: bool) {
    let s = Arc::new(Mutex::new(Shared {
        pagers: {
            let mut p = Pagers::new();
            p.reserve_room(8).unwrap();
            p.reserve_pager().unwrap();
            p.create(PAGER);
            p
        },
        present: false,
    }));
    let s2 = s.clone();
    let supplier = thread::spawn(move || {
        let mut g = s2.lock().unwrap();
        g.present = true;
        g.pagers.supply(PAGER, VMO, 0, 1);
        drained(&mut g.pagers)
    });
    let registered = wait_for_page(&s, 7, buggy);
    let woken = supplier.join().unwrap();
    let g = s.lock().unwrap();
    if registered {
        // it waits for a page that is there: it must have been woken
        assert!(
            woken.contains(&7) || g.pagers.result(7).is_some(),
            "a lost wake: the waiter registered after the supply and nobody will wake it"
        );
    } else {
        assert!(woken.is_empty());
    }
}

#[test]
fn shuttle_a_supply_is_never_lost() {
    shuttle::check_random(|| supply_is_never_lost(false), 3000);
}

#[test]
#[should_panic(expected = "a lost wake")]
fn shuttle_negative_control_registering_after_releasing_the_lock_loses_a_wake() {
    shuttle::check_random(|| supply_is_never_lost(true), 3000);
}

/// Two waiters for the same page and a failure: both are woken with it, exactly once (the second `fail` finds nobody).
#[test]
fn shuttle_a_failure_wakes_every_waiter_once() {
    shuttle::check_random(
        || {
            let s = Arc::new(Mutex::new(Shared {
                pagers: {
                    let mut p = Pagers::new();
                    p.reserve_room(8).unwrap();
                    p.reserve_pager().unwrap();
                    p.create(PAGER);
                    p
                },
                present: false,
            }));
            let waiters: Vec<_> = [1u64, 2]
                .into_iter()
                .map(|tid| {
                    let s = s.clone();
                    thread::spawn(move || {
                        s.lock().unwrap().pagers.wait(tid, PAGER, VMO, 0).unwrap();
                    })
                })
                .collect();
            let s2 = s.clone();
            let failer = thread::spawn(move || {
                let mut g = s2.lock().unwrap();
                g.pagers
                    .fail(PAGER, VMO, 0, 1, abi::status::Status::TimedOut);
                let a = drained(&mut g.pagers);
                g.pagers
                    .fail(PAGER, VMO, 0, 1, abi::status::Status::TimedOut);
                let b = drained(&mut g.pagers);
                (a, b)
            });
            for w in waiters {
                w.join().unwrap();
            }
            let (a, b) = failer.join().unwrap();
            assert!(b.is_empty(), "nobody is woken twice");
            let mut g = s.lock().unwrap();
            // a waiter that registered after the failure still waits (the page was never supplied): the pager asks again when the next one
            // is needed, and every waiter that was woken got the failure
            for t in &a {
                assert_eq!(
                    g.pagers.cancel(*t),
                    Some(Err(abi::status::Status::TimedOut))
                );
            }
            let left = g.pagers.waits();
            assert!(left <= 2);
        },
        3000,
    );
}

fn drained(p: &mut Pagers) -> Vec<u64> {
    let mut out = Vec::new();
    p.drain_woken(&mut |w| out.extend_from_slice(w));
    out
}
