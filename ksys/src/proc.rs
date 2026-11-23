//! Processes and the calls that act across them (TS3b): the process record, and `process_map`, `process_unmap`, `process_self`,
//! `process_info`, `thread_info`, `job_root` and `resource_root` over the process table, with the machine's part (TLB shootdowns) behind
//! a small trait.

use crate::{fail, Reply};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use kcore::aspace::AddressSpace;
use kcore::aspace::Vmos;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs};
use kcore::job::{JobId, Jobs, Resource};
use kcore::pagetable::{Perms, PhysMem};
use kcore::sched::Tid;
use kcore::Mm;

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;

/// What the process calls need from the machine: the translations other harts may still hold for an address space that changed.
pub trait Machine {
    /// Have every other hart drop its cached translations (a region was unmapped).
    fn shootdown(&mut self);
    /// Drop this hart's own cached translations.
    fn flush_tlb(&mut self);
    /// `pages` executable pages were just made visible (PL7): make every hart's instruction fetches agree with memory.
    fn icache_sync(&mut self, _pages: u64) {}
    /// A number below `n` from the kernel's entropy pool (HD1), for the placement of a mapping that asked for ANYWHERE; 0 where there is no pool.
    fn random_below(&mut self, _n: u64) -> u64 {
        0
    }
    /// Negative control (MM8): leave out the write-protection of a SNAPSHOT's source mappings. Never true in a real kernel.
    fn skip_cow_protect(&self) -> bool {
        false
    }
    /// CPU nanoseconds the live thread `tid` has run (AB6: `thread_info` 1 and `process_info` 1); 0 where there is no scheduler.
    fn thread_cpu_ns(&self, _tid: Tid) -> u64 {
        0
    }
    /// The same for the calling thread (`thread_info` with handle 0).
    fn current_thread_cpu_ns(&self) -> u64 {
        0
    }
    /// Negative control: leave out the shootdown that call number `call` (`VMO_DECOMMIT` or `PROCESS_PROTECT`) owes the other harts before a
    /// frame is freed or a permission is taken away. Never true in a real kernel.
    fn skip_shootdown(&self, _call: usize) -> bool {
        false
    }
}

/// A call acting on the process table: every process, the kernel objects, the memory and the ledger, and which process is calling.
pub struct PCtx<'a, M: PhysMem, X: Machine> {
    pub objs: &'a mut Kobjs,
    pub procs: &'a mut BTreeMap<u64, Box<Process>>,
    /// The calling process.
    pub me: u64,
    pub mm: &'a mut Mm<M>,
    pub vmos: &'a mut Vmos,
    pub jobs: &'a mut Jobs,
    pub machine: &'a mut X,
}

pub struct Process {
    /// The user threads of this process that have started and not ended (T191): what a kill, a suspend or a teardown walks, instead of every thread
    /// in the system. Kept in step with `UserState::thread_pid` (the same threads, keyed the other way): a thread is added when it registers
    /// itself on its first run and removed when it ends or its process is torn down.
    pub live_tids: alloc::collections::BTreeSet<Tid>,
    /// The address space behind its `AS` lock (L17): shared by `Arc` so a syscall can hold the locks of the spaces it touches ([`klock::aslock::AsCell::hold`])
    /// while the process table is borrowed as the pure code likes.
    pub aspace: alloc::sync::Arc<klock::aslock::AsCell>,
    pub handles: HandleTable,
    /// The address-space identifier and the generation it was allocated in (L18); the allocator is `UserState::asids`.
    pub tag: kcore::asid::AsTag,
    pub vmos: Vec<u64>,
    pub entry: u64,
    pub arg: u64,
    pub exit_code: Option<i32>,
    /// The creator's end of the bootstrap channel (a handle in `UserState::ktable`), until
    /// a test or server takes it with `take_bootstrap`.
    pub kernel_end: Option<Handle>,
    /// The kernel's end of the channel exceptions of this process's threads are delivered on (EX2; a handle in `UserState::ktable`).
    pub exc_channel: Option<Handle>,
    /// Where the first thread starts the stack (0: the default `STACK_TOP`).
    pub start_sp: u64,
    /// The process's object (for TERMINATED and kill-on-close), if it has one.
    pub obj: Option<kcore::object::ObjId>,
    pub state: ProcState,
    /// Started by `process_start`: its own thread tears it down when it ends and nobody
    /// joins that thread.
    pub detached: bool,
    /// `process_kill` asked for it to end, with this exit code; its thread dies at its next
    /// return to user mode (or wakes from a wait to do so).
    pub killed: Option<i32>,
    /// `process_suspend` stopped it: every thread parks at its next return to user mode (a thread blocked in a call
    /// stays blocked, and parks when the call returns) until `process_resume` or a kill. SIG2, SYSCALLS.md.
    pub stopped: bool,
    /// User threads that exist and have not ended (counted from `thread_spawn`, before the
    /// thread first runs). The process is torn down when this reaches 0.
    pub threads_alive: usize,
    /// The thread objects of the live threads, to mark them TERMINATED when they end.
    pub thread_objs: BTreeMap<Tid, kcore::object::ObjId>,
    /// The job this process is charged to (it holds one use of it): its process, its threads
    /// and, through the VMOs made for it, its memory.
    pub job: JobId,
    /// Pages charged to `job` for the page-table frames this process's mappings can need (a
    /// worst-case estimate per mapped region, plus the root table), by region start; credited
    /// when the region goes or the process ends. Table frames are not VMO pages, so without this
    /// a program could spend memory the page limit never sees.
    pub table_charge: BTreeMap<u64, u64>,
    /// Where each `process_map` charge in `table_charge` ends (its key is where it starts): the charge is credited when no region is left in
    /// that span, not when the region that started there goes (a range unmap or a protect can split a mapping: the tables of what stays
    /// must stay charged, MM7).
    pub charge_end: BTreeMap<u64, u64>,
    /// The stacks `thread_spawn` provisioned, by thread: freed when the thread ends (or with the process).
    pub stacks: BTreeMap<Tid, SpawnStack>,
    /// Where the next stack search in the arena starts (just above the last stack handed out).
    pub stack_hint: u64,
    /// Started by the kernel (`start`): may ask for the root job (`job_root`). Nothing a program
    /// creates is privileged.
    pub privileged: bool,
}

/// The stack a `thread_spawn` provisioned: its region in the address space and the VMO behind it.
#[derive(Clone, Copy)]
pub struct SpawnStack {
    pub region: u64,
    pub vmo: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Created and possibly populated, no thread yet.
    Created,
    Running,
}

fn bad(s: Status) -> Reply {
    fail(s)
}

fn e(s: Status) -> (Status, Option<u64>, Option<u64>) {
    (s, None, None)
}

/// Resolve a process handle and a VMO handle of the caller, checking them in the order SYSCALLS.md lists (BAD_HANDLE, WRONG_TYPE,
/// ACCESS_DENIED). Returns the target pid and the VMO id.
/// Is `[vaddr, vaddr + length)` a non-empty range inside the user half? (The range calls act only on regions, and regions are in the user half;
/// this keeps it so if a kernel-half region ever exists.)
fn in_user_range(vaddr: u64, length: u64) -> bool {
    length > 0
        && vaddr >= abi::user::BASE
        && vaddr
            .checked_add(length)
            .is_some_and(|end| end <= abi::user::TOP)
}

/// Where an ANYWHERE mapping of `bytes` goes (MM7): the mapping arena `[MAP_ARENA_BASE, the stack arena)`, at a random page-aligned place: eight
/// random positions are tried, then the first gap that fits from a random start, wrapping once; `None` when nothing fits.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the arena bounds are constants below the canonical limit; the chosen start is below `span`
fn place_anywhere<X: Machine>(machine: &mut X, aspace: &AddressSpace, bytes: u64) -> Option<u64> {
    const LO: u64 = abi::user::MAP_ARENA_BASE;
    let hi = abi::user::STACK_ARENA_BASE;
    if bytes > hi - LO {
        return None;
    }
    let slots = (hi - LO - bytes) / 4096 + 1;
    // eight random positions first; then the first gap that fits from a random start, wrapping once
    for _ in 0..8 {
        let at = LO + machine.random_below(slots) * 4096;
        if !aspace.has_region_in(at, at + bytes) {
            return Some(at);
        }
    }
    let from = LO + machine.random_below(slots) * 4096;
    aspace
        .find_gap(LO, hi, bytes, from)
        .or_else(|| aspace.find_gap(LO, hi, bytes, LO))
}

fn resolve_map_args(
    objs: &Kobjs,
    caller: &Process,
    ph: Handle,
    vh: Handle,
    flags: u64,
) -> Result<(u64, u64, Perms), Status> {
    let pe = caller.handles.get(ph)?;
    let ve = caller.handles.get(vh)?;
    let tpid = match objs.get(pe.obj) {
        Some(ipc::KObj::Process(p)) => p.pid,
        _ => return Err(Status::WrongType),
    };
    let vid = match objs.get(ve.obj) {
        Some(ipc::KObj::Vmo(id)) => *id,
        _ => return Err(Status::WrongType),
    };
    let mut need = Rights(Rights::MAP.0 | Rights::READ.0);
    if flags & PF_W != 0 {
        need = need.union(Rights::WRITE);
    }
    if flags & PF_X != 0 {
        need = need.union(Rights::EXECUTE);
    }
    if !pe.rights.contains(Rights::MANAGE) || !ve.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    // what the mapping may later be raised to: read always, write and execute as the VMO handle's rights allow
    let max = Perms {
        read: true,
        write: ve.rights.contains(Rights::WRITE),
        exec: ve.rights.contains(Rights::EXECUTE),
        user: true,
    };
    Ok((tpid, vid, max))
}

/// Drop what the copy log makes stale in every address space and have every hart drop its translations (COW1): after any call or fault
/// that may have made a page of a VMO family visible or replaced it. One atomic load when nothing is pending.
pub fn cow_sync<M: PhysMem, X: Machine>(
    procs: &mut BTreeMap<u64, Box<Process>>,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    machine: &mut X,
) {
    if !kcore::aspace::COPY_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let cleared =
        kcore::aspace::invalidate_copies(procs.values_mut().map(|p| p.aspace.held_mut()), mm, vmos);
    if cleared > 0 {
        machine.shootdown();
        machine.flush_tlb();
    }
}

/// A SNAPSHOT moved the pages of VMO `vmo` to a node it shares with the child: take the write permission off every translation of it, and
/// have every hart drop what it cached, before anything runs (COW1; the negative control `fault-skip-cow-protect` leaves it out).
fn snapshot_protect<M: PhysMem, X: Machine>(
    procs: &mut BTreeMap<u64, Box<Process>>,
    mm: &mut Mm<M>,
    machine: &mut X,
    vmo: u64,
) {
    if machine.skip_cow_protect() {
        return;
    }
    let mut changed = 0u64;
    for p in procs.values_mut() {
        changed = changed.saturating_add(p.aspace.held_mut().write_protect_vmo(mm, vmo));
    }
    if changed > 0 {
        machine.shootdown();
        machine.flush_tlb();
    }
}

/// `None`: `n` is not one of these calls.
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
pub fn proc_call<M: PhysMem, X: Machine>(
    n: usize,
    a: [u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Option<Reply> {
    let PCtx {
        objs,
        procs,
        me,
        mm,
        vmos,
        jobs,
        machine,
    } = c;
    let me = *me;
    let r: Reply = match n {
        nr::PROCESS_MAP => (|| {
            let (ph, vh) = (Handle(a[0]), Handle(a[1]));
            let (vaddr, offset, length, flags) = (a[2], a[3], a[4], a[5]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (tpid, vid, max_perms) =
                resolve_map_args(objs, caller, ph, vh, flags).map_err(e)?;
            let caller_job = caller.job;
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            let aligned = |x: u64| x.is_multiple_of(4096);
            // ANYWHERE (MM7): the flag asks the kernel to choose a free place (`vaddr` must be 0), at random among those that fit in the
            // mapping arena. Every other argument is checked first (with the arena's start standing in for the address), so a refused
            // call costs no entropy.
            // (HD5: the reserved bits are named and refused, before any other argument is looked at beyond the handles)
            if flags & (abi::map_flags::SHADOW_STACK | abi::map_flags::RESERVED_9) != 0 {
                return bad(Status::NotSupported);
            }
            let anywhere = flags & abi::map_flags::ANYWHERE != 0;
            let flags = flags & !abi::map_flags::ANYWHERE;
            if anywhere && vaddr != 0 {
                return bad(Status::InvalidArgs);
            }
            let probe = if anywhere { abi::user::BASE } else { vaddr };
            let in_user = probe >= abi::user::BASE
                && probe
                    .checked_add(length)
                    .is_some_and(|end| end <= abi::user::TOP);
            let vmo_pages = vmos.pages(vid).ok_or(e(Status::BadHandle))?;
            let inside_vmo = offset
                .checked_add(length)
                .is_some_and(|end| end <= vmo_pages * 4096);
            if !aligned(probe)
                || !aligned(offset)
                || !aligned(length)
                || length == 0
                || flags & !(PF_R | PF_W | PF_X) != 0
                || (flags & PF_W != 0 && flags & PF_X != 0)
                || !in_user
                || !inside_vmo
            {
                return bad(Status::InvalidArgs);
            }
            let vaddr = if anywhere {
                match place_anywhere(&mut **machine, target.aspace.held_ref(), length) {
                    Some(at) => at,
                    None => return bad(Status::NoMemory),
                }
            } else {
                vaddr
            };
            // A VMO charged to a job that is neither the caller's nor the address space owner's must already be committed over the
            // range (slice V, INVARIANTS V1): its pages are paid for, so a touch can neither fail nor charge a stranger, and a
            // process cannot be killed by a VMO it was handed. A parent mapping its own VMO, or one charged to the child's job,
            // into a child stays lazy.
            let vmo_job = vmos.job_of(vid);
            if vmo_job != Some(caller_job)
                && vmo_job != Some(target.job)
                && !vmos.is_committed(vid, offset / 4096, length / 4096)
            {
                return bad(Status::BadState);
            }
            let perms = Perms {
                read: true,
                write: flags & PF_W != 0,
                exec: flags & PF_X != 0,
                user: true,
            };
            // Page tables for this region, worst case: one leaf table per 512 pages it spans (plus one for each end not aligned to
            // them) and two interior ones.
            let tables = length / 4096 / 512 + 4;
            jobs.charge(target.job, Resource::Pages, tables)
                .map_err(|x| e(crate::job_status(x)))?;
            if let Err(x) = target.aspace.held_mut().map_region_max(
                vmos,
                vaddr,
                length / 4096,
                vid,
                offset / 4096,
                perms,
                max_perms,
            ) {
                jobs.credit(target.job, Resource::Pages, tables);
                return Err(match x {
                    kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                });
            }
            // (a key that is already there is a mapping that was split and whose start has been mapped again: its charge stays, and this one adds)
            *target.table_charge.entry(vaddr).or_default() += tables;
            let end = vaddr + length;
            let extent = target.charge_end.entry(vaddr).or_insert(end);
            *extent = (*extent).max(end);
            // a mapping that touches one of the same VMO with the same permissions becomes part of it (fewer regions)
            target
                .aspace
                .held_mut()
                .merge_near(mm, vmos, jobs, vaddr, vaddr + length);
            Ok((anywhere.then_some(vaddr), None))
        })(),
        nr::VMO_DECOMMIT | nr::VMO_SET_SIZE => (|| {
            // Both take pages away from a VMO, so both first unmap them from every address space that maps them, then have every hart drop
            // its translations, and only then let the VMO give the frames back (TLB2).
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let id = ipc::vmo_of(objs, &caller.handles, Handle(a[0]), Rights::WRITE).map_err(e)?;
            let size = vmos.pages(id).ok_or(e(Status::BadHandle))?;
            let (first, count, new_size) = if n == nr::VMO_DECOMMIT {
                if vmos.kind(id) != Some(kcore::aspace::VmoKind::Plain) {
                    return Err(e(Status::NotSupported));
                }
                let (offset, len) = (a[1], a[2]);
                if !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) || len == 0 {
                    return Err(e(Status::InvalidArgs));
                }
                let first = offset / 4096;
                let count = len / 4096;
                if first.checked_add(count).is_none_or(|end| end > size) {
                    return Err(e(Status::InvalidArgs));
                }
                (first, count, None)
            } else {
                // (a VMO that is not resizable, which every device and DMA VMO is, is refused before the size is looked at)
                if !vmos.is_resizable(id) {
                    return Err(e(Status::AccessDenied));
                }
                let bytes = a[1];
                if bytes == 0
                    || !bytes.is_multiple_of(4096)
                    || bytes / 4096 > abi::limits::MAX_VMO_PAGES
                {
                    return Err(e(Status::InvalidArgs));
                }
                let new = bytes / 4096;
                // growing takes nothing away; shrinking takes the tail
                (new.min(size), size.saturating_sub(new), Some(new))
            };
            // (a VMO with children cannot change size: refused before anything is unmapped)
            if new_size.is_some() && vmos.has_children(id) {
                return Err(e(Status::BadState));
            }
            if count > 0 {
                // V1 (INVARIANTS): a process of another job than the VMO's holds its mapping on the promise that touching it never fails on the
                // VMO's job and never ends it; taking pages from under such a mapping would break that (a touch would charge the VMO's job, or
                // fault past a new end). The holder's own job may take pages back; a stranger's mapping stops the call.
                let vmo_job = vmos.job_of(id).ok_or(e(Status::BadHandle))?;
                if procs.values().any(|p| {
                    p.job != vmo_job && p.aspace.held_ref().maps_vmo_range(id, first, count)
                }) {
                    return Err(e(Status::BadState));
                }
                // (with the VMO's descendants: a PRIVATE child shows the pages of its source through its own mappings)
                let mut cleared = 0u64;
                vmos.for_each_below(id, 0, &mut |d, base| {
                    if let Some(f) = first
                        .checked_sub(base)
                        .or((first + count > base).then_some(0))
                    {
                        let n = (first + count).saturating_sub(base).saturating_sub(f);
                        for p in procs.values_mut() {
                            cleared = cleared
                                .saturating_add(p.aspace.held_mut().unmap_vmo_pages(mm, d, f, n));
                        }
                    }
                });
                // other harts may still cache the translations of what was just unmapped: before any frame is freed. (When nothing was
                // mapped there is nothing to drop: every earlier removal of a translation was shot down when it was made.)
                if cleared > 0 {
                    if !machine.skip_shootdown(n) {
                        machine.shootdown();
                    }
                    machine.flush_tlb();
                }
            }
            let r = match new_size {
                None => vmos.decommit(mm, jobs, id, first, count).map(|_| ()),
                Some(new) => vmos.set_size(mm, jobs, id, new),
            };
            match r {
                Ok(()) => Ok((None, None)),
                Err(kcore::aspace::VmoError::Gone) => Err(e(Status::BadHandle)),
                Err(kcore::aspace::VmoError::NotResizable) => Err(e(Status::AccessDenied)),
                Err(kcore::aspace::VmoError::HasChildren) => Err(e(Status::BadState)),
                Err(kcore::aspace::VmoError::NoMemory) => Err(e(Status::NoMemory)),
                Err(_) => Err(e(Status::InvalidArgs)),
            }
        })(),
        nr::VMO_CREATE_CHILD => (|| {
            use kcore::aspace::{ChildMode, VmoError};
            let (offset, bytes, job_handle, flags) = (a[1], a[2], a[3], a[4]);
            let mode = match flags {
                abi::child_flags::SNAPSHOT => ChildMode::Snapshot,
                abi::child_flags::PRIVATE => ChildMode::Private,
                _ => return bad(Status::InvalidArgs),
            };
            if !offset.is_multiple_of(4096) || !bytes.is_multiple_of(4096) || bytes == 0 {
                return bad(Status::InvalidArgs);
            }
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let id = ipc::vmo_of(objs, &caller.handles, Handle(a[0]), Rights::READ).map_err(e)?;
            let caller_job = caller.job;
            let job = if job_handle == 0 {
                caller_job
            } else {
                ipc::job_of(objs, &caller.handles, Handle(job_handle), Rights::MANAGE).map_err(e)?
            };
            if mode == ChildMode::Snapshot {
                // the pages never change payer (COW2)
                let vmo_job = vmos.job_of(id).ok_or(e(Status::BadHandle))?;
                if vmo_job != job {
                    return bad(Status::AccessDenied);
                }
                // V1: a process of another job maps the source on the promise that a write never fails on the VMO's job; a SNAPSHOT
                // would make its next write a copy charged to that job
                if procs.values().any(|p| {
                    p.job != vmo_job && p.aspace.held_ref().maps_vmo_range(id, 0, u64::MAX)
                }) {
                    return bad(Status::BadState);
                }
            }
            // everything that can fail comes before the source changes: the handle's room, the quota, the bookkeeping
            vmos.reserve_n(2).map_err(e)?;
            let child = match vmos.child_prepare(jobs, id, offset / 4096, bytes / 4096, job, mode) {
                Ok(c) => c,
                Err(VmoError::Gone) => return bad(Status::BadHandle),
                Err(VmoError::JobMismatch) => return bad(Status::AccessDenied),
                Err(VmoError::TooDeep) => return bad(Status::BadState),
                Err(VmoError::NotCopyable) => return bad(Status::NotSupported),
                Err(VmoError::NoMemory) => return bad(Status::NoMemory),
                Err(_) => return bad(Status::InvalidArgs),
            };
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            let handle = match ipc::vmo_object_create_in(
                objs,
                &mut caller.handles,
                child,
                &mut ipc::Quota {
                    jobs,
                    job: Some(caller_job),
                },
            ) {
                Ok(h) => h,
                Err(x) => {
                    let _ = vmos.release(mm, jobs, child);
                    return Err(e(x));
                }
            };
            vmos.child_attach(jobs, child, id, offset / 4096, bytes / 4096, mode);
            if mode == ChildMode::Snapshot {
                snapshot_protect(procs, mm, &mut **machine, id);
            }
            Ok((Some(handle.0), None))
        })(),
        nr::PROCESS_CLONE_REGION => (|| {
            use kcore::aspace::{ChildMode, VmoError};
            let (vaddr, mode) = (a[2], a[3]);
            if (mode != abi::clone_mode::SHARE && mode != abi::clone_mode::SNAPSHOT)
                || !vaddr.is_multiple_of(4096)
            {
                return bad(Status::InvalidArgs);
            }
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, spid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE).map_err(e)?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[1]), Rights::MANAGE).map_err(e)?;
            let caller_job = caller.job;
            let source = procs.get(&spid).ok_or(e(Status::BadState))?;
            let target = procs.get(&tpid).ok_or(e(Status::BadState))?;
            if source.exit_code.is_some()
                || source.killed.is_some()
                || target.exit_code.is_some()
                || target.killed.is_some()
            {
                return bad(Status::BadState);
            }
            let region = source
                .aspace
                .held_ref()
                .region_at_or_after(vaddr)
                .filter(|r| r.start == vaddr)
                .ok_or(e(Status::InvalidArgs))?;
            let length = region.pages * 4096;
            let end = vaddr.checked_add(length).ok_or(e(Status::InvalidArgs))?;
            if target.aspace.held_ref().has_region_in(vaddr, end) {
                return bad(Status::InvalidArgs);
            }
            // (the mapping below cannot fail for want of a region slot)
            if target.aspace.held_ref().region_count() >= abi::limits::MAX_REGIONS {
                return bad(Status::NoMemory);
            }
            let target_job = target.job;
            let vid = region.vmo;
            let vmo_job = vmos.job_of(vid).ok_or(e(Status::BadHandle))?;
            let snapshot = mode == abi::clone_mode::SNAPSHOT;
            if snapshot {
                // a fork child runs in its parent's job: the copy is charged to the source's job, which must be the target's
                if vmo_job != target_job {
                    return bad(Status::AccessDenied);
                }
                // (V1, as `vmo_create_child`)
                if procs.values().any(|p| {
                    p.job != vmo_job && p.aspace.held_ref().maps_vmo_range(vid, 0, u64::MAX)
                }) {
                    return bad(Status::BadState);
                }
            } else if vmo_job != caller_job
                && vmo_job != target_job
                && !vmos.is_committed(vid, region.vmo_page, region.pages)
            {
                // (V1, as `process_map`: a stranger's VMO must already be paid for)
                return bad(Status::BadState);
            }
            let tables = region.pages / 512 + 4;
            jobs.charge(target_job, Resource::Pages, tables)
                .map_err(|x| e(crate::job_status(x)))?;
            let undo = |jobs: &mut Jobs| jobs.credit(target_job, Resource::Pages, tables);
            let (map_vmo, map_page, child) = if snapshot {
                if let Err(x) = vmos.reserve_n(2) {
                    undo(jobs);
                    return bad(x);
                }
                match vmos.child_prepare(
                    jobs,
                    vid,
                    region.vmo_page,
                    region.pages,
                    vmo_job,
                    ChildMode::Snapshot,
                ) {
                    Ok(c) => (c, 0, Some(c)),
                    Err(x) => {
                        undo(jobs);
                        return bad(match x {
                            VmoError::TooDeep => Status::BadState,
                            VmoError::JobMismatch => Status::AccessDenied,
                            VmoError::Gone => Status::BadHandle,
                            VmoError::NotCopyable => Status::NotSupported,
                            VmoError::NoMemory => Status::NoMemory,
                            _ => Status::InvalidArgs,
                        });
                    }
                }
            } else {
                (vid, region.vmo_page, None)
            };
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if let Err(x) = target.aspace.held_mut().map_region_max(
                vmos,
                vaddr,
                region.pages,
                map_vmo,
                map_page,
                region.perms,
                region.max_perms,
            ) {
                if let Some(c) = child {
                    let _ = vmos.release(mm, jobs, c);
                }
                undo(jobs);
                return Err(match x {
                    kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                });
            }
            *target.table_charge.entry(vaddr).or_default() += tables;
            let extent = target.charge_end.entry(vaddr).or_insert(end);
            *extent = (*extent).max(end);
            target
                .aspace
                .held_mut()
                .merge_near(mm, vmos, jobs, vaddr, end);
            if let Some(c) = child {
                // the child is part of the family only now (nothing could fail since `child_prepare`); the region holds its own reference
                vmos.child_attach(
                    jobs,
                    c,
                    vid,
                    region.vmo_page,
                    region.pages,
                    ChildMode::Snapshot,
                );
                let _ = vmos.release(mm, jobs, c);
                snapshot_protect(procs, mm, &mut **machine, vid);
            }
            Ok((None, None))
        })(),
        nr::PROCESS_UNMAP => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE).map_err(e)?;
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            // length 0: the region that starts at `vaddr`, whole (the original call). Otherwise (MM7) every page of `[vaddr, vaddr + length)`:
            // holes are fine, regions are split at the edges.
            if a[2] == 0 {
                target
                    .aspace
                    .held_mut()
                    .unmap_region(mm, vmos, jobs, a[1])
                    .map_err(|_| e(Status::InvalidArgs))?;
            } else {
                if !in_user_range(a[1], a[2]) {
                    return bad(Status::InvalidArgs);
                }
                target
                    .aspace
                    .held_mut()
                    .unmap_range(mm, vmos, jobs, a[1], a[2])
                    .map_err(|x| match x {
                        kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                        _ => e(Status::InvalidArgs),
                    })?;
            }
            // other harts may still cache the translations of what was just unmapped
            machine.shootdown();
            // the page-table charge of a `process_map` is credited when no region is left in the span it covered (a mapping that was split
            // keeps the charge for what stays: its tables are still there)
            let gone: alloc::vec::Vec<u64> = target
                .charge_end
                .iter()
                .filter(|&(&k, &end)| !target.aspace.held_ref().has_region_in(k, end))
                .map(|(&k, _)| k)
                .collect();
            for k in gone {
                target.charge_end.remove(&k);
                if let Some(tables) = target.table_charge.remove(&k) {
                    jobs.credit(target.job, Resource::Pages, tables);
                }
            }
            machine.flush_tlb();
            Ok((None, None))
        })(),
        nr::PROCESS_PROTECT => (|| {
            let (vaddr, length, flags) = (a[1], a[2], a[3]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE).map_err(e)?;
            if flags & !(PF_R | PF_W | PF_X) != 0
                || flags & PF_W != 0 && flags & PF_X != 0
                || !in_user_range(vaddr, length)
            {
                return bad(Status::InvalidArgs);
            }
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            let perms = Perms {
                read: true,
                write: flags & PF_W != 0,
                exec: flags & PF_X != 0,
                user: true,
            };
            let stats = target
                .aspace
                .held_mut()
                .protect_range(mm, vmos, jobs, vaddr, length, perms)
                .map_err(|x| match x {
                    kcore::aspace::ProtectError::Denied => e(Status::AccessDenied),
                    kcore::aspace::ProtectError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                })?;
            // a permission taken away may still be cached by every hart; code made visible must reach every instruction fetch
            if stats.downgraded {
                if !machine.skip_shootdown(n) {
                    machine.shootdown();
                }
                machine.flush_tlb();
            }
            if stats.exec_pages > 0 {
                machine.icache_sync(stats.exec_pages);
            }
            Ok((None, None))
        })(),
        nr::PROCESS_MAP_INFO => (|| {
            let (vaddr, buf) = (a[1], a[2]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            let target = procs.get(&tpid).ok_or(e(Status::BadState))?;
            let bits =
                |p: Perms| u64::from(p.read) | u64::from(p.write) << 1 | u64::from(p.exec) << 2;
            // the region at or after `vaddr`; none: a zeroed record and 0
            let (record, next) = match target.aspace.held_ref().region_at_or_after(vaddr) {
                Some(r) => {
                    let len = r.pages * 4096;
                    let next = target
                        .aspace
                        .held_ref()
                        .region_at_or_after(r.start + len)
                        .map_or(0, |n| n.start);
                    (
                        [
                            r.start,
                            len,
                            bits(r.perms),
                            bits(r.max_perms),
                            r.vmo,
                            r.vmo_page * 4096,
                        ],
                        next,
                    )
                }
                None => ([0; 6], 0),
            };
            let mut words = [[0u8; 8]; 6];
            for (out, w) in words.iter_mut().zip(record.iter()) {
                *out = w.to_le_bytes();
            }
            let bytes: [u8; 48] = words.concat().try_into().unwrap_or([0; 48]);
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            kcore::usercopy::copy_to_user(caller.aspace.held_mut(), mm, vmos, jobs, buf, &bytes)
                .map_err(|_| e(Status::InvalidArgs))?;
            Ok((Some(next), None))
        })(),
        // `process_info(process, which)`: 0 the exit code of an ended process (BAD_STATE while it runs), 1 the CPU nanoseconds its threads have
        // run, ended and live (needs INSPECT, works while it runs; AB6)
        nr::PROCESS_INFO if a[1] == 1 => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (pid, terminated, ended) =
                ipc::process_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)
                    .map_err(e)?;
            let live = if terminated {
                0
            } else {
                procs.get(&pid).map_or(0, |p| {
                    p.live_tids
                        .iter()
                        .fold(0u64, |s, &t| s.saturating_add(machine.thread_cpu_ns(t)))
                })
            };
            Ok((Some(ended.saturating_add(live)), None))
        })(),
        nr::PROCESS_INFO if a[1] != 0 => fail(Status::InvalidArgs),
        nr::PROCESS_INFO => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, _, terminated, code) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            if !terminated {
                return bad(Status::BadState);
            }
            Ok((Some(code as i64 as u64), None))
        })(),
        // `thread_info(thread, which)`: 0 the exit code of an ended thread, 1 its CPU nanoseconds (works while it runs; AB6)
        // (handle 0 names the calling thread, as in `thread_set_sched`: it needs no right)
        nr::THREAD_INFO if a[1] == 1 && a[0] == 0 => {
            Ok((Some(machine.current_thread_cpu_ns()), None))
        }
        nr::THREAD_INFO if a[1] == 1 => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (obj, pid, terminated, final_ns) =
                ipc::thread_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)
                    .map_err(e)?;
            if terminated {
                return Ok((Some(final_ns), None));
            }
            let tid = procs.get(&pid).and_then(|p| {
                p.thread_objs
                    .iter()
                    .find(|(_, &o)| o == obj)
                    .map(|(&t, _)| t)
            });
            Ok((Some(tid.map_or(0, |t| machine.thread_cpu_ns(t))), None))
        })(),
        nr::THREAD_INFO if a[1] != 0 => fail(Status::InvalidArgs),
        nr::THREAD_INFO => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (terminated, code) =
                ipc::thread_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            if !terminated {
                return bad(Status::BadState);
            }
            Ok((Some(code as i64 as u64), None))
        })(),
        nr::RESOURCE_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            match ipc::resource_object_create_in(
                objs,
                &mut caller.handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => Err(e(x)),
            }
        })(),
        nr::DEBUG_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            match ipc::debug_object_create_in(
                objs,
                &mut caller.handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => Err(e(x)),
            }
        })(),
        nr::JOB_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            jobs.retain(JobId::ROOT);
            match ipc::job_object_create(
                objs,
                &mut caller.handles,
                JobId::ROOT,
                Rights::JOB,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => {
                    jobs.release(JobId::ROOT);
                    Err(e(x))
                }
            }
        })(),
        nr::PROCESS_SELF => (|| {
            let p = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            let rights =
                Rights(Rights::MANAGE.0 | Rights::INSPECT.0 | Rights::WAIT.0 | Rights::DUPLICATE.0);
            if p.handles.room() < 1 || p.handles.reserve(1).is_err() {
                return bad(Status::NoMemory);
            }
            // A process its parent created has a kill-on-close object (the parent's handle to it); the child's own view must not be
            // another reference to *that* object, or the parent closing its handle would never destroy it and the child would not
            // be killed (review 2026-11-14). Such a child gets a separate view object; the view is only ever held by the child.
            let own = p.obj.filter(|&o| objs.is_alive(o));
            let kill_on_close = matches!(own.and_then(|o| objs.get(o)), Some(ipc::KObj::Process(po)) if po.kill_on_close);
            match own.filter(|_| !kill_on_close) {
                Some(obj) => {
                    // another view of the same process object
                    objs.retain(obj);
                    let h = p
                        .handles
                        .insert(objs, obj, rights)
                        .expect("room was checked");
                    Ok((Some(h.0), None))
                }
                None => {
                    let job = p.job;
                    let (h, obj) = ipc::process_object_create_in(
                        objs,
                        &mut p.handles,
                        me,
                        false,
                        &mut ipc::Quota {
                            jobs,
                            job: Some(job),
                        },
                    )
                    .map_err(e)?;
                    // narrow the default rights: a self handle cannot be sent away (a table whose last free slot is on its last
                    // generation refuses the replace: give the new object back)
                    let Ok(h) = p.handles.replace(objs, h, rights) else {
                        let _ = p.handles.close(objs, h);
                        return bad(Status::NoMemory);
                    };
                    if own.is_none() {
                        p.obj = Some(obj);
                    }
                    Ok((Some(h.0), None))
                }
            }
        })(),
        _ => return None,
    };
    cow_sync(procs, mm, vmos, &mut **machine);
    Some(r)
}
