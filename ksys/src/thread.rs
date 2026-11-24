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
    /// The thread was made SUSPENDED: its spawner registers it stopped (`thread_registered_by_spawner`).
    pub suspended: bool,
    /// What the spawner records once it has the tid (`thread_registered_by_spawner`): the process, the thread object, the stack, round robin, and
    /// the register image the thread will start from (made, with the rest of the room, before anything changed: KT9).
    pub pid: u64,
    pub obj: Option<ObjId>,
    pub stack: Option<crate::SpawnStack>,
    pub rr: bool,
    /// Does `a1` carry the tid (a spawned thread) or 0 (a process's first thread)?
    pub spawned: bool,
    pub image: Option<kcore::fbox::FBox<[u64; 32]>>,
}

/// The register image (GENERAL layout: x1 to x31, then pc) a new thread starts from: `sp` in x2, `tp` in x4, `arg` in a0 and `a1` in a1.
pub fn initial_general(entry: u64, sp: u64, arg: u64, tp: u64, a1: u64) -> [u64; 32] {
    let mut w = [0u64; 32];
    w[1] = sp;
    w[3] = tp;
    w[9] = arg;
    w[10] = a1;
    w[31] = entry;
    w
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
    us.starts.insert_reserved(
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
            suspended: false,
            a2: 0,
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
    jobs: &mut Jobs,
    me: u64,
    creator_prio: u8,
    a: [u64; 6],
) -> Result<Spawn, Status> {
    use abi::spawn as sp;
    let ph = Handle(a[0]);
    // the record first (a bad pointer is the first error), then the handle
    let raw = {
        let c = us.procs.get_mut(&me).ok_or(Status::BadState)?;
        copy_from_user(c.aspace.held_mut(), mm, vmos, jobs, a[1], sp::ARGS_BYTES)?
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
    let max = jobs.sched(tjob).map_or(10, |j| j.max_prio);
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
    let entry_ok = target
        .aspace
        .held_ref()
        .region_perms_at(entry)
        .is_some_and(|p| p.exec);
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
        || flags & !(sp::ROUND_ROBIN | sp::DETACHED | sp::CALLER_STACK | sp::SUSPENDED) != 0
        // (a suspended thread nobody holds a handle to could never be resumed)
        || (flags & sp::SUSPENDED != 0 && flags & sp::DETACHED != 0)
    {
        return Err(Status::InvalidArgs);
    }
    if total_threads(us) >= us.backstops.user_threads {
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
            .held_ref()
            .find_gap(lo, hi, total, target.stack_hint)
            .or_else(|| target.aspace.held_ref().find_gap(lo, hi, total, lo));
        match at {
            Some(at) => Some(at + guard_len),
            None => return Err(Status::NoMemory),
        }
    };
    let prio = if prio_req == 0 {
        clamp_prio(jobs, tjob, creator_prio)
    } else {
        prio_req as u8
    };
    let detached = flags & sp::DETACHED != 0;
    // (KT9: every allocation the registration needs is made here, before anything is charged or changed: the thread's record, its start, its
    // place in the process, and its register image)
    let mut image = kcore::fbox::FBox::try_new([0u64; 32]).map_err(|_| Status::NoMemory)?;
    us.reserve_thread(tpid)?;
    charge_thread(jobs, tjob).map_err(crate::job_status)?;
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
                jobs,
                job: Some(job),
            },
        ) {
            Ok(x) => Some(x),
            Err(e) => {
                credit_thread(jobs, tjob);
                return Err(e);
            }
        }
    };
    // the stack: a VMO of its own (nobody holds a handle), mapped read-write; its table pages are charged like any mapping's
    let mut stack = None;
    let mut top = caller_sp;
    if let Some(region) = place {
        let tables = stack_len / 4096 / 512 + 4;
        let undo = |made: Option<(Handle, ObjId)>,
                    procs: &mut crate::ProcTable,
                    objs: &mut kcore::ipc::Kobjs,
                    jobs: &mut Jobs| {
            if let Some((h, _)) = made {
                if let Some(c) = procs.get_mut(&me) {
                    let _ = c.handles.close(objs, h);
                }
            }
            credit_thread(jobs, tjob);
        };
        if let Err(e) = jobs.charge(tjob, Resource::Pages, tables) {
            undo(made, procs, objs, jobs);
            return Err(crate::job_status(e));
        }
        if let Err(e) = vmos.reserve_one() {
            undo(made, procs, objs, jobs);
            jobs.credit(tjob, Resource::Pages, tables);
            return Err(e);
        }
        // (KT9: the room for the stack's page-table charge record, before the VMO exists)
        if procs
            .get_mut(&tpid)
            .expect("target exists")
            .table_charge
            .reserve(1)
            .is_err()
        {
            undo(made, procs, objs, jobs);
            jobs.credit(tjob, Resource::Pages, tables);
            return Err(Status::NoMemory);
        }
        let vmo = vmos.create_in(jobs, stack_len / 4096, tjob);
        let perms = Perms {
            read: true,
            write: true,
            exec: false,
            user: true,
        };
        let t = procs.get_mut(&tpid).expect("target exists");
        t.aspace
            .held_mut()
            .set_region_limit(jobs.region_limit(t.job));
        if let Err(e) =
            t.aspace
                .held_mut()
                .map_region(vmos, region, stack_len / 4096, vmo, 0, perms)
        {
            let _ = vmos.release(mm, jobs, vmo);
            jobs.credit(tjob, Resource::Pages, tables);
            undo(made, procs, objs, jobs);
            return Err(match e {
                kcore::aspace::RegionError::TooMany => Status::NoMemory,
                _ => Status::InvalidArgs,
            });
        }
        t.table_charge.insert_reserved(region, tables);
        t.stack_hint = region + stack_len;
        stack = Some(crate::SpawnStack { region, vmo });
        top = region + stack_len;
    }
    procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
    // (AB7: every thread of a process starts with the system library's base in `a2`, so that code that starts at the program's entry finds it)
    let lib_base = procs.get(&tpid).map_or(0, |p| p.libsys.base);
    let token = new_start(us, tpid, entry, top, arg, made.map(|(_, o)| o));
    let st = us.starts.get_mut(&token).expect("just registered");
    st.tp = tp;
    st.stack = stack;
    st.rr = flags & sp::ROUND_ROBIN != 0;
    st.spawned = true;
    st.a2 = lib_base;
    let suspended = flags & sp::SUSPENDED != 0;
    st.suspended = suspended;
    if let (true, Some((_, o))) = (suspended, made) {
        ipc::thread_set_suspended(&mut us.objs, o, true);
    }
    *image = initial_general(entry, top, arg, tp, 0);
    if let Some(w) = image.get_mut(11) {
        *w = lib_base; // (`a2`: AB7)
    }
    Ok(Spawn {
        token,
        handle: made.map_or(0, |(h, _)| h.0),
        prio,
        suspended,
        pid: tpid,
        obj: made.map(|(_, o)| o),
        stack,
        rr: flags & sp::ROUND_ROBIN != 0,
        spawned: true,
        image: Some(image),
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
    map: &mut dyn crate::StartMap,
    me: u64,
    a: [u64; 6],
) -> Result<Spawn, Status> {
    let (ph, bh) = (Handle(a[0]), Handle(a[3]));
    let (entry, sp, arg, features) = (a[1], a[2], a[4], a[5]);
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
    // HD5: the `features` word is reserved: the named mechanisms are not there (NOT_SUPPORTED), anything else is not a feature (INVALID_ARGS)
    if features & !abi::start_features::ALL != 0 {
        return Err(Status::InvalidArgs);
    }
    if features != 0 {
        return Err(Status::NotSupported);
    }
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    if target.state != crate::ProcState::Created
        || target.exit_code.is_some()
        || target.killed.is_some()
    {
        return Err(Status::BadState);
    }
    if total_threads(us) >= us.backstops.user_threads {
        return Err(Status::NoMemory);
    }
    // (KT9: the first thread's record, start and register image are made before anything changes)
    let mut image = kcore::fbox::FBox::try_new([0u64; 32]).map_err(|_| Status::NoMemory)?;
    us.reserve_thread(tpid)?;
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    let entry_ok = target
        .aspace
        .held_ref()
        .region_perms_at(entry)
        .is_some_and(|p| p.exec);
    let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
    if !entry_ok || !sp_ok {
        return Err(Status::InvalidArgs);
    }
    let tjob = target.job;
    // The thread's charge comes before anything changes: a refusal must leave the caller's handle with the caller and the process exactly as it
    // was (SYSCALLS.md: on any error nothing changes), or a retry would find handle 1 taken.
    charge_thread(jobs, tjob).map_err(crate::job_status)?;
    // (room in the target's table first, in memory: the handle is moved only when its arrival cannot fail)
    {
        let t = us.procs.get_mut(&tpid).expect("target exists");
        // (and, KT9, the records of the library's page-table charge: made after the mapping, with nothing to allocate)
        if let Err(e) = t.handles.reserve(1).and_then(|()| t.reserve_charge()) {
            credit_thread(jobs, tjob);
            return Err(e);
        }
    }
    // AB7: the system library goes into the address space now, while an error can still undo it (the charge for its page tables as the mappings of
    // `process_map` are charged: its span's worth, recorded like theirs).
    let (lib, span) = {
        let t = us.procs.get_mut(&tpid).expect("target exists");
        match map.map(t.aspace.held_mut(), jobs) {
            Ok(x) => x,
            Err(e) => {
                credit_thread(jobs, tjob);
                return Err(e);
            }
        }
    };
    // (as `process_map`'s estimate: a leaf table per 512 pages and four more; the span is a few pages)
    let tables = if span == 0 {
        0
    } else {
        (span / 4096 / 512).saturating_add(4)
    };
    if tables > 0 {
        if let Err(x) = jobs.charge(tjob, Resource::Pages, tables) {
            let t = us.procs.get_mut(&tpid).expect("target exists");
            map.unmap(t.aspace.held_mut(), jobs, lib, span);
            credit_thread(jobs, tjob);
            return Err(crate::job_status(x));
        }
    }
    // Commit: the channel end moves into the new process as its handle 1.
    let first_gen = us.first_handle_gen;
    let UserState { procs, objs, .. } = us;
    let caller = procs.get_mut(&me).expect("caller exists");
    let moved = match caller.handles.remove_for_transfer(objs, bh) {
        Ok(m) => m,
        Err(e) => {
            let t = procs.get_mut(&tpid).expect("target exists");
            map.unmap(t.aspace.held_mut(), jobs, lib, span);
            jobs.credit(tjob, Resource::Pages, tables);
            credit_thread(jobs, tjob);
            return Err(e);
        }
    };
    let target = procs.get_mut(&tpid).expect("target exists");
    let first = target
        .handles
        .insert(objs, moved.obj, moved.rights)
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
    target.libsys = lib;
    if tables > 0 {
        target.add_charge(lib.base, tables, lib.base.saturating_add(span));
    }
    target.state = crate::ProcState::Running;
    target.threads_alive = 1;
    // the first thread of a process starts at the default priority, clamped to its job's maximum
    let prio = clamp_prio(jobs, tjob, 10);
    let token = new_start(us, tpid, entry, sp, arg, None);
    if let Some(t) = us.starts.get_mut(&token) {
        t.a2 = lib.base;
    }
    *image = initial_general(entry, sp, arg, 0, 0);
    if let Some(w) = image.get_mut(11) {
        *w = lib.base; // (`a2`: the system library's base, AB7)
    }
    Ok(Spawn {
        token,
        handle: 0,
        prio,
        suspended: false,
        pid: tpid,
        obj: None,
        stack: None,
        rr: false,
        spawned: false,
        image: Some(image),
    })
}

// ---- EX1: thread_suspend, thread_resume, thread state ----

use kcore::suspend::{AtReturn, Resumed, Suspend};

/// The thread object of a live thread of `tid`, or `BadState`.
fn thread_obj(us: &UserState, pid: u64, tid: kcore::sched::Tid) -> Result<ObjId, Status> {
    us.threads
        .get(&tid)
        .filter(|t| t.pid == pid)
        .and_then(|t| t.obj)
        .ok_or(Status::BadState)
}

/// `thread_suspend(thread)`: MANAGE; the thread's suspend count goes up. `blocked` says whether the thread is blocked in a call right now (it has
/// stopped where it is). Returns the thread and whether SUSPENDED rose; the caller makes the thread stop at its next return to user mode (a
/// reschedule interrupt for one running user code on another hart) and commits the signal.
pub fn thread_suspend(
    us: &mut UserState,
    me: u64,
    current: kcore::sched::Tid,
    h: u64,
    blocked: impl FnOnce(kcore::sched::Tid) -> bool,
) -> Result<(kcore::sched::Tid, bool), Status> {
    let (tid, pid) = crate::thread_target(&us.objs, &us.procs, me, current, h, Rights::MANAGE)?;
    let obj = thread_obj(us, pid, tid)?;
    let blocked = blocked(tid);
    let parked = us.threads.get(&tid).is_some_and(|t| t.parked);
    let entry = us.suspend_entry(tid).ok_or(Status::BadState)?;
    let mut raise = entry.suspend(blocked).ok_or(Status::NoMemory)?;
    if parked {
        // asleep in the park at the return to user mode (a stopped process, or a thread that was resumed and has not run yet to leave it): it is
        // parked, not blocked in a call, and SUSPENDED rises now if it was not up
        if let AtReturn::Park { raise: r } = entry.return_to_user() {
            raise |= r;
        }
    }
    if raise {
        ipc::thread_set_suspended(&mut us.objs, obj, true);
    }
    Ok((tid, raise))
}

/// `thread_resume(thread)`: MANAGE; the count goes down; at 0 SUSPENDED falls. Returns the thread and whether a parked thread is to be woken.
/// BAD_STATE when the thread is not suspended.
pub fn thread_resume(
    us: &mut UserState,
    me: u64,
    current: kcore::sched::Tid,
    h: u64,
) -> Result<(kcore::sched::Tid, bool), Status> {
    let (tid, pid) = crate::thread_target(&us.objs, &us.procs, me, current, h, Rights::MANAGE)?;
    let obj = thread_obj(us, pid, tid)?;
    let entry = us.suspend_of_mut(tid).ok_or(Status::BadState)?;
    let r = entry.resume().ok_or(Status::BadState)?;
    match r {
        Resumed::Still => Ok((tid, false)),
        Resumed::Running { clear, wake } => {
            us.suspend_remove(tid);
            if clear {
                ipc::thread_set_suspended(&mut us.objs, obj, false);
            }
            Ok((tid, wake))
        }
    }
}

/// The thread `tid` of `pid` reached the return to user mode: does it park (and has SUSPENDED risen)? Called by the thread itself.
pub fn thread_at_return(us: &mut UserState, pid: u64, tid: kcore::sched::Tid) -> AtReturn {
    let Some(entry) = us.suspend_of_mut(tid) else {
        return AtReturn::Run;
    };
    let r = entry.return_to_user();
    if let AtReturn::Park { raise: true } = r {
        if let Ok(obj) = thread_obj(us, pid, tid) {
            ipc::thread_set_suspended(&mut us.objs, obj, true);
        }
    }
    r
}

/// The thread is about to block in a call with a suspend pending and SUSPENDED not yet up: it has stopped where it is.
pub fn thread_blocks(us: &mut UserState, pid: u64, tid: kcore::sched::Tid) -> bool {
    let Some(entry) = us.suspend_of_mut(tid) else {
        return false;
    };
    let raise = entry.blocks();
    if raise {
        if let Ok(obj) = thread_obj(us, pid, tid) {
            ipc::thread_set_suspended(&mut us.objs, obj, true);
        }
    }
    raise
}

/// Is the thread suspended (its count is above 0)?
pub fn thread_is_suspended(us: &UserState, tid: kcore::sched::Tid) -> bool {
    us.suspend_of(tid).is_some_and(|s| s.active())
}

/// Is the thread suspended with SUSPENDED not up (a resume and a new suspend came before the woken thread ran)? A parked thread that finds this
/// leaves its sleep and parks again, which raises the signal.
pub fn thread_stop_pending(us: &UserState, tid: kcore::sched::Tid) -> bool {
    us.suspend_of(tid)
        .is_some_and(|s| s.active() && !s.stopped())
}

/// A thread that was parked leaves the return to user mode (woken with a count of 0, or killed).
pub fn thread_unparked(us: &mut UserState, tid: kcore::sched::Tid) {
    if let Some(s) = us.suspend_of_mut(tid) {
        s.unparked();
    }
}

/// What `thread_read_state` and `thread_write_state` may act on: the thread (MANAGE) that has stopped: suspended and parked at the return to
/// user mode, or (for a read) blocked in a call where it was suspended. BAD_STATE otherwise, and for a write while the thread is blocked.
pub fn thread_state_target(
    us: &UserState,
    me: u64,
    current: kcore::sched::Tid,
    h: u64,
    write: bool,
) -> Result<(kcore::sched::Tid, u64), Status> {
    if h == 0 {
        // (a thread cannot read its own registers: it is running)
        return Err(Status::BadState);
    }
    let (tid, pid) = crate::thread_target(&us.objs, &us.procs, me, current, h, Rights::MANAGE)?;
    let s = us.suspend_of(tid).ok_or(Status::BadState)?;
    if !s.stopped() {
        return Err(Status::BadState);
    }
    if write && !s.parked() {
        return Err(Status::BadState);
    }
    Ok((tid, pid))
}

/// A thread that ends takes its record, and with it its suspend state, its register images and its flags.
#[inline(never)]
pub fn thread_suspend_forget(us: &mut UserState, tid: kcore::sched::Tid) {
    us.unregister_thread(tid);
}

/// The spawner's registration of a new thread, once it has the tid (EX1, KT9): the record goes in with the room `thread_spawn` made, so the
/// thread is known to `thread_suspend`, kill and the rest from the moment the call returns and nothing here can fail. A SUSPENDED thread is
/// stopped, with its register image, before the handle that can resume it is handed back (it has not run: it is where a thread parked at the
/// return to user mode is).
#[inline(never)]
pub fn thread_registered_by_spawner(us: &mut UserState, tid: kcore::sched::Tid, sp: &mut Spawn) {
    let mut rec = crate::UThread {
        pid: sp.pid,
        obj: sp.obj,
        sched_flags: u8::from(sp.rr),
        ..Default::default()
    };
    if let Some(mut image) = sp.image.take() {
        if sp.spawned {
            if let Some(w) = image.get_mut(10) {
                *w = tid; // (`a1` is the tid for a spawned thread)
            }
        }
        rec.initial_regs = Some(image);
    }
    if sp.suspended {
        let mut s = Suspend::new();
        let _ = s.suspend(true);
        let _ = s.return_to_user();
        rec.suspend = Some(s);
        us.n_suspended = us.n_suspended.saturating_add(1);
    }
    us.register_thread(tid, rec, sp.stack);
    if let Some(o) = sp.obj {
        ipc::thread_set_tid(&mut us.objs, o, tid);
        if sp.suspended {
            ipc::thread_set_suspended(&mut us.objs, o, true);
        }
    }
}
