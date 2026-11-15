//! The kernel's lock type and lock-order checker (SCHED-SMP-SPEC 3.3 and 3.4, OD95).
//!
//! * [`Class`] is the planned hierarchy: a hart holding class *n* may take only classes numbered above *n*. A class
//!   may be taken twice only where the table says so ([`Class::allows_two`]), in ascending instance order.
//! * [`Held`] is the checker's per-hart stack (eight deep) of what the hart holds, and the pure rule
//!   [`Held::check`] that the kernel's lock sites call through [`Machine::acquired`].
//! * [`Spin`] is a fair (ticket) spinlock over [`kcore::ticket`] that saves and restores the interrupt state through a
//!   [`Machine`], tells the checker, and hands out a guard. It is **not** reentrant (the legacy lock and the console
//!   lock keep their own reentrancy).
//!
//! This is the one pure-side crate that contains `unsafe` (the lock's `UnsafeCell`), run under Miri, loom and shuttle.

#![no_std]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use kcore::ticket::{Depth, Ticket};

/// The lock classes in acquisition order (SCHED-SMP-SPEC 3.3). `Legacy` is the big kernel lock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Class {
    Legacy = 1,
    As = 2,
    Obj = 3,
    Procs = 4,
    Futex = 5,
    Vmo = 6,
    Jobs = 7,
    Pi = 8,
    Tt = 9,
    Rq = 10,
    Kstack = 11,
    Frame = 12,
    Heap = 13,
    Uart = 14,
}

impl Class {
    /// Every class, in order.
    pub const ALL: [Class; 14] = [
        Class::Legacy,
        Class::As,
        Class::Obj,
        Class::Procs,
        Class::Futex,
        Class::Vmo,
        Class::Jobs,
        Class::Pi,
        Class::Tt,
        Class::Rq,
        Class::Kstack,
        Class::Frame,
        Class::Heap,
        Class::Uart,
    ];

    /// May a hart hold two locks of this class at once (in ascending instance order)? Only the run queues (migration,
    /// pull); every other class "never two" (3.3).
    pub fn allows_two(self) -> bool {
        self == Class::Rq
    }

    /// Is a recursive take by the holding hart legal? The legacy lock (a depth count) and the console line (a panic
    /// breaks it); every other class panics on a recursive take.
    pub fn reentrant(self) -> bool {
        matches!(self, Class::Legacy | Class::Uart)
    }
}

/// An order violation: taking `taking` (instance `instance`) while holding `held`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Violation {
    pub held: Class,
    pub held_instance: u32,
    pub taking: Class,
    pub instance: u32,
}

/// The most locks one hart may hold at once.
pub const MAX_HELD: usize = 8;

/// What one hart holds: a stack of `(class, instance)`.
#[derive(Clone, Copy, Debug)]
pub struct Held {
    stack: [(Class, u32); MAX_HELD],
    len: usize,
}

impl Default for Held {
    fn default() -> Self {
        Self::new()
    }
}

impl Held {
    pub const fn new() -> Held {
        Held {
            stack: [(Class::Legacy, 0); MAX_HELD],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn holds(&self, class: Class) -> bool {
        self.stack[..self.len].iter().any(|&(c, _)| c == class)
    }

    /// Would taking `(class, instance)` now be legal? Legal when nothing is held, or the new class is greater than
    /// the top's, or the classes are equal and the table allows two and the instance is greater, or the class is
    /// reentrant and the hart already holds it (a recursive take: it is not pushed again).
    pub fn check(&self, class: Class, instance: u32) -> Result<(), Violation> {
        let Some(&(top, top_instance)) = self.stack[..self.len].last() else {
            return Ok(());
        };
        let bad = Violation {
            held: top,
            held_instance: top_instance,
            taking: class,
            instance,
        };
        // the whole stack, not only its top: a reentrant take of a class lower down is still checked against what is
        // held above it
        if class.reentrant() && self.holds(class) {
            // a recursive take of an outer lock while a later one is held is the wrong order too, unless the later
            // classes are the same class (the console re-taken by itself)
            return if top <= class { Ok(()) } else { Err(bad) };
        }
        if class > top || (class == top && class.allows_two() && instance > top_instance) {
            Ok(())
        } else {
            Err(bad)
        }
    }

    /// Record an acquisition (after `check`). A recursive take of a reentrant class is not pushed again. Panics when
    /// more than [`MAX_HELD`] locks are held.
    pub fn push(&mut self, class: Class, instance: u32) {
        if class.reentrant() && self.holds(class) {
            return;
        }
        assert!(self.len < MAX_HELD, "more than {MAX_HELD} locks held");
        self.stack[self.len] = (class, instance);
        self.len += 1;
    }

    /// Record a release. Locks are normally released in reverse order, but any held lock may go (a guard can be
    /// dropped out of order); a lock that is not held is a bug.
    pub fn pop(&mut self, class: Class, instance: u32) {
        let at = self.stack[..self.len]
            .iter()
            .rposition(|&(c, i)| c == class && i == instance);
        if let Some(i) = at {
            self.stack.copy_within(i + 1..self.len, i);
            self.len -= 1;
        } else {
            debug_assert!(
                class.reentrant(),
                "released a {class:?} lock the hart does not hold"
            );
        }
    }
}

/// What the lock needs from the machine. The kernel implements it; host tests use a std, loom or shuttle one.
pub trait Machine {
    /// Disable interrupts; return whether they were on.
    fn irq_save() -> bool;
    /// Restore the state `irq_save` returned.
    fn irq_restore(was: bool);
    /// This hart's index.
    fn hart() -> u32;
    /// Between looks at a held lock.
    fn relax();
    /// The checker: the hart is about to take `(class, instance)` (it panics on a violation) ...
    fn acquired(class: Class, instance: u32);
    /// ... and has released it.
    fn released(class: Class, instance: u32);
}

/// A spinlock of a class, protecting a `T`.
pub struct Spin<T, M: Machine> {
    ticket: Ticket,
    class: Class,
    instance: u32,
    value: UnsafeCell<T>,
    _m: PhantomData<fn() -> M>,
}

// SAFETY: `value` is reached only through a guard, and a guard exists only while the ticket lock is held, so there is
// never more than one reference; `T: Send` because the holder may be any hart.
unsafe impl<T: Send, M: Machine> Sync for Spin<T, M> {}
// SAFETY: moving the lock moves the value, which is `Send`.
unsafe impl<T: Send, M: Machine> Send for Spin<T, M> {}

impl<T, M: Machine> Spin<T, M> {
    #[cfg(not(any(loom, shuttle)))]
    pub const fn new(class: Class, instance: u32, value: T) -> Self {
        Spin {
            ticket: Ticket::new(),
            class,
            instance,
            value: UnsafeCell::new(value),
            _m: PhantomData,
        }
    }
    #[cfg(any(loom, shuttle))]
    pub fn new(class: Class, instance: u32, value: T) -> Self {
        Spin {
            ticket: Ticket::new(),
            class,
            instance,
            value: UnsafeCell::new(value),
            _m: PhantomData,
        }
    }

    /// Take the lock: save and disable interrupts, run the order check, spin for the ticket.
    pub fn lock(&self) -> Guard<'_, T, M> {
        let was = M::irq_save();
        M::acquired(self.class, self.instance);
        let me = M::hart();
        assert!(
            !self.ticket.held_by(me),
            "recursive take of a {:?} lock",
            self.class
        );
        let depth = Depth::new();
        self.ticket.lock(me, &depth, M::relax);
        Guard {
            lock: self,
            depth,
            was,
        }
    }

    /// Take the lock only if it is free right now.
    pub fn try_lock(&self) -> Option<Guard<'_, T, M>> {
        let was = M::irq_save();
        let me = M::hart();
        let depth = Depth::new();
        if self.ticket.held_by(me) || !self.ticket.try_lock(me, &depth) {
            M::irq_restore(was);
            return None;
        }
        // (a successful try_lock is a take like any other: the checker sees it after the fact, which is as good for
        // order violations because the violation is a property of the sequence, not of the wait)
        M::acquired(self.class, self.instance);
        Some(Guard {
            lock: self,
            depth,
            was,
        })
    }

    /// Is nobody holding or waiting for the lock?
    pub fn is_free(&self) -> bool {
        self.ticket.is_free()
    }
}

/// The lock is held while this exists.
pub struct Guard<'a, T, M: Machine> {
    lock: &'a Spin<T, M>,
    depth: Depth,
    was: bool,
}

impl<T, M: Machine> Deref for Guard<'_, T, M> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock, so no other reference to the value exists.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T, M: Machine> DerefMut for Guard<'_, T, M> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` makes this the only reference through this guard.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T, M: Machine> Drop for Guard<'_, T, M> {
    fn drop(&mut self) {
        let me = M::hart();
        M::released(self.lock.class, self.lock.instance);
        self.lock.ticket.unlock(me, &self.depth);
        M::irq_restore(self.was);
    }
}
