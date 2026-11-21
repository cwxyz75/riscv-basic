//! The decisions of the calls that end, stop or reschedule processes and threads (TS3b/c): which process, thread or job a handle
//! names and whether the caller may, and the state changes that belong to the tables. What the machine then does (waking threads,
//! kicking harts, tearing a process down, setting a priority) stays in the kernel; each function returns what it must do.

use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use kcore::ipc::{self, Kobjs};
use kcore::job::{JobId, Jobs, SchedError, SchedLimits};
use kcore::sched::Tid;

use crate::Process;

/// `process_kill`: the pid to end. A process that has ended (or is gone) is BAD_STATE.
pub fn kill_target(
    objs: &Kobjs,
    procs: &BTreeMap<u32, Box<Process>>,
    me: u32,
    ph: Handle,
) -> Result<u32, Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, terminated, _) = ipc::process_of(objs, &caller.handles, ph, Rights::KILL)?;
    if terminated || !procs.contains_key(&tpid) {
        return Err(Status::BadState);
    }
    Ok(tpid)
}

/// `process_suspend` / `process_resume`: set the stopped flag of the target (and of its object) and return its pid. An ended or
/// killed process is BAD_STATE. The caller bumps the attention epoch and, to stop, kicks the other harts.
pub fn stop_target(
    objs: &mut Kobjs,
    procs: &mut BTreeMap<u32, Box<Process>>,
    me: u32,
    ph: Handle,
    stop: bool,
) -> Result<u32, Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, terminated, _) = ipc::process_of(objs, &caller.handles, ph, Rights::KILL)?;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    if terminated || target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    target.stopped = stop;
    if let Some(obj) = target.obj {
        ipc::process_set_stopped(objs, obj, stop);
    }
    Ok(tpid)
}

/// `job_kill`: forbid new processes and threads in the job (their limits become 0) and name the processes inside it other than the
/// caller; the flag says whether the caller is inside too (and so ends last).
pub fn job_kill_victims(
    objs: &Kobjs,
    procs: &BTreeMap<u32, Box<Process>>,
    jobs: &mut Jobs,
    me: u32,
    jh: Handle,
) -> Result<(Vec<u32>, bool), Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let job = ipc::job_of(objs, &caller.handles, jh, Rights::KILL)?;
    let mut limits = jobs.limits(job).ok_or(Status::BadState)?;
    limits.processes = 0;
    limits.threads = 0;
    let _ = jobs.set_limits(job, limits);
    let inside: Vec<u32> = procs
        .iter()
        .filter(|(_, p)| jobs.is_within(p.job, job))
        .map(|(&pid, _)| pid)
        .collect();
    let me_inside = inside.contains(&me);
    Ok((inside.into_iter().filter(|&p| p != me).collect(), me_inside))
}

/// The kernel thread behind a thread handle with `need`, and the process it belongs to. Handle 0 is the calling thread.
pub fn thread_target(
    objs: &Kobjs,
    procs: &BTreeMap<u32, Box<Process>>,
    me: u32,
    current: Tid,
    h: u64,
    need: Rights,
) -> Result<(Tid, u32), Status> {
    if h == 0 {
        return Ok((current, me));
    }
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let handle = Handle(h);
    ipc::thread_of(objs, &caller.handles, handle, need)?;
    let obj = caller.handles.get(handle)?.obj;
    let pid = match objs.get(obj) {
        Some(ipc::KObj::Thread(t)) => t.pid,
        _ => return Err(Status::WrongType),
    };
    let tid = procs.get(&pid).and_then(|p| {
        p.thread_objs
            .iter()
            .find(|(_, o)| **o == obj)
            .map(|(t, _)| *t)
    });
    match tid {
        Some(t) => Ok((t, pid)),
        None => Err(Status::BadState), // the thread has ended (or never started)
    }
}

/// `thread_set_sched(thread, priority, flags)`: validate and name the thread, with the priority and flags to set. The priority may
/// not exceed the thread's job's `max_prio`.
pub fn thread_set_sched_target(
    objs: &Kobjs,
    procs: &BTreeMap<u32, Box<Process>>,
    jobs: &Jobs,
    me: u32,
    current: Tid,
    a: [u64; 6],
) -> Result<(Tid, u8, u8), Status> {
    let (prio, flags) = (a[1], a[2]);
    if !(1..=63).contains(&prio) || flags & !1 != 0 {
        return Err(Status::InvalidArgs);
    }
    let (tid, pid) = thread_target(objs, procs, me, current, a[0], Rights::MANAGE)?;
    let job = procs.get(&pid).ok_or(Status::BadState)?.job;
    let max = jobs.sched(job).ok_or(Status::BadState)?.max_prio;
    if prio as u8 > max {
        return Err(Status::AccessDenied);
    }
    Ok((tid, prio as u8, flags as u8))
}

/// `job_set_sched(job, max_prio, rt_budget_ns, cpu_weight)`: needs SET_LIMITS; no value may exceed the parent job's. Sets the job's
/// scheduling limits and returns the job (the caller then lowers the priorities of its threads above the new maximum).
pub fn job_set_sched(
    objs: &Kobjs,
    procs: &BTreeMap<u32, Box<Process>>,
    jobs: &mut Jobs,
    me: u32,
    a: [u64; 6],
) -> Result<JobId, Status> {
    if a[1] > 63 || a[3] > 65536 {
        return Err(Status::InvalidArgs);
    }
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let job = ipc::job_of(objs, &caller.handles, Handle(a[0]), Rights::SET_LIMITS)?;
    let limits = SchedLimits {
        max_prio: a[1] as u8,
        rt_budget: a[2],
        cpu_weight: a[3] as u32,
    };
    jobs.set_sched(job, limits).map_err(|e| match e {
        SchedError::NoSuchJob => Status::BadState,
        SchedError::AboveParent => Status::AccessDenied,
        SchedError::Invalid => Status::InvalidArgs,
    })?;
    Ok(job)
}
