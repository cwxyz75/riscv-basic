//! Thread creation (TS3c, AB5): `thread_spawn` over the user-mode state, with what the machine decides (the creating thread's priority) passed in
//! and what the machine does (spawning the kernel thread) left to the caller, which gets back a [`Spawn`].

use crate::UserState;
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use kcore::aspace::Vmos;
use kcore::ipc::{self, Quota};
use kcore::job::{JobError, JobId, Jobs, Resource};
use kcore::object::ObjId;
use kcore::pagetable::{Perms, PhysMem};
use kcore::usercopy::copy_from_user;
use kcore::Mm;

/// Frames in a kernel stack.
pub const KSTACK_PAGES: u64 = abi::layout::KSTACK_BYTES / 4096;

/// What the caller does after `thread_spawn`: spawn the kernel thread `token` names, at priority `prio`, and return `handle`.
#[derive(Debug, PartialEq, Eq)]
pub struct Spawn {
    pub token: usize,
    pub handle: u64,
    pub prio: u8,
}

/// User threads alive in the whole system.
pub fn total_threads(us: &UserState) -> usize {
    us.procs.values().map(|p| p.threads_alive).sum()
}

/// What one more thread costs a job (slice T, T1): a thread slot, and the four pages of its kernel stack (billed to the job that owns the
/// thread, for as long as it lives). All or nothing.
pub fn charge_thread(jobs: &mut Jobs, job: JobId) -> Result<(), JobError> {
    jobs.charge(job, Resource::Threads, 1)?;
    if let Err(e) = jobs.charge(job, Resource::Pages, KSTACK_PAGES) {
        jobs.credit(job, Resource::Threads, 1);
        return Err(e);
    }
    Ok(())
}

pub fn credit_thread(jobs: &mut Jobs, job: JobId) {
    jobs.credit(job, Resource::Threads, 1);
    jobs.credit(job, Resource::Pages, KSTACK_PAGES);
}

/// The base priority a new thread starts at (SCHED-SMP-SPEC 4.9): `want`, clamped to the job's `max_prio` (and at least 1: a user thread is
/// never the idle priority).
pub fn clamp_prio(jobs: &Jobs, job: JobId, want: u8) -> u8 {
    let max = jobs.sched(job).map_or(10, |s| s.max_prio);
    want.min(max).max(1)
}

/// Register a thread start and return its token.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a u64 counter that gains one per thread start
pub fn new_start(
    us: &mut UserState,
    pid: u64,
    entry: u64,
    sp: u64,
    arg: u64,
    obj: Option<ObjId>,
) -> usize {
    let token = us.next_token;
    us.next_token += 1;
    us.starts.insert(
        token,
        crate::ThreadStart {
            pid,
            entry,
            sp,
            arg,
            obj,
            tp: 0,
            stack: None,
            rr: false,
            spawned: false,
        },
    );
    token
}

/// `thread_spawn(process, args) -> handle, tid` (SYSCALLS.md, SCHED-SMP-SPEC 5.3): create and start a thread, with a stack the kernel maps in
/// the process's stack arena (freed when the thread ends) unless the caller brings its own. `handle` is 0 for a detached thread. The caller
/// spawns the kernel thread `token` names at `prio` and returns the handle and the tid.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: stack_bytes and guard_bytes are at most MAX_STACK (checked into `sizes_ok`), so the sums are far below u64::MAX; the target process was looked up under the same lock; the record read is of fixed size (64 bytes) at indices below 8
#[inline(never)]
pub fn thread_spawn<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    me: u64,
    creator_prio: u8,
    a: [u64; 6],
) -> Result<Spawn, Status> {
    use abi::spawn as sp;
    let ph = Handle(a[0]);
    // the record first (a bad pointer is the first error), then the handle
    let raw = {
        let c = us.procs.get_mut(&me).ok_or(Status::BadState)?;
        copy_from_user(&mut c.aspace, mm, vmos, a[1], sp::ARGS_BYTES)?
    };
    let w = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
    let (entry, arg, tp, stack_bytes, guard_bytes) = (w(0), w(1), w(2), w(3), w(4));
    let prio_req = (w(5) & 0xffff_ffff) as u32;
    let flags = (w(5) >> 32) as u32;
    let (caller_sp, reserved) = (w(6), w(7));
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(&us.objs, &caller.handles, ph, Rights::MANAGE)?;
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    let tjob = target.job;
    // a priority above the job's maximum is refused (ACCESS_DENIED), not clamped: the program asked for it
    let max = vmos.jobs.sched(tjob).map_or(10, |j| j.max_prio);
    if (1..=63).contains(&prio_req) && prio_req as u8 > max {
        return Err(Status::AccessDenied);
    }
    if target.state != crate::ProcState::Running
        || target.exit_code.is_some()
        || target.killed.is_some()
    {
        return Err(Status::BadState);
    }
    let own_stack = flags & sp::CALLER_STACK != 0;
    let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
    let sizes_ok = stack_bytes <= sp::MAX_STACK && guard_bytes <= sp::MAX_STACK;
    let sp_ok = if own_stack {
        caller_sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&caller_sp)
    } else {
        caller_sp == 0
    };
    // (with a caller stack the sizes mean nothing: asking for some is a mistake)
    let sizes_ok = sizes_ok && (!own_stack || (stack_bytes == 0 && guard_bytes == 0));
    if !entry_ok
        || !sizes_ok
        || !sp_ok
        || reserved != 0
        || prio_req > 63
        || flags & !(sp::ROUND_ROBIN | sp::DETACHED | sp::CALLER_STACK) != 0
    {
        return Err(Status::InvalidArgs);
    }
    if target.threads_alive >= abi::limits::MAX_THREADS_PER_PROCESS
        || total_threads(us) >= abi::limits::MAX_USER_THREADS
    {
        return Err(Status::NoMemory);
    }
    // where the stack goes: a gap in the arena for the guard and the stack, at or after the hint, wrapping once
    let stack_len = if stack_bytes == 0 {
        sp::DEFAULT_STACK
    } else {
        stack_bytes.next_multiple_of(4096)
    };
    let guard_len = if guard_bytes == 0 {
        4096
    } else {
        guard_bytes.next_multiple_of(4096)
    };
    let place = if own_stack {
        None
    } else {
        let (lo, hi) = (abi::user::STACK_ARENA_BASE, abi::user::STACK_ARENA_END);
        // the stack's guard below it, and one more guard above it: the next stack up has a guard of its own below *its* start, so two stacks are
        // never flush (a neighbour's overflow always faults)
        let total = guard_len + stack_len + guard_len;
        let at = target
            .aspace
            .find_gap(lo, hi, total, target.stack_hint)
            .or_else(|| target.aspace.find_gap(lo, hi, total, lo));
        match at {
            Some(at) => Some(at + guard_len),
            None => return Err(Status::NoMemory),
        }
    };
    let prio = if prio_req == 0 {
        clamp_prio(&vmos.jobs, tjob, creator_prio)
    } else {
        prio_req as u8
    };
    let detached = flags & sp::DETACHED != 0;
    charge_thread(&mut vmos.jobs, tjob).map_err(crate::job_status)?;
    let UserState { objs, procs, .. } = us;
    // the thread's object and handle (not for a detached thread)
    let made = if detached {
        None
    } else {
        let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
        let job = caller.job;
        match ipc::thread_object_create_in(
            objs,
            &mut caller.handles,
            tpid,
            &mut Quota {
                jobs: &mut vmos.jobs,
                job: Some(job),
            },
        ) {
            Ok(x) => Some(x),
            Err(e) => {
                credit_thread(&mut vmos.jobs, tjob);
                return Err(e);
            }
        }
    };
    // the stack: a VMO of its own (nobody holds a handle), mapped read-write; its table pages are charged like any mapping's
    let mut stack = None;
    let mut top = caller_sp;
    if let Some(region) = place {
        let tables = stack_len / 4096 / 512 + 4;
        let undo =
            |made: Option<(Handle, ObjId)>,
             procs: &mut alloc::collections::BTreeMap<u64, alloc::boxed::Box<crate::Process>>,
             objs: &mut kcore::ipc::Kobjs,
             jobs: &mut Jobs| {
                if let Some((h, _)) = made {
                    if let Some(c) = procs.get_mut(&me) {
                        let _ = c.handles.close(objs, h);
                    }
                }
                credit_thread(jobs, tjob);
            };
        if let Err(e) = vmos.jobs.charge(tjob, Resource::Pages, tables) {
            undo(made, procs, objs, &mut vmos.jobs);
            return Err(crate::job_status(e));
        }
        if let Err(e) = vmos.reserve_one() {
            undo(made, procs, objs, &mut vmos.jobs);
            vmos.jobs.credit(tjob, Resource::Pages, tables);
            return Err(e);
        }
        let vmo = vmos.create_in(stack_len / 4096, tjob);
        let perms = Perms {
            read: true,
            write: true,
            exec: false,
            user: true,
        };
        let t = procs.get_mut(&tpid).expect("target exists");
        if let Err(e) = t
            .aspace
            .map_region(vmos, region, stack_len / 4096, vmo, 0, perms)
        {
            let _ = vmos.release(mm, vmo);
            vmos.jobs.credit(tjob, Resource::Pages, tables);
            undo(made, procs, objs, &mut vmos.jobs);
            return Err(match e {
                kcore::aspace::RegionError::TooMany => Status::NoMemory,
                _ => Status::InvalidArgs,
            });
        }
        t.table_charge.insert(region, tables);
        t.stack_hint = region + stack_len;
        stack = Some(crate::SpawnStack { region, vmo });
        top = region + stack_len;
    }
    procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
    let token = new_start(us, tpid, entry, top, arg, made.map(|(_, o)| o));
    let st = us.starts.get_mut(&token).expect("just registered");
    st.tp = tp;
    st.stack = stack;
    st.rr = flags & sp::ROUND_ROBIN != 0;
    st.spawned = true;
    Ok(Spawn {
        token,
        handle: made.map_or(0, |(h, _)| h.0),
        prio,
    })
}

/// `process_start(process, entry, sp, channel, arg)` (SYSCALLS.md): move the bootstrap channel end into a Created process as its handle 1 and register
/// its first thread. The caller spawns the kernel thread `token` names at `prio`. On any error nothing has changed.
#[allow(clippy::expect_used, clippy::panic)]
// INVARIANT: the caller and the target were looked up under the same lock a few lines above; the first thread's record exists; a Created process's table is empty, so the moved handle is handle 1
#[inline(never)]
pub fn process_start(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u64,
    a: [u64; 6],
) -> Result<(usize, u8), Status> {
    let (ph, bh) = (Handle(a[0]), Handle(a[3]));
    let (entry, sp, arg) = (a[1], a[2], a[4]);
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let pe = caller.handles.get(ph)?;
    let be = caller.handles.get(bh)?;
    let tpid = match us.objs.get(pe.obj) {
        Some(ipc::KObj::Process(p)) => p.pid,
        _ => return Err(Status::WrongType),
    };
    if !matches!(us.objs.get(be.obj), Some(ipc::KObj::Channel(_))) {
        return Err(Status::WrongType);
    }
    if !pe.rights.contains(Rights::MANAGE) || !be.rights.contains(Rights::TRANSFER) {
        return Err(Status::AccessDenied);
    }
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    if target.state != crate::ProcState::Created
        || target.exit_code.is_some()
        || target.killed.is_some()
    {
        return Err(Status::BadState);
    }
    if total_threads(us) >= abi::limits::MAX_USER_THREADS {
        return Err(Status::NoMemory);
    }
    let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
    let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
    if !entry_ok || !sp_ok {
        return Err(Status::InvalidArgs);
    }
    let tjob = target.job;
    // The thread's charge comes before anything changes: a refusal must leave the caller's handle with the caller and the process exactly as it
    // was (SYSCALLS.md: on any error nothing changes), or a retry would find handle 1 taken.
    charge_thread(jobs, tjob).map_err(crate::job_status)?;
    // (room in the target's table first, in memory: the handle is moved only when its arrival cannot fail)
    if let Err(e) = us
        .procs
        .get_mut(&tpid)
        .expect("target exists")
        .handles
        .reserve(1)
    {
        credit_thread(jobs, tjob);
        return Err(e);
    }
    // Commit: the channel end moves into the new process as its handle 1.
    let first_gen = us.first_handle_gen;
    let UserState { procs, objs, .. } = us;
    let caller = procs.get_mut(&me).expect("caller exists");
    let moved = match caller.handles.remove_for_transfer(objs, bh) {
        Ok(m) => m,
        Err(e) => {
            credit_thread(jobs, tjob);
            return Err(e);
        }
    };
    let target = procs.get_mut(&tpid).expect("target exists");
    let first = target
        .handles
        .insert(moved.obj, moved.rights)
        .expect("an empty table has room, and its memory was reserved");
    assert_eq!(
        first,
        abi::BOOTSTRAP_HANDLE,
        "the bootstrap channel must be handle 1"
    );
    // (AB3, test builds: the generations the later handles start at; the bootstrap handle above is value 1 by the ABI)
    target.handles.set_first_generation(first_gen);
    target.entry = entry;
    target.arg = arg;
    target.start_sp = sp;
    target.state = crate::ProcState::Running;
    target.threads_alive = 1;
    // the first thread of a process starts at the default priority, clamped to its job's maximum
    let prio = clamp_prio(jobs, tjob, 10);
    Ok((new_start(us, tpid, entry, sp, arg, None), prio))
}
