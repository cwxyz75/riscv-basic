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
        nr::JOB_ROOT | nr::RESOURCE_ROOT | nr::DEBUG_ROOT | nr::PROCESS_SELF => {
            process_table_call(n, a)
        }
        nr::PROCESS_MAP => process_table_call(n, a),
        nr::PROCESS_UNMAP => process_unmap_call(a),
        nr::PROCESS_PROTECT => process_protect_call(n, a),
        nr::PROCESS_MAP_INFO => process_table_call(n, a),
        nr::VMO_DECOMMIT | nr::VMO_SET_SIZE => vmo_range_call(n, a),
        nr::VMO_CREATE_CHILD | nr::PROCESS_CLONE_REGION => process_table_call(n, a),
        nr::PROCESS_START => process_start(a),
        nr::PROCESS_KILL => process_kill(a),
        nr::PROCESS_SUSPEND => process_stop(a, true),
        nr::PROCESS_RESUME => process_stop(a, false),
        nr::PROCESS_INFO => process_table_call(n, a),
        nr::THREAD_SPAWN => thread_spawn(a),
        nr::THREAD_INFO => process_table_call(n, a),
        nr::THREAD_INTERRUPT => thread_interrupt(a),
        nr::PROCESS_EXCEPTION_CHANNEL => exception_channel_call(a, false),
        nr::JOB_EXCEPTION_CHANNEL => exception_channel_call(a, true),
        nr::EXCEPTION_RESUME => exception_resume_call(a),
        nr::PAGER_CREATE => pager_create_call(),
        nr::PAGER_SUPPLY => pager_supply_call(a),
        nr::PAGER_FAIL => pager_fail_call(a),
        // (dirty tracking is reserved: the names exist, the calls answer NOT_SUPPORTED, MM9)
        nr::PAGER_QUERY_DIRTY | nr::PAGER_WRITEBACK_DONE => bad(Status::NotSupported),
        nr::THREAD_SUSPEND => thread_suspend_call(a),
        nr::THREAD_RESUME => thread_resume_call(a),
        nr::THREAD_READ_STATE => thread_state_call(a, false),
        nr::THREAD_WRITE_STATE => thread_state_call(a, true),
        _ => bad(Status::BadSyscall),
    }
}

/// What the process calls of `ksys` need of this machine.
/// The machine under `ksys`. CPU times (AB6) are read **before** the memory lock is taken (the scheduler's tables come before it in the lock order)
/// and kept here for the call to look up.
#[derive(Default)]
pub(super) struct KernelMachine {
    cpu: Vec<(kcore::sched::Tid, u64)>,
    current: u64,
}

impl KernelMachine {
    /// For `thread_info` 1 and `process_info` 1: the CPU time of every thread the call may be asked about.
    fn read_cpu(&mut self, s: &UserState, me: u64, n: usize, a: [u64; 6]) {
        let Some(caller) = s.procs.get(&me) else {
            return;
        };
        let h = abi::Handle(a[0]);
        if n == nr::THREAD_INFO {
            if a[0] == 0 {
                self.current = thread::cpu_ns(thread::current());
            } else if let Ok((obj, pid, ..)) =
                ipc::thread_cpu_info(&s.objs, &caller.handles, h, Rights::INSPECT)
            {
                let tid = match s.objs.get(obj) {
                    Some(ipc::KObj::Thread(t)) => t.tid,
                    _ => None,
                }
                .filter(|t| s.procs.get(&pid).is_some_and(|p| p.live_tids.contains(t)));
                if let Some(t) = tid {
                    self.cpu.push((t, thread::cpu_ns(t)));
                }
            }
        } else if let Ok((pid, ..)) =
            ipc::process_cpu_info(&s.objs, &caller.handles, h, Rights::INSPECT)
        {
            if let Some(p) = s.procs.get(&pid) {
                for &t in p.live_tids.iter() {
                    self.cpu.push((t, thread::cpu_ns(t)));
                }
            }
        }
    }
}

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
    fn thread_cpu_ns(&self, tid: kcore::sched::Tid) -> u64 {
        self.cpu
            .iter()
            .find(|(t, _)| *t == tid)
            .map_or(0, |(_, ns)| *ns)
    }
    fn current_thread_cpu_ns(&self) -> u64 {
        self.current
    }
    fn random_below(&mut self, n: u64) -> u64 {
        // (a pool that is not seeded yet gives nothing: the placement is then the first fit, which is still a valid place)
        crate::entropy::placement_below(n)
    }
}

/// `vmo_decommit` and `vmo_set_size` (T207 PR 3a, 3d): the pages go a batch at a time, and each batch in three parts under different locks, with nothing
/// held between them: **A** the frames leave the VMO's index (memory locks only; from here a fault sees zeros), **B** each process that maps the span
/// has it cleared, one address space at a time, found by the VMO's list of mappings, **C** every hart drops its translations and the frames go back.
/// There is a switch point between batches. The VMO is busy from the first part to the last (`Vmos::range_end`, always run); a failure in the middle
/// leaves the batches before it done.
#[allow(clippy::large_stack_frames)] // KT13: audited, as `process_table_call`: the batch of frames is on this frame
fn vmo_range_call(n: usize, a: [u64; 6]) -> Reply {
    // one part under the locks the part needs: `own` is the address space to hold (none for A and C); `f` gets the call's context
    fn section<R>(
        must: bool, // (false for C: the frames taken must go back whatever became of the caller)
        own: Option<u64>,
        f: impl FnOnce(&mut ksys::PCtx<'_, kmem::DirectMap, KernelMachine>) -> R,
    ) -> Result<R, Status> {
        locked_state(|s| -> Result<R, Status> {
            let me = if must {
                caller_pid(s)?
            } else {
                caller_pid(s).unwrap_or(0)
            };
            let mut machine = KernelMachine::default();
            let _as = match own {
                Some(pid) => AsLocks::one(s, pid),
                None => AsLocks::none(),
            };
            let k = kmem::get();
            let mut ctx = ksys::PCtx {
                objs: &mut s.objs,
                procs: &mut s.procs,
                me,
                mm: k.mm,
                vmos: k.vmos,
                jobs: k.jobs,
                machine: &mut machine,
            };
            Ok(f(&mut ctx))
        })
    }
    let call = match section(true, None, |ctx| ksys::vmo_range_begin(ctx, n, a)) {
        Ok(Ok(Some(c))) => c,
        Ok(Ok(None)) => return Ok((None, None)),
        Ok(Err(e)) | Err(e) => return bad(e),
    };
    let r: Result<(), Status> = (|| loop {
        // A
        let mut batch = section(true, None, |ctx| ksys::vmo_range_take(ctx, call))??;
        // B (a failure here stops the walk, not the batch: C still runs, or the frames taken would never go back)
        let mut walked: Result<(), Status> = Ok(());
        loop {
            let owner = match section(true, None, |ctx| {
                ksys::vmo_range_next_owner(ctx, call, &batch)
            }) {
                Ok(Some(o)) => o,
                Ok(None) => break,
                Err(e) => {
                    walked = Err(e);
                    break;
                }
            };
            if let Err(e) = section(true, Some(owner), |ctx| {
                ksys::vmo_range_unmap_owner(ctx, n, call, &mut batch, owner)
            }) {
                walked = Err(e);
                break;
            }
        }
        // C
        let done = section(false, None, |ctx| {
            ksys::vmo_range_finish(ctx, n, call, &mut batch)
        })??;
        walked?;
        if done {
            return Ok(());
        }
        // (the switch point: nothing is held; another hart takes the locks, and a thread that was woken runs)
        irq::without(|| {});
    })();
    let ok = r.is_ok();
    locked_state(|_| {
        let k = kmem::get();
        k.vmos.range_end(k.mm, k.jobs, call.id, ok)
    });
    match r {
        Ok(()) => Ok((None, None)),
        Err(e) => bad(e),
    }
}

/// `process_unmap` (T207 PR 3b): the regions are marked dying under the locks of the call (every failure is there), then the mappings go a batch at a
/// time, each batch under the target's address-space lock and the memory locks with a shootdown before they are let go, and the switch point between
/// two batches. A sibling that touches the range meanwhile takes an exception (a dying region is no region).
#[allow(clippy::large_stack_frames)] // KT13: audited, as `process_table_call`
fn process_unmap_call(a: [u64; 6]) -> Reply {
    // the first part: `AsLocks::all` (the handle names the target)
    let call = locked_state(|s| -> Result<ksys::UnmapCall, Status> {
        let me = caller_pid(s)?;
        let mut machine = KernelMachine::default();
        let _as = AsLocks::all(s)?;
        let k = kmem::get();
        let mut ctx = ksys::PCtx {
            objs: &mut s.objs,
            procs: &mut s.procs,
            me,
            mm: k.mm,
            vmos: k.vmos,
            jobs: k.jobs,
            machine: &mut machine,
        };
        ksys::process_unmap_begin(&mut ctx, a)
    });
    let call = match call {
        Ok(c) => c,
        Err(e) => return bad(e),
    };
    let part = |end: bool| -> Result<bool, Status> {
        locked_state(|s| -> Result<bool, Status> {
            let me = caller_pid(s)?;
            let mut machine = KernelMachine::default();
            let _as = AsLocks::one(s, call.tpid);
            let k = kmem::get();
            let mut ctx = ksys::PCtx {
                objs: &mut s.objs,
                procs: &mut s.procs,
                me,
                mm: k.mm,
                vmos: k.vmos,
                jobs: k.jobs,
                machine: &mut machine,
            };
            if end {
                ksys::process_unmap_end(&mut ctx, call);
                Ok(true)
            } else {
                ksys::process_unmap_step(&mut ctx, call)
            }
        })
    };
    let mut r = part(false);
    while r == Ok(false) {
        irq::without(|| {});
        r = part(false);
    }
    match r {
        Ok(_) => {
            let _ = part(true);
            Ok((None, None))
        }
        Err(e) => bad(e),
    }
}

/// `process_protect` (T207 PR 3b): the regions take their new permissions under the locks of the call (every failure is there), then the page-table
/// entries follow a batch at a time under the target's address-space lock, with the switch point between batches; the shootdown and the
/// instruction-cache sync come before the call returns (M9).
#[allow(clippy::large_stack_frames)] // KT13: audited, as `process_table_call`
fn process_protect_call(n: usize, a: [u64; 6]) -> Reply {
    let call = locked_state(|s| -> Result<ksys::ProtectCall, Status> {
        let me = caller_pid(s)?;
        let mut machine = KernelMachine::default();
        let _as = AsLocks::all(s)?;
        let k = kmem::get();
        let mut ctx = ksys::PCtx {
            objs: &mut s.objs,
            procs: &mut s.procs,
            me,
            mm: k.mm,
            vmos: k.vmos,
            jobs: k.jobs,
            machine: &mut machine,
        };
        ksys::process_protect_begin(&mut ctx, a)
    });
    let mut call = match call {
        Ok(c) => c,
        Err(e) => return bad(e),
    };
    let part = |call: &mut ksys::ProtectCall, end: bool| -> Result<bool, Status> {
        locked_state(|s| -> Result<bool, Status> {
            let me = caller_pid(s)?;
            let mut machine = KernelMachine::default();
            let _as = AsLocks::one(s, call.tpid);
            let k = kmem::get();
            let mut ctx = ksys::PCtx {
                objs: &mut s.objs,
                procs: &mut s.procs,
                me,
                mm: k.mm,
                vmos: k.vmos,
                jobs: k.jobs,
                machine: &mut machine,
            };
            if end {
                ksys::process_protect_end(&mut ctx, n, *call);
                Ok(true)
            } else {
                ksys::process_protect_step(&mut ctx, n, call)
            }
        })
    };
    let mut r = part(&mut call, false);
    while r == Ok(false) {
        irq::without(|| {});
        r = part(&mut call, false);
    }
    match r {
        Ok(_) => {
            let _ = part(&mut call, true);
            Ok((None, None))
        }
        Err(e) => {
            // (what was changed before the refusal still reaches every instruction fetch: M9)
            let _ = part(&mut call, true);
            bad(e)
        }
    }
}

/// The calls that only read and change the process table (`ksys::proc_call`).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB (the call table's locals; T172 shrinks them)
pub(super) fn process_table_call(n: usize, a: [u64; 6]) -> Reply {
    if (n == nr::PROCESS_INFO || n == nr::THREAD_INFO) && a[1] == 1 {
        thread::charge_running(); // (the CPU time of what runs now, the caller included, is brought up to date before it is read: AB6)
    }
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let mut machine = KernelMachine::default();
        if (n == nr::PROCESS_INFO || n == nr::THREAD_INFO) && a[1] == 1 {
            machine.read_cpu(s, me, n, a); // (before the memory lock: AB6)
        }
        // (the calls that name a process, or all of them: every address space, before the memory locks; these touch none)
        let _as = if matches!(
            n,
            nr::PROCESS_SELF
                | nr::JOB_ROOT
                | nr::RESOURCE_ROOT
                | nr::DEBUG_ROOT
                | nr::PROCESS_INFO
                | nr::THREAD_INFO
        ) {
            AsLocks::none()
        } else {
            AsLocks::all(s).map_err(|e| (e, None, None))?
        };
        let k = kmem::get();
        let mut ctx = ksys::PCtx {
            objs: &mut s.objs,
            procs: &mut s.procs,
            me,
            mm: k.mm,
            vmos: k.vmos,
            jobs: k.jobs,
            machine: &mut machine,
        };
        ksys::proc_call(n, a, &mut ctx).unwrap_or_else(|| bad(Status::BadSyscall))
    })
}

/// After a call or a fault: drop what the copy log made stale in every address space (COW1). A load when nothing is pending.
pub(crate) fn cow_sync() {
    if kcore::aspace::DRAIN_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        drain_dying();
    }
    if !kcore::aspace::COPY_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    cow_sync_held();
}

/// Give back the pages of the VMOs whose last reference went (T207 PR 3c), a batch at a time under the memory locks, with the switch point between
/// batches; nothing is held at it. Run at the tail of every call and fault, so no thread is back in user mode with its VMO still draining.
pub(crate) fn drain_dying() {
    loop {
        let was = irq::save_and_disable();
        let more = {
            let k = kmem::mem();
            k.vmos.drain_dying(k.mm, k.jobs, kcore::aspace::RANGE_STEP)
        };
        irq::restore(was);
        // (the switch point takes the BKL: a caller that already holds it leaves the rest for the next call, which finds DRAIN_PENDING still set)
        if !more || crate::bkl::holds() {
            return;
        }
        irq::without(|| {});
    }
}

fn cow_sync_held() {
    with_state(|s| {
        // a translation a copy-on-write fault replaced (in the address space of the thread that took it): every hart drops its TLB. This part takes the
        // caller's space only (no allocation), so it is done even when there is no memory for the list below
        {
            let _own = AsLocks::caller(s);
            let replaced = caller_pid(s)
                .ok()
                .and_then(|pid| s.procs.get_mut(&pid))
                .is_some_and(|p| p.aspace.held_mut().take_replaced() > 0);
            if replaced {
                crate::smp::shootdown();
                kmem::flush_tlb();
            }
        }
        // (every address space: the copy log names pages of any; before the memory locks. No memory for the list: the log stays pending, which keeps the
        // flag set, and the next call or fault applies it: the other spaces may hold a stale entry until then, T246)
        let Ok(_as) = AsLocks::all(s) else {
            return;
        };
        let k = kmem::get();
        ksys::cow_sync(&mut s.procs, k.mm, k.vmos, &mut KernelMachine::default());
    });
}

pub(super) fn caller_pid(s: &UserState) -> Result<u64, Status> {
    s.pid_of(thread::current()).ok_or(Status::BadState)
}

pub(super) fn process_create(a: [u64; 6]) -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let kernel_half_ok = kmem::kernel_half_unchanged(); // (before the memory locks: it takes them itself)
        let k = kmem::get();
        let handle = ksys::process_create(
            s,
            k.mm,
            k.jobs,
            &kmem::kernel_table(),
            kernel_half_ok,
            me,
            random_stack_hint(),
            a,
        )
        .map_err(|e| (e, None, None))?;
        Ok((Some(handle), None))
    })
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn process_start(a: [u64; 6]) -> Reply {
    if !thread::user_thread_ids_left() {
        return bad(Status::NoMemory);
    }
    // The kernel stack is made first (NO_MEMORY with nothing changed), the thread is spawned in the same critical section that makes its start
    // and registers it (its first act is to take the object lock, so it cannot run before that is done: L20), and a refusal frees the stack.
    locked_state(|s| -> Result<(), ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let _as = AsLocks::all(s).map_err(|e| (e, None, None))?;
        let prepared = thread::prepare_user_thread().ok_or((Status::NoMemory, None, None))?;
        // (the memory locks are let go before the spawn: a kernel stack comes from the frames, and the object lock comes before them)
        let started = {
            let mut k = kmem::get();
            super::syslib::ensure(&mut k);
            let mut lib = super::syslib::KernelLib {
                mm: &mut *k.mm,
                vmos: &mut *k.vmos,
            };
            ksys::process_start(s, k.jobs, &mut lib, me, a).map_err(|e| (e, None, None))?
        };
        spawn_registered(s, prepared, started);
        Ok(())
    })?;
    Ok((None, None))
}

pub(super) fn process_kill(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0]);
    let code = a[1] as i32;
    let (tpid, me) = locked_state(|s| -> Result<(u64, u64), ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let tpid = ksys::kill_target(&s.objs, &s.procs, me, ph).map_err(|e| (e, None, None))?;
        Ok((tpid, me))
    })?;
    if tpid == me {
        exit_process(code);
    }
    // (class N around the kill only: the self-kill above diverges, and a `nofail` that never returns would leave the hart in class N for good)
    crate::heap::nofail(|| request_kill(tpid, code));
    Ok((None, None))
}

/// `process_suspend(process)` and `process_resume(process)` (SIG2): stop or continue every thread of a process. Both need
/// KILL on the handle (the right to end a process is the right to stop it), are idempotent, and refuse an ended process
/// (BAD_STATE). A process may stop itself: its calling thread parks as it returns.
pub(super) fn process_stop(a: [u64; 6], stop: bool) -> Reply {
    let ph = Handle(a[0]);
    let tpid = locked_state(|s| -> Result<u64, ReplyErr> {
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

/// Spawn the kernel thread a call made a start for, on the stack prepared for it, and register it (EX1): known to `thread_suspend` and the rest from
/// the moment the call returns, a SUSPENDED one stopped, with its register image, before the handle that can resume it is handed back. Cannot fail.
#[inline(never)]
fn spawn_registered(
    s: &mut UserState,
    prepared: thread::PreparedThread,
    mut spawn: ksys::Spawn,
) -> Tid {
    let tid = thread::spawn_prepared(prepared, "user", spawn.prio, user_thread_main, spawn.token);
    ksys::thread_registered_by_spawner(s, tid, &mut spawn);
    thread::pi_register(tid); // (the donation graph, L23: its room was reserved by `prepare_user_thread`)
    if spawn.suspended {
        sync_suspends(s);
    }
    tid
}

/// `thread_spawn(process, args) -> handle, tid` (SYSCALLS.md, SCHED-SMP-SPEC 5.3): create and start a thread (`ksys::thread_spawn`), then spawn it.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn thread_spawn(a: [u64; 6]) -> Reply {
    if !thread::user_thread_ids_left() {
        return bad(Status::NoMemory);
    }
    // One critical section (KT9): the kernel stack first (NO_MEMORY with nothing changed), the call, the spawn and the spawner's half of the registration.
    // The new thread's first act is to take the object lock, so it cannot run a line of `user_thread_main` (or exit) before that half is done (L20).
    let (handle, tid) = locked_state(|s| -> Result<(u64, Tid), ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let creator = creator_prio(s);
        let _as = AsLocks::all(s).map_err(|e| (e, None, None))?;
        let prepared = thread::prepare_user_thread().ok_or((Status::NoMemory, None, None))?;
        let k = kmem::get();
        let spawn = ksys::thread_spawn(s, k.mm, k.vmos, k.jobs, me, creator, a)
            .map_err(|e| (e, None, None))?;
        drop(k);
        let handle = spawn.handle;
        let tid = spawn_registered(s, prepared, spawn);
        Ok((handle, tid))
    })?;
    Ok((Some(handle), Some(tid)))
}

/// `thread_interrupt(thread)` (T56, SCHED-SMP-SPEC task 26): set the thread's interrupt flag and wake it. The next blocking
/// wait that would sleep (object_wait, port_wait, futex_wait, channel_call, with a deadline other than 0) returns
/// INTERRUPTED and takes the flag; one that is satisfied, or a poll, leaves it. It ends waits, not running code.
/// Needs MANAGE on a thread handle; a thread that has ended is BAD_STATE.
pub(super) fn thread_interrupt(a: [u64; 6]) -> Reply {
    let tid = locked_state(|s| -> Result<Tid, ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::MANAGE)?;
        s.set_interrupt(tid, true);
        sync_interrupts(s);
        Ok(tid)
    })?;
    // (after the state closure has ended: a wake can switch threads)
    thread::wake(tid);
    Ok((None, None))
}

/// `pager_create() -> (pager, channel)` (MM9).
pub(super) fn pager_create_call() -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (p, c) =
            ksys::pager::pager_create(s, kmem::jobs().jobs, me).map_err(|e| (e, None, None))?;
        Ok((Some(p), Some(c)))
    })
}

/// `pager_supply(pager, vmo, offset, length, source, source_offset)` (MM9). A partly done supply still ends the waits of the pages it moved.
#[allow(clippy::large_stack_frames)] // INVARIANT: the lock set of every address space and the supply's arguments: one frame on the syscall path, no recursion
pub(super) fn pager_supply_call(a: [u64; 6]) -> Reply {
    let r = locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let _as = AsLocks::all(s).map_err(|e| (e, None, None))?;
        let k = kmem::get();
        ksys::pager::pager_supply(
            s,
            k.mm,
            k.vmos,
            k.jobs,
            &mut KernelMachine::default(),
            me,
            a,
        )
        .map_err(|e| (e, None, None))?;
        Ok((None, None))
    });
    super::waits::wake_pager_waiters();
    r
}

/// `pager_fail(pager, vmo, offset, length, status)` (MM9).
pub(super) fn pager_fail_call(a: [u64; 6]) -> Reply {
    let r = locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let k = kmem::get();
        ksys::pager::pager_fail(s, k.vmos, me, a).map_err(|e| (e, None, None))?;
        Ok((None, None))
    });
    super::waits::wake_pager_waiters();
    r
}

/// `process_exception_channel(process) -> channel` and `job_exception_channel(job) -> channel` (EX2): the user end of the channel the kernel writes
/// the exceptions of that process's threads (or of the processes below that job) on.
pub(super) fn exception_channel_call(a: [u64; 6], job: bool) -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let target = if job {
            ksys::exception::Target::Job(a[0])
        } else {
            ksys::exception::Target::Process(a[0])
        };
        let h = ksys::exception::exception_channel(s, kmem::jobs().jobs, me, target)
            .map_err(|e| (e, None, None))?;
        Ok((Some(h), None))
    })
}

/// Do what a decision about an exception comes to: wake a retried thread, end a process.
pub(super) fn act_on_resolution(r: Option<ksys::exception::Resolved>) {
    use ksys::exception::Resolved;
    match r {
        Some(Resolved::Retry { tid, wake }) => {
            locked_state(|s| sync_suspends(s));
            if wake {
                thread::wake(tid);
            }
        }
        Some(Resolved::Kill { pid, code }) => request_kill(pid, code),
        Some(Resolved::Passed) | None => {}
    }
}

/// `exception_resume(exception, how)` (EX2): RETRY, NEXT or KILL.
pub(super) fn exception_resume_call(a: [u64; 6]) -> Reply {
    let r = locked_state(|s| -> Result<ksys::exception::Resolved, ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        ksys::exception::exception_resume(s, kmem::jobs().jobs, me, a[0], a[1])
            .map_err(|e| (e, None, None))
    })?;
    act_on_resolution(Some(r));
    Ok((None, None))
}

/// `thread_suspend(thread)` (EX1): raise the thread's suspend count. It stops at its next return to user mode (a thread running user code on
/// another hart is reached by the same kick as a kill), or where it is if it is blocked in a call.
pub(super) fn thread_suspend_call(a: [u64; 6]) -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        ksys::thread_suspend(s, me, thread::current(), a[0], thread::is_blocked)
            .map_err(|e| (e, None, None))?;
        sync_suspends(s);
        bump_attention();
        crate::smp::kick_others();
        Ok((None, None))
    })
}

/// `thread_resume(thread)` (EX1): lower the count; at 0 the thread runs again (a parked one is woken).
pub(super) fn thread_resume_call(a: [u64; 6]) -> Reply {
    let (tid, wake) = locked_state(|s| -> Result<(Tid, bool), ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let r = ksys::thread_resume(s, me, thread::current(), a[0]).map_err(|e| (e, None, None));
        sync_suspends(s);
        r
    })?;
    if wake {
        thread::wake(tid);
    }
    Ok((None, None))
}

/// The user registers of a stopped thread, in the GENERAL layout: its starting image if it has not run, else its saved frame.
#[inline(never)]
#[allow(clippy::large_stack_frames)] // KT13: audited, a register image on the stack (T172 shrinks them)
fn general_of(
    s: &UserState,
    tid: Tid,
    frame: Option<*mut crate::trap::TrapFrame>,
) -> Option<[u64; 32]> {
    if let Some(image) = s.thread(tid).and_then(|t| t.initial_regs.as_deref()) {
        return Some(*image);
    }
    // (a write the thread has not applied yet is what it will have)
    if let Some(g) = s
        .thread(tid)
        .and_then(|t| t.pending_regs.as_deref())
        .and_then(|p| p.general)
    {
        return Some(g);
    }
    // SAFETY: the thread is stopped in the kernel after a trap from user mode, so the frame at the top of its kernel stack is its saved registers and is mapped.
    // The object lock does not order this read against the thread's own writes to the frame: a thread whose wait just ended writes its reply before it parks,
    // so a read in that window can see old and new register values mixed (T241, part 2: not corruption).
    Some(unsafe { (*frame?).general() })
}

/// `thread_read_state(thread, kind, buffer, length)` and `thread_write_state(...)` (EX1): the registers of a thread that has stopped. The
/// layouts are in `abi::thread_state`; `length` must be exactly the layout's size (VECTOR: 32 + 32 `vlenb` bytes).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: vlenb is a small power of two (at most 8 KiB), so the layout's size is far below u64::MAX
pub(super) fn thread_state_call(a: [u64; 6], write: bool) -> Reply {
    use abi::thread_state as ts;
    let (kind, buf, len) = (a[1], a[2], a[3]);
    let want = match kind {
        ts::GENERAL => (ts::GENERAL_WORDS * 8) as u64,
        ts::FP => (ts::FP_WORDS * 8) as u64,
        ts::VECTOR => match crate::fp::vector_vlenb() {
            Some(n) => (ts::VECTOR_HEADER_WORDS * 8 + 32 * n) as u64,
            None => return bad(Status::NotSupported),
        },
        ts::CFI => return bad(Status::NotSupported), // (HD5: reserved)
        _ => return bad(Status::InvalidArgs),
    };
    if len != want {
        return bad(Status::InvalidArgs);
    }
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = ksys::thread_state_target(s, me, thread::current(), a[0], write)
            .map_err(|e| (e, None, None))?;
        // (before the memory locks: the frame lookup takes the big lock, which must not be taken under a later lock)
        let frame_ptr = thread::user_frame_ptr(tid);
        if write {
            state_write(s, me, tid, kind, buf, len as usize, frame_ptr)
        } else {
            state_read(s, me, tid, kind, buf, frame_ptr)
        }
    })
}

/// The first `N` little-endian words of `b` (a layout of whole words; the caller checked its length): on the stack, nothing allocated.
fn le_array<const N: usize>(b: &[u8]) -> [u64; N] {
    let mut out = [0u64; N];
    for (o, c) in out.iter_mut().zip(b.as_chunks::<8>().0) {
        *o = u64::from_le_bytes(*c);
    }
    out
}

/// `words` as little-endian bytes in a buffer reserved first: NO_MEMORY, with nothing done, when there is no room (T248).
fn words_to_bytes(words: &[u64]) -> Result<alloc::vec::Vec<u8>, ReplyErr> {
    let mut bytes = alloc::vec::Vec::new();
    bytes
        .try_reserve_exact(words.len().saturating_mul(8))
        .map_err(|_| (Status::NoMemory, None, None))?;
    for w in words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    Ok(bytes)
}

#[inline(never)]
#[allow(clippy::large_stack_frames, clippy::indexing_slicing)] // KT13: audited, a register image on the stack (T172 shrinks them); INVARIANT: each layout was checked to have its exact size, so the word counts are 32 and 33
fn state_write(
    s: &mut UserState,
    me: u64,
    tid: Tid,
    kind: u64,
    buf: u64,
    len: usize,
    frame_ptr: Option<*mut crate::trap::TrapFrame>,
) -> Reply {
    use abi::thread_state as ts;
    // (the memory locks are held for the copy only: writing FP or vector state charges the job, which takes them itself)
    let data = {
        let _as = AsLocks::one(s, me);
        let k = kmem::get();
        let caller = s.procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        kcore::usercopy::copy_from_user(caller.aspace.held_mut(), k.mm, k.vmos, k.jobs, buf, len)
            .map_err(|e| (e, None, None))?
    };
    // (the two fixed layouts become words; the vector layout is written as bytes: it can be large and its allocation was the copy's)
    // A thread that has not run is written through its starting image. One that has is parked at the return to user mode holding a `&mut` to its
    // own frame, so nobody else writes the frame: the write is left for it (`pending_regs`) and it applies it itself when it wakes.
    let ran = !s.thread(tid).is_some_and(|t| t.initial_regs.is_some());
    let _ = frame_ptr;
    match kind {
        ts::GENERAL => {
            let g: [u64; 32] = le_array(&data);
            // the pc is a user address, on a 2-byte boundary
            if !(abi::user::BASE..abi::user::TOP).contains(&g[31]) || g[31] & 1 != 0 {
                return bad(Status::InvalidArgs);
            }
            if let Some(image) = s
                .thread_mut(tid)
                .and_then(|t| t.initial_regs.as_deref_mut())
            {
                *image = g;
            } else {
                // (the record is made at an R point: no room is NO_MEMORY, with nothing written)
                match s.pending_regs_entry(tid) {
                    Ok(p) => p.general = Some(g),
                    Err(e) => return bad(e),
                }
            }
        }
        ts::FP => {
            let g: [u64; 33] = le_array(&data);
            if ran {
                if let Err(e) = s.pending_regs_entry(tid) {
                    return bad(e);
                }
            }
            if !crate::fp::write_fp(tid, &g) {
                return bad(Status::NoMemory);
            }
            if ran {
                if let Ok(p) = s.pending_regs_entry(tid) {
                    p.fp = true;
                }
            }
        }
        _ => {
            if ran {
                if let Err(e) = s.pending_regs_entry(tid) {
                    return bad(e);
                }
            }
            if !crate::fp::write_vec(tid, &data) {
                return bad(Status::NoMemory);
            }
            if ran {
                if let Ok(p) = s.pending_regs_entry(tid) {
                    p.vec = true;
                }
            }
        }
    }
    Ok((None, None))
}

#[inline(never)]
#[allow(clippy::large_stack_frames)] // KT13: audited, a register image on the stack (T172 shrinks them)
fn state_read(
    s: &mut UserState,
    me: u64,
    tid: Tid,
    kind: u64,
    buf: u64,
    frame_ptr: Option<*mut crate::trap::TrapFrame>,
) -> Reply {
    use abi::thread_state as ts;
    let bytes: alloc::vec::Vec<u8> = match kind {
        ts::GENERAL => {
            let g = general_of(s, tid, frame_ptr).ok_or((Status::BadState, None, None))?;
            words_to_bytes(&g)?
        }
        ts::FP => words_to_bytes(&crate::fp::read_fp(tid))?,
        _ => crate::fp::read_vec(tid).ok_or((Status::NoMemory, None, None))?,
    };
    let _as = AsLocks::one(s, me);
    let k = kmem::get();
    let caller = s.procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
    kcore::usercopy::copy_to_user(caller.aspace.held_mut(), k.mm, k.vmos, k.jobs, buf, &bytes)
        .map_err(|e| (e, None, None))?;
    Ok((None, None))
}

// ---- jobs (S4.5, OD56-OD58) ----

/// The priority a thread created by the calling thread inherits: the caller's own base if it is a user thread, else
/// the default (10).
pub(super) fn creator_prio(s: &UserState) -> u8 {
    let me = thread::current();
    if s.threads.contains_key(&me) {
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
    locked_state(|s| -> Reply {
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
        if let Some(t) = s.thread_mut(tid) {
            t.sched_flags = flags;
        }
        thread::set_priority(tid, prio);
        Ok((None, None))
    })
}

/// `thread_get_sched(thread) -> sched, tid`: `sched` packs the base priority (bits 0 to 7), the flags (8 to 15) and
/// the effective priority (16 to 23: the base raised by what is donated to the thread, L23). Needs INSPECT (none for 0).
pub(super) fn thread_get_sched(a: [u64; 6]) -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::INSPECT)?;
        let prio = u64::from(thread::priority_of(tid).ok_or((Status::BadState, None, None))?);
        let eff = u64::from(thread::effective_priority_of(tid).unwrap_or(prio as u8));
        let flags = u64::from(s.thread(tid).map_or(0, |t| t.sched_flags));
        Ok((Some(prio | flags << 8 | eff << 16), Some(tid)))
    })
}

/// `job_set_sched(job, max_prio, rt_budget_ns, cpu_weight)`: needs SET_LIMITS. No value may exceed the parent job's;
/// lowering `max_prio` lowers the base priority of every thread in the job and below that is above it.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn job_set_sched(a: [u64; 6]) -> Reply {
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        // (`JOBS` is held only while the ledger is read: `thread::priority_of` takes the big kernel lock, which comes before it)
        let job = {
            let jobs = kmem::jobs();
            let job = ksys::job_set_sched(&s.objs, &s.procs, jobs.jobs, me, a)
                .map_err(|e| (e, None, None))?;
            thread::job_sched().sync_budgets(jobs.jobs); // (the scheduler's mirror of every job's RT budget)
            job
        };
        // PR1: the threads of the job and everything below it are held to the maximum their own job now allows. One thread at a time, in the same
        // critical section: nothing is collected, so the call cannot fail for want of memory (T248)
        let mut after = 0u64;
        while let Some((&tid, t)) = s.threads.range(after.saturating_add(1)..).next() {
            after = tid;
            let max = {
                let jobs = kmem::jobs();
                let Some(pj) = s.procs.get(&t.pid).map(|p| p.job) else {
                    continue;
                };
                match jobs.jobs.sched(pj) {
                    Some(l) if jobs.jobs.is_within(pj, job) => l.max_prio,
                    _ => continue,
                }
            };
            if thread::priority_of(tid).is_some_and(|now| now > max) {
                thread::set_priorities(&[(tid, max.max(1))]);
            }
        }
        Ok((None, None))
    })
}

/// `job_kill(job, code)`: end every process in the job and its descendants with exit code
/// `code` and forbid new processes and threads there (their limits become 0). Needs KILL. A
/// caller inside the job ends too, last.
pub(super) fn job_kill(a: [u64; 6]) -> Reply {
    let code = a[1] as i32;
    let (job, me_inside, me) = locked_state(|s| -> Result<(_, bool, u64), ReplyErr> {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (job, me_inside) =
            ksys::job_kill_begin(&s.objs, &s.procs, kmem::jobs().jobs, me, Handle(a[0]))
                .map_err(|e| (e, None, None))?;
        Ok((job, me_inside, me))
    })?;
    // (a cursor by process id, one victim per step: no list is made, so nothing here can be refused for want of memory. The handle is looked at
    // again at every step: a victim's teardown can let go of the big lock, the caller's other threads can close the handle, and a job id is
    // reused once its job is gone)
    let mut after = 0u64;
    while let Some(pid) = locked_state(|s| {
        ksys::job_kill_next(
            &s.objs,
            &s.procs,
            kmem::jobs().jobs,
            me,
            Handle(a[0]),
            job,
            after,
        )
    }) {
        after = pid;
        // (class N around each kill only: the caller's own exit below diverges)
        crate::heap::nofail(|| request_kill(pid, code));
    }
    if me_inside {
        exit_process(code);
    }
    Ok((None, None))
}

/// Where a new process's `thread_spawn` stacks start looking for a gap (HD2): a random page in the lower half of the stack arena (first fit from its
/// base when the kernel does not randomise).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the draw is below half the arena's pages, so the sum stays inside the arena
pub(super) fn random_stack_hint() -> u64 {
    let half = (abi::user::STACK_ARENA_END - abi::user::STACK_ARENA_BASE) / 4096 / 2;
    abi::user::STACK_ARENA_BASE + crate::entropy::placement_below(half) * 4096
}
