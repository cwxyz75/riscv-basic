//! The ticket lock and depth hand-over behind the big kernel lock (S5b, K3), as pure logic the
//! concurrency checkers can explore (T38).
//!
//! A [`Ticket`] is a fair lock: callers take a ticket and are served in order. It is
//! **reentrant per owner** (a depth count, kept by the caller in a per-hart cell) and **the lock
//! follows the thread**: [`Ticket::swap_depth`] gives the held lock to an incoming thread with
//! its own saved depth without releasing it. The kernel's `bkl` module wraps this with the hart
//! identity, interrupts and the lock-order checker; the protocol itself lives here.

use crate::sync::{AtomicU32, Ordering};

pub struct Ticket {
    next: AtomicU32,
    serving: AtomicU32,
    /// Owner id + 1 (0 = free). Owner ids are chosen by the caller (hart indices).
    owner: AtomicU32,
}

/// Per-owner nesting state: the depth of the lock the owner currently holds.
pub struct Depth(AtomicU32);

impl Depth {
    #[cfg(not(any(loom, shuttle)))]
    pub const fn new() -> Depth {
        Depth(AtomicU32::new(0))
    }
    #[cfg(any(loom, shuttle))]
    pub fn new() -> Depth {
        Depth(AtomicU32::new(0))
    }
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Depth {
    fn default() -> Self {
        Depth::new()
    }
}

impl Ticket {
    #[cfg(not(any(loom, shuttle)))]
    pub const fn new() -> Ticket {
        Ticket {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            owner: AtomicU32::new(0),
        }
    }
    #[cfg(any(loom, shuttle))]
    pub fn new() -> Ticket {
        Ticket {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            owner: AtomicU32::new(0),
        }
    }

    /// Does `who` hold the lock?
    pub fn held_by(&self, who: u32) -> bool {
        self.owner.load(Ordering::Relaxed) == who + 1
    }

    /// Take the lock for `who` (spinning with `relax` between looks), or deepen it if `who`
    /// already holds it.
    pub fn lock(&self, who: u32, depth: &Depth, relax: impl Fn()) {
        if self.held_by(who) {
            depth.0.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            relax();
        }
        self.owner.store(who + 1, Ordering::Relaxed);
        depth.0.store(1, Ordering::Relaxed);
    }

    /// Take the lock only if nobody holds or waits for it (or deepen it if `who` holds it).
    pub fn try_lock(&self, who: u32, depth: &Depth) -> bool {
        if self.held_by(who) {
            depth.0.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        // Acquire: this load is what pairs with the previous holder's Release in `unlock`; the
        // CAS below reads `next`, which the unlock never wrote (loom caught a Relaxed load here).
        let serving = self.serving.load(Ordering::Acquire);
        if self
            .next
            .compare_exchange(
                serving,
                serving.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            self.owner.store(who + 1, Ordering::Relaxed);
            depth.0.store(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Release one level; returns true when the lock was actually released.
    pub fn unlock(&self, who: u32, depth: &Depth) -> bool {
        debug_assert!(self.held_by(who), "released by a non-holder");
        let d = depth.0.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(d >= 1, "depth underflow");
        if d == 1 {
            self.owner.store(0, Ordering::Relaxed);
            self.serving.fetch_add(1, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// The context switch's hand-over: `who` keeps the lock, but the nesting depth now belongs
    /// to the incoming thread. Returns the outgoing thread's depth.
    pub fn swap_depth(&self, who: u32, depth: &Depth, incoming: u32) -> u32 {
        debug_assert!(self.held_by(who));
        let out = depth.0.load(Ordering::Relaxed);
        depth.0.store(incoming, Ordering::Relaxed);
        out
    }

    /// Nobody holds it and nobody waits.
    pub fn is_free(&self) -> bool {
        self.next.load(Ordering::Relaxed) == self.serving.load(Ordering::Relaxed)
    }
}

impl Default for Ticket {
    fn default() -> Self {
        Ticket::new()
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn reentrant_and_released_at_depth_zero() {
        let t = Ticket::new();
        let d = Depth::new();
        t.lock(0, &d, || {});
        t.lock(0, &d, || {});
        assert!(t.held_by(0) && !t.is_free());
        assert!(!t.unlock(0, &d));
        assert!(t.held_by(0));
        assert!(t.unlock(0, &d));
        assert!(t.is_free() && !t.held_by(0));
    }

    #[test]
    fn try_lock_fails_when_held_by_another() {
        let t = Ticket::new();
        let (d0, d1) = (Depth::new(), Depth::new());
        assert!(t.try_lock(0, &d0));
        assert!(!t.try_lock(1, &d1));
        assert!(t.try_lock(0, &d0)); // reentrant
        t.unlock(0, &d0);
        t.unlock(0, &d0);
        assert!(t.try_lock(1, &d1));
    }

    #[test]
    fn swap_depth_hands_over_without_releasing() {
        let t = Ticket::new();
        let d = Depth::new();
        t.lock(0, &d, || {});
        t.lock(0, &d, || {});
        assert_eq!(t.swap_depth(0, &d, 1), 2);
        assert!(t.held_by(0) && !t.is_free());
        assert!(t.unlock(0, &d)); // the incoming thread's depth was 1
        assert!(t.is_free());
    }
}
