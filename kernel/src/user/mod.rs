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
/// The guest syscall fuzzer (KT8): started by the boot-time suite in a limited job, not part of the boot image.
pub static KFUZZ_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kfuzz.elf"));
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
    // (T239, debug builds: the object lock makes this one flag, and a closure that opens `with_state` again would alias the `&mut`)
    #[cfg(debug_assertions)]
    let _one = StateOpen::enter();
    f(state())
}

/// Debug builds: a `with_state` closure is open (the flag is the object lock's, so one for the machine).
#[cfg(debug_assertions)]
static STATE_OPEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static STATE_SITE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[cfg(debug_assertions)]
struct StateOpen;

#[cfg(debug_assertions)]
impl StateOpen {
    #[track_caller]
    fn enter() -> StateOpen {
        #[allow(clippy::panic)]
        // INVARIANT: only a kernel bug reaches it: `with_state` is called by kernel paths alone, none of which calls it again inside its closure whatever a program passes, and the check exists only in debug builds
        if STATE_OPEN.swap(true, Ordering::Relaxed) {
            let at = core::panic::Location::caller();
            let outer = STATE_SITE.load(Ordering::Relaxed);
            panic!(
                "with_state opened at {at} inside the one opened at line {outer} of the same file or another: two live `&mut UserState` (T239)"
            );
        }
        STATE_SITE.store(core::panic::Location::caller().line(), Ordering::Relaxed);
        StateOpen
    }
}

#[cfg(debug_assertions)]
impl Drop for StateOpen {
    fn drop(&mut self) {
        STATE_OPEN.store(false, Ordering::Relaxed);
    }
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

/// The calling thread's handle table and job, with the object registry, in a section of the object lock of their own (the device-domain
/// calls, IOMMU6, which leave the lock between their phases). `None` (and `f` not run) if the calling thread has no process.
pub(crate) fn with_caller<R>(
    f: impl FnOnce(&mut ipc::Kobjs, &mut HandleTable, JobId) -> R,
) -> Option<R> {
    locked_state(|s| {
        let pid = s.pid_of(thread::current())?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        let job = p.job;
        Some(f(objs, &mut p.handles, job))
    })
}

/// `with_caller` for a step that also runs with no caller (the kernel's drain after a teardown): the objects, and the caller's handles
/// and job when the calling thread's process is still there. Takes the object lock.
pub(crate) fn with_caller_opt<R>(
    f: impl FnOnce(&mut ipc::Kobjs, Option<(&mut HandleTable, JobId)>) -> R,
) -> R {
    locked_state(|s| {
        let pid = s.pid_of(thread::current());
        let UserState { objs, procs, .. } = s;
        let caller = pid.and_then(|pid| procs.get_mut(&pid)).map(|p| {
            let job = p.job;
            (&mut p.handles, job)
        });
        f(objs, caller)
    })
}

/// The object registry, under the object lock this hart already holds (the IOMMU's fault interrupt raises DEVICE_FAULT through it).
pub(crate) fn with_objs<R>(f: impl FnOnce(&mut ipc::Kobjs) -> R) -> R {
    with_state(|s| f(&mut s.objs))
}

/// `with_state` for the code that does not hold the object lock yet (a kernel thread, a boot-time check): it takes the lock for the closure.
#[track_caller]
pub(crate) fn locked_state<R>(f: impl FnOnce(&mut UserState) -> R) -> R {
    irq::without(|| {
        // (KT14: the debug guard of `with_state` covers this entry too, so a closure of either kind that opens the other is caught)
        #[cfg(debug_assertions)]
        let _one = StateOpen::enter();
        f(state())
    })
}

#[track_caller]
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn state() -> &'static mut UserState {
    debug_assert!(
        !irq::enabled() && crate::bkl::held(),
        "user state touched without the big kernel lock"
    );
    // SAFETY: the object lock is held (debug-checked), so no other hart has the state; every caller drops the reference before any context switch, and
    // `with_state` and `locked_state` never nest (a debug guard panics on a second open of either while one is live: T239, KT14), so no two live `&mut` alias.
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
            // (KT9, T177: the address-space cells `process_create` arms, one per process the backstop allows)
            s.fill_as_pool(b.processes);
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
    any_state(|s| s.backstops)
}

/// The `##TEST` results user programs have reported so far (pass, fail).
pub fn reported() -> (u32, u32) {
    locked_state(|s| (s.pass, s.fail))
}

/// How many processes exist right now (a leak counter for tests).
/// (jobs alive including the root, and the root job's live processes, threads, objects, queued
/// IPC bytes and kernel bytes): `(1, 0, 0, 0, 0, 0)` when nothing is left (a leak oracle for the ledger, INVARIANTS
/// J2). Applies the credits of destroyed objects first. The kernel bytes exclude the page-index leaves of the VMOs that are alive (T249: each VMO's
/// index is charged to its job while it lives; the tests count the surviving VMOs themselves), so a leaf charge that outlives its VMO shows.
pub fn job_ledger() -> (usize, u64, u64, u64, u64, u64) {
    locked_state(|s| {
        ipc::settle(&mut s.objs, kmem::jobs().jobs);
        // (the memory locks before the job ledger's: the lock order)
        let index = kmem::get().vmos.index_bytes_of_job(JobId::ROOT);
        let jobs = &kmem::jobs().jobs;
        let u = jobs.usage(JobId::ROOT).unwrap_or_default();
        (
            jobs.live(),
            u.processes,
            u.threads,
            u.objects,
            u.ipc_bytes,
            u.kernel_bytes.saturating_sub(index),
        )
    })
}

pub fn process_count() -> usize {
    locked_state(|s| s.procs.len())
}

/// The kinds of kernel object a leak report counts, by [`kind_index`] (the last: a handle whose object is gone).
const KIND_NAMES: [&str; 15] = [
    "channel",
    "event",
    "vmo",
    "process",
    "thread",
    "exception",
    "pager",
    "socket",
    "port",
    "job",
    "interrupt",
    "resource",
    "iodomain",
    "debug",
    "gone",
];

/// [`KIND_NAMES`]'s index for a handle whose object is gone.
const KIND_GONE: usize = 14;

fn kind_index(o: &ipc::KObj) -> usize {
    use ipc::KObj as K;
    match o {
        K::Channel(_) => 0,
        K::Event(_) => 1,
        K::Vmo(_) => 2,
        K::Process(_) => 3,
        K::Thread(_) => 4,
        K::Exception(_) => 5,
        K::Pager(_) => 6,
        K::Socket(_) => 7,
        K::Port(_) => 8,
        K::Job(_) => 9,
        K::Interrupt(_) => 10,
        K::Resource => 11,
        K::IoDomain(_) => 12,
        K::Debug => 13,
    }
}

/// Print every process that exists (a leak test's failure report, T278): what it is waiting for to be torn down. The report is built
/// under the lock and printed after it.
#[allow(clippy::large_stack_frames)] // KT13: audited, a failure-only report of the leak checks, run once on the boot thread's stack after init has ended
pub fn report_processes() {
    use core::fmt::Write as _;
    let lines: alloc::vec::Vec<alloc::string::String> = locked_state(|s| {
        // (every live kernel object by kind, and each process object with the process it names and whether it ended: which process
        // handle is what keeps a process alive)
        let mut live = [0usize; KIND_NAMES.len()];
        let mut process_objs = alloc::vec::Vec::new();
        for o in s.objs.live_ids() {
            let Some(obj) = s.objs.get(o) else { continue };
            if let Some(c) = live.get_mut(kind_index(obj)) {
                *c = c.saturating_add(1);
            }
            if let ipc::KObj::Process(po) = obj {
                process_objs.push((o, po.pid, po.terminated));
            }
        }
        let live: alloc::vec::Vec<(&str, usize)> = KIND_NAMES
            .iter()
            .zip(live)
            .filter(|&(_, n)| n > 0)
            .map(|(&k, n)| (k, n))
            .collect();
        let summary = alloc::format!(
            "leak: live objects {live:?}; process objects (object, pid, ended) {process_objs:?}"
        );
        // (T278: every process on its way out, in the table or not: how long it has been ending and where its teardown is)
        let dying = dying::report(|pid| s.procs.contains_key(&pid));
        core::iter::once(summary)
            .chain(dying)
            .chain(s.procs.iter()
            .map(|(pid, p)| {
                // (what its handles name, by kind: a fingerprint of which program it is, since a process has no name)
                let mut kinds = [0usize; KIND_NAMES.len()];
                for o in p.handles.objects() {
                    let k = s.objs.get(o).map_or(KIND_GONE, kind_index);
                    if let Some(c) = kinds.get_mut(k) {
                        *c = c.saturating_add(1);
                    }
                }
                let held: alloc::vec::Vec<(&str, usize)> = KIND_NAMES
                    .iter()
                    .zip(kinds)
                    .filter(|&(_, n)| n > 0)
                    .map(|(&k, n)| (k, n))
                    .collect();
                alloc::format!(
                    "leak: proc {pid}: state {:?} detached {} privileged {} killed {:?} exit {:?} stopped {} threads alive {} live {:?} \
                     handles {} {held:?} vmos {} job {:?} object {:?} bootstrap end {:?} exception channel {:?} entry {:#x}",
                    p.state,
                    p.detached,
                    p.privileged,
                    p.killed,
                    p.exit_code,
                    p.stopped,
                    p.threads_alive,
                    p.live_tids.iter().collect::<alloc::vec::Vec<_>>(),
                    p.handles.len(),
                    p.vmos.len(),
                    p.job,
                    p.obj,
                    p.kernel_end,
                    p.exc_channel,
                    p.entry
                )
            }))
            .collect()
    });
    let mut out = crate::console::Console;
    for l in lines {
        let _ = writeln!(out, "{l}");
    }
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
pub(crate) mod dying;
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

/// Debug builds: what the user state says about the processes that are not finished (for `debug_stat` 132, which holds the state already:
/// `with_state` is not opened again, T239).
#[cfg(debug_assertions)]
#[allow(clippy::large_stack_frames)] // KT13: audited, a debug-only report
pub(crate) fn dump_state(s: &UserState) {
    use core::fmt::Write;
    let mut out = crate::console::Console;
    let _ = writeln!(
        out,
        "##SCHED user: job exception channels {:?}, suspends {:?}, parked {:?}",
        s.job_exc.keys().collect::<alloc::vec::Vec<_>>(),
        s.threads
            .iter()
            .filter(|(_, t)| t.suspend.is_some())
            .map(|(tid, _)| *tid)
            .collect::<alloc::vec::Vec<_>>(),
        s.threads
            .iter()
            .filter(|(_, t)| t.parked)
            .map(|(tid, _)| *tid)
            .collect::<alloc::vec::Vec<_>>()
    );
    for (pid, p) in s.procs.iter() {
        if p.live_tids.is_empty() && !p.stopped && p.killed.is_none() {
            continue;
        }
        let _ = writeln!(
            out,
            "##SCHED proc {pid}: state {:?} stopped {} killed {:?} exc_channel {:?} threads {:?} job {:?}",
            p.state, p.stopped, p.killed, p.exc_channel, p.live_tids, p.job
        );
    }
}
