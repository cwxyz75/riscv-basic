//! The PI futex under loom and shuttle (L23): a waiter that times out races the owner's unlock, and the owner's death races a timeout. Each of them
//! takes the waiter out of the queue exactly once, and the lock is never lost: it is handed over or the waiter has left. Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test pifutex_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test pifutex_concurrency
#![cfg(any(loom, shuttle))]

use kcore::donate::Donations;
use kcore::pifutex::{Lock, Outcome, PiFutexes, Unlock, Word, OWNER_DIED, OWNER_MASK};
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

struct W(Arc<AtomicU32>);
impl Word for W {
    fn load(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
    fn cas(&self, old: u32, new: u32) -> Result<u32, u32> {
        self.0
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
    }
}

const KEY: (u64, u64) = (1, 0x1000);

/// One bucket: the table and the graph under one lock (the kernel's bucket lock with the `PI` lock inside it).
type Bucket = Arc<Mutex<(PiFutexes, Donations)>>;

fn setup() -> (Bucket, W) {
    let mut d = Donations::new();
    d.add_thread(1, 5); // the owner
    d.add_thread(2, 20); // the waiter
    let word = Arc::new(AtomicU32::new(0));
    let w = W(word);
    let mut pi = PiFutexes::new();
    assert_eq!(
        pi.lock(&mut d, &w, KEY, 1, |_| true, true),
        Ok(Lock::Acquired)
    );
    assert!(matches!(
        pi.lock(&mut d, &w, KEY, 2, |_| true, true),
        Ok(Lock::Blocked { .. })
    ));
    (Arc::new(Mutex::new((pi, d))), w)
}

/// The waiter gives up (a deadline) while the owner unlocks: one of them takes it out of the queue, and the owner's priority returns to its base.
fn timeout_races_unlock() {
    let (b, w) = setup();
    let b2 = b.clone();
    let word = W(w.0.clone());
    let unlocker = thread::spawn(move || {
        let mut g = b2.lock().unwrap();
        let (pi, d) = &mut *g;
        matches!(
            pi.unlock(d, &word, KEY, 1),
            Ok(Unlock::Handed { to: 2, .. })
        )
    });
    // the waiter's own wake-up path, as the kernel runs it: still queued means it gives up, else the owner left it an outcome
    let outcome = {
        let mut g = b.lock().unwrap();
        let (pi, d) = &mut *g;
        if pi.still_waiting(2) {
            let _ = pi.cancel(d, 2);
            None
        } else {
            pi.take_outcome(2)
        }
    };
    let handed = unlocker.join().unwrap();
    let mut g = b.lock().unwrap();
    let (pi, d) = &mut *g;
    if handed {
        // the owner got there first: the waiter was granted the lock
        assert_eq!(outcome, Some(Outcome::Granted));
        assert_eq!(w.0.load(Ordering::SeqCst) & OWNER_MASK, 2);
    } else {
        // the waiter got there first: it left, and the unlock found no queue and freed the word
        assert_eq!(outcome, None);
        assert_eq!(w.0.load(Ordering::SeqCst) & OWNER_MASK, 0);
    }
    assert_eq!(d.eff(1), Some(5), "the owner is back at its base priority");
    assert!(!pi.still_waiting(2));
    d.audit().unwrap();
}

/// The owner dies while the waiter times out: the waiter leaves exactly once (woken with `Died`, or gone by its own cancel), the word is poisoned or
/// not, and nothing is left in the tables.
fn death_races_timeout() {
    let (b, w) = setup();
    let b2 = b.clone();
    let word = W(w.0.clone());
    let dier = thread::spawn(move || {
        let mut g = b2.lock().unwrap();
        let (pi, d) = &mut *g;
        let (woken, _) = pi.owner_died(d, 1, |_| None);
        if !woken.is_empty() {
            PiFutexes::poison(&word);
        }
        woken.iter().map(|(_, w)| w.len()).sum::<usize>()
    });
    let gave_up = {
        let mut g = b.lock().unwrap();
        let (pi, d) = &mut *g;
        let changed = !pi.cancel(d, 2).is_empty();
        changed
    };
    let woken = dier.join().unwrap();
    let mut g = b.lock().unwrap();
    let (pi, d) = &mut *g;
    // one of the two took the waiter out
    assert_eq!(usize::from(gave_up) + woken, 1, "taken out once");
    if woken == 1 {
        assert_eq!(pi.take_outcome(2), Some(Outcome::Died));
        assert!(w.0.load(Ordering::SeqCst) & OWNER_DIED != 0);
    }
    assert!(pi.is_empty(), "nothing is left queued");
    d.audit().unwrap();
}

#[cfg(loom)]
#[test]
fn loom_timeout_races_unlock() {
    loom::model(timeout_races_unlock);
}
#[cfg(loom)]
#[test]
fn loom_death_races_timeout() {
    loom::model(death_races_timeout);
}
#[cfg(shuttle)]
#[test]
fn shuttle_timeout_races_unlock() {
    shuttle::check_random(timeout_races_unlock, 3000);
}
#[cfg(shuttle)]
#[test]
fn shuttle_death_races_timeout() {
    shuttle::check_random(death_races_timeout, 3000);
}
