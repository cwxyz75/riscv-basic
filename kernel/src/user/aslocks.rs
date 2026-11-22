//! The `AS` locks a syscall takes (L17, SCHED-SMP-SPEC 3.3, KERNEL-V1-SPEC 4.5): the address spaces it touches, locked in ascending process
//! order **before** the memory locks (`kmem::get`), because `AS` comes first in the order. The pure code under them reaches an address space
//! through `AsCell::held_mut`, which checks that this hart holds its lock.
//!
//! While the big lock is held nothing else needs a process's address space except a page fault on another hart (L17 part c), which takes the
//! one lock and nothing else of the big lock's. A syscall never sleeps with these held: they are dropped (the value goes out of scope) before
//! any wait, as the memory locks are.

use super::*;
use klock::aslock::{AsCell, AsHold};

/// Address spaces held until this is dropped.
pub(super) struct AsLocks(#[allow(dead_code)] Vec<AsHold>);

impl AsLocks {
    /// The address spaces of the processes `pids` (any order, repeats and absent ones are fine), in ascending order. The big lock is held.
    pub(super) fn of(s: &UserState, pids: &[u64]) -> AsLocks {
        let mut ids: Vec<u64> = pids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let cells: Vec<alloc::sync::Arc<AsCell>> = ids
            .iter()
            .filter_map(|pid| s.procs.get(pid).map(|p| p.aspace.clone()))
            .collect();
        AsLocks(cells.iter().map(AsCell::hold).collect())
    }

    /// The calling process's address space.
    pub(super) fn caller(s: &UserState) -> AsLocks {
        match caller_pid(s) {
            Ok(me) => AsLocks::of(s, &[me]),
            Err(_) => AsLocks(Vec::new()),
        }
    }

    /// Every process's address space, ascending: for the calls that look at or change all of them (a decommit, a snapshot, a pager's
    /// supply) and for the calls that name a process by a handle (resolving it first would repeat the call's own checks).
    pub(super) fn all(s: &UserState) -> AsLocks {
        let cells: Vec<alloc::sync::Arc<AsCell>> =
            s.procs.values().map(|p| p.aspace.clone()).collect();
        AsLocks(cells.iter().map(AsCell::hold).collect())
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
