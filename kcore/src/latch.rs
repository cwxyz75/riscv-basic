//! A one-shot wake token: the smallest piece of the "one carefully written
//! block/wake path" (OS-ARCHITECTURE.md, 5.4), and the first thing the
//! concurrency checkers exercise.
//!
//! A waiter calls [`Latch::take`]; a waker calls [`Latch::set`]. A set that
//! happens before the take is not lost (take returns true), a set that happens
//! after finds nobody and leaves the token for the next take, and one set is
//! consumed by exactly one take, never two and never none. That is the "no lost
//! wakeup, no double wakeup" property.

use crate::sync::{AtomicU32, Ordering};

const EMPTY: u32 = 0;
const SET: u32 = 1;

pub struct Latch {
    state: AtomicU32,
}

impl Latch {
    #[cfg(not(any(loom, shuttle)))]
    pub const fn new() -> Latch {
        Latch {
            state: AtomicU32::new(EMPTY),
        }
    }

    // The checkers' atomics are not const-constructible.
    #[cfg(any(loom, shuttle))]
    pub fn new() -> Latch {
        Latch {
            state: AtomicU32::new(EMPTY),
        }
    }

    /// Leave the token. Idempotent: setting a set latch changes nothing (wakeups
    /// do not queue up).
    pub fn set(&self) {
        self.state.store(SET, Ordering::Release);
    }

    /// Consume the token if it is there.
    pub fn take(&self) -> bool {
        self.state
            .compare_exchange(SET, EMPTY, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

impl Default for Latch {
    fn default() -> Self {
        Latch::new()
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn set_then_take_once() {
        let l = Latch::new();
        assert!(!l.take());
        l.set();
        assert!(l.take());
        assert!(!l.take());
    }

    #[test]
    fn wakeups_do_not_queue() {
        let l = Latch::new();
        l.set();
        l.set();
        assert!(l.take());
        assert!(!l.take());
    }
}
