//! The kernel's lock type and lock-order checker (SCHED-SMP-SPEC 3.3 and 3.4, OD95).
//!
//! * [`Class`] is the planned hierarchy: a hart holding class *n* may take only classes numbered above *n*. A class
//!   may be taken twice only where the table says so ([`Class::allows_two`]), in ascending instance order.
//! * [`Held`] is the checker's per-hart stack (`MAX_HELD` deep: enough for every process's address space and the locks after them) of what the hart holds, and the pure rule
//!   [`Held::check`] that the kernel's lock sites call through [`Machine::acquired`]. A release of a lock the hart does not hold
//!   panics ([`Held::pop`]), for every class but the console's.
//! * [`Spin`] is a fair (ticket) spinlock over [`kcore::ticket`] that saves and restores the interrupt state through a
//!   [`Machine`], tells the checker, and hands out a guard. It is **not** reentrant (the legacy lock and the console
//!   lock keep their own reentrancy).
//!
//! This is the one pure-side crate that contains `unsafe` (the lock's `UnsafeCell`), run under Miri, loom and shuttle.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::panic,
        clippy::unreachable,
        clippy::undocumented_unsafe_blocks
    )
)] // KT3 (KERNEL-V1-SPEC 7.4); sites that remain carry `#[allow] // INVARIANT:`
#![no_std]

pub mod aslock;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use kcore::ticket::{Depth, Ticket};

/// The lock classes in acquisition order (SCHED-SMP-SPEC 3.3). `Obj` is the object lock (L20: what was the big kernel lock): the object registry, every handle table, the process table, the wait table and the rest of `UserState`; reentrant until its users are all explicit.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Class {
    Obj = 1,
    As = 2,
    Procs = 3,
    Futex = 4,
    Vmo = 5,
    Jobs = 6,
    Pi = 7,
    Tt = 8,
    Rq = 9,
    Kstack = 10,
    /// The IOMMU (IOMMU-SPEC 11.1): the device directory, the command and fault queues, and (from IOMMU6) the domain store. After
    /// `OBJ` (the fault interrupt arrives holding it) and `VMO`/`JOBS`, before `FRAME` (a map takes table frames under it).
    Iommu = 11,
    Frame = 12,
    Heap = 13,
    Entropy = 14,
    Uart = 15,
}

impl Class {
    /// Every class, in order.
    pub const ALL: [Class; 15] = [
        Class::Obj,
        Class::As,
        Class::Procs,
        Class::Futex,
        Class::Vmo,
        Class::Jobs,
        Class::Pi,
        Class::Tt,
        Class::Rq,
        Class::Kstack,
        Class::Iommu,
        Class::Frame,
        Class::Heap,
        Class::Entropy,
        Class::Uart,
    ];

    /// May a hart hold two locks of this class at once (in ascending instance order)? The run queues (migration,
    /// pull), the address spaces (a call that names a target process takes the caller's and the target's, ascending by process id); every other class "never two" (3.3).
    pub fn allows_two(self) -> bool {
        matches!(self, Class::Rq | Class::As | Class::Tt | Class::Kstack)
    }

    /// How many locks of this class one hart may hold at once, when `allows_two`: two run queues, and as many address spaces as there are
    /// processes (a call that touches all of them takes them ascending, SCHED-SMP-SPEC 3.3).
    pub fn most_at_once(self) -> usize {
        match self {
            Class::As => 64,
            Class::Kstack => 3, // (the slots, the kernel page table's edits, the kernel's address space: T224)
            _ => 2,
        }
    }

    /// Is a recursive take by the holding hart legal? The legacy lock (a depth count) and the console line (a panic
    /// breaks it); every other class panics on a recursive take.
    pub fn reentrant(self) -> bool {
        matches!(self, Class::Obj | Class::Uart)
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
#[cfg(not(kani))]
pub const MAX_HELD: usize = 72;
/// (Under Kani the stack is 8 deep: the proofs' sequences hold at most five locks, so the capacity never decides an outcome, and
/// the smaller array keeps every symbolic index into it small.)
#[cfg(kani)]
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
            stack: [(Class::Obj, 0); MAX_HELD],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[allow(clippy::indexing_slicing)] // INVARIANT: len <= MAX_HELD, the stack's length, by `push`
    pub fn holds(&self, class: Class) -> bool {
        self.stack[..self.len].iter().any(|&(c, _)| c == class)
    }

    /// Would taking `(class, instance)` now be legal? Legal when nothing is held, or the new class is greater than
    /// the top's, or the classes are equal and the table allows two and the instance is greater, or the class is
    /// reentrant and the hart already holds it (a recursive take: it is not pushed again).
    #[allow(clippy::indexing_slicing)] // INVARIANT: len <= MAX_HELD; the slice is of the entries in use
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
        // ("two" means two: a third lock of the class is refused)
        let same = self.stack[..self.len]
            .iter()
            .filter(|&&(c, _)| c == class)
            .count();
        if class > top
            || (class == top
                && class.allows_two()
                && instance > top_instance
                && same < class.most_at_once())
        {
            Ok(())
        } else {
            Err(bad)
        }
    }

    /// Record an acquisition (after `check`). A recursive take of a reentrant class is not pushed again. Panics when
    /// more than [`MAX_HELD`] locks are held.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: `push` asserts len < MAX_HELD first (a kernel bug panics, with a message): the index and the increment are in range
    pub fn push(&mut self, class: Class, instance: u32) {
        if class.reentrant() && self.holds(class) {
            return;
        }
        assert!(self.len < MAX_HELD, "more than {MAX_HELD} locks held");
        self.stack[self.len] = (class, instance);
        self.len += 1;
    }

    /// Record a release. Locks are normally released in reverse order, but any held lock may go (a guard can be
    /// dropped out of order); a lock that is not held (never taken, released twice, or another instance) is a bug and
    /// panics, for every class but the console.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i is the position of a held entry, below len; len >= 1 when an entry was found
    pub fn pop(&mut self, class: Class, instance: u32) {
        if let Some(i) = self.position(class, instance) {
            self.stack.copy_within(i + 1..self.len, i);
            self.len -= 1;
        } else {
            // (a reentrant class is released only at its outermost take, so it is held then too. The console is the one lenient
            // class: a panic breaks its lock (`console::break_lock`) while another hart may be inside it, and that hart's nested
            // take then counts as an outermost one and releases the entry early, so its outer release finds nothing: T85 (5))
            assert!(
                class == Class::Uart,
                "released a {class:?} lock the hart does not hold"
            );
        }
    }

    /// Where `(class, instance)` is on the stack (the last such entry), if the hart holds it.
    #[allow(clippy::indexing_slicing)] // INVARIANT: len <= MAX_HELD; the slice is of the entries in use
    fn position(&self, class: Class, instance: u32) -> Option<usize> {
        self.stack[..self.len]
            .iter()
            .rposition(|&(c, i)| c == class && i == instance)
    }
}

/// What the lock needs from the machine. The kernel implements it; host tests use a std, loom or shuttle one.
pub trait Machine {
    /// Disable interrupts; return whether they were on.
    fn irq_save() -> bool;
    /// Restore the state `irq_save` returned.
    fn irq_restore(was: bool);
    /// This hart's index (below the kernel's `MAX_HARTS`).
    fn hart() -> usize;
    /// Between looks at a held lock.
    fn relax();
    /// The checker: the hart is about to take `(class, instance)` (it panics on a violation) ...
    #[track_caller]
    fn acquired(class: Class, instance: u32);
    /// ... and has released it.
    fn released(class: Class, instance: u32);
}

/// The calling hart's index as the ticket lock's owner id (`kcore::ticket` keeps `id + 1` in a `u32`, 0 meaning free). Panics
/// on an index that does not fit, which no machine with `MAX_HARTS` harts has: two harts must never share an id (T135, T155).
fn owner_id<M: Machine>() -> u32 {
    let hart = M::hart();
    match u32::try_from(hart) {
        Ok(id) if id < u32::MAX => id,
        #[allow(clippy::panic)]
        // INVARIANT: hart indices are below MAX_HARTS (64); an index past u32::MAX - 1 is a kernel bug
        _ => panic!("hart index {hart} has no ticket owner id"),
    }
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

    #[track_caller]
    /// Take the lock: save and disable interrupts, run the order check, spin for the ticket.
    pub fn lock(&self) -> Guard<'_, T, M> {
        let was = M::irq_save();
        M::acquired(self.class, self.instance);
        let me = owner_id::<M>();
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
            _not_send: PhantomData,
        }
    }

    /// Take the lock only if it is free right now.
    pub fn try_lock(&self) -> Option<Guard<'_, T, M>> {
        let was = M::irq_save();
        let me = owner_id::<M>();
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
            _not_send: PhantomData,
        })
    }

    /// Does the calling hart hold the lock?
    pub fn held_by_this_hart(&self) -> bool {
        self.ticket.held_by(owner_id::<M>())
    }

    /// The value's address, for a holder that proved it holds the lock (`aslock`).
    pub(crate) fn value_ptr(&self) -> *mut T {
        self.value.get()
    }

    /// The value, consuming the lock: no one else can hold it.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    /// The value, through `&mut self`: no other reference to the lock exists, so no one can hold it.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
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
    /// A guard belongs to the hart (and `Held` stack) that took it: not `Send`.
    _not_send: PhantomData<*mut ()>,
}

// SAFETY: `&Guard` gives `&T` only, so sharing a guard shares `&T`: sound exactly when `T: Sync`.
unsafe impl<T: Sync, M: Machine> Sync for Guard<'_, T, M> {}

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
        let me = owner_id::<M>();
        M::released(self.lock.class, self.lock.instance);
        self.lock.ticket.unlock(me, &self.depth);
        M::irq_restore(self.was);
    }
}

/// Bounded proofs of the order table and the held stack (T288; `cargo kani -p klock`, by hand: OD177, OD182). The host tests in
/// `tests/order.rs` check every pair and chain of three at chosen instances; these check every class, every instance below a bound
/// and every interleaving of takes and out-of-order releases up to a depth.
#[cfg(kani)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::panic
)] // INVARIANT: proof harnesses over bounded values (indices below the class count and the stack's length)
mod proofs {
    use super::*;

    /// How many operations a sequence has, and the instances it uses (below the bound: equal, above and below each other).
    const STEPS: usize = 5;
    const INSTANCES: u32 = 3;

    fn any_class() -> Class {
        let i: usize = kani::any();
        kani::assume(i < Class::ALL.len());
        Class::ALL[i]
    }

    /// A legal stack: the classes never descend; equal neighbours are of a class that allows two, in ascending instance; no class is
    /// held more often than it may be. So the top is the greatest entry, which is why `check` may look at the top only.
    fn legal(h: &Held) -> bool {
        let s = &h.stack[..h.len];
        let mut ok = true;
        let mut k = 1;
        while k < s.len() {
            let ((a, ia), (b, ib)) = (s[k - 1], s[k]);
            ok &= a < b || (a == b && a.allows_two() && ia < ib);
            k += 1;
        }
        for &(c, _) in s {
            let n = s.iter().filter(|&&(d, _)| d == c).count();
            ok &= n == 1 || (c.allows_two() && n <= c.most_at_once());
        }
        ok
    }

    /// The order on the classes is a strict total order, and `ALL` lists them in it, numbered 1 to 15 (the kernel's masks rely on
    /// the numbers).
    #[kani::proof]
    fn the_classes_are_strictly_and_totally_ordered() {
        let (i, j, k): (usize, usize, usize) = (kani::any(), kani::any(), kani::any());
        let n = Class::ALL.len();
        kani::assume(i < n && j < n && k < n);
        let (a, b, c) = (Class::ALL[i], Class::ALL[j], Class::ALL[k]);
        assert_eq!(a as usize, i + 1);
        // trichotomy, irreflexivity, transitivity, and the order of `ALL`
        assert!(u8::from(a < b) + u8::from(a == b) + u8::from(b < a) == 1);
        assert!(!(a < a));
        if a < b && b < c {
            assert!(a < c);
        }
        assert_eq!(a < b, i < j);
    }

    /// Any sequence of takes (of any class and instance, pushed when `check` allows them) and releases (of any held entry, out of
    /// order too) keeps the stack legal, and no take that `check` accepts is below anything held: the checker never accepts a
    /// descending acquisition, whatever was released before it.
    #[kani::proof]
    #[kani::unwind(7)]
    fn the_held_stack_never_accepts_a_descending_acquisition() {
        let mut h = Held::new();
        let mut step = 0;
        while step < STEPS {
            if kani::any() || h.len == 0 {
                let class = any_class();
                let instance: u32 = kani::any();
                kani::assume(instance < INSTANCES);
                let again = class.reentrant() && h.holds(class);
                if h.check(class, instance).is_ok() {
                    for &(c, i) in &h.stack[..h.len] {
                        if again {
                            // a recursive take of a reentrant class: nothing above it but itself
                            assert!(c <= class);
                        } else {
                            assert!(
                                c < class || (c == class && class.allows_two() && i < instance)
                            );
                        }
                    }
                    let before = h.len;
                    h.push(class, instance);
                    assert_eq!(h.len, if again { before } else { before + 1 });
                }
            } else {
                let at: usize = kani::any();
                kani::assume(at < h.len);
                let (c, i) = h.stack[at];
                h.pop(c, i);
            }
            assert!(legal(&h));
            step += 1;
        }
    }

    /// A release of a lock the hart does not hold panics, for every class but the console, whatever is held (T85 (5)). `pop`
    /// releases what `position` finds and otherwise asserts the class is the console, so this proves, for every stack the takes
    /// can build and every `(class, instance)`, that `position` finds exactly the held entries: an unheld one is never released
    /// quietly. (`#[kani::should_panic]` would show only that some unheld release panics, not that every one does.)
    #[kani::proof]
    #[kani::unwind(6)]
    fn a_release_finds_exactly_the_held_entries() {
        let mut h = Held::new();
        let mut step = 0;
        while step < 4 {
            let (c, i): (Class, u32) = (any_class(), kani::any());
            kani::assume(i < INSTANCES);
            if h.check(c, i).is_ok() {
                h.push(c, i);
            }
            step += 1;
        }
        let (c, i): (Class, u32) = (any_class(), kani::any());
        let mut held = false;
        let mut k = 0;
        while k < h.len {
            held |= h.stack[k] == (c, i);
            k += 1;
        }
        match h.position(c, i) {
            Some(at) => assert!(held && at < h.len && h.stack[at] == (c, i)),
            None => assert!(!held),
        }
    }

    /// End to end: a release of an unheld lock of any class but the console reaches the panic. (`should_panic`: a panic is
    /// found and every failure found is one; the harness above is the "every" half.)
    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::should_panic]
    fn a_release_of_an_unheld_lock_panics() {
        let mut h = Held::new();
        let mut step = 0;
        while step < 3 {
            let (c, i): (Class, u32) = (any_class(), kani::any());
            kani::assume(i < INSTANCES);
            if h.check(c, i).is_ok() {
                h.push(c, i);
            }
            step += 1;
        }
        let (c, i): (Class, u32) = (any_class(), kani::any());
        kani::assume(c != Class::Uart);
        kani::assume(!h.stack[..h.len].iter().any(|&e| e == (c, i)));
        h.pop(c, i);
    }

    /// ... and for the console it leaves the stack as it was.
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_release_of_an_unheld_console_lock_changes_nothing() {
        let mut h = Held::new();
        let mut step = 0;
        while step < 3 {
            let (c, i): (Class, u32) = (any_class(), kani::any());
            kani::assume(i < INSTANCES && c != Class::Uart);
            if h.check(c, i).is_ok() {
                h.push(c, i);
            }
            step += 1;
        }
        let before = h.len;
        h.pop(Class::Uart, 0);
        assert_eq!(h.len, before);
    }
}
