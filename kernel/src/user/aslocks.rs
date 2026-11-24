//! The `AS` locks a syscall takes (L17, SCHED-SMP-SPEC 3.3, KERNEL-V1-SPEC 4.5): the address spaces it touches, locked in ascending process
//! order **before** the memory locks (`kmem::get`), because `AS` comes first in the order. The pure code under them reaches an address space
//! through `AsCell::held_mut`, which checks that this hart holds its lock.
//!
//! While the big lock is held nothing else needs a process's address space except a page fault on another hart (L17 part c), which takes the
//! one lock and nothing else of the big lock's. A syscall never sleeps with these held: they are dropped (the value goes out of scope) before
//! any wait, as the memory locks are.

use super::*;
use klock::aslock::{AsCell, AsHold};

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
        let mut cells: Vec<alloc::sync::Arc<AsCell>> = Vec::new();
        cells.try_reserve_exact(n).map_err(|_| Status::NoMemory)?;
        cells.extend(s.procs.values().map(|p| p.aspace.clone()));
        let mut many: Vec<AsHold> = Vec::new();
        many.try_reserve_exact(n).map_err(|_| Status::NoMemory)?;
        many.extend(cells.iter().map(AsCell::hold));
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
