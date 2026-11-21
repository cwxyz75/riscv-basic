//! The process, thread and job syscalls that need the scheduler (what the pure half in `ksys` decides, the kernel carries out).

use super::*;

// ---------------------------------------------------------------------------
// Process syscalls (S3c-2a). They touch two processes (the caller and a target), so they
// work in short interrupts-off steps that never hold both at once.

pub(super) fn bad(s: Status) -> Reply {
    Err((s, None, None))
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn process_call(n: usize, a: [u64; 6]) -> Reply {
    match n {
        nr::PROCESS_CREATE => process_create(a),
        nr::JOB_SET_SCHED => job_set_sched(a),
        nr::THREAD_SET_SCHED => thread_set_sched(a),
        nr::THREAD_GET_SCHED => thread_get_sched(a),
        nr::JOB_KILL => job_kill(a),
        nr::JOB_ROOT | nr::RESOURCE_ROOT | nr::PROCESS_SELF => process_table_call(n, a),
        nr::PROCESS_MAP => process_table_call(n, a),
        nr::PROCESS_UNMAP => process_table_call(n, a),
        nr::PROCESS_PROTECT | nr::PROCESS_MAP_INFO => process_table_call(n, a),
        nr::VMO_DECOMMIT | nr::VMO_SET_SIZE => process_table_call(n, a),
        nr::VMO_CREATE_CHILD | nr::PROCESS_CLONE_REGION => process_table_call(n, a),
        nr::PROCESS_START => process_start(a),
        nr::PROCESS_KILL => process_kill(a),
        nr::PROCESS_SUSPEND => process_stop(a, true),
        nr::PROCESS_RESUME => process_stop(a, false),
        nr::PROCESS_INFO => process_table_call(n, a),
        nr::THREAD_SPAWN => thread_spawn(a),
        nr::THREAD_INFO => process_table_call(n, a),
        nr::THREAD_INTERRUPT => thread_interrupt(a),
        _ => bad(Status::BadSyscall),
    }
}

/// What the process calls of `ksys` need of this machine.
pub(super) struct KernelMachine;

impl ksys::Machine for KernelMachine {
    fn shootdown(&mut self) {
        crate::smp::shootdown();
    }
    fn flush_tlb(&mut self) {
        kmem::flush_tlb();
    }
    fn icache_sync(&mut self, pages: u64) {
        crate::smp::icache_sync(pages);
    }
    fn skip_shootdown(&self, call: usize) -> bool {
        (call == nr::VMO_DECOMMIT && cfg!(feature = "fault-decommit-no-shootdown"))
            || (call == nr::PROCESS_PROTECT && cfg!(feature = "fault-protect-no-shootdown"))
    }
    fn skip_cow_protect(&self) -> bool {
        cfg!(feature = "fault-skip-cow-protect")
    }
    fn random_below(&mut self, n: u64) -> u64 {
        // (a pool that is not seeded yet gives nothing: the placement is then the first fit, which is still a valid place)
        crate::entropy::below(n).unwrap_or(0)
    }
}

/// The calls that only read and change the process table (`ksys::proc_call`).
pub(super) fn process_table_call(n: usize, a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let k = kmem::get();
        let mut ctx = ksys::PCtx {
            objs: &mut s.objs,
            procs: &mut s.procs,
            me,
            mm: k.mm,
            vmos: k.vmos,
            jobs: k.jobs,
            machine: &mut KernelMachine,
        };
        ksys::proc_call(n, a, &mut ctx).unwrap_or_else(|| bad(Status::BadSyscall))
    })
}

/// After a call or a fault: drop what the copy log made stale in every address space (COW1). A load when nothing is pending.
pub(crate) fn cow_sync() {
    if !kcore::aspace::COPY_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    irq::without(|| {
        let s = state();
        // a translation a copy-on-write fault replaced (in the address space of the thread that took it): every hart drops its TLB
        let replaced = caller_pid(s)
            .ok()
            .and_then(|pid| s.procs.get_mut(&pid))
            .is_some_and(|p| p.aspace.take_replaced() > 0);
        if replaced {
            crate::smp::shootdown();
            kmem::flush_tlb();
        }
        let k = kmem::get();
        ksys::cow_sync(&mut s.procs, k.mm, k.vmos, &mut KernelMachine);
    });
}

pub(super) fn caller_pid(s: &UserState) -> Result<u64, Status> {
    s.thread_pid
        .get(&thread::current())
        .copied()
        .ok_or(Status::BadState)
}

pub(super) fn process_create(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let kernel_half_ok = kmem::kernel_half_unchanged(); // (before the memory locks: it takes them itself)
        let k = kmem::get();
        let handle = ksys::process_create(s, k.mm, k.jobs, k.aspace.table(), kernel_half_ok, me, a)
            .map_err(|e| (e, None, None))?;
        Ok((Some(handle), None))
    })
}

pub(super) fn process_start(a: [u64; 6]) -> Reply {
    if !thread::user_thread_ids_left() {
        return bad(Status::NoMemory);
    }
    let (started, first_prio) = irq::without(|| -> Result<(usize, u8), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        ksys::process_start(s, kmem::jobs().jobs, me, a).map_err(|e| (e, None, None))
    })?;
    // The new thread (registered by itself once it runs). Spawned after the state borrow is
    // gone: spawning can switch to it at once.
    thread::spawn_free_detached("user", first_prio, user_thread_main, started);
    Ok((None, None))
}

pub(super) fn process_kill(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0]);
    let code = a[1] as i32;
    let (tpid, me) = irq::without(|| -> Result<(u64, u64), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let tpid = ksys::kill_target(&s.objs, &s.procs, me, ph).map_err(|e| (e, None, None))?;
        Ok((tpid, me))
    })?;
    if tpid == me {
        exit_process(code);
    }
    request_kill(tpid, code);
    Ok((None, None))
}

/// `process_suspend(process)` and `process_resume(process)` (SIG2): stop or continue every thread of a process. Both need
/// KILL on the handle (the right to end a process is the right to stop it), are idempotent, and refuse an ended process
/// (BAD_STATE). A process may stop itself: its calling thread parks as it returns.
pub(super) fn process_stop(a: [u64; 6], stop: bool) -> Reply {
    let ph = Handle(a[0]);
    let tpid = irq::without(|| -> Result<u64, ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let tpid = ksys::stop_target(&mut s.objs, &mut s.procs, me, ph, stop)
            .map_err(|e| (e, None, None))?;
        bump_attention();
        if stop {
            // a thread running user code on another hart parks at its next trap: the flag is set first and the kick comes
            // in the same locked region (as for a kill)
            crate::smp::kick_others();
        }
        Ok(tpid)
    })?;
    if !stop {
        wake_threads_of(tpid);
    }
    Ok((None, None))
}

/// `thread_spawn(process, args) -> handle, tid` (SYSCALLS.md, SCHED-SMP-SPEC 5.3): create and start a thread (`ksys::thread_spawn`), then spawn it.
pub(super) fn thread_spawn(a: [u64; 6]) -> Reply {
    if !thread::user_thread_ids_left() {
        return bad(Status::NoMemory);
    }
    let spawn = irq::without(|| -> Result<ksys::Spawn, ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let creator = creator_prio(s);
        let k = kmem::get();
        ksys::thread_spawn(s, k.mm, k.vmos, k.jobs, me, creator, a).map_err(|e| (e, None, None))
    })?;
    // Spawned after the state borrow is gone: spawning can switch to the new thread at once.
    let tid = thread::spawn_free_detached("user", spawn.prio, user_thread_main, spawn.token);
    Ok((Some(spawn.handle), Some(tid)))
}

/// `thread_interrupt(thread)` (T56, SCHED-SMP-SPEC task 26): set the thread's interrupt flag and wake it. The next blocking
/// wait that would sleep (object_wait, port_wait, futex_wait, channel_call, with a deadline other than 0) returns
/// INTERRUPTED and takes the flag; one that is satisfied, or a poll, leaves it. It ends waits, not running code.
/// Needs MANAGE on a thread handle; a thread that has ended is BAD_STATE.
pub(super) fn thread_interrupt(a: [u64; 6]) -> Reply {
    let tid = irq::without(|| -> Result<Tid, ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::MANAGE)?;
        s.interrupts.insert(tid);
        Ok(tid)
    })?;
    // (after the state closure has ended: a wake can switch threads)
    thread::wake(tid);
    Ok((None, None))
}

// ---- jobs (S4.5, OD56-OD58) ----

/// The priority a thread created by the calling thread inherits: the caller's own base if it is a user thread, else
/// the default (10).
pub(super) fn creator_prio(s: &UserState) -> u8 {
    let me = thread::current();
    if s.thread_pid.contains_key(&me) {
        thread::priority_of(me).unwrap_or(10)
    } else {
        10
    }
}

/// The kernel thread behind a thread handle with `need`, and the process it belongs to.
pub(super) fn thread_target(
    s: &UserState,
    me: u64,
    h: u64,
    need: Rights,
) -> Result<(Tid, u64), ReplyErr> {
    ksys::thread_target(&s.objs, &s.procs, me, thread::current(), h, need)
        .map_err(|e| (e, None, None))
}

/// `thread_set_sched(thread, priority, flags)`: set a thread's base priority (1 to 63) and flags (bit 0: round robin
/// within its level). Needs MANAGE on the handle (none for 0, the caller), and the priority may not exceed the
/// thread's job's `max_prio`.
pub(super) fn thread_set_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, prio, flags) = ksys::thread_set_sched_target(
            &s.objs,
            &s.procs,
            kmem::jobs().jobs,
            me,
            thread::current(),
            a,
        )
        .map_err(|e| (e, None, None))?;
        s.sched_flags.insert(tid, flags);
        thread::set_priority(tid, prio);
        Ok((None, None))
    })
}

/// `thread_get_sched(thread) -> sched, tid`: `sched` packs the base priority (bits 0 to 7), the flags (8 to 15) and
/// the effective priority (16 to 23; equal to the base until donation exists). Needs INSPECT (none for 0).
pub(super) fn thread_get_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::INSPECT)?;
        let prio = u64::from(thread::priority_of(tid).ok_or((Status::BadState, None, None))?);
        let flags = u64::from(s.sched_flags.get(&tid).copied().unwrap_or(0));
        Ok((Some(prio | flags << 8 | prio << 16), Some(tid)))
    })
}

/// `job_set_sched(job, max_prio, rt_budget_ns, cpu_weight)`: needs SET_LIMITS. No value may exceed the parent job's;
/// lowering `max_prio` lowers the base priority of every thread in the job and below that is above it.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn job_set_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        // (`JOBS` is held only while the ledger is read: `thread::priority_of` takes the big kernel lock, which comes before it)
        let (job, candidates) = {
            let jobs = kmem::jobs();
            let job = ksys::job_set_sched(&s.objs, &s.procs, jobs.jobs, me, a)
                .map_err(|e| (e, None, None))?;
            // PR1: the threads of the job and everything below it, with the maximum their own job now allows
            let candidates: Vec<(Tid, u8)> = s
                .thread_pid
                .iter()
                .filter_map(|(&tid, pid)| {
                    let pj = s.procs.get(pid)?.job;
                    let max = jobs.jobs.sched(pj)?.max_prio;
                    jobs.jobs.is_within(pj, job).then_some((tid, max))
                })
                .collect();
            (job, candidates)
        };
        let _ = job;
        let clamp: Vec<(Tid, u8)> = candidates
            .into_iter()
            .filter_map(|(tid, max)| {
                let now = thread::priority_of(tid)?;
                (now > max).then_some((tid, max.max(1)))
            })
            .collect();
        thread::set_priorities(&clamp);
        Ok((None, None))
    })
}

/// `job_kill(job, code)`: end every process in the job and its descendants with exit code
/// `code` and forbid new processes and threads there (their limits become 0). Needs KILL. A
/// caller inside the job ends too, last.
pub(super) fn job_kill(a: [u64; 6]) -> Reply {
    let code = a[1] as i32;
    let (victims, me_inside, me) = irq::without(|| -> Result<(Vec<u64>, bool, u64), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (victims, me_inside) =
            ksys::job_kill_victims(&s.objs, &s.procs, kmem::jobs().jobs, me, Handle(a[0]))
                .map_err(|e| (e, None, None))?;
        Ok((victims, me_inside, me))
    })?;
    for pid in victims {
        request_kill(pid, code);
    }
    if me_inside {
        let _ = me;
        exit_process(code);
    }
    Ok((None, None))
}
