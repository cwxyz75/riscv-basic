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
#[derive(PartialEq, Eq)]
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

// (by hand, not derived: the derived `fmt` builds arrays of every field's name and value, over KT13's 1 KiB frame in an unoptimised build)
impl core::fmt::Debug for Spawn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Spawn");
        debug_spawn_0(self, &mut d);
        debug_spawn_1(self, &mut d);
        d.finish()
    }
}

/// Fields of `Spawn`'s `Debug`, a few per frame (KT13).
#[inline(never)]
fn debug_spawn_0(s: &Spawn, d: &mut core::fmt::DebugStruct<'_, '_>) {
    d.field("token", &s.token);
    d.field("handle", &s.handle);
    d.field("prio", &s.prio);
    d.field("suspended", &s.suspended);
    d.field("pid", &s.pid);
    d.field("obj", &s.obj);
}

/// Fields of `Spawn`'s `Debug`, a few per frame (KT13).
#[inline(never)]
fn debug_spawn_1(s: &Spawn, d: &mut core::fmt::DebugStruct<'_, '_>) {
    d.field("stack", &s.stack);
    d.field("rr", &s.rr);
    d.field("spawned", &s.spawned);
    d.field("image", &s.image);
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
/// spawns the kernel thread `token` names at `prio` and returns the handle and the tid. Each step is a frame of its own (KT13).
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
    let args = spawn_args(us, mm, vmos, jobs, me, creator_prio, &a)?;
    spawn_rest(us, (mm, vmos, jobs), me, &args)
}

/// `thread_spawn` after its checks. A frame of its own (KT13).
#[inline(never)]
fn spawn_rest<M: PhysMem>(
    us: &mut UserState,
    (mm, vmos, jobs): (&mut Mm<M>, &mut Vmos, &mut Jobs),
    me: u64,
    args: &SpawnArgs,
) -> Result<Spawn, Status> {
    // (KT9: every allocation the registration needs is made here, before anything is charged or changed: the thread's record, its start, its
    // place in the process, and its register image)
    let image = new_image()?;
    us.reserve_thread(args.tpid)?;
    charge_thread(jobs, args.tjob).map_err(crate::job_status)?;
    let made = spawn_object(us, jobs, me, args)?;
    // the stack: a VMO of its own (nobody holds a handle), mapped read-write; its table pages are charged like any mapping's
    let stack = match args.place {
        Some(region) => Some(spawn_stack(us, (mm, vmos, jobs), me, args, region, made)?),
        None => None,
    };
    Ok(spawn_register(us, args, made, stack, image))
}

/// A `thread_spawn` call, checked (`spawn_args`).
struct SpawnArgs {
    tpid: u64,
    tjob: JobId,
    entry: u64,
    arg: u64,
    tp: u64,
    flags: u32,
    caller_sp: u64,
    stack_len: u64,
    /// Where the kernel's stack goes (above its guard); `None`: the caller brings its own.
    place: Option<u64>,
    prio: u8,
}

/// A new thread's register image, in its box (zeroed: `initial_general` fills it). NO_MEMORY without memory. A frame of its own (KT13).
#[inline(never)]
pub(crate) fn new_image() -> Result<kcore::fbox::FBox<[u64; 32]>, Status> {
    let room = kcore::fbox::FBox::room().ok_or(Status::NoMemory)?;
    Ok(room.fill_with(|| [0u64; 32]))
}

/// Fill a new thread's register image (`initial_general`, with the system library's base in `a2`: AB7), in its box.
#[inline(never)]
pub(crate) fn fill_image(
    image: &mut [u64; 32],
    (entry, sp, arg, tp, a1): (u64, u64, u64, u64, u64),
    lib_base: u64,
) {
    *image = initial_general(entry, sp, arg, tp, a1);
    if let Some(w) = image.get_mut(11) {
        *w = lib_base; // (`a2`: AB7)
    }
}

/// The 64-byte spawn record at `ptr` (in the caller's memory), as its eight words (into `w`). A bad pointer is the call's first error.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: the record is copied in whole (64 bytes) and read at word indices below 8, so the slices are inside it and `try_into` of eight bytes cannot fail
#[inline(never)]
fn spawn_record<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    ptr: u64,
    w: &mut [u64; 8],
) -> Result<(), Status> {
    let c = us.procs.get_mut(&me).ok_or(Status::BadState)?;
    let raw = copy_from_user(
        c.aspace.held_mut(),
        mm,
        vmos,
        jobs,
        ptr,
        abi::spawn::ARGS_BYTES,
    )?;
    for (i, x) in w.iter_mut().enumerate() {
        *x = u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
    }
    Ok(())
}

/// `thread_spawn`'s checks: the record first (a bad pointer is the first error), then the handle, the priority, the target's state, the entry,
/// the sizes and the flags, the thread backstop, and the place of the stack. Nothing changes. A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: stack_bytes and guard_bytes are at most MAX_STACK (checked into `sizes_ok`), so the sums are far below u64::MAX
#[inline(never)]
fn spawn_args<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    creator_prio: u8,
    a: &[u64; 6],
) -> Result<SpawnArgs, Status> {
    let mut w = [0u64; 8];
    spawn_record(us, mm, vmos, jobs, me, a[1], &mut w)?;
    let prio_req = (w[5] & 0xffff_ffff) as u32;
    let flags = (w[5] >> 32) as u32;
    let tpid = spawn_target(us, me, a[0])?;
    let (tjob, stack_len, place) = spawn_checks(us, jobs, tpid, &w)?;
    let prio = if prio_req == 0 {
        clamp_prio(jobs, tjob, creator_prio)
    } else {
        prio_req as u8
    };
    Ok(SpawnArgs {
        tpid,
        tjob,
        entry: w[0],
        arg: w[1],
        tp: w[2],
        flags,
        caller_sp: w[6],
        stack_len,
        place,
        prio,
    })
}

/// `spawn_args`'s checks against the target process `tpid` (`spawn_valid`), the thread backstop, and the stack's plan (`stack_plan`): the
/// target's job, the stack's length and its place.
#[inline(never)]
fn spawn_checks(
    us: &UserState,
    jobs: &Jobs,
    tpid: u64,
    w: &[u64; 8],
) -> Result<(JobId, u64, Option<u64>), Status> {
    let target = us.procs.get(&tpid).ok_or(Status::BadState)?;
    spawn_valid(target, jobs, w)?;
    if total_threads(us) >= us.backstops.user_threads {
        return Err(Status::NoMemory);
    }
    let own_stack = (w[5] >> 32) as u32 & abi::spawn::CALLER_STACK != 0;
    let (stack_len, place) = stack_plan(target, own_stack, (w[3], w[4]))?;
    Ok((target.job, stack_len, place))
}

/// The process `thread_spawn`'s handle `h` names (MANAGE).
#[inline(never)]
fn spawn_target(us: &UserState, me: u64, h: u64) -> Result<u64, Status> {
    let caller = us.procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(&us.objs, &caller.handles, Handle(h), Rights::MANAGE)?;
    Ok(tpid)
}

/// `spawn_args`'s checks of the record against the target (in the order of SYSCALLS.md): the priority (above the job's maximum it is refused,
/// ACCESS_DENIED, not clamped: the program asked for it), the target's state, then the entry, the sizes, the stack pointer and the flags.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: none: comparisons only
#[inline(never)]
fn spawn_valid(target: &crate::Process, jobs: &Jobs, w: &[u64; 8]) -> Result<(), Status> {
    use abi::spawn as sp;
    let (entry, stack_bytes, guard_bytes) = (w[0], w[3], w[4]);
    let prio_req = (w[5] & 0xffff_ffff) as u32;
    let flags = (w[5] >> 32) as u32;
    let (caller_sp, reserved) = (w[6], w[7]);
    let max = jobs.sched(target.job).map_or(10, |j| j.max_prio);
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
    Ok(())
}

/// The length of a new thread's stack and where the kernel puts it (`None`: the caller brings its own): a gap in the arena for the guard and
/// the stack, at or after the hint, wrapping once.
#[inline(never)]
fn stack_plan(
    target: &crate::Process,
    own_stack: bool,
    (stack_bytes, guard_bytes): (u64, u64),
) -> Result<(u64, Option<u64>), Status> {
    use abi::spawn as sp;
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
    if own_stack {
        return Ok((stack_len, None));
    }
    Ok((stack_len, Some(stack_place(target, stack_len, guard_len)?)))
}

/// Where a new stack of `stack_len` bytes goes in `target`'s stack arena (above its guard of `guard_len`): a gap for the guard, the stack and
/// one more guard above it (the next stack up has a guard of its own below *its* start, so two stacks are never flush: a neighbour's overflow
/// always faults), at or after the hint, wrapping once. NO_MEMORY: no gap.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the lengths are at most MAX_STACK each
#[inline(never)]
fn stack_place(target: &crate::Process, stack_len: u64, guard_len: u64) -> Result<u64, Status> {
    let (lo, hi) = (abi::user::STACK_ARENA_BASE, abi::user::STACK_ARENA_END);
    let total = guard_len + stack_len + guard_len;
    let at = target
        .aspace
        .held_ref()
        .find_gap(lo, hi, total, target.stack_hint)
        .or_else(|| target.aspace.held_ref().find_gap(lo, hi, total, lo));
    match at {
        Some(at) => Ok(at + guard_len),
        None => Err(Status::NoMemory),
    }
}

/// `thread_spawn`'s thread object and the caller's handle to it (not for a detached thread: `None`). On `Err` the thread's charge is credited.
/// A frame of its own (KT13).
#[inline(never)]
fn spawn_object(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u64,
    args: &SpawnArgs,
) -> Result<Option<(Handle, ObjId)>, Status> {
    if args.flags & abi::spawn::DETACHED != 0 {
        return Ok(None);
    }
    let UserState { objs, procs, .. } = us;
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    let job = caller.job;
    match ipc::thread_object_create_in(
        objs,
        &mut caller.handles,
        args.tpid,
        &mut Quota {
            jobs,
            job: Some(job),
        },
    ) {
        Ok(x) => Ok(Some(x)),
        Err(e) => {
            credit_thread(jobs, args.tjob);
            Err(e)
        }
    }
}

/// `thread_spawn`'s stack at `region`: a VMO of its own (nobody holds a handle), mapped read-write; its table pages are charged like any
/// mapping's. On `Err` everything the call did is undone (the handle `made`, the thread's charge). A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: the stack's length is at most MAX_STACK; the target process was looked up under the same lock
#[inline(never)]
fn spawn_stack<M: PhysMem>(
    us: &mut UserState,
    (mm, vmos, jobs): (&mut Mm<M>, &mut Vmos, &mut Jobs),
    me: u64,
    args: &SpawnArgs,
    region: u64,
    made: Option<(Handle, ObjId)>,
) -> Result<crate::SpawnStack, Status> {
    let (tpid, tjob, stack_len) = (args.tpid, args.tjob, args.stack_len);
    let UserState { objs, procs, .. } = us;
    let tables = stack_len / 4096 / 512 + 4;
    if let Err((e, charged)) = stack_room(procs, vmos, jobs, (tpid, tjob), tables) {
        undo_spawn(made, procs, objs, jobs, (me, tjob));
        if charged {
            jobs.credit(tjob, Resource::Pages, tables);
        }
        return Err(e);
    }
    let t = procs.get_mut(&tpid).expect("target exists");
    match stack_map(t, (mm, vmos, jobs), (region, stack_len, tables)) {
        Ok(vmo) => Ok(crate::SpawnStack { region, vmo }),
        Err(e) => {
            jobs.credit(tjob, Resource::Pages, tables);
            undo_spawn(made, procs, objs, jobs, (me, tjob));
            Err(e)
        }
    }
}

/// `spawn_stack`'s room, before the VMO exists: the page-table charge (`tables` pages), a VMO slot, and the charge's record (KT9). On `Err`,
/// whether the charge was made (the caller credits it).
#[allow(clippy::expect_used)] // INVARIANT: the target process was looked up under the same lock
#[inline(never)]
fn stack_room(
    procs: &mut crate::ProcTable,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    (tpid, tjob): (u64, JobId),
    tables: u64,
) -> Result<(), (Status, bool)> {
    jobs.charge(tjob, Resource::Pages, tables)
        .map_err(|e| (crate::job_status(e), false))?;
    vmos.reserve_one().map_err(|e| (e, true))?;
    procs
        .get_mut(&tpid)
        .expect("target exists")
        .table_charge
        .reserve(1)
        .map_err(|_| (Status::NoMemory, true))
}

/// Undo `thread_spawn`'s handle to the thread's object (if it made one) and the thread's charge.
#[inline(never)]
fn undo_spawn(
    made: Option<(Handle, ObjId)>,
    procs: &mut crate::ProcTable,
    objs: &mut kcore::ipc::Kobjs,
    jobs: &mut Jobs,
    (me, tjob): (u64, JobId),
) {
    if let Some((h, _)) = made {
        if let Some(c) = procs.get_mut(&me) {
            let _ = c.handles.close(objs, h);
        }
    }
    credit_thread(jobs, tjob);
}

/// `spawn_stack`'s VMO (room for it was reserved), mapped read-write at `region` in `t`, with the page-table charge's record (its room was
/// reserved too). On `Err` the VMO goes again. A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the stack's length is at most MAX_STACK
#[inline(never)]
fn stack_map<M: PhysMem>(
    t: &mut crate::Process,
    (mm, vmos, jobs): (&mut Mm<M>, &mut Vmos, &mut Jobs),
    (region, stack_len, tables): (u64, u64, u64),
) -> Result<u64, Status> {
    let vmo = vmos.create_in(jobs, stack_len / 4096, t.job);
    let perms = Perms {
        read: true,
        write: true,
        exec: false,
        user: true,
    };
    t.aspace
        .held_mut()
        .set_region_limit(jobs.region_limit(t.job));
    if let Err(e) = t
        .aspace
        .held_mut()
        .map_region(vmos, region, stack_len / 4096, vmo, 0, perms)
    {
        let _ = vmos.release(mm, jobs, vmo);
        return Err(match e {
            kcore::aspace::RegionError::TooMany => Status::NoMemory,
            _ => Status::InvalidArgs,
        });
    }
    t.table_charge.insert_reserved(region, tables);
    t.stack_hint = region + stack_len;
    Ok(vmo)
}

/// A spawned thread's start, registered by `new_start`, completed: its thread pointer, its stack, round robin, SUSPENDED, and the system
/// library's base in `a2` (AB7).
#[allow(clippy::expect_used)] // INVARIANT: the start was registered by the caller just before
#[inline(never)]
fn fill_start(
    us: &mut UserState,
    token: usize,
    args: &SpawnArgs,
    stack: Option<crate::SpawnStack>,
    lib_base: u64,
) {
    use abi::spawn as sp;
    let st = us.starts.get_mut(&token).expect("just registered");
    st.tp = args.tp;
    st.stack = stack;
    st.rr = args.flags & sp::ROUND_ROBIN != 0;
    st.spawned = true;
    st.a2 = lib_base;
    st.suspended = args.flags & sp::SUSPENDED != 0;
}

/// The end of `thread_spawn` (nothing can fail): the thread counted in its process, its start registered (with the system library's base in
/// `a2`: AB7, every thread of a process starts with it so that code that starts at the program's entry finds it), a SUSPENDED thread's object
/// marked, and its register image filled.
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: the target process was looked up under the same lock; the start was registered just above
#[inline(never)]
fn spawn_register(
    us: &mut UserState,
    args: &SpawnArgs,
    made: Option<(Handle, ObjId)>,
    stack: Option<crate::SpawnStack>,
    mut image: kcore::fbox::FBox<[u64; 32]>,
) -> Spawn {
    let top = stack.map_or(args.caller_sp, |s| s.region + args.stack_len);
    use abi::spawn as sp;
    let (tpid, flags) = (args.tpid, args.flags);
    us.procs
        .get_mut(&tpid)
        .expect("target exists")
        .threads_alive += 1;
    let lib_base = us.procs.get(&tpid).map_or(0, |p| p.libsys.base);
    let token = new_start(us, tpid, args.entry, top, args.arg, made.map(|(_, o)| o));
    let suspended = flags & sp::SUSPENDED != 0;
    fill_start(us, token, args, stack, lib_base);
    if let (true, Some((_, o))) = (suspended, made) {
        ipc::thread_set_suspended(&mut us.objs, o, true);
    }
    fill_image(
        &mut image,
        (args.entry, top, args.arg, args.tp, 0),
        lib_base,
    );
    Spawn {
        token,
        handle: made.map_or(0, |(h, _)| h.0),
        prio: args.prio,
        suspended,
        pid: tpid,
        obj: made.map(|(_, o)| o),
        stack,
        rr: flags & sp::ROUND_ROBIN != 0,
        spawned: true,
        image: Some(image),
    }
}

/// `process_start(process, entry, sp, channel, arg)` (SYSCALLS.md): move the bootstrap channel end into a Created process as its handle 1 and register
/// its first thread. The caller spawns the kernel thread `token` names at `prio`. On any error nothing has changed. Each step is a frame of its own
/// (KT13).
#[inline(never)]
pub fn process_start(
    us: &mut UserState,
    jobs: &mut Jobs,
    map: &mut dyn crate::StartMap,
    me: u64,
    a: [u64; 6],
) -> Result<Spawn, Status> {
    let a = &a;
    let tpid = start_target(us, me, a)?;
    // (KT9: the first thread's record, start and register image are made before anything changes)
    let image = new_image()?;
    us.reserve_thread(tpid)?;
    let tjob = start_entry(us, tpid, a)?;
    // The thread's charge comes before anything changes: a refusal must leave the caller's handle with the caller and the process exactly as it
    // was (SYSCALLS.md: on any error nothing changes), or a retry would find handle 1 taken.
    charge_thread(jobs, tjob).map_err(crate::job_status)?;
    start_rest(us, jobs, map, (me, tpid, tjob), a, image)
}

/// `process_start` once the thread is charged: the library (`start_lib`), the commit (`start_commit`) and the first thread's start.
#[inline(never)]
fn start_rest(
    us: &mut UserState,
    jobs: &mut Jobs,
    map: &mut dyn crate::StartMap,
    (me, tpid, tjob): (u64, u64, JobId),
    a: &[u64; 6],
    image: kcore::fbox::FBox<[u64; 32]>,
) -> Result<Spawn, Status> {
    let (lib, span, tables) = start_lib(us, jobs, map, (tpid, tjob))?;
    start_commit(us, jobs, map, (me, tpid, tjob), a, (lib, span, tables))?;
    Ok(start_register(us, jobs, (tpid, tjob), a, lib, image))
}

/// `process_start`'s checks before anything is made: the two handles (a process with MANAGE, a channel with TRANSFER), the reserved `features`
/// word (HD5: the named mechanisms are not there, NOT_SUPPORTED; anything else is not a feature, INVALID_ARGS), the target's state (Created),
/// and the thread backstop. The target. A frame of its own (KT13).
#[inline(never)]
fn start_target(us: &UserState, me: u64, a: &[u64; 6]) -> Result<u64, Status> {
    let (ph, bh) = (Handle(a[0]), Handle(a[3]));
    let features = a[5];
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
    if features & !abi::start_features::ALL != 0 {
        return Err(Status::InvalidArgs);
    }
    if features != 0 {
        return Err(Status::NotSupported);
    }
    start_ready(us, tpid)?;
    Ok(tpid)
}

/// `process_start`'s target is Created and not ending, and the thread backstop has room.
#[inline(never)]
fn start_ready(us: &UserState, tpid: u64) -> Result<(), Status> {
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
    Ok(())
}

/// `process_start`'s entry (in an executable region of the target) and stack pointer (16-byte aligned, in the user range): the target's job.
#[inline(never)]
fn start_entry(us: &UserState, tpid: u64, a: &[u64; 6]) -> Result<JobId, Status> {
    let (entry, sp) = (a[1], a[2]);
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
    Ok(target.job)
}

/// `process_start`'s room in the target's table (in memory: the handle is moved only when its arrival cannot fail) and, KT9, the records of the
/// library's page-table charge; then the system library mapped (AB7: now, while an error can still undo it) and its page tables charged as the
/// mappings of `process_map` are (its span's worth: a leaf table per 512 pages and four more). On `Err` the thread's charge is credited and the
/// library unmapped. (The library, its span, and the tables charged.) A frame of its own (KT13).
#[allow(clippy::expect_used)] // INVARIANT: the target was looked up under the same lock
#[inline(never)]
fn start_lib(
    us: &mut UserState,
    jobs: &mut Jobs,
    map: &mut dyn crate::StartMap,
    (tpid, tjob): (u64, JobId),
) -> Result<(crate::LibRange, u64, u64), Status> {
    let t = us.procs.get_mut(&tpid).expect("target exists");
    if let Err(e) = t.handles.reserve(1).and_then(|()| t.reserve_charge()) {
        credit_thread(jobs, tjob);
        return Err(e);
    }
    let (lib, span) = match map.map(t.aspace.held_mut(), jobs) {
        Ok(x) => x,
        Err(e) => {
            credit_thread(jobs, tjob);
            return Err(e);
        }
    };
    let tables = if span == 0 {
        0
    } else {
        (span / 4096 / 512).saturating_add(4)
    };
    if tables > 0 {
        if let Err(x) = jobs.charge(tjob, Resource::Pages, tables) {
            map.unmap(t.aspace.held_mut(), jobs, lib, span);
            credit_thread(jobs, tjob);
            return Err(crate::job_status(x));
        }
    }
    Ok((lib, span, tables))
}

/// `process_start`'s commit: the channel end moves into the new process as its handle 1 (a failure to take it from the caller undoes the
/// library, its charge and the thread's), and the process gets its entry, argument, stack pointer and library, and is Running with one thread.
#[allow(clippy::expect_used, clippy::panic)]
// INVARIANT: the caller and the target were looked up under the same lock; a Created process's table is empty and its room was reserved, so the moved handle is handle 1
#[inline(never)]
fn start_commit(
    us: &mut UserState,
    jobs: &mut Jobs,
    map: &mut dyn crate::StartMap,
    (me, tpid, tjob): (u64, u64, JobId),
    a: &[u64; 6],
    (lib, span, tables): (crate::LibRange, u64, u64),
) -> Result<(), Status> {
    let bh = Handle(a[3]);
    let first_gen = us.first_handle_gen;
    let UserState { procs, objs, .. } = us;
    let caller = procs.get_mut(&me).expect("caller exists");
    let moved = match caller.handles.remove_for_transfer(objs, bh) {
        Ok(m) => m,
        Err(e) => {
            let t = procs.get_mut(&tpid).expect("target exists");
            undo_lib(t, jobs, map, tjob, (lib, span, tables));
            return Err(e);
        }
    };
    let target = procs.get_mut(&tpid).expect("target exists");
    insert_bootstrap(target, objs, moved, first_gen);
    started(target, a, (lib, span, tables));
    Ok(())
}

/// The started process's record: its entry, argument, stack pointer and library (with the library's page-table charge), Running with one thread.
#[inline(never)]
fn started(
    target: &mut crate::Process,
    a: &[u64; 6],
    (lib, span, tables): (crate::LibRange, u64, u64),
) {
    target.entry = a[1];
    target.arg = a[4];
    target.start_sp = a[2];
    target.libsys = lib;
    if tables > 0 {
        target.add_charge(lib.base, tables, lib.base.saturating_add(span));
    }
    target.state = crate::ProcState::Running;
    target.threads_alive = 1;
}

/// Undo `start_lib`: the library unmapped, its page-table charge and the thread's credited.
#[inline(never)]
fn undo_lib(
    t: &mut crate::Process,
    jobs: &mut Jobs,
    map: &mut dyn crate::StartMap,
    tjob: JobId,
    (lib, span, tables): (crate::LibRange, u64, u64),
) {
    map.unmap(t.aspace.held_mut(), jobs, lib, span);
    jobs.credit(tjob, Resource::Pages, tables);
    credit_thread(jobs, tjob);
}

/// The channel end `moved` arrives in the new process `target` as its handle 1 (a Created process's table is empty and its room was reserved).
#[allow(clippy::expect_used, clippy::panic)] // INVARIANT: as `start_commit`
#[inline(never)]
fn insert_bootstrap(
    target: &mut crate::Process,
    objs: &mut kcore::ipc::Kobjs,
    moved: kcore::handle::Entry,
    first_gen: u64,
) {
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
}

/// The end of `process_start`: the first thread's start (at the default priority, clamped to its job's maximum; the system library's base in
/// `a2`: AB7) and its register image.
#[inline(never)]
fn start_register(
    us: &mut UserState,
    jobs: &Jobs,
    (tpid, tjob): (u64, JobId),
    a: &[u64; 6],
    lib: crate::LibRange,
    mut image: kcore::fbox::FBox<[u64; 32]>,
) -> Spawn {
    let (entry, sp, arg) = (a[1], a[2], a[4]);
    let prio = clamp_prio(jobs, tjob, 10);
    let token = new_start(us, tpid, entry, sp, arg, None);
    if let Some(t) = us.starts.get_mut(&token) {
        t.a2 = lib.base;
    }
    fill_image(&mut image, (entry, sp, arg, 0, 0), lib.base);
    Spawn {
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
    }
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
