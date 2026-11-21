//! Exceptions (EX2, KERNEL-V1-SPEC 3.6): a thread's fault offered to a handler on a channel, level by level (the process's own channel, then its
//! job's, then each ancestor's), and what the handler does about it. Pure: the kernel stops the thread and wakes it; this decides where the
//! exception goes and who resolves it, exactly once.

use crate::{Process, UserState};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use kcore::ipc::{self, ExcLevel, ExceptionObj, KObj, Quota};
use kcore::job::{JobId, Jobs};
use kcore::object::ObjId;
use kcore::sched::Tid;

/// What a thread took: the `abi::arch::fault` kind, the pc, the address and the architecture's raw cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub kind: u32,
    pub pc: u64,
    pub address: u64,
    pub cause: u64,
}

/// What `exception_resume` (or the close of the last exception handle) decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// RETRY: the thread goes on; `wake` says it is parked and must be woken.
    Retry { tid: Tid, wake: bool },
    /// NEXT: the next level took it; the thread stays stopped.
    Passed,
    /// KILL, or NEXT with no level left: the process ends with `code`.
    Kill { pid: u64, code: i32 },
}

/// The level after `l`: a process's channel is followed by its job's, a job's by its parent's.
pub fn next_level(us: &UserState, jobs: &Jobs, pid: u64, l: ExcLevel) -> Option<ExcLevel> {
    match l {
        ExcLevel::Process => us.procs.get(&pid).map(|p| ExcLevel::Job(p.job)),
        ExcLevel::Job(j) => jobs.parent(j).map(ExcLevel::Job),
    }
}

/// Offer thread `tid` of `pid`'s fault to the first level at or after `from` (`None`: the process's own) that has a channel that takes it. True when
/// one did: the exception object and the thread handle are in a message there, and the caller stops the thread. False: nobody wants it, and
/// the process ends as it always did.
pub fn deliver(
    us: &mut UserState,
    jobs: &mut Jobs,
    pid: u64,
    tid: Tid,
    f: &Fault,
    from: Option<ExcLevel>,
) -> bool {
    let mut level = match from {
        None => Some(ExcLevel::Process),
        Some(l) => next_level(us, jobs, pid, l),
    };
    while let Some(l) = level {
        if offer(us, jobs, pid, tid, f, l) {
            return true;
        }
        level = next_level(us, jobs, pid, l);
    }
    false
}

/// Offer it at one level. Nothing is left behind when the level does not take it.
fn offer(us: &mut UserState, jobs: &mut Jobs, pid: u64, tid: Tid, f: &Fault, l: ExcLevel) -> bool {
    let chan = match l {
        ExcLevel::Process => us.procs.get(&pid).and_then(|p| p.exc_channel),
        ExcLevel::Job(j) => us.job_exc.get(&j).copied(),
    };
    let Some(chan) = chan else {
        return false;
    };
    let Some(job) = us.procs.get(&pid).map(|p| p.job) else {
        return false;
    };
    let UserState {
        objs,
        ktable,
        procs,
        ..
    } = us;
    if ipc::channel_peer_closed(objs, ktable, chan) {
        return false; // (nobody reads it any more)
    }
    let mut q = Quota {
        jobs,
        job: Some(job),
    };
    // the thread: its object if it has one, else one made now (a thread started by `process_start` has none)
    let existing = procs
        .get(&pid)
        .and_then(|p| p.thread_objs.get(&tid).copied());
    let (th, tobj) = match existing {
        Some(obj) => match ktable.insert(objs, obj, Rights::THREAD) {
            Ok(h) => (h, obj),
            Err(_) => return false,
        },
        None => match ipc::thread_object_create_in(objs, ktable, pid, &mut q) {
            Ok(x) => x,
            Err(_) => return false,
        },
    };
    let made_obj = existing.is_none();
    let e = ExceptionObj {
        pid,
        tid,
        kind: f.kind,
        pc: f.pc,
        address: f.address,
        cause: f.cause,
        level: l,
        resolved: false,
    };
    let eh = match ipc::exception_object_create_in(objs, ktable, e, &mut q) {
        Ok((h, _)) => h,
        Err(_) => {
            let _ = ktable.close(objs, th);
            return false;
        }
    };
    // five little-endian words
    let mut msg = alloc::vec::Vec::new();
    for w in [f.kind as u64, tid, f.pc, f.address, f.cause] {
        msg.extend_from_slice(&w.to_le_bytes());
    }
    match ipc::channel_write_in(objs, ktable, chan, &msg, &[th, eh], q.jobs) {
        Ok(()) => {
            if made_obj {
                if let Some(p) = procs.get_mut(&pid) {
                    p.thread_objs.insert(tid, tobj);
                }
            }
            true
        }
        Err(_) => {
            // (the handles are still ours: they and their objects go)
            let _ = ktable.close(objs, th);
            let _ = ktable.close(objs, eh);
            false
        }
    }
}

/// The thread of an exception is stopped (its suspend count goes up and SUSPENDED is raised when it parks): the faulting thread calls this
/// after `deliver` took the exception.
pub fn stop_thread(us: &mut UserState, tid: Tid) {
    let _ = us.suspends.entry(tid).or_default().suspend(false);
}

/// Let the thread of a retried exception go: its suspend count falls; returns whether it is parked and to be woken.
fn release_thread(us: &mut UserState, pid: u64, tid: Tid) -> bool {
    let Some(s) = us.suspends.get_mut(&tid) else {
        return false;
    };
    match s.resume() {
        Some(kcore::suspend::Resumed::Running { clear, wake }) => {
            us.suspends.remove(&tid);
            if clear {
                if let Some(obj) = us
                    .procs
                    .get(&pid)
                    .and_then(|p| p.thread_objs.get(&tid).copied())
                {
                    ipc::thread_set_suspended(&mut us.objs, obj, false);
                }
            }
            wake
        }
        _ => false,
    }
}

fn alive(p: Option<&Process>) -> bool {
    p.is_some_and(|p| p.exit_code.is_none() && p.killed.is_none())
}

/// Act on a decision for exception `e` (not yet resolved, its process alive).
fn resolve(us: &mut UserState, jobs: &mut Jobs, e: &ExceptionObj, how: u64) -> Resolved {
    let fault = Fault {
        kind: e.kind,
        pc: e.pc,
        address: e.address,
        cause: e.cause,
    };
    let kill = Resolved::Kill {
        pid: e.pid,
        code: abi::arch::fault::exit_code(e.kind),
    };
    match how {
        abi::exception::RETRY => {
            let wake = release_thread(us, e.pid, e.tid);
            Resolved::Retry { tid: e.tid, wake }
        }
        abi::exception::NEXT => {
            if deliver(us, jobs, e.pid, e.tid, &fault, Some(e.level)) {
                Resolved::Passed
            } else {
                kill
            }
        }
        _ => kill,
    }
}

/// `exception_resume(exception, how)` (MANAGE on the exception): INVALID_ARGS for another `how`; BAD_STATE when it was resolved already or its
/// process has ended.
pub fn exception_resume(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u64,
    h: u64,
    how: u64,
) -> Result<Resolved, Status> {
    if !matches!(
        how,
        abi::exception::RETRY | abi::exception::NEXT | abi::exception::KILL
    ) {
        return Err(Status::InvalidArgs);
    }
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let (obj, e) = ipc::exception_of(&us.objs, &caller.handles, Handle(h), Rights::MANAGE)?;
    if e.resolved || !alive(us.procs.get(&e.pid).map(|p| &**p)) {
        return Err(Status::BadState);
    }
    ipc::exception_set_resolved(&mut us.objs, obj);
    Ok(resolve(us, jobs, &e, how))
}

/// The last handle to an exception was closed without a decision: it is NEXT. `None` when it was resolved already or its process is gone.
pub fn exception_dropped(
    us: &mut UserState,
    jobs: &mut Jobs,
    e: &ExceptionObj,
) -> Option<Resolved> {
    if e.resolved || !alive(us.procs.get(&e.pid).map(|p| &**p)) {
        return None;
    }
    Some(resolve(us, jobs, e, abi::exception::NEXT))
}

/// Which exception channel a call is about.
pub enum Target {
    Process(u64),
    Job(u64),
}

/// `process_exception_channel` / `job_exception_channel`: make the channel and give the caller its user end (a new handle, returned). A second
/// call while the first channel's user end is open is BAD_STATE; one whose user end has closed replaces it.
pub fn exception_channel(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u64,
    target: Target,
) -> Result<u64, Status> {
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let (existing, job, pid_or_job) = match target {
        Target::Process(h) => {
            let (_, tpid, terminated, _) =
                ipc::process_of(&us.objs, &caller.handles, Handle(h), Rights::MANAGE)?;
            let p = us.procs.get(&tpid).ok_or(Status::BadState)?;
            if terminated || p.exit_code.is_some() || p.killed.is_some() {
                return Err(Status::BadState);
            }
            (p.exc_channel, p.job, Ok(tpid))
        }
        Target::Job(h) => {
            let job = ipc::job_of(&us.objs, &caller.handles, Handle(h), Rights::MANAGE)?;
            (us.job_exc.get(&job).copied(), job, Err(job))
        }
    };
    if caller.handles.room() < 1 {
        return Err(Status::NoMemory);
    }
    if let Some(old) = existing {
        if !ipc::channel_peer_closed(&us.objs, &us.ktable, old) {
            return Err(Status::BadState);
        }
    }
    let UserState {
        objs,
        ktable,
        procs,
        job_exc,
        ..
    } = us;
    let (kernel_end, user_end) = ipc::channel_create_in(
        objs,
        ktable,
        &mut Quota {
            jobs,
            job: Some(job),
        },
    )?;
    let entry = ktable
        .remove_for_transfer(objs, user_end)
        .map_err(|_| Status::BadState)?;
    let handle = match procs.get_mut(&me).ok_or(Status::BadState)?.handles.insert(
        objs,
        entry.obj,
        entry.rights,
    ) {
        Ok(h) => h,
        Err(e) => {
            let _ = ktable.close(objs, kernel_end);
            return Err(e);
        }
    };
    // (the old channel's kernel end goes now that the new one is in place)
    if let Some(old) = existing {
        let _ = ktable.close(objs, old);
    }
    match pid_or_job {
        Ok(tpid) => {
            if let Some(p) = procs.get_mut(&tpid) {
                p.exc_channel = Some(kernel_end);
            }
        }
        Err(j) => {
            job_exc.insert(j, kernel_end);
        }
    }
    Ok(handle.0)
}

/// Is `obj` an exception object? (the kernel's cleanup of dead objects)
pub fn is_exception(us: &UserState, obj: ObjId) -> bool {
    matches!(us.objs.get(obj), Some(KObj::Exception(_)))
}

/// An ended thread's stop state and a stopped process's exceptions: nothing to keep once a thread is gone.
pub fn forget_thread(us: &mut UserState, tid: Tid) {
    us.suspends.remove(&tid);
}

/// The kernel ends of job exception channels whose user end has closed: closed, so the channel and the job it is charged to can go (the kernel
/// calls this when objects have been destroyed). A process's own channel goes with the process.
pub fn reap_closed_job_channels(us: &mut UserState) {
    if us.job_exc.is_empty() {
        return;
    }
    let UserState {
        objs,
        ktable,
        job_exc,
        ..
    } = us;
    let closed: alloc::vec::Vec<JobId> = job_exc
        .iter()
        .filter(|(_, h)| ipc::channel_peer_closed(objs, ktable, **h))
        .map(|(j, _)| *j)
        .collect();
    for j in closed {
        if let Some(h) = job_exc.remove(&j) {
            let _ = ktable.close(objs, h);
        }
    }
}

/// A job that is no longer there: its channel's kernel end closes.
pub fn forget_job(us: &mut UserState, job: JobId) {
    if let Some(h) = us.job_exc.remove(&job) {
        let UserState { objs, ktable, .. } = us;
        let _ = ktable.close(objs, h);
    }
}
