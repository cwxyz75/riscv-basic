//! The per-thread **park word** of the one block/wake path (SCHED-SMP-SPEC 3.7, OD86, invariant PK1).
//!
//! ```text
//!  waiter                                              waker
//!  1 park := Prepared                                  a  make the condition true (under its lock)
//!  2 register in the lock's wait structure             b  find the waiter, remove it
//!  3 re-check the condition: ready? park := Idle       c  CAS(park, Prepared -> Notified): fails: not waiting, done
//!  6 (under the run-queue lock) if park == Notified:   d  (under the run-queue lock) if the thread is Blocked,
//!        park := Idle, do not block                         move it to a run queue
//!  8 state := Blocked; switch away
//! ```
//!
//! A wake is **never remembered**: it counts only for a thread that is `Prepared` (about to block) or already
//! blocked. That replaces the wake token of OD34: the token let a wake that found nobody waiting be consumed by an
//! unrelated later block, which made spurious wakeups a property of the design; here the waiter says "I am about to
//! wait" first, so there is nothing to remember. The word is an atomic because the waker's step c runs without the
//! run-queue lock.
//!
//! The kernel still runs this under the big kernel lock (task 10); the loom and shuttle models in
//! `tests/park_concurrency.rs` explore the protocol without it, with the three ways it can go wrong as negative
//! controls.

use crate::sync::{AtomicU32, Ordering};

const IDLE: u32 = 0;
const PREPARED: u32 = 1;
const NOTIFIED: u32 = 2;

/// What a thread's park word says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkState {
    /// Not waiting for anything.
    Idle,
    /// About to wait (or waiting): a wake counts.
    Prepared,
    /// A wake arrived while it was prepared: it must not block.
    Notified,
}

pub struct Park(AtomicU32);

impl Park {
    #[cfg(not(any(loom, shuttle)))]
    pub const fn new() -> Park {
        Park(AtomicU32::new(IDLE))
    }
    #[cfg(any(loom, shuttle))]
    pub fn new() -> Park {
        Park(AtomicU32::new(IDLE))
    }

    pub fn state(&self) -> ParkState {
        match self.0.load(Ordering::SeqCst) {
            IDLE => ParkState::Idle,
            PREPARED => ParkState::Prepared,
            _ => ParkState::Notified,
        }
    }

    /// Waiter, step 1: from now on a wake counts. Called **before** registering in any wait structure.
    pub fn prepare(&self) {
        self.0.store(PREPARED, Ordering::SeqCst);
    }

    /// Waiter, steps 3 and 9: not waiting any more (the condition was already true, or the wait ended).
    pub fn cancel(&self) {
        self.0.store(IDLE, Ordering::SeqCst);
    }

    /// Waker, step c: tell a prepared thread that its wake has come. True if this call did it (the thread is
    /// blocked, or will see `Notified` before it blocks); false if the thread was not waiting (nothing is
    /// remembered) or another waker got there first.
    pub fn notify(&self) -> bool {
        self.0
            .compare_exchange(PREPARED, NOTIFIED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Waiter, step 6, **under the run-queue lock** (the same lock the waker takes in step d): a notification that
    /// arrived is consumed and the thread must not block. True: do not block.
    pub fn take_notified(&self) -> bool {
        if self.0.load(Ordering::SeqCst) == NOTIFIED {
            self.0.store(IDLE, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}

impl Default for Park {
    fn default() -> Self {
        Park::new()
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn a_wake_that_finds_nobody_waiting_is_not_remembered() {
        let p = Park::new();
        assert!(!p.notify(), "idle: nothing to notify");
        assert_eq!(p.state(), ParkState::Idle);
        p.prepare();
        assert!(!p.take_notified(), "nothing arrived: block");
    }

    #[test]
    fn a_wake_between_prepare_and_block_stops_the_block() {
        let p = Park::new();
        p.prepare();
        assert!(p.notify());
        assert_eq!(p.state(), ParkState::Notified);
        assert!(!p.notify(), "only one waker gets the thread");
        assert!(p.take_notified(), "the thread does not block");
        assert_eq!(p.state(), ParkState::Idle);
    }

    #[test]
    fn cancel_forgets_the_wait() {
        let p = Park::new();
        p.prepare();
        p.cancel();
        assert!(!p.notify());
    }
}
