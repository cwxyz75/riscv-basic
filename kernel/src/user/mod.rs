//! User mode (S3b): processes, the program loader, the syscall dispatcher and the
//! handling of traps from U-mode. The logic that can be pure (ELF validation, user memory
//! copies, handle tables, channels) lives in `kcore` and is tested on the host; this file
//! wires it to the machine.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use crate::console::Console;
use crate::{irq, kmem, thread};
use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::user::{MAX_DEBUG_WRITE, STACK_PAGES, STACK_TOP};
use abi::Handle;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::aspace::{Access, AddressSpace};
use kcore::elf;
use kcore::handle::HandleTable;
use kcore::handle::MAX_SLOTS as MAX_HANDLES;
use kcore::ipc::{self, ReadError};
use kcore::job::{JobId, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::sched::Tid;
use kcore::usercopy::copy_from_user;
use ksys::{charge_thread, credit_thread, new_start};
use ksys::{ProcState, Process, UserState};

/// The embedded user programs (see build.rs); empty if they were not built.
pub static SUITE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/suite.elf"));
pub static CRASH_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crash.elf"));
/// The interactive supervisor (`init`, S4a): the first program of the interactive boot.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub static INIT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/init.elf"));
/// The boot image (S4-SPEC section 3): every user program and the fixture files, as one
/// archive that becomes a read-only VMO for `init`.
pub static BOOT_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bootimg.bin"));
/// The conformance program of the system library (AB9): started by the boot-time suite, not part of the boot image.
pub static ABI_CONFORMANCE_ELF: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/abi_conformance.elf"));
pub static HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));

/// The quota to charge new objects to: `job` (the creating process's, or the root's for the
/// kernel's own objects).
#[track_caller]
fn quota(job: JobId) -> QuotaGuard {
    let held = kmem::jobs();
    // SAFETY: the reference is to the ledger the guard in `held` keeps locked; `QuotaGuard` drops the quota before the guard.
    let jobs: &'static mut kcore::job::Jobs = unsafe { &mut *(held.jobs as *mut kcore::job::Jobs) };
    QuotaGuard {
        q: ipc::Quota {
            jobs,
            job: Some(job),
        },
        _held: held,
    }
}

/// A [`ipc::Quota`] that holds the `JOBS` lock while it exists (it derefs to the quota).
pub(crate) struct QuotaGuard {
    q: ipc::Quota<'static>,
    _held: kmem::JobsRef,
}

impl core::ops::Deref for QuotaGuard {
    type Target = ipc::Quota<'static>;
    fn deref(&self) -> &Self::Target {
        &self.q
    }
}

impl core::ops::DerefMut for QuotaGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.q
    }
}

static mut USER: Option<UserState> = None;

/// Set once the user state exists (`state()` made it): the commit-on-drop of `irq::without` looks at it before touching the state (L20).
pub(crate) static USER_READY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Run `f` on the user state with the object lock held (L20): the only way in. The lock is the outermost-aware critical section of `irq::without` (reentrant:
/// a helper may take it again), and ending the outermost one commits what `f` touched to the waiters (W1'). The reference does not outlive the closure.
#[track_caller]
pub(crate) fn with_state<R>(f: impl FnOnce(&mut UserState) -> R) -> R {
    f(state())
}

/// `with_state` for a helper that is reached both with the object lock held and without it (see `bkl::ensure`).
#[track_caller]
pub(crate) fn any_state<R>(f: impl FnOnce(&mut UserState) -> R) -> R {
    if crate::bkl::holds() {
        with_state(f)
    } else {
        locked_state(f)
    }
}

/// `with_state` for the code that does not hold the object lock yet (a kernel thread, a boot-time check): it takes the lock for the closure.
#[track_caller]
pub(crate) fn locked_state<R>(f: impl FnOnce(&mut UserState) -> R) -> R {
    irq::without(|| f(state()))
}

/// The user state for the few places that run inside a section the caller made (an `irq::without` that returns early with `?` or `return`, which a closure
/// would change): the object lock is asserted held. Everything else goes through `with_state`.
#[track_caller]
fn state_locked() -> &'static mut UserState {
    state()
}

#[track_caller]
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn state() -> &'static mut UserState {
    debug_assert!(
        !irq::enabled() && crate::bkl::held(),
        "user state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (debug-checked); every caller drops the reference
    // before any context switch, so no two live `&mut` alias.
    unsafe {
        let slot = &mut *addr_of_mut!(USER);
        slot.get_or_insert_with(|| {
            let mut s = UserState::with_irq_slots(crate::aia::slots());
            // the backstops (PL8): derived from the RAM the machine has, the kernel-stack window's slots bounding the threads
            let b = kcore::limits::Backstops::for_ram(
                kmem::stats().memory_frames.saturating_mul(4096),
                crate::kstack::MAX_SLOTS as usize,
            );
            s.backstops = b;
            s.objs.set_caps(b.objects, b.ipc_bytes);
            // (the kernel's own table holds one end per process and job: bounded by the object backstop, like the rest)
            s.ktable = kcore::handle::HandleTable::new(b.objects);
            // the ASID field the harts implement (L18): the allocator hands out ASIDs from it and turns the generation when they are used up
            s.asids = kcore::asid::Asids::new(if cfg!(feature = "fault-asid-reuse-no-flush") {
                1
            } else {
                crate::arch::asid_bits()
            });
            // (L19) the generation, for the return to user mode that holds no big lock
            trap::publish_asid_generation(s.asids.generation_cell());
            // AB3 (test builds): counters that start near a boundary, `ff=koid:..,objgen:..,handlegen:..`
            if let Some(n) = crate::dtb::ff_value("koid") {
                s.objs.fast_forward_koid(n);
            }
            if let Some(n) = crate::dtb::ff_value("objgen") {
                s.objs.set_first_generation(n);
            }
            if let Some(n) = crate::dtb::ff_value("handlegen") {
                s.first_handle_gen = n;
            }
            USER_READY.store(true, Ordering::Release);
            s
        })
    }
}

/// The backstops in force (a test hook: `debug_stat` 120 to 124).
pub fn backstops() -> kcore::limits::Backstops {
    with_state(|s| s.backstops)
}

/// The `##TEST` results user programs have reported so far (pass, fail).
pub fn reported() -> (u32, u32) {
    locked_state(|s| (s.pass, s.fail))
}

/// How many processes exist right now (a leak counter for tests).
/// (jobs alive including the root, and the root job's live processes, threads, objects, queued
/// IPC bytes and kernel bytes): `(1, 0, 0, 0, 0, 0)` when nothing is left (a leak oracle for the ledger, INVARIANTS
/// J2). Applies the credits of destroyed objects first.
pub fn job_ledger() -> (usize, u64, u64, u64, u64, u64) {
    locked_state(|s| {
        ipc::settle(&mut s.objs, kmem::jobs().jobs);
        let jobs = &kmem::jobs().jobs;
        let u = jobs.usage(JobId::ROOT).unwrap_or_default();
        (
            jobs.live(),
            u.processes,
            u.threads,
            u.objects,
            u.ipc_bytes,
            u.kernel_bytes,
        )
    })
}

pub fn process_count() -> usize {
    locked_state(|s| s.procs.len())
}

/// Threads sleeping on a futex (for leak checks).
pub fn futex_sleepers() -> usize {
    futex_table().len()
}

/// Threads waiting for a pager and pagers still alive (for leak checks, PG1).
pub fn pager_leftovers() -> usize {
    locked_state(|s| s.pagers.waits().saturating_add(s.pager_ends.len()))
}

/// The number of live kernel objects (for leak checks).
pub fn live_objects() -> usize {
    locked_state(|s| s.objs.live())
}

/// Why a program could not be started (the detail is shown through `Debug` in test failures).
#[derive(Debug)]
#[allow(dead_code)]
pub enum LoadError {
    Elf(elf::ElfError),
    NoMemory,
    NoProgram,
    /// The kernel's top-level page-table slots changed since boot (new processes would not see them).
    KernelHalfChanged,
}

mod aslocks;
mod calls;
mod kobjects;
mod lifecycle;
mod loader;
mod syslib;
mod trap;
mod waits;
mod wake;
#[allow(unused_imports)]
pub use aslocks::*;
#[allow(unused_imports)]
pub use calls::*;
#[allow(unused_imports)]
pub use kobjects::*;
#[allow(unused_imports)]
pub use lifecycle::*;
#[allow(unused_imports)]
pub use loader::*;
#[allow(unused_imports)]
pub use trap::*;
#[allow(unused_imports)]
pub use waits::*;
#[allow(unused_imports)]
pub use wake::*;
