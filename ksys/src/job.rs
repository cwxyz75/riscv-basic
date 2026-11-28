//! The job calls that need only the caller's handles and the ledger (TS3b): `job_create`, `job_set_limits`, `job_info`. (`job_kill`
//! and `job_set_sched` act on processes and threads and stay in the kernel.)

use crate::{Ctx, Reply};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use kcore::ipc;
use kcore::job::{IoResource, JobError, Limits, Resource};
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
    let raw = copy_from_user(c.aspace, c.mm, c.vmos, c.jobs, ptr, 48)?;
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

/// `None`: `n` is not one of these calls. Each call is a frame of its own (KT13): this one only routes.
pub fn job_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let a = &a;
    let r = match n {
        nr::JOB_CREATE => job_create(a, c),
        nr::JOB_SET_REGION_LIMIT | nr::JOB_SET_IO_LIMITS | nr::JOB_SET_LIMITS => {
            job_set(n, a, c).map(|()| (None, None))
        }
        nr::JOB_INFO => job_info(a, c),
        _ => return None,
    };
    Some(r.map_err(|s| (s, None, None)))
}

/// `job_create(parent, limits_ptr) -> handle`: a child job of the job `parent` (needs MANAGE) with the six limits at `limits_ptr`. The child can
/// never use more than any ancestor allows, whatever its own limits say.
#[inline(never)]
fn job_create<M: PhysMem>(
    a: &[u64; 6],
    c: &mut Ctx<'_, M>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    // credits of objects destroyed since the last charge come first: a job that has just closed its children must be able
    // to make another at once
    ipc::settle(c.objs, c.jobs);
    let parent = ipc::job_of(c.objs, c.handles, Handle(a[0]), Rights::MANAGE)?;
    let limits = read_limits(c, a[1])?;
    let id = c.jobs.create(parent, limits).map_err(job_status)?;
    c.job_sched.reset(id.index()); // (the slot may have been used before)
    c.job_sched.sync_budgets(c.jobs);
    match ipc::job_object_create(
        c.objs,
        c.handles,
        id,
        Rights::JOB,
        &mut ipc::Quota {
            jobs: c.jobs,
            job: Some(c.job),
        },
    ) {
        Ok(h) => Ok((Some(h.0), None)),
        Err(x) => {
            c.jobs.release(id);
            Err(x)
        }
    }
}

/// The calls that set a job's limits; each needs SET_LIMITS (not MANAGE), and usage above a new limit is not undone (later charges fail until it is
/// back under).
/// - `job_set_limits(job, limits_ptr)`: the six limits at `limits_ptr`.
/// - `job_set_region_limit(job, limit)`: the most regions one address space of a process of the job may hold (PL8); the smallest value along the
///   job's ancestors applies. A space already above a lowered limit keeps its regions and can add none.
/// - `job_set_io_limits(job, io_pages, vectors)` (IOMMU-SPEC 5.3): the job's limits on I/O-mapped pages and on interrupt vectors (`u64::MAX`: none
///   of its own; the smallest along the ancestors applies).
#[inline(never)]
fn job_set<M: PhysMem>(n: usize, a: &[u64; 6], c: &mut Ctx<'_, M>) -> Result<(), Status> {
    let job = ipc::job_of(c.objs, c.handles, Handle(a[0]), Rights::SET_LIMITS)?;
    match n {
        nr::JOB_SET_REGION_LIMIT => c.jobs.set_region_limit(job, a[1]).map_err(job_status),
        nr::JOB_SET_IO_LIMITS => c.jobs.set_io_limits(job, a[1], a[2]).map_err(job_status),
        _ => {
            let limits = read_limits(c, a[1])?;
            c.jobs.set_limits(job, limits).map_err(job_status)
        }
    }
}

/// `job_info(job, which) -> used, limit` for `which` 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes (everything in the job and
/// below it), 6 the scheduling limits, 7 the processors, 8 kernel bytes, 9 and 10 the CPU time, 11 the region limit, 12 and 13 the I/O usage.
/// Needs INSPECT.
#[inline(never)]
fn job_info<M: PhysMem>(
    a: &[u64; 6],
    c: &mut Ctx<'_, M>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let job = ipc::job_of(c.objs, c.handles, Handle(a[0]), Rights::INSPECT)?;
    let res = match a[1] {
        0 => Resource::Processes,
        1 => Resource::Threads,
        2 => Resource::Pages,
        3 => Resource::Jobs,
        4 => Resource::Objects,
        5 => Resource::IpcBytes,
        // 8: kernel memory in bytes (MM5), its limit an eighth of the page limit
        8 => Resource::KernelBytes,
        _ => return job_info_other(a[1], job, c),
    };
    let jobs = &c.jobs;
    let used = jobs.usage(job).ok_or(Status::BadState)?.get(res);
    let limits = jobs.limits(job).ok_or(Status::BadState)?;
    let limit = match res {
        Resource::Processes => limits.processes,
        Resource::Threads => limits.threads,
        Resource::Pages => limits.pages,
        Resource::Jobs => limits.jobs,
        Resource::Objects => limits.objects,
        Resource::IpcBytes => limits.ipc_bytes,
        Resource::KernelBytes => limits.effective(Resource::KernelBytes),
    };
    Ok((Some(used), Some(limit)))
}

/// `job_info`'s answers that are not a ledger resource (`which` 6, 7 and 9 to 13; anything else is INVALID_ARGS).
#[inline(never)]
fn job_info_other<M: PhysMem>(
    which: u64,
    job: kcore::job::JobId,
    c: &mut Ctx<'_, M>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    match which {
        // 6: the job's scheduling limits: `max_prio` and `rt_budget` (nanoseconds per 100 ms window)
        6 => {
            let s = c.jobs.sched(job).ok_or(Status::BadState)?;
            Ok((Some(u64::from(s.max_prio)), Some(s.rt_budget)))
        }
        // 7: the processors a job's threads may run on: every hart that is online (there is no affinity yet), the count
        // in both `a1` and `a2` (UV6: what `/proc/cpuinfo` and `nproc` report)
        7 => Ok((Some(c.harts_online), Some(c.harts_online))),
        // 9 (SCHED-SMP-SPEC 4.5, 4.10): the real-time CPU nanoseconds this job's threads have run in the current 100 ms window, and the
        // job's budget per window (accounted, not enforced). The spec's 7 and 8 were taken by UV6 and MM5 before this call was built.
        9 => {
            let s = c.jobs.sched(job).ok_or(Status::BadState)?;
            Ok((
                Some(c.job_sched.rt_used(job.index(), (c.clock)())),
                Some(s.rt_budget),
            ))
        }
        // 10: CPU nanoseconds run by the threads of this job and of the jobs below it (jobs already freed are not counted); no limit (0)
        10 => Ok((Some(cpu_below(job, c)), Some(0))),
        // 11 (PL8): the most regions one address space of this job's processes may hold: the effective limit (the smallest along the ancestors, at most
        // `MAX_REGIONS`) in both `a1` and `a2`
        11 => {
            let l = c
                .jobs
                .region_limit(job)
                .min(abi::limits::MAX_REGIONS as u64);
            Ok((Some(l), Some(l)))
        }
        // 12 and 13 (IOMMU-SPEC 5.3): I/O-mapped pages and interrupt vectors of the job's device domains and of those below it,
        // and the effective limit (the smallest along the ancestors; `u64::MAX`: none)
        12 => io_info(job, IoResource::MappedPages, c),
        13 => io_info(job, IoResource::Vectors, c),
        _ => Err(Status::InvalidArgs),
    }
}

/// CPU nanoseconds run by the threads of `job` and of the jobs below it (`job_info` 10).
#[inline(never)]
fn cpu_below<M: PhysMem>(job: kcore::job::JobId, c: &mut Ctx<'_, M>) -> u64 {
    let mut sum = 0u64;
    c.jobs.for_each_live(|j| {
        if c.jobs.is_within(j, job) {
            sum = sum.saturating_add(c.job_sched.cpu(j.index()));
        }
    });
    sum
}

/// `job_info` 12 and 13: the I/O usage `r` of `job` and of those below it, and its effective limit.
#[inline(never)]
fn io_info<M: PhysMem>(
    job: kcore::job::JobId,
    r: IoResource,
    c: &mut Ctx<'_, M>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let used = c.jobs.io_usage(job, r).ok_or(Status::BadState)?;
    Ok((Some(used), Some(c.jobs.io_limit(job, r))))
}
