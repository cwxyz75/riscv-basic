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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
pub static HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));

/// The quota to charge new objects to: `job` (the creating process's, or the root's for the
/// kernel's own objects).
fn quota(job: JobId) -> ipc::Quota<'static> {
    ipc::Quota {
        jobs: &mut kmem::get().vmos.jobs,
        job: Some(job),
    }
}

static mut USER: Option<UserState> = None;

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
            s
        })
    }
}

/// The `##TEST` results user programs have reported so far (pass, fail).
pub fn reported() -> (u32, u32) {
    irq::without(|| {
        let s = state();
        (s.pass, s.fail)
    })
}

/// How many processes exist right now (a leak counter for tests).
/// (jobs alive including the root, and the root job's live processes, threads, objects and queued
/// IPC bytes): `(1, 0, 0, 0, 0)` when nothing is left (a leak oracle for the ledger, INVARIANTS
/// J2). Applies the credits of destroyed objects first.
pub fn job_ledger() -> (usize, u64, u64, u64, u64) {
    irq::without(|| {
        ipc::settle(&mut state().objs, &mut kmem::get().vmos.jobs);
        let jobs = &kmem::get().vmos.jobs;
        let u = jobs.usage(JobId::ROOT).unwrap_or_default();
        (jobs.live(), u.processes, u.threads, u.objects, u.ipc_bytes)
    })
}

pub fn process_count() -> usize {
    irq::without(|| state().procs.len())
}

/// Threads sleeping on a futex (for leak checks).
pub fn futex_sleepers() -> usize {
    irq::without(|| state().futexes.len())
}

/// The number of live kernel objects (for leak checks).
pub fn live_objects() -> usize {
    irq::without(|| state().objs.live())
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

mod calls;
mod kobjects;
mod lifecycle;
mod loader;
mod trap;
mod waits;
mod wake;
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
