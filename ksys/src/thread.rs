//! Thread creation (TS3c): `thread_create` over the user-mode state, with what the machine decides (the creating thread's priority) passed in
//! and what the machine does (spawning the kernel thread) left to the caller, which gets back a [`Spawn`].

use crate::UserState;
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use kcore::ipc::{self, Quota};
use kcore::job::{JobError, JobId, Jobs, Resource};
use kcore::object::ObjId;

/// Frames in a kernel stack.
pub const KSTACK_PAGES: u64 = abi::layout::KSTACK_BYTES / 4096;

/// What the caller does after `thread_create`: spawn the kernel thread `token` names, at priority `prio`, and return `handle`.
#[derive(Debug, PartialEq, Eq)]
pub struct Spawn {
    pub token: usize,
    pub handle: u32,
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
    pid: u32,
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

/// `thread_create(process, entry, sp, arg) -> handle` (SYSCALLS.md). `me` is the calling process, `creator_prio` the base priority the new
/// thread inherits from the creating thread (the default 10 if that is not a user thread).
#[allow(clippy::expect_used, clippy::arithmetic_side_effects)] // INVARIANT: the target process was looked up a few lines above under the same lock; threads_alive is below MAX_THREADS_PER_PROCESS (checked first)
pub fn thread_create(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u32,
    creator_prio: u8,
    a: [u64; 6],
) -> Result<Spawn, Status> {
    let ph = Handle(a[0] as u32);
    let (entry, sp, arg) = (a[1], a[2], a[3]);
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(&us.objs, &caller.handles, ph, Rights::MANAGE)?;
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    if target.state != crate::ProcState::Running
        || target.exit_code.is_some()
        || target.killed.is_some()
    {
        return Err(Status::BadState);
    }
    let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
    let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
    if !entry_ok || !sp_ok {
        return Err(Status::InvalidArgs);
    }
    if target.threads_alive >= abi::limits::MAX_THREADS_PER_PROCESS
        || total_threads(us) >= abi::limits::MAX_USER_THREADS
    {
        return Err(Status::NoMemory);
    }
    let tjob = target.job;
    // a new thread inherits its creator's base priority, clamped to its job's maximum
    let prio = clamp_prio(jobs, tjob, creator_prio);
    charge_thread(jobs, tjob).map_err(crate::job_status)?;
    let UserState { objs, procs, .. } = us;
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    let job = caller.job;
    let (handle, obj) = match ipc::thread_object_create_in(
        objs,
        &mut caller.handles,
        tpid,
        &mut Quota {
            jobs,
            job: Some(job),
        },
    ) {
        Ok(x) => x,
        Err(e) => {
            credit_thread(jobs, tjob);
            return Err(e);
        }
    };
    procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
    let token = new_start(us, tpid, entry, sp, arg, Some(obj));
    Ok(Spawn {
        token,
        handle: handle.0,
        prio,
    })
}
