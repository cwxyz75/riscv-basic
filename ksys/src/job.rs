//! The job calls that need only the caller's handles and the ledger (TS3b): `job_create`, `job_set_limits`, `job_info`. (`job_kill`
//! and `job_set_sched` act on processes and threads and stay in the kernel.)

use crate::{fail, Ctx, Reply};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use kcore::ipc;
use kcore::job::{JobError, Limits, Resource};
use kcore::pagetable::PhysMem;
use kcore::usercopy::copy_from_user;

/// What a failed ledger call looks like to a program: running into a limit is "out of memory" (the one resource-exhausted status),
/// a job that is gone is a bad state.
pub fn job_status(e: JobError) -> Status {
    match e {
        JobError::Exceeded | JobError::TooManyJobs => Status::NoMemory,
        JobError::NoSuchJob => Status::BadState,
    }
}

/// Read six limits (processes, threads, pages, jobs, objects, ipc_bytes: `u64::MAX` means none) from the 48 bytes of user memory at
/// `ptr`.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)] // INVARIANT: the record is six words (48 bytes, copied in whole), read at word indices 0 to 5, so the slices are inside it and `try_into` of eight bytes cannot fail
fn read_limits<M: PhysMem>(c: &mut Ctx<'_, M>, ptr: u64) -> Result<Limits, Status> {
    let raw = copy_from_user(c.aspace, c.mm, c.vmos, ptr, 48)?;
    let w = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
    Ok(Limits {
        processes: w(0),
        threads: w(1),
        pages: w(2),
        jobs: w(3),
        objects: w(4),
        ipc_bytes: w(5),
    })
}

/// `None`: `n` is not one of these calls.
pub fn job_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let h = Handle(a[0] as u32);
    let e = |s: Status| (s, None, None);
    let r: Reply = match n {
        // `job_create(parent, limits_ptr) -> handle`: a child job of the job `parent` (needs MANAGE) with the six limits at
        // `limits_ptr`. The child can never use more than any ancestor allows, whatever its own limits say.
        nr::JOB_CREATE => (|| {
            // credits of objects destroyed since the last charge come first: a job that has just closed its children must be able
            // to make another at once
            ipc::settle(c.objs, &mut c.vmos.jobs);
            let parent = ipc::job_of(c.objs, c.handles, h, Rights::MANAGE).map_err(e)?;
            let limits = read_limits(c, a[1]).map_err(e)?;
            let jobs = &mut c.vmos.jobs;
            let id = jobs.create(parent, limits).map_err(|x| e(job_status(x)))?;
            match ipc::job_object_create(
                c.objs,
                c.handles,
                id,
                Rights::JOB,
                &mut ipc::Quota {
                    jobs: &mut c.vmos.jobs,
                    job: Some(c.job),
                },
            ) {
                Ok(h) => Ok((Some(u64::from(h.0)), None)),
                Err(x) => {
                    c.vmos.jobs.release(id);
                    Err(e(x))
                }
            }
        })(),
        // `job_set_limits(job, limits_ptr)`: needs SET_LIMITS (not MANAGE). Usage above a new limit is not undone: later charges
        // fail until it is back under.
        nr::JOB_SET_LIMITS => (|| {
            let job = ipc::job_of(c.objs, c.handles, h, Rights::SET_LIMITS).map_err(e)?;
            let limits = read_limits(c, a[1]).map_err(e)?;
            c.vmos
                .jobs
                .set_limits(job, limits)
                .map_err(|x| e(job_status(x)))?;
            Ok((None, None))
        })(),
        // `job_info(job, which) -> used, limit` for `which` 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes
        // (everything in the job and below it), 6 the scheduling limits, 7 the processors. Needs INSPECT.
        nr::JOB_INFO => (|| {
            let job = ipc::job_of(c.objs, c.handles, h, Rights::INSPECT).map_err(e)?;
            let res = match a[1] {
                0 => Resource::Processes,
                1 => Resource::Threads,
                2 => Resource::Pages,
                3 => Resource::Jobs,
                4 => Resource::Objects,
                5 => Resource::IpcBytes,
                // 6: the job's scheduling limits: `max_prio` and `rt_budget` (nanoseconds per 100 ms window)
                6 => {
                    let s = c.vmos.jobs.sched(job).ok_or(e(Status::BadState))?;
                    return Ok((Some(u64::from(s.max_prio)), Some(s.rt_budget)));
                }
                // 7: the processors a job's threads may run on: every hart that is online (there is no affinity yet), the count
                // in both `a1` and `a2` (UV6: what `/proc/cpuinfo` and `nproc` report)
                7 => return Ok((Some(c.harts_online), Some(c.harts_online))),
                _ => return fail(Status::InvalidArgs),
            };
            let jobs = &c.vmos.jobs;
            let used = jobs.usage(job).ok_or(e(Status::BadState))?.get(res);
            let limits = jobs.limits(job).ok_or(e(Status::BadState))?;
            let limit = match res {
                Resource::Processes => limits.processes,
                Resource::Threads => limits.threads,
                Resource::Pages => limits.pages,
                Resource::Jobs => limits.jobs,
                Resource::Objects => limits.objects,
                Resource::IpcBytes => limits.ipc_bytes,
            };
            Ok((Some(used), Some(limit)))
        })(),
        _ => return None,
    };
    Some(r)
}
