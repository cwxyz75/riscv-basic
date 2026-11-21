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
    pub aspace: AddressSpace,
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
fn resolve_map_args(
    objs: &Kobjs,
    caller: &Process,
    ph: Handle,
    vh: Handle,
    flags: u64,
) -> Result<(u64, u64), Status> {
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
    Ok((tpid, vid))
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
            let (tpid, vid) = resolve_map_args(objs, caller, ph, vh, flags).map_err(e)?;
            let caller_job = caller.job;
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            let aligned = |x: u64| x.is_multiple_of(4096);
            let in_user = vaddr >= abi::user::BASE
                && vaddr
                    .checked_add(length)
                    .is_some_and(|end| end <= abi::user::TOP);
            let vmo_pages = vmos.pages(vid).ok_or(e(Status::BadHandle))?;
            let inside_vmo = offset
                .checked_add(length)
                .is_some_and(|end| end <= vmo_pages * 4096);
            if !aligned(vaddr)
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
            if let Err(x) =
                target
                    .aspace
                    .map_region(vmos, vaddr, length / 4096, vid, offset / 4096, perms)
            {
                jobs.credit(target.job, Resource::Pages, tables);
                return Err(match x {
                    kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                });
            }
            target.table_charge.insert(vaddr, tables);
            Ok((None, None))
        })(),
        nr::PROCESS_UNMAP => (|| {
            let ph = Handle(a[0]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, ph, Rights::MANAGE).map_err(e)?;
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            target
                .aspace
                .unmap_region(mm, vmos, jobs, a[1])
                .map_err(|_| e(Status::InvalidArgs))?;
            // other harts may still cache the translations of what was just unmapped
            machine.shootdown();
            if let Some(tables) = target.table_charge.remove(&a[1]) {
                jobs.credit(target.job, Resource::Pages, tables);
            }
            machine.flush_tlb();
            Ok((None, None))
        })(),
        nr::PROCESS_INFO => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, _, terminated, code) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            if !terminated {
                return bad(Status::BadState);
            }
            Ok((Some(code as i64 as u64), None))
        })(),
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
    Some(r)
}
