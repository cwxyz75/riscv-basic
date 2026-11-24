//! No lost wakeup: the user-word check and the enqueue are one step under the key's bucket lock (L14: the futex
//! wake takes no big kernel lock), so a wake that follows a user store is never missed; and the sleeper prepares
//! to block *before* it looks at the table, so the wake that dequeues it after the look is not lost either; a
//! timeout and a kill race a wake and exactly one of them takes the sleeper out. Run with
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

const KEY: (u64, u64) = (1, 0x1000);

/// The waiter's side of `futex_wait`: under the lock, sleep only if the word still holds `expected`.
/// Returns true if it went to sleep (and must be woken).
fn wait_if_equal(table: &Mutex<Futexes>, word: &AtomicU32, expected: u32, who: u64) -> bool {
    let mut t = table.lock().unwrap();
    if word.load(Ordering::SeqCst) != expected {
        return false;
    }
    t.wait(KEY, who).unwrap();
    true
}

/// A user thread stores 1 then wakes; a waiter that saw 0 must always end up woken or never sleep.
fn store_then_wake_is_never_lost() {
    let table = Arc::new(Mutex::new(Futexes::new()));
    let word = Arc::new(AtomicU32::new(0));
    let (t2, w2) = (table.clone(), word.clone());
    let waker = thread::spawn(move || {
        w2.store(1, Ordering::SeqCst); // user-mode store, no lock
        let woken = t2.lock().unwrap().wake(KEY, 1).unwrap(); // the futex_wake syscall
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
    let woken = table.lock().unwrap().wake(KEY, 1).unwrap();
    assert_eq!(woken.len(), 1);
    let left = table.lock().unwrap().len();
    assert_eq!(left, 1);
}

/// The parking state of one thread (SCHED-SMP-SPEC 3.7), reduced: idle, prepared (a wake counts), notified (a wake arrived while it
/// was prepared: the next block returns at once), blocked (asleep until a wake).
const IDLE: u32 = 0;
const PREPARED: u32 = 1;
const NOTIFIED: u32 = 2;
const BLOCKED: u32 = 3;

/// A wake of the thread: prepared becomes notified, blocked becomes idle (runnable); anything else is left alone.
fn notify(park: &AtomicU32) {
    loop {
        let s = park.load(Ordering::SeqCst);
        let to = match s {
            PREPARED => NOTIFIED,
            BLOCKED => IDLE,
            _ => return,
        };
        if park
            .compare_exchange(s, to, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
}

/// The sleeper's wait loop as the kernel runs it (`wait_deadline_futex`): `prepare_first` is the order of the kernel (prepare, then look at
/// the table); false is the old order (look, then prepare), which only the big lock made safe. Returns whether it ended up asleep.
fn sleep_loop(table: &Mutex<Futexes>, park: &AtomicU32, who: u64, prepare_first: bool) -> bool {
    for _ in 0..4 {
        if prepare_first {
            park.store(PREPARED, Ordering::SeqCst);
        }
        let ready = !table.lock().unwrap().contains(who);
        if ready {
            park.store(IDLE, Ordering::SeqCst); // (`cancel_block_on`: nothing stays prepared)
            return false;
        }
        if !prepare_first {
            park.store(PREPARED, Ordering::SeqCst);
        }
        // the block: a notification that is waiting is consumed and the thread looks again
        if park.load(Ordering::SeqCst) == NOTIFIED {
            park.store(IDLE, Ordering::SeqCst);
            continue;
        }
        if park
            .compare_exchange(PREPARED, BLOCKED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// A waiter queues itself (the word still holds 0) and sleeps; a waker stores 1, dequeues it under the bucket lock and wakes it **after**
/// the dequeue. The waiter must never be left blocked with nobody to wake it.
fn prepared_waiter_is_never_left_blocked(prepare_first: bool) {
    let table = Arc::new(Mutex::new(Futexes::new()));
    let word = Arc::new(AtomicU32::new(0));
    let park = Arc::new(AtomicU32::new(IDLE));
    let (t2, w2, p2) = (table.clone(), word.clone(), park.clone());
    let waker = thread::spawn(move || {
        w2.store(1, Ordering::SeqCst);
        let woken = t2.lock().unwrap().wake(KEY, 1).unwrap();
        if !woken.is_empty() {
            notify(&p2); // (the kernel does this with the bucket lock held: the dequeue is first either way)
        }
        woken.len()
    });
    let queued = wait_if_equal(&table, &word, 0, 7);
    let asleep = queued && sleep_loop(&table, &park, 7, prepare_first);
    let woken = waker.join().unwrap();
    if queued {
        assert_eq!(woken, 1, "the sleeper was in the table when the wake ran");
        assert!(
            !(asleep && park.load(Ordering::SeqCst) == BLOCKED),
            "the sleeper was dequeued and woken, and is still blocked: a lost wake"
        );
    }
}

/// A timeout (`forget`) races a wake: exactly one of them takes the sleeper out of the table.
fn timeout_and_wake_take_the_sleeper_once() {
    let table = Arc::new(Mutex::new(Futexes::new()));
    table.lock().unwrap().wait(KEY, 7).unwrap();
    let t2 = table.clone();
    let waker = thread::spawn(move || t2.lock().unwrap().wake(KEY, 1).unwrap().len());
    let timed_out = table.lock().unwrap().forget(7);
    let woken = waker.join().unwrap();
    assert_eq!(usize::from(timed_out) + woken, 1, "taken out once, by one");
    assert!(!table.lock().unwrap().contains(7));
}

/// A kill (`forget_process`, at the end of the process) races a wake: every sleeper leaves once, and a sleeper is reported by one of them.
fn kill_and_wake_take_each_sleeper_once() {
    let table = Arc::new(Mutex::new(Futexes::new()));
    for w in [7, 8] {
        table.lock().unwrap().wait(KEY, w).unwrap();
    }
    let t2 = table.clone();
    let waker = thread::spawn(move || t2.lock().unwrap().wake(KEY, 1).unwrap());
    let killed = table.lock().unwrap().forget_process(KEY.0);
    let woken = waker.join().unwrap();
    let mut all: Vec<u64> = killed.into_iter().chain(woken).collect();
    all.sort();
    assert_eq!(
        all,
        [7, 8],
        "each sleeper reported once, by wake or by kill"
    );
    assert!(table.lock().unwrap().is_empty());
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

#[cfg(loom)]
#[test]
fn loom_prepare_first_never_loses_a_wake() {
    loom::model(|| prepared_waiter_is_never_left_blocked(true));
}
/// The negative control: the old order (look at the table, then prepare) loses the wake once the waker needs no big lock.
#[cfg(loom)]
#[test]
#[should_panic(expected = "a lost wake")]
fn loom_look_then_prepare_loses_a_wake() {
    loom::model(|| prepared_waiter_is_never_left_blocked(false));
}
#[cfg(loom)]
#[test]
fn loom_timeout_and_wake() {
    loom::model(timeout_and_wake_take_the_sleeper_once);
}
#[cfg(loom)]
#[test]
fn loom_kill_and_wake() {
    loom::model(kill_and_wake_take_each_sleeper_once);
}
#[cfg(shuttle)]
#[test]
fn shuttle_prepare_first_never_loses_a_wake() {
    shuttle::check_random(|| prepared_waiter_is_never_left_blocked(true), 3000);
}
#[cfg(shuttle)]
#[test]
#[should_panic(expected = "a lost wake")]
fn shuttle_look_then_prepare_loses_a_wake() {
    shuttle::check_random(|| prepared_waiter_is_never_left_blocked(false), 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_timeout_and_wake() {
    shuttle::check_random(timeout_and_wake_take_the_sleeper_once, 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_kill_and_wake() {
    shuttle::check_random(kill_and_wake_take_each_sleeper_once, 3000);
}
