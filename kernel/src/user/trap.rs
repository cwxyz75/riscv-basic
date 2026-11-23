//! Entering and leaving user mode: the trap from U-mode, the syscall dispatcher and the user console.

use super::*;

pub(super) fn aspace_token_of(p: &Process) -> u64 {
    crate::arch::aspace_token_asid(p.aspace.root(), p.tag.asid)
}

/// The token to install for `p` on this hart (interrupts off, big lock held): a tag of an older generation names an ASID that may be
/// another space's now, so the space takes a fresh one first, and this hart flushes everything if it has not since the generation turned
/// (TLB1 per generation, L18).
pub(super) fn install_token_of(asids: &mut kcore::asid::Asids, p: &mut Process) -> u64 {
    asids.refresh(&mut p.tag);
    let me = crate::percpu::this();
    if p.tag.gen > me.asid_seen.load(Ordering::Relaxed)
        && !cfg!(feature = "fault-asid-reuse-no-flush")
    {
        crate::arch::tlb_flush_all();
        me.asid_seen.store(p.tag.gen, Ordering::Relaxed);
    }
    aspace_token_of(p)
}

/// Register writes (`thread_write_state`) that were left for this thread while it was parked: applied here, by the thread to its own frame (the
/// `&mut` this function was given, so no one else ever writes it). Only a frame from user mode.
fn apply_pending_regs(frame: &mut crate::trap::TrapFrame) {
    let pending = irq::without(|| {
        let s = state();
        if s.pending_regs.is_empty() {
            return None;
        }
        s.pending_regs.remove(&thread::current())
    });
    let Some(p) = pending else {
        return;
    };
    if !frame.is_from_user() {
        return; // (a kernel frame: nothing was written for it)
    }
    if let Some(g) = p.general {
        frame.set_general(&g);
    }
    crate::fp::apply_written(thread::current(), frame, p.fp, p.vec);
}

/// Make the calling thread's process the current address space, just before returning to
/// user mode. Needed because the hart's address space belongs to whichever user thread last ran:
/// threads of different processes are switched between, and the kernel half is shared so
/// kernel code runs under any of them, but returning to user mode must use *this* one.
pub fn activate_current(frame: &mut crate::trap::TrapFrame) {
    // (the epoch first, then the marks: a kill that lands after this load moves the epoch and is seen at the next trap)
    crate::percpu::this()
        .attention_seen
        .store(ATTENTION.load(Ordering::SeqCst), Ordering::SeqCst);
    // A process that was killed while its thread was not running dies here, at the point
    // where it would otherwise return to user mode.
    park_while_stopped();
    apply_pending_regs(frame);
    let token = irq::without(|| {
        let s = state();
        let UserState {
            asids,
            procs,
            thread_pid,
            ..
        } = s;
        let pid = thread_pid.get(&thread::current()).copied().unwrap_or(0);
        crate::percpu::this().cur_pid.store(pid, Ordering::Relaxed);
        let p = procs.get_mut(&pid);
        crate::percpu::this().cur_as.store(
            p.as_ref().map_or(core::ptr::null_mut(), |p| {
                alloc::sync::Arc::as_ptr(&p.aspace).cast_mut()
            }),
            Ordering::Release,
        );
        let p = p?;
        // (a tag of an older generation names an ASID that may be another space's now: take a fresh one before this space is installed,
        // and flush this hart if it has not since the generation turned (TLB1 per generation, L18))
        let token = install_token_of(asids, p);
        // (what the next return of this thread can use with no big lock: `activate_current_quiet`; written only when the thread or its tag is not what
        // this hart wrote last: the common syscall returns to the same thread under the same tag and takes no table for it)
        let me = crate::percpu::this();
        let tid = thread::current() as u64;
        let key = (p.tag.gen << 16) | u64::from(p.tag.asid);
        if me.bound_tid.load(Ordering::Relaxed) != tid
            || me.bound_tag.load(Ordering::Relaxed) != key
        {
            thread::bind_user(
                thread::current(),
                thread::UserBinding {
                    pid,
                    tag: p.tag,
                    token,
                    aspace: alloc::sync::Arc::as_ptr(&p.aspace) as usize,
                },
            );
            me.bound_tid.store(tid, Ordering::Relaxed);
            me.bound_tag.store(key, Ordering::Relaxed);
        }
        Some(token)
    });
    if let Some(token) = token {
        if crate::percpu::this()
            .active_aspace
            .swap(token, Ordering::SeqCst)
            != token
            || crate::arch::aspace_current() != token
        {
            // SAFETY: `token` names a live process root whose upper half is the kernel's, with an ASID from the allocator (never 0) of
            // the current generation (`install_token_of` refreshed it and flushed this hart if the generation had turned just above).
            // No flush: translations are tagged with the ASID (L18).
            unsafe { crate::arch::aspace_install_noflush(token) };
        }
    }
}

/// The return to user mode of a thread that slept in a quiet call and wakes with nothing to look at (L19): `seen` is the attention epoch the call
/// started under, and if it has not moved no process has been marked killed or stopped, no register write or suspend is pending, since. The
/// thread's process, tag and token come from its own record (written by the full path), the tag must be of the allocator's current generation (a
/// stale one needs a fresh ASID: the full path), and this hart flushes if it has not since the generation turned, exactly as `install_token_of`
/// does. False: nothing was changed, and the caller takes the full path.
pub fn activate_current_quiet(seen: u64) -> bool {
    if ATTENTION.load(Ordering::SeqCst) != seen {
        return false;
    }
    let me = crate::percpu::this();
    let Some(b) = thread::user_binding(thread::current()) else {
        return false;
    };
    if b.tag.gen != asid_generation() {
        return false;
    }
    if b.tag.gen > me.asid_seen.load(Ordering::Relaxed)
        && !cfg!(feature = "fault-asid-reuse-no-flush")
    {
        crate::arch::tlb_flush_all();
        me.asid_seen.store(b.tag.gen, Ordering::Relaxed);
    }
    me.attention_seen.store(seen, Ordering::SeqCst);
    me.cur_pid.store(b.pid, Ordering::Relaxed);
    me.cur_as
        .store(b.aspace as *mut klock::aslock::AsCell, Ordering::Release);
    if me.active_aspace.swap(b.token, Ordering::SeqCst) != b.token
        || crate::arch::aspace_current() != b.token
    {
        // SAFETY: `b.token` was made by the full path for this thread's process: a live root whose upper half is the kernel's, with an ASID of the
        // generation just checked to be the current one. No flush: translations are tagged with the ASID (L18).
        unsafe { crate::arch::aspace_install_noflush(b.token) };
    }
    true
}

/// The allocator's generation, for a return to user mode that does not hold the big lock (L19): set once at boot, when the user state is made.
static ASID_GEN: core::sync::atomic::AtomicPtr<AtomicU64> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Publish the allocator's generation cell (once, at boot, under the big lock; the cell lives for ever).
pub(super) fn publish_asid_generation(cell: alloc::sync::Arc<AtomicU64>) {
    ASID_GEN.store(
        alloc::sync::Arc::into_raw(cell).cast_mut(),
        Ordering::Release,
    );
}

fn asid_generation() -> u64 {
    let p = ASID_GEN.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: `publish_asid_generation` leaked an `Arc` (never freed), so the cell is alive for the rest of the run.
    unsafe { (*p).load(Ordering::Acquire) }
}

/// `debug_stat(debug, which)` (HD4): a counter of the kernel, for a program that holds a debug resource with INSPECT.
fn debug_stat_call(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let p = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        ipc::debug_of(&s.objs, &p.handles, Handle(a[0]), Rights::INSPECT)
            .map_err(|e| (e, None, None))?;
        Ok((Some(thread::wake_stat(a[1])), None))
    })
}

/// The attention epoch (L12b): bumped, under the big lock, whenever a process is marked killed or stopped, **after** the mark is set and
/// before any IPI that tells a hart to look. A hart that returns to user mode records the epoch it saw (`activate_current`, before it
/// reads the marks); a trap from user mode that finds the epoch unchanged has nothing to die or stop for, and needs no big lock.
pub(super) static ATTENTION: AtomicU64 = AtomicU64::new(1);

/// How much of the reachability sweep one creating call does (MM10): up to this many slices of this many marked objects or slots each.
const SWEEP_SLICES_PER_CALL: usize = 4;
const SWEEP_SLICE_BUDGET: usize = 256;

pub(super) fn bump_attention() {
    ATTENTION.fetch_add(1, Ordering::SeqCst);
}

/// A user page fault resolved with **no big kernel lock** (L17 part c, KERNEL-V1-SPEC 4.5): the faulting hart takes its process's `AS` lock,
/// then the memory locks (`VMO`, `JOBS`, `FRAME`), maps the page, and lets go. True: the page is mapped and the thread retries the instruction.
/// False: nothing was changed that the full path would not do again (a bad address, a region that is not there, a page a pager has not
/// supplied, no memory), and the trap takes the full path, which finds the same answer under the big lock and acts on it (a kill, a pager wait).
///
/// The process is the one `activate_current` (or the thread's first entry) recorded for this hart: the faulting thread belongs to it, and a
/// process is torn down only when its last thread has gone, so the cell outlives this call. The lock is what keeps the space from changing under
/// the fault: every other reach of the space (a map, an unmap, a protect, a copy) holds it too (AS1).
pub fn page_fault_quiet(frame: &mut crate::trap::TrapFrame) -> bool {
    use crate::arch::{FaultKind, UserTrap};
    let UserTrap::Fault(fault) = frame.decode_user() else {
        return false;
    };
    if fault.kind != FaultKind::PageFault || fault.address >= abi::user::TOP {
        return false;
    }
    let cell = crate::percpu::this().cur_as.load(Ordering::Acquire);
    if cell.is_null() {
        return false;
    }
    // SAFETY: see above: the hart runs a thread of the process the cell belongs to, so the cell's `Arc` is alive for the whole call.
    let cell = unsafe { &*cell };
    let access = fault.access.unwrap_or(Access::Read);
    let (r, log_pending) = {
        let _as = cell.lock();
        let space = cell.held_mut();
        let r = {
            let k = kmem::mem();
            // (the pager-need slot is shared with the kernel copies of syscalls on other harts, which read it after they let the memory locks go:
            // this fault must leave it as it found it; the need it may meet is in its error, and the full path asks again)
            let saved = k.vmos.swap_need(None);
            let r = space.handle_fault_mapped(k.mm, k.vmos, k.jobs, fault.address, access);
            let _ = k.vmos.swap_need(saved);
            r
        };
        // (MM8) translations this fault took away from the space (a write that replaced a shared page): every hart drops its TLB **before** the
        // thread runs on, and by this fault's own hart, not by whichever hart syncs the global copy log first, so that the duty is not lost to it. The
        // memory locks are let go first, as `cow_sync` does it.
        if space.take_replaced() > 0 {
            crate::smp::shootdown();
            kmem::flush_tlb();
        }
        // The copy log (`COPY_PENDING`) is read while the lock is held: a sync takes every address space's lock, so none is running now, and
        // one that follows finds what this fault logged. If it is set the big lock serialises with a sync that is already under way.
        (r, kcore::aspace::COPY_PENDING.load(Ordering::Acquire))
    };
    let Ok(m) = r else {
        return false;
    };
    if m.exec > 0 {
        // (PL7) code made visible: every hart's instruction fetches must agree with memory before it runs
        crate::smp::icache_sync(m.exec);
    }
    kmem::flush_pages(m.first, m.last, crate::arch::aspace_current_asid());
    // The log's sync takes the big lock and can switch threads, so it is the full return to user mode (`activate_current` after a switch).
    if log_pending {
        crate::bkl::lock();
        super::calls::cow_sync();
        commit_wake();
        thread::preempt_point();
        activate_current(frame);
        crate::bkl::unlock();
    }
    true
}

/// What the first half of the quiet `futex_wait` found.
enum QuietEnter {
    /// Nothing was changed that the full path would not do again.
    Fallback,
    /// The call is complete (a poll, or the word differs): the status to report.
    Done(abi::status::Status),
    /// The thread is queued: sleep.
    Sleep(ksys::FutexWord),
}

/// The first half of `futex_wait_quiet`, in a frame of its own (the memory-lock guards would push the caller over the stack budget): translate, compare
/// and queue under the process's `AS` lock.
#[inline(never)]
fn futex_wait_enter(cell: &klock::aslock::AsCell, a: [u64; 6], pid: u64, who: u64) -> QuietEnter {
    let _as = cell.lock();
    let space = cell.held_mut();
    let r = {
        let k = kmem::mem();
        let saved = k.vmos.swap_need(None); // (as the quiet fault does: the slot is the kernel copies' on other harts)
        let r = ksys::futex_wait_resolve_in(space, k.mm, k.vmos, k.jobs, a);
        let _ = k.vmos.swap_need(saved);
        r
    };
    let Ok(w) = r else {
        return QuietEnter::Fallback; // (the full path finds the same error, or the pager need, and acts on it)
    };
    if space.take_replaced() > 0 {
        crate::smp::shootdown();
        kmem::flush_tlb();
    }
    // (MM8, COW1) a first touch of a page of a VMO that has children logs a copy that every address space must drop before this thread sleeps or
    // returns: the full path syncs it (`cow_sync`), and nothing is queued yet, so it does the call again
    if kcore::aspace::COPY_PENDING.load(Ordering::Acquire) {
        return QuietEnter::Fallback;
    }
    // (the enqueue under the held `AS`, the memory locks let go: `FUTEX` is before them)
    match ksys::futex_wait_enqueue(&futex_table(), &kmem::DirectMap, pid, who, &w) {
        ksys::FutexCheck::Done(status) => QuietEnter::Done(status),
        ksys::FutexCheck::Sleep => QuietEnter::Sleep(w),
    }
}

/// `futex_wait` from user mode with **no big kernel lock** (L19, T213): the quiet path of the trap handler calls this with the frame past the call
/// instruction, `light_save` done and the return quiet. True: the call is complete (its result is in the frame, and the thread may have slept and
/// switched harts meanwhile). False: nothing was changed, and the trap takes the full path, which does the call from the beginning
/// (the caller puts `sepc` back before the call instruction).
///
/// The space is the one the hart recorded for its running process, held under its `AS` lock while the word is translated and compared and the
/// thread queued (the frame cannot go: every unmap and decommit takes `AS`); the memory locks are let go before the bucket lock is taken (`FUTEX`
/// comes before them). The sleep holds nothing. It ends in the table (a wake took the thread out: the call succeeded) or with the thread still in
/// it: the deadline, or the attention epoch moved (a kill, a stop, a suspend, an interrupt): the thread leaves the table (a leave that finds it
/// gone was a wake after all) and **the call starts again on the full path**, which reports TIMED_OUT, INTERRUPTED or the kill as it always has.
pub fn futex_wait_quiet(frame: &mut crate::trap::TrapFrame) -> bool {
    let a = [
        frame.arg(10),
        frame.arg(11),
        frame.arg(12),
        frame.arg(13),
        frame.arg(14),
        frame.arg(15),
    ];
    let me = crate::percpu::this();
    let (pid, cell) = (
        me.cur_pid.load(Ordering::Relaxed),
        me.cur_as.load(Ordering::Acquire),
    );
    // (the epoch this hart last saw, **before** the flags: a kill, stop or suspend marks its state and then moves the epoch, so one that lands after this
    // load makes `ATTENTION` differ from `seen` (the sleep's `ready` and the return both look), and one that landed before it is a difference here)
    let seen = me.attention_seen.load(Ordering::SeqCst);
    if ATTENTION.load(Ordering::SeqCst) != seen {
        return false;
    }
    if cell.is_null()
        || SUSPEND_STATES.load(Ordering::Acquire) != 0
        || INTERRUPT_FLAGS.load(Ordering::Acquire) != 0
        || kcore::aspace::COPY_PENDING.load(Ordering::Acquire)
    {
        return false;
    }
    // SAFETY: the hart runs a thread of the process the cell belongs to (`activate_current` recorded it), so its `Arc` is alive for the whole call; the
    // thread keeps running on whichever hart after a sleep, but only the lock below is taken through the reference, before any sleep.
    let cell = unsafe { &*cell };
    let who = thread::current() as u64;
    let w = match futex_wait_enter(cell, a, pid, who) {
        QuietEnter::Fallback => return false,
        QuietEnter::Done(status) => {
            reply(frame, fail(status));
            return true;
        }
        QuietEnter::Sleep(w) => w,
    };
    let key = (pid, w.addr);
    thread::wait_deadline_futex_quiet(
        || {
            ATTENTION.load(Ordering::SeqCst) != seen
                || INTERRUPT_FLAGS.load(Ordering::Acquire) != 0
                || !futex_table().contains(key, who)
        },
        w.deadline,
    );
    if futex_table().forget(key, who) {
        // still queued: the deadline or an event ended the wait, and nothing was reported: the full path does the call again
        return false;
    }
    reply(frame, Ok((None, None)));
    if !activate_current_quiet(seen)
        || kcore::aspace::COPY_PENDING.load(Ordering::Acquire)
        || thread::need_resched_pending()
    {
        crate::bkl::lock();
        super::calls::cow_sync();
        commit_wake();
        thread::preempt_point();
        activate_current(frame);
        crate::bkl::unlock();
    }
    true
}

/// `futex_wake` from user mode with **no big kernel lock** (L14): the trap handler's quiet path calls this with the frame past the call
/// instruction. The process is the one `activate_current` recorded for this hart's running thread; the rest takes only the key's bucket lock
/// and the scheduler's own locks. A wake that outranks this thread leaves `need_resched` set: the caller takes the lock for the switch.
pub fn futex_wake_quiet(frame: &mut crate::trap::TrapFrame) {
    let a = [
        frame.arg(10),
        frame.arg(11),
        frame.arg(12),
        frame.arg(13),
        frame.arg(14),
        frame.arg(15),
    ];
    let pid = crate::percpu::this().cur_pid.load(Ordering::Relaxed);
    reply(frame, futex_wake(pid, a));
}

/// Has nothing happened since this hart last returned to user mode that could end or stop the thread it is about to resume? (No
/// lock needed: both sides are atomics, and the marks are set before the epoch moves.)
pub fn user_return_is_quiet() -> bool {
    crate::percpu::this().attention_seen.load(Ordering::SeqCst) == ATTENTION.load(Ordering::SeqCst)
}

/// The return to user mode of a thread that made a syscall, with **no big lock** (L20): true when nothing has been recorded that the thread must look at
/// (the hart's attention epoch is the current one) and the thread's own record lets it install its address space on this hart without the allocator
/// (`activate_current_quiet`: the process, the tag of the current generation, the token). False: the caller takes the full path.
pub fn return_to_user_is_quiet() -> bool {
    let seen = ATTENTION.load(Ordering::SeqCst);
    crate::percpu::this().attention_seen.load(Ordering::SeqCst) == seen
        && activate_current_quiet(seen)
}

/// A trap from user mode (called from the trap handler with interrupts off).
pub fn user_trap(frame: &mut crate::trap::TrapFrame) {
    user_trap_inner(frame);
    crate::chaos::point(crate::chaos::Point::ReturnToUser);
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the process of a trapping thread exists; -1000 - cause is a small negative number (cause < 64)
pub(super) fn user_trap_inner(frame: &mut crate::trap::TrapFrame) {
    use crate::arch::{FaultKind, UserTrap};
    if frame.is_syscall() {
        frame.skip_trap_instruction(); // past the call instruction
        syscall(frame);
        // (MM9) a kernel copy found a page of a pager's VMO missing: wait for it here, with no lock held
        pager_after_call(frame);
        // (MM8) a copy-on-write fault inside a kernel copy to or from user memory replaced translations this and other harts may cache
        super::calls::cow_sync();
        return;
    }
    match frame.decode_user() {
        UserTrap::Syscall => unreachable_syscall(),
        UserTrap::Fault(fault) if fault.kind == FaultKind::PageFault => {
            let cause = fault.code;
            let access = fault.access.unwrap_or(Access::Read);
            let va = fault.address;
            let mut pager_need = None;
            let mut resolved = {
                let pid = state().thread_pid.get(&thread::current()).copied();
                match pid {
                    Some(pid) if va < abi::user::TOP => {
                        let p = state().procs.get_mut(&pid).expect("process exists");
                        let _as = klock::aslock::AsCell::hold(&p.aspace);
                        let k = kmem::get();
                        let r = p
                            .aspace
                            .held_mut()
                            .handle_fault_mapped(k.mm, k.vmos, k.jobs, va, access);
                        if let Ok(m) = &r {
                            if m.exec > 0 {
                                // (PL7) code made visible: every hart's instruction fetches must agree with memory before it runs
                                crate::smp::icache_sync(m.exec);
                            }
                            // (the ASID this hart is running under, not the process's current tag: a refresh by another hart can have moved the tag while this
                            // thread still runs under the old one)
                            kmem::flush_pages(m.first, m.last, crate::arch::aspace_current_asid());
                        }
                        if let Err(kcore::aspace::FaultError::NeedPager(n)) = &r {
                            pager_need = Some(*n);
                            // (the fault has the need in its error: the slot a kernel copy reads must not keep it for another thread's call)
                            let _ = k.vmos.take_need();
                        }
                        if cfg!(feature = "fault-log") {
                            if let Err(e) = &r {
                                let _ = core::fmt::Write::write_fmt(
                                    &mut crate::console::Console,
                                    format_args!("kernel: handle_fault failed: {e:?} (pid {pid}, va {va:#x})\n"),
                                );
                            }
                        }
                        r.is_ok()
                    }
                    Some(pid) => {
                        if cfg!(feature = "fault-log") {
                            let _ = core::fmt::Write::write_fmt(
                                &mut crate::console::Console,
                                format_args!("kernel: fault above TOP (pid {pid}, va {va:#x})\n"),
                            );
                        }
                        false
                    }
                    None => {
                        if cfg!(feature = "fault-log") {
                            let _ = core::fmt::Write::write_fmt(
                                &mut crate::console::Console,
                                format_args!(
                                    "kernel: fault in thread {} with no process\n",
                                    thread::current()
                                ),
                            );
                        }
                        false
                    }
                }
            };
            // (MM9) the page is a pager's and was not supplied: wait for it with no lock held (PG1); on a supply the access is tried again, and
            // a pager that failed or died raises a PAGER exception
            let mut pager_status = None;
            if let Some(need) = pager_need {
                match pager_wait(need, false) {
                    Ok(()) => resolved = true,
                    Err(status) => pager_status = Some(status),
                }
            }
            // (MM8) a copy or a first touch of a page other mappings may show an older version of: drop those, on every hart
            super::calls::cow_sync();
            if resolved {
                // (the pages the fault mapped were flushed one by one above)
            } else {
                // a diagnostic for headless debugging (off by default: the tests crash programs
                // on purpose and the shell tests read the console line by line)
                if cfg!(feature = "fault-log") {
                    let _ = core::fmt::Write::write_fmt(
                        &mut crate::console::Console,
                        format_args!(
                            "kernel: user fault, cause {cause}, pc {:#x}, address {va:#x}, ra {:#x}, sp {:#x}\n",
                            fault.pc,
                            frame.arg(1),
                            frame.arg(2)
                        ),
                    );
                }
                match pager_status {
                    Some(status) => raise_fault(ksys::exception::Fault {
                        kind: abi::arch::fault::PAGER,
                        pc: fault.pc,
                        address: fault.address,
                        cause: i64::from(status.code()) as u64,
                    }),
                    None => fault_or_exit(&fault),
                }
            }
        }
        UserTrap::Fault(fault)
            if fault.kind == FaultKind::Illegal && crate::fp::first_use(frame) =>
        {
            // The first floating-point instruction of this thread (slice A): the unit is now enabled
            // for it, from zeros; the instruction is retried. (A real illegal instruction traps again
            // with the unit on and ends the process below.)
        }
        UserTrap::Fault(fault) => fault_or_exit(&fault),
    }
}

/// A fault the kernel cannot resolve (EX2): offered to the process's exception channel, then its job's and each ancestor's; the thread stops until a
/// handler resolves it (RETRY comes back here and returns to user mode to try the instruction again). With no handler anywhere the process ends,
/// as it always did, with `-1000 - kind`.
fn fault_or_exit(fault: &crate::arch::Fault) {
    raise_fault(ksys::exception::Fault {
        kind: fault.kind.abi_kind(),
        pc: fault.pc,
        address: fault.address,
        cause: fault.code,
    });
}

/// `fault_or_exit` for a fault already described (a PAGER fault, MM9, has the pager's status as its cause).
fn raise_fault(f: ksys::exception::Fault) {
    let delivered = irq::without(|| {
        let s = state();
        let tid = thread::current();
        let Some(&pid) = s.thread_pid.get(&tid) else {
            return false;
        };
        if !ksys::exception::deliver(s, kmem::jobs().jobs, pid, tid, &f, None) {
            return false;
        }
        // (the thread is stopped before anyone can read the message: the big lock is held throughout)
        ksys::exception::stop_thread(s, tid);
        sync_suspends(s);
        true
    });
    if delivered {
        commit_wake(); // the channel is readable now
        park_while_stopped();
    } else {
        exit_process(abi::arch::fault::exit_code(f.kind));
    }
}

pub(super) fn status_ret(frame: &mut crate::trap::TrapFrame, s: Status) {
    frame.set_return(s.code() as i64 as u64, None, None);
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn syscall(frame: &mut crate::trap::TrapFrame) {
    crate::latency::set_syscall(frame.arg(17) as usize);
    let growth_at_entry = kcore::handle::growth_events();
    let (n, a) = (
        frame.arg(17) as usize,
        [
            frame.arg(10),
            frame.arg(11),
            frame.arg(12),
            frame.arg(13),
            frame.arg(14),
            frame.arg(15),
        ],
    );
    match n {
        nr::THREAD_YIELD => {
            status_ret(frame, Status::Ok);
            thread::yield_now();
        }
        nr::PROCESS_EXIT => exit_process(a[0] as i32),
        nr::THREAD_EXIT => leave(a[0] as i32, false),
        nr::CLOCK_GET => frame.set_return(0, Some(thread::now_ns()), None),
        nr::DEBUG_STAT => reply(frame, debug_stat_call(a)),
        nr::OBJECT_WAIT => reply(frame, object_wait(a)),
        nr::PORT_WAIT => {
            // (a poll moves the port's cursor, which counts as a touch)
            let r = port_wait(a);
            commit_wake();
            reply(frame, r);
        }
        nr::PORT_WAIT_MANY => {
            let r = port_wait_many(a);
            commit_wake();
            reply(frame, r);
        }
        nr::FUTEX_WAIT => reply(frame, futex_wait(a)),
        nr::FUTEX_LOCK_PI => reply(frame, futex_lock_pi(a)),
        nr::FUTEX_UNLOCK_PI => reply(frame, futex_unlock_pi(a)),
        nr::FUTEX_WAKE => {
            // (the quiet path of the trap handler takes this call without the big lock when it can; this is the same code with it held)
            let pid = irq::without(|| caller_pid(state()).unwrap_or(0));
            reply(frame, futex_wake(pid, a));
        }
        nr::PROCESS_CREATE
        | nr::JOB_SET_SCHED
        | nr::THREAD_SET_SCHED
        | nr::THREAD_GET_SCHED
        | nr::JOB_KILL
        | nr::JOB_ROOT
        | nr::RESOURCE_ROOT
        | nr::DEBUG_ROOT
        | nr::PROCESS_SELF
        | nr::PROCESS_MAP
        | nr::PROCESS_UNMAP
        | nr::PROCESS_PROTECT
        | nr::PROCESS_MAP_INFO
        | nr::VMO_DECOMMIT
        | nr::VMO_SET_SIZE
        | nr::VMO_CREATE_CHILD
        | nr::PROCESS_CLONE_REGION
        | nr::PROCESS_START
        | nr::PROCESS_KILL
        | nr::PROCESS_SUSPEND
        | nr::PROCESS_RESUME
        | nr::PROCESS_INFO
        | nr::THREAD_SPAWN
        | nr::THREAD_INFO
        | nr::THREAD_INTERRUPT
        | nr::THREAD_SUSPEND
        | nr::PROCESS_EXCEPTION_CHANNEL
        | nr::JOB_EXCEPTION_CHANNEL
        | nr::EXCEPTION_RESUME
        | nr::THREAD_RESUME
        | nr::THREAD_READ_STATE
        | nr::THREAD_WRITE_STATE
        | nr::PAGER_CREATE
        | nr::PAGER_SUPPLY
        | nr::PAGER_FAIL
        | nr::PAGER_QUERY_DIRTY
        | nr::PAGER_WRITEBACK_DONE => {
            let r = process_call(n, a);
            notify();
            reply(frame, r);
        }
        nr::CHANNEL_CALL => {
            let r = channel_call(a);
            notify();
            reply(frame, r);
        }
        _ => {
            // (L20: the object calls are one section under the object lock, as the final design has them (SCHED-SMP-SPEC 3.3, T70))
            let result = irq::without(|| dispatch(n, a));
            if matches!(n, nr::CHANNEL_READ | nr::CHANNEL_READ_TXID) {
                // (L22) a thread that read a `channel_call` request is lent its caller's priority, while the caller still waits for that reply
                lend_to_reader();
            }
            // A syscall that can change a signal wakes every waiter (after the borrows of
            // `dispatch` are gone, since waking may switch threads).
            if matches!(
                n,
                nr::CHANNEL_WRITE
                    | nr::CHANNEL_READ
                    | nr::CHANNEL_READ_TXID
                    | nr::HANDLE_CLOSE
                    // a replaced handle's old value names nothing: waits on it end
                    | nr::HANDLE_REPLACE
                    | nr::EVENT_SIGNAL
                    // creating objects can sweep garbage, which closes peers
                    | nr::CHANNEL_CREATE
                    | nr::EVENT_CREATE
                    | nr::SOCKET_CREATE
                    | nr::PORT_CREATE
                    // data moved, space freed, or a binding that is ready at once
                    | nr::SOCKET_WRITE
                    | nr::SOCKET_READ
                    | nr::SOCKET_SHUTDOWN
                    | nr::PORT_BIND
                    | nr::PORT_UNBIND
                    | nr::PORT_REARM
            ) {
                if n == nr::HANDLE_CLOSE {
                    // (EX2) a job exception channel whose user end was just closed: the kernel's end goes (one test of an empty table otherwise)
                    irq::without(|| ksys::exception::reap_closed_job_channels(state()));
                }
                notify();
            }
            // Slice W step 3: wake the threads whose wait this syscall ended, by id (a handle removed
            // by `replace` or sent away is a forced touch, made where it happens)
            commit_wake();
            reply(frame, result);
        }
    }
    if n == nr::THREAD_YIELD {
        // status already set above
    }
    // (MM5) whatever this call added to the caller's handle table is charged to its job now, between calls, never in the middle of one
    settle_handle_table(growth_at_entry);
}

/// The requests the call just read (`channel_read`, `channel_read_txid`) were sent by threads blocked in `channel_call`: each lends its priority to
/// the calling thread, the one that read the request (L22). Skipped for a caller whose call has ended (its request stayed queued after a timeout).
fn lend_to_reader() {
    let me = thread::current();
    let calls = irq::without(|| {
        let s = state();
        let read = s.objs.take_dequeued_calls();
        read.into_iter()
            .filter(|(caller, call)| s.calls.get(&(*caller as Tid)) == Some(call))
            .map(|(caller, _)| caller as Tid)
            .collect::<Vec<Tid>>()
    });
    for caller in calls {
        thread::pi_donate(caller, me);
    }
}

pub(super) fn reply(frame: &mut crate::trap::TrapFrame, r: Reply) {
    match r {
        Ok((a1, a2)) => frame.set_return(0, a1, a2),
        Err((s, a1, a2)) => frame.set_return(s.code() as i64 as u64, a1, a2),
    }
}

pub(super) type ReplyErr = (Status, Option<u64>, Option<u64>);
pub(super) type Reply = Result<(Option<u64>, Option<u64>), ReplyErr>;

pub(super) fn fail(s: Status) -> Reply {
    Err((s, None, None))
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: a is [u64; 6] and every `h(i)`, `over(i)` is called with a constant below 6; the process of a calling thread exists; sizes were bounded by the limits checked first (nbytes <= MAX_MSG_BYTES, nhandles <= MAX_MSG_HANDLES, so nhandles * 4 and pages * 4096 are small); IRQ slots are below the table size (checked by the source number)
pub(super) fn dispatch(n: usize, a: [u64; 6]) -> Reply {
    let tid = thread::current();
    let s = state();
    let Some(&pid) = s.thread_pid.get(&tid) else {
        return fail(Status::BadState);
    };
    // Sweep before creating kernel objects if garbage cycles have piled up (OD39).
    if matches!(
        n,
        nr::CHANNEL_CREATE
            | nr::EVENT_CREATE
            | nr::SOCKET_CREATE
            | nr::PORT_CREATE
            | nr::INTERRUPT_CREATE
    ) && (s.objs.sweep_active() || s.objs.needs_sweep())
    {
        // (MM10) the sweep runs in slices: this call starts one if garbage cycles have piled up, and runs a few slices of it; the next
        // creating call carries on. The worst interrupts-off span of a sweep is a slice, not the whole object graph.
        if !s.objs.sweep_active() {
            let roots = s.roots();
            s.objs.sweep_begin(&roots);
        }
        let mut slices = 0;
        while s.objs.sweep_active() && slices < SWEEP_SLICES_PER_CALL {
            s.objs.sweep_step(SWEEP_SLICE_BUDGET);
            slices += 1;
        }
        // near the cap a creation would be refused while garbage fills the registry: sweep now, all of it, with the roots as they are (a
        // sweep that was running only frees what was garbage when it began, so `sweep` finishes it and then runs a fresh one)
        if s.objs.live() + 8 >= s.objs.cap() {
            let roots = s.roots();
            s.objs.sweep(&roots);
        }
    }
    let UserState {
        objs,
        procs,
        line,
        pass,
        fail: failed,
        irq_objs,
        ..
    } = s;
    let p = procs.get_mut(&pid).expect("process exists");
    let h = |i: usize| Handle(a[i]);
    // The syscall layer that needs no machine is in the `ksys` crate (TS3): the width check of the 32-bit arguments first, then the calls
    // on handles, events, channels, sockets and ports, over this machine's memory.
    let bad_args = ksys::wide_argument_error(n, &a);
    if let Some(status) = bad_args {
        return fail(status);
    }
    if n == nr::JOB_INFO {
        thread::charge_running(); // (`job_info` 9 and 10 read the CPU accounts: the caller's own running time is charged first)
    }
    {
        // (the memory locks are held only inside this block and in the two calls below that need them: every other call may take the
        // big kernel lock, which comes before them in the order)
        let as_cell = p.aspace.clone();
        let _as = klock::aslock::AsCell::hold(&as_cell); // (the caller's address space, before the memory locks)
        let k = kmem::get();
        let mut ctx = ksys::Ctx {
            objs: &mut *objs,
            handles: &mut p.handles,
            job: p.job,
            aspace: as_cell.held_mut(),
            mm: k.mm,
            vmos: k.vmos,
            jobs: k.jobs,
            faults: ksys::Faults {
                skip_forced_touch: cfg!(feature = "fault-skip-forced-touch"),
            },
            harts_online: crate::smp::online() as u64,
            job_sched: crate::thread::job_sched(),
            clock: crate::timer::now_ns,
            windows: crate::kmem::device_windows(),
        };
        if let Some(r) = ksys::ipc_call(n, a, &mut ctx) {
            return r;
        }
        if let Some(r) = ksys::vmo_call(n, a, &mut ctx) {
            return r;
        }
        if let Some(r) = ksys::job_call(n, a, &mut ctx) {
            return r;
        }
    }
    match n {
        nr::DEBUG_WRITE => {
            // (HD4: a debug resource handle with WRITE: no program writes to the console, or forges a test line, without one)
            ipc::debug_of(objs, &p.handles, h(0), Rights::WRITE).map_err(|e| (e, None, None))?;
            let (ptr, len) = (a[1], a[2] as usize);
            if len > MAX_DEBUG_WRITE {
                return fail(Status::TooBig);
            }
            let bytes = {
                let _as = klock::aslock::AsCell::hold(&p.aspace);
                let k = kmem::get();
                copy_from_user(p.aspace.held_mut(), k.mm, k.vmos, k.jobs, ptr, len)
            }
            .map_err(|e| (e, None, None))?;
            emit(line, pass, failed, &bytes);
            Ok((None, None))
        }
        nr::RESOURCE_DTB => {
            ipc::resource_of(objs, &p.handles, h(0)).map_err(|e| (e, None, None))?;
            let Some(blob) =
                crate::dtb::dtb_blob().filter(|b| b.len() <= abi::limits::MAX_DTB_BYTES)
            else {
                return fail(Status::NotSupported);
            };
            let pages = (blob.len() as u64).div_ceil(4096);
            let k = kmem::get();
            if k.vmos.reserve_one().is_err() {
                return fail(Status::NoMemory);
            }
            let id = k.vmos.create_in(k.jobs, pages, p.job);
            // the firmware's seeds are the kernel's (HD1): the copy the device manager sees has them zeroed
            // (scrubbed in a heap copy first, so no frame of the VMO ever holds them)
            let seeds = kcore::fdt::chosen_seeds(blob).unwrap_or_default();
            let mut copy = blob.to_vec();
            for (off, len) in [seeds.rng_seed, seeds.kaslr_seed].into_iter().flatten() {
                if let Some(range) = copy.get_mut(off..off.saturating_add(len)) {
                    range.fill(0);
                }
            }
            if k.vmos.write(k.mm, k.jobs, id, 0, &copy).is_err() {
                let _ = k.vmos.release(k.mm, k.jobs, id);
                return fail(Status::NoMemory);
            }
            let rights =
                Rights(Rights::READ.0 | Rights::MAP.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0);
            match ipc::vmo_object_create_with(
                objs,
                &mut p.handles,
                id,
                rights,
                &mut ipc::Quota {
                    jobs: &mut *k.jobs,
                    job: Some(p.job),
                },
            ) {
                Ok(hd) => Ok((Some(hd.0), Some(blob.len() as u64))),
                Err(e) => {
                    let _ = k.vmos.release(k.mm, k.jobs, id);
                    fail(e)
                }
            }
        }
        nr::INTERRUPT_CREATE => {
            ipc::resource_of(objs, &p.handles, h(0)).map_err(|e| (e, None, None))?;
            let irq = a[1];
            // (only the sources the tree's admitted devices name on the APLIC: 0, a number past 32 bits and any other source are refused)
            if irq == 0 || irq > u64::from(u32::MAX) || !crate::aia::irq_allowed(irq as u32) {
                return fail(Status::InvalidArgs);
            }
            let slot = irq as usize;
            let owned = matches!(*irq_objs.get(slot).unwrap_or(&None), Some(o)
                if matches!(objs.get(o), Some(ipc::KObj::Interrupt(i)) if i.irq == irq as u32));
            if owned {
                return fail(Status::BadState);
            }
            match ipc::interrupt_object_create_in(
                objs,
                &mut p.handles,
                irq as u32,
                &mut quota(p.job),
            ) {
                Ok((hd, obj)) => {
                    if let Some(s) = irq_objs.get_mut(slot) {
                        *s = Some(obj);
                    }
                    crate::aia::enable(irq as u32);
                    Ok((Some(hd.0), None))
                }
                Err(e) => fail(e),
            }
        }
        nr::INTERRUPT_ACK => {
            let (obj, _) = ipc::interrupt_of(objs, &p.handles, h(0), Rights::MANAGE)
                .map_err(|e| (e, None, None))?;
            let (irq, claims) = ipc::interrupt_ack(objs, obj).map_err(|e| (e, None, None))?;
            crate::aia::reenable(irq);
            Ok((Some(claims), None))
        }
        _ => fail(Status::BadSyscall),
    }
}

/// `##BENCH <name> <decimal value> <unit>`: names and units are single words of letters, digits, `_` and `/`.
pub(super) fn is_bench_line(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("##BENCH ") else {
        return false;
    };
    let mut w = rest.split(' ');
    let word = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
    };
    match (w.next(), w.next(), w.next(), w.next()) {
        (Some(n), Some(v), Some(u), None) => {
            word(n) && !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && word(u)
        }
        _ => false,
    }
}

/// Write user output to the console as whole lines. Each `debug_write` call is flushed at
/// its end (a call that does not finish with a newline gets one), and a line longer than
/// 512 bytes is cut, so a program can never leave a half line behind to swallow its next
/// message. A well-formed `##TEST name PASS|FAIL ...` line is a result and is counted;
/// any other line starting `##` is prefixed so user code cannot forge the kernel's
/// verdict lines (`##DONE`, `##PANIC`, `##BOOT`).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a pair of test counters
pub(super) fn emit(line: &mut String, pass: &mut u32, fail: &mut u32, bytes: &[u8]) {
    let mut flush = |text: String| {
        // A well-formed `##BENCH name value unit` line (SCHED-SMP-SPEC 6.1, amends OD28) passes through like a result.
        if is_bench_line(&text) {
            let _ = writeln!(Console, "{text}");
            return;
        }
        let verdict = text.strip_prefix("##TEST ").and_then(|rest| {
            let mut words = rest.split_whitespace();
            words.next()?;
            match words.next()? {
                "PASS" => Some(true),
                "FAIL" => Some(false),
                _ => None,
            }
        });
        match verdict {
            Some(true) => {
                *pass = pass.saturating_add(1);
                let _ = writeln!(Console, "{text}");
            }
            Some(false) => {
                *fail = fail.saturating_add(1);
                let _ = writeln!(Console, "{text}");
            }
            None if text.starts_with("##") => {
                // One piece, so the console's line guard sees a line that starts with "user:".
                let _ = Console.write_str(&alloc::format!("user: {text}\n"));
            }
            None => {
                let _ = writeln!(Console, "{text}");
            }
        }
    };
    for &b in bytes {
        if b == b'\n' {
            flush(core::mem::take(line));
        } else {
            // Printable ASCII only: control bytes and anything else become '.'.
            line.push(if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            });
            if line.len() >= 512 {
                flush(core::mem::take(line));
            }
        }
    }
    if !line.is_empty() {
        flush(core::mem::take(line));
    }
}

/// `decode_user` says Syscall only for a trap `is_syscall` already took.
#[cold]
fn unreachable_syscall() {}
