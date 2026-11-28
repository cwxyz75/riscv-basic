//! The job calls that need only the caller's handles and the ledger (TS3b): `job_create`, `job_set_limits`, `job_info`. (`job_kill`
//! and `job_set_sched` act on processes and threads and stay in the kernel.)

use crate::{fail, Ctx, Reply};
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

/// `None`: `n` is not one of these calls.
pub fn job_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let h = Handle(a[0]);
    let e = |s: Status| (s, None, None);
    let r: Reply = match n {
        // `job_create(parent, limits_ptr) -> handle`: a child job of the job `parent` (needs MANAGE) with the six limits at
        // `limits_ptr`. The child can never use more than any ancestor allows, whatever its own limits say.
        nr::JOB_CREATE => (|| {
            // credits of objects destroyed since the last charge come first: a job that has just closed its children must be able
            // to make another at once
            ipc::settle(c.objs, c.jobs);
            let parent = ipc::job_of(c.objs, c.handles, h, Rights::MANAGE).map_err(e)?;
            let limits = read_limits(c, a[1]).map_err(e)?;
            let jobs = &mut c.jobs;
            let id = jobs.create(parent, limits).map_err(|x| e(job_status(x)))?;
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
                    Err(e(x))
                }
            }
        })(),
        // `job_set_limits(job, limits_ptr)`: needs SET_LIMITS (not MANAGE). Usage above a new limit is not undone: later charges
        // fail until it is back under.
        // `job_set_region_limit(job, limit)`: the most regions one address space of a process of the job may hold (PL8); the smallest value along the
        // job's ancestors applies. Needs SET_LIMITS. A space already above a lowered limit keeps its regions and can add none.
        nr::JOB_SET_REGION_LIMIT => (|| {
            let job = ipc::job_of(c.objs, c.handles, h, Rights::SET_LIMITS).map_err(e)?;
            c.jobs
                .set_region_limit(job, a[1])
                .map_err(|x| e(job_status(x)))?;
            Ok((None, None))
        })(),
        // `job_set_io_limits(job, io_pages, vectors)` (IOMMU-SPEC 5.3): the job's limits on I/O-mapped pages and on interrupt vectors
        // (`u64::MAX`: none of its own; the smallest along the ancestors applies). Needs SET_LIMITS. Usage above a new limit is not undone.
        nr::JOB_SET_IO_LIMITS => (|| {
            let job = ipc::job_of(c.objs, c.handles, h, Rights::SET_LIMITS).map_err(e)?;
            c.jobs
                .set_io_limits(job, a[1], a[2])
                .map_err(|x| e(job_status(x)))?;
            Ok((None, None))
        })(),
        nr::JOB_SET_LIMITS => (|| {
            let job = ipc::job_of(c.objs, c.handles, h, Rights::SET_LIMITS).map_err(e)?;
            let limits = read_limits(c, a[1]).map_err(e)?;
            c.jobs
                .set_limits(job, limits)
                .map_err(|x| e(job_status(x)))?;
            Ok((None, None))
        })(),
        // `job_info(job, which) -> used, limit` for `which` 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes
        // (everything in the job and below it), 6 the scheduling limits, 7 the processors, 8 kernel bytes. Needs INSPECT.
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
                    let s = c.jobs.sched(job).ok_or(e(Status::BadState))?;
                    return Ok((Some(u64::from(s.max_prio)), Some(s.rt_budget)));
                }
                // 7: the processors a job's threads may run on: every hart that is online (there is no affinity yet), the count
                // in both `a1` and `a2` (UV6: what `/proc/cpuinfo` and `nproc` report)
                7 => return Ok((Some(c.harts_online), Some(c.harts_online))),
                // 8: kernel memory in bytes (MM5), its limit an eighth of the page limit
                8 => Resource::KernelBytes,
                // 9 (SCHED-SMP-SPEC 4.5, 4.10): the real-time CPU nanoseconds this job's threads have run in the current 100 ms window, and the
                // job's budget per window (accounted, not enforced). The spec's 7 and 8 were taken by UV6 and MM5 before this call was built.
                9 => {
                    let s = c.jobs.sched(job).ok_or(e(Status::BadState))?;
                    return Ok((
                        Some(c.job_sched.rt_used(job.index(), (c.clock)())),
                        Some(s.rt_budget),
                    ));
                }
                // 10: CPU nanoseconds run by the threads of this job and of the jobs below it (jobs already freed are not counted); no limit (0)
                10 => {
                    let mut sum = 0u64;
                    c.jobs.for_each_live(|j| {
                        if c.jobs.is_within(j, job) {
                            sum = sum.saturating_add(c.job_sched.cpu(j.index()));
                        }
                    });
                    return Ok((Some(sum), Some(0)));
                }
                // 11 (PL8): the most regions one address space of this job's processes may hold: the effective limit (the smallest along the ancestors, at most
                // `MAX_REGIONS`) in both `a1` and `a2`
                11 => {
                    let l = c
                        .jobs
                        .region_limit(job)
                        .min(abi::limits::MAX_REGIONS as u64);
                    return Ok((Some(l), Some(l)));
                }
                // 12 and 13 (IOMMU-SPEC 5.3): I/O-mapped pages and interrupt vectors of the job's device domains and of those below it,
                // and the effective limit (the smallest along the ancestors; `u64::MAX`: none)
                12 | 13 => {
                    let r = if a[1] == 12 {
                        IoResource::MappedPages
                    } else {
                        IoResource::Vectors
                    };
                    let used = c.jobs.io_usage(job, r).ok_or(e(Status::BadState))?;
                    return Ok((Some(used), Some(c.jobs.io_limit(job, r))));
                }
                _ => return fail(Status::InvalidArgs),
            };
            let jobs = &c.jobs;
            let used = jobs.usage(job).ok_or(e(Status::BadState))?.get(res);
            let limits = jobs.limits(job).ok_or(e(Status::BadState))?;
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
        })(),
        _ => return None,
    };
    Some(r)
}
