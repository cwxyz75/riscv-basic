//! The `AS` locks a syscall takes (L17, SCHED-SMP-SPEC 3.3, KERNEL-V1-SPEC 4.5): the address spaces it touches, locked in ascending process
//! order **before** the memory locks (`kmem::get`), because `AS` comes first in the order. The pure code under them reaches an address space
//! through `AsCell::held_mut`, which checks that this hart holds its lock.
//!
//! While the big lock is held nothing else needs a process's address space except a page fault on another hart (L17 part c), which takes the
//! one lock and nothing else of the big lock's. A syscall never sleeps with these held: they are dropped (the value goes out of scope) before
//! any wait, as the memory locks are.

use super::*;
use klock::aslock::{AsCell, AsHold};

/// The vector `AsLocks::all` fills, made once at boot with room for the most processes there can be (`AS_HELD_MOST`), so that taking every address
/// space allocates nothing and cannot fail (KT9, T246: the copy-on-write sync after every call needs it). The big lock is held whenever it is taken or
/// put back, so there is one user at a time.
struct Scratch(core::cell::UnsafeCell<Vec<AsHold>>);

// SAFETY: touched only with the big (object) lock held (`AsLocks::all` and `Drop` run inside `locked_state` or `with_state`), so no two harts use it at
// once; a held address space is released by the hart that took it, before the lock is.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch(core::cell::UnsafeCell::new(Vec::new()));

/// Make the scratch (once, at boot; an allocation failure there is a boot failure by design).
pub fn reserve_scratch() {
    // SAFETY: boot, one hart running, nothing holds the scratch.
    let v = unsafe { &mut *SCRATCH.0.get() };
    v.reserve_exact(kcore::limits::AS_HELD_MOST);
}

/// Take the scratch out (an empty vector, with no room, when it is already out: the caller then reserves with a fallible call).
fn take_scratch() -> Vec<AsHold> {
    // SAFETY: the big lock is held, see `Scratch`.
    core::mem::take(unsafe { &mut *SCRATCH.0.get() })
}

/// Put the (empty) vector back, keeping the larger room.
fn put_scratch(v: Vec<AsHold>) {
    // SAFETY: the big lock is held, see `Scratch`.
    let slot = unsafe { &mut *SCRATCH.0.get() };
    if v.capacity() >= slot.capacity() {
        *slot = v;
    }
}

/// Address spaces held until this is dropped (released last taken first, the order the checker and the saved interrupt state expect). One space is held
/// inline (no allocation: the common call); many are in a vector that is reserved with a fallible call (KT9: a failing allocation is `NoMemory`).
pub(super) struct AsLocks {
    one: Option<AsHold>,
    many: Vec<AsHold>,
}

impl Drop for AsLocks {
    fn drop(&mut self) {
        while let Some(h) = self.many.pop() {
            drop(h);
        }
        drop(self.one.take());
        put_scratch(core::mem::take(&mut self.many));
    }
}

impl AsLocks {
    /// The address space of process `pid` (nothing when it is absent). The big lock is held. Allocates nothing.
    pub(super) fn one(s: &UserState, pid: u64) -> AsLocks {
        AsLocks {
            one: s.procs.get(&pid).map(|p| AsCell::hold(&p.aspace)),
            many: Vec::new(),
        }
    }

    /// No address space (a call that touches none).
    pub(super) fn none() -> AsLocks {
        AsLocks {
            one: None,
            many: Vec::new(),
        }
    }

    /// The calling process's address space.
    pub(super) fn caller(s: &UserState) -> AsLocks {
        match caller_pid(s) {
            Ok(me) => AsLocks::one(s, me),
            Err(_) => AsLocks::none(),
        }
    }

    /// Every process's address space, ascending: for the calls that look at or change all of them (a decommit, a snapshot, a pager's
    /// supply) and for the calls that name a process by a handle (resolving it first would repeat the call's own checks). `NoMemory` when the
    /// list cannot be made.
    pub(super) fn all(s: &UserState) -> Result<AsLocks, Status> {
        let n = s.procs.len();
        let mut many = take_scratch();
        if many.try_reserve_exact(n).is_err() {
            put_scratch(many);
            return Err(Status::NoMemory);
        }
        many.extend(s.procs.values().map(|p| AsCell::hold(&p.aspace)));
        Ok(AsLocks { one: None, many })
    }
}

/// The hooks the `AS` lock runs on: this machine's interrupt state, hart number, spin watchdog and order checker.
pub fn install_hooks() {
    use klock::Machine as _;
    fn acquired(c: klock::Class, i: u32) {
        crate::bkl::Machine::acquired(c, i)
    }
    static HOOKS: klock::aslock::Hooks = klock::aslock::Hooks {
        irq_save: crate::bkl::Machine::irq_save,
        irq_restore: crate::bkl::Machine::irq_restore,
        hart: crate::bkl::Machine::hart,
        relax: crate::bkl::Machine::relax,
        acquired,
        released: crate::bkl::Machine::released,
    };
    klock::aslock::install(&HOOKS);
}
