//! A user thread's life: its first code, ending a process, the kernel's own probes of kill, stop and interrupt.

use super::*;

#[allow(clippy::expect_used)]
// INVARIANT: the start record was registered by the spawner for this token, and the process of a running thread exists (both under the big lock)
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn user_thread_main(token: usize) {
    let top = thread::current_stack_top();
    // Interrupts stay off from here until `enter_user` is in U-mode: a kill that arrives in
    // between would otherwise be seen only at the next trap, after user code had run. The big
    // kernel lock is held while the thread's start is looked up and dropped just before the
    // jump to user mode (nothing user-visible runs under it).
    let _ = irq::save_and_disable();
    crate::bkl::lock();
    let (ts, killed) = {
        let s = state();
        let ts = s
            .starts
            .remove(&token)
            .expect("a thread start was registered");
        // Registered here, by the thread itself, before it can make a syscall: the spawner
        // cannot be preempted into leaving a window where the thread is unknown.
        s.thread_pid.insert(thread::current(), ts.pid);
        // (EX1) the register image the thread will start from, which `thread_read_state` and `thread_write_state` act on until it does; a
        // SUSPENDED thread's spawner has made it already
        let tid = thread::current();
        // (a resume may have come before this thread ever ran: the image the spawner made is the sign that it registered the stopped state, and
        // then it is not made again)
        let spawner_done = s.initial_regs.contains_key(&tid);
        s.initial_regs.entry(tid).or_insert_with(|| {
            ksys::initial_general(
                ts.entry,
                ts.sp,
                ts.arg,
                ts.tp,
                if ts.spawned { tid } else { 0 },
            )
        });
        if ts.suspended && !spawner_done {
            ksys::thread_start_suspended(s, tid);
            sync_suspends(s);
        }
        let p = s
            .procs
            .get_mut(&ts.pid)
            .expect("a process lives while one of its threads does");
        p.live_tids.insert(thread::current());
        if let Some(o) = ts.obj {
            p.thread_objs.insert(thread::current(), o);
        }
        if let Some(st) = ts.stack {
            p.stacks.insert(thread::current(), st);
        }
        if ts.rr {
            s.sched_flags.insert(thread::current(), 1);
        }
        // A thread that was created before its job's maximum was lowered has not been seen by the clamp (it was in
        // no table yet): clamp it now (PR1; review of task 8).
        let max = kmem::jobs()
            .jobs
            .sched(p.job)
            .map_or(63, |j| j.max_prio)
            .max(1);
        let late_clamp = thread::priority_of(thread::current()).is_some_and(|now| now > max);
        if late_clamp {
            thread::set_priority(thread::current(), max);
        }
        (ts, p.killed)
    };
    // Killed before it ever ran: it never runs user code.
    if let Some(code) = killed {
        crate::bkl::unlock();
        irq::enable();
        leave(code, true);
    }
    // A thread created in a stopped process waits here: it runs no user code before `process_resume` (or dies with the
    // process).
    // (the return to user mode is a defined point for a switch a wake asked for, X7: and a suspend, a stop or a kill can land while this thread
    // is switched out there, so look again until a pass finds nothing)
    loop {
        park_while_stopped();
        thread::preempt_point();
        if irq::without(|| killed_now().is_none() && !stopped_now() && !suspended_now()) {
            break;
        }
    }
    // The marks above were read before `preempt_point`, which can switch and let a kill or stop land (and another thread on this hart
    // record the new epoch) before this thread resumes: its first trap takes the full path, which looks again (PR 267 review)
    crate::percpu::this()
        .attention_seen
        .store(0, Ordering::SeqCst);
    // The address space's token is taken last, here, with interrupts off and the lock held and nothing that can sleep or switch between
    // this and `enter_user`: a tag taken earlier could be stale by now (the thread may have slept in a stopped process or been moved to
    // another hart after a generation turned), and the hart's generation flush belongs to the hart that enters (L18).
    let token = {
        let UserState { asids, procs, .. } = state();
        let p = procs
            .get_mut(&ts.pid)
            .expect("a process lives while one of its threads does");
        install_token_of(asids, p)
    };
    crate::percpu::this()
        .active_aspace
        .store(token, Ordering::SeqCst);
    // The lock is released here, with interrupts still off: only `enter_user`'s own instructions
    // (loads from this thread's stack and CSR writes) run before `sret`.
    // (EX1) the image the thread starts from (the spawner's or an earlier `thread_write_state`'s, or the one made at registration), with its FP and
    // vector state loaded if a write gave it any, in this hart's entry frame
    let entry_frame = {
        let image = state().initial_regs.remove(&thread::current());
        let image = image.unwrap_or_else(|| {
            ksys::initial_general(
                ts.entry,
                ts.sp,
                ts.arg,
                ts.tp,
                if ts.spawned { thread::current() } else { 0 },
            )
        });
        let mut frame = crate::trap::TrapFrame::from_general(&image);
        crate::fp::mark_present(thread::current(), &mut frame);
        crate::fp::return_to_user(&mut frame);
        let hart = crate::percpu::this().index.load(Ordering::Relaxed);
        // SAFETY: interrupts are off and the big lock is held; this hart's element is used by this hart only.
        let slot = unsafe { (*ENTRY_FRAMES.0.get()).get_mut(hart) }
            .expect("an entry frame for every hart");
        *slot = frame;
        slot as *const crate::trap::TrapFrame
    };
    crate::bkl::unlock();
    user_mode_entered();
    // SAFETY: `enter_user_frame` switches to the process's address space and drops to U-mode from the register image prepared above (in this hart's
    // entry frame, which only this hart uses and only with interrupts off); it never returns.
    unsafe { crate::arch::enter_user_frame(entry_frame, token, top) }
}

/// A hart's frame for the drop to user mode: the register image a new thread starts from is built here (not on the stack the assembly then
/// copies it onto), used by this hart alone with interrupts off between the fill and the jump.
struct EntryFrames(core::cell::UnsafeCell<[crate::trap::TrapFrame; crate::percpu::MAX_HARTS]>);
// SAFETY: each hart touches only its own element, with interrupts off.
unsafe impl Sync for EntryFrames {}
static ENTRY_FRAMES: EntryFrames = EntryFrames(core::cell::UnsafeCell::new(
    [const { crate::trap::TrapFrame::ZERO }; crate::percpu::MAX_HARTS],
));

/// Wait for a process's thread to finish, tear the process down, and return its exit code.
pub fn finish(pid: u64, tid: Tid) -> i32 {
    thread::join(tid);
    // Other threads of the process may outlive the first one.
    while irq::without(|| state().procs.get(&pid).map_or(0, |p| p.threads_alive)) > 0 {
        thread::sleep_ticks(1);
    }
    teardown(pid)
}

/// Take a process apart: close its handle table, sweep what nothing can reach any more,
/// mark its process object TERMINATED, free its address space and memory, and wake
/// everything that was waiting on any of that. Returns its exit code. Runs with interrupts
/// off, never holds a borrow across `notify`, and is safe to call from the dying thread
/// itself (it first switches to the kernel page table, whose upper half every process
/// shares).
pub(super) fn teardown(pid: u64) -> i32 {
    // Back to the kernel's own page table before the process's root is freed.
    kmem::use_kernel_table();
    let Some(mut process) = teardown_take(pid) else {
        // Another killer got here first (two kills of a never-started process can race): it
        // does the teardown.
        return i32::MIN;
    };
    // A process that vanished without a recorded code is not mistaken for `exit(-1)`.
    let code = process.exit_code.or(process.killed).unwrap_or(i32::MIN);
    teardown_objects(&mut process, code);
    teardown_memory(process);
    kmem::flush_tlb();
    notify(); // peers of what this process held now see PEER_CLOSED; waiters see TERMINATED
    code
}

// (the three steps of `teardown` are functions of their own, never merged into one frame: the optimised kernel's stack budget, KT13, T173)

#[inline(never)]
pub(super) fn teardown_take(pid: u64) -> Option<Box<Process>> {
    irq::without(|| {
        let s = state();
        // (the process's own set of live threads says which entries go: no walk of every thread in the system, T191)
        let process = s.procs.remove(&pid);
        if let Some(p) = process.as_ref() {
            for tid in &p.live_tids {
                s.thread_pid.remove(tid);
            }
        }
        // nobody of this process sleeps on a futex any more (a killed sleeper has normally left
        // already; this is the backstop)
        s.futexes.forget_process(pid);
        process
    })
}

#[inline(never)]
pub(super) fn teardown_objects(process: &mut Process, code: i32) {
    irq::without(|| {
        let s = state();
        process.handles.drain(&mut s.objs);
        if let Some(end) = process.kernel_end.take() {
            let _ = s.ktable.close(&mut s.objs, end);
        }
        if let Some(end) = process.exc_channel.take() {
            let _ = s.ktable.close(&mut s.objs, end);
        }
        // Anything still alive that nothing can reach (reference cycles). The roots are
        // computed here, in the same interrupts-off step as the sweep, and include the
        // kernel's own handles.
        let roots = s.roots();
        s.objs.sweep(&roots);
        if let Some(obj) = process.obj {
            ipc::process_mark_terminated(&mut s.objs, obj, code);
        }
        if !s.line.is_empty() {
            let line = core::mem::take(&mut s.line);
            let _ = writeln!(Console, "{line}");
        }
    });
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[inline(never)]
pub(super) fn teardown_memory(process: Box<Process>) {
    irq::without(|| {
        let k = kmem::get();
        let mut process = process;
        process.aspace.destroy_user(k.mm, k.vmos, k.jobs);
        for id in core::mem::take(&mut process.vmos) {
            let _ = k.vmos.release(k.mm, k.jobs, id);
        }
        // the stacks `thread_spawn` mapped for threads that were still alive (their mappings went with the address space)
        for st in process.stacks.values() {
            let _ = k.vmos.release(k.mm, k.jobs, st.vmo);
        }
        let tables: u64 = process.table_charge.values().sum();
        // the address space and its page tables are gone: no hart may keep translations through them
        crate::smp::shootdown();
        k.jobs.credit(process.job, Resource::Pages, tables);
        k.jobs.credit(
            process.job,
            Resource::KernelBytes,
            process.handles.charged(),
        );
        k.jobs.credit(process.job, Resource::Processes, 1);
        k.jobs.release(process.job);
    });
}

/// Unmap and free the stack `thread_spawn` gave thread `tid`, if it has one. The thread is in the kernel, on its
/// kernel stack, so the stack it stood on in user mode is nobody's; the shootdown comes before the frames can be reused.
pub(super) fn free_spawn_stack(p: &mut Process, tid: Tid) {
    let Some(st) = p.stacks.remove(&tid) else {
        return;
    };
    let k = kmem::get();
    let _ = p.aspace.unmap_region(k.mm, k.vmos, k.jobs, st.region);
    if let Some(tables) = p.table_charge.remove(&st.region) {
        k.jobs.credit(p.job, Resource::Pages, tables);
    }
    // other harts may cache translations of the stack: they go before the frames can be reused
    crate::smp::shootdown();
    kmem::flush_tlb();
    let _ = k.vmos.release(k.mm, k.jobs, st.vmo);
}

pub(super) fn exit_process(code: i32) -> ! {
    leave(code, true)
}

/// The calling thread ends. With `whole`, its process ends too (`process_exit`, a fault, a
/// kill): every other thread is told to die. Without it (`thread_exit`) only this thread goes,
/// and the process ends when the last thread does. The process is torn down by its last thread
/// (a process started by `process_start`), or by `finish` (one the kernel started).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: threads_alive >= 1 for a thread that is leaving
pub(super) fn leave(code: i32, whole: bool) -> ! {
    if whole {
        LAST_EXIT_CYCLES.store(crate::timer::now(), Ordering::SeqCst);
    }
    let me = irq::without(|| {
        let s = state();
        let tid = thread::current();
        crate::fp::thread_gone(tid);
        s.sched_flags.remove(&tid);
        s.interrupts.remove(&tid);
        ksys::thread_suspend_forget(s, tid);
        sync_suspends(s);
        let pid = s.thread_pid.remove(&tid)?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        p.live_tids.remove(&tid);
        // A code the process was already given by a kill wins over this thread's own.
        let code = p.killed.unwrap_or(code);
        if let Some(o) = p.thread_objs.remove(&tid) {
            ipc::thread_mark_terminated(objs, o, code);
        }
        p.threads_alive -= 1;
        credit_thread(kmem::jobs().jobs, p.job);
        if !whole {
            // (a whole-process exit leaves the stacks to the teardown)
            free_spawn_stack(p, tid);
        }
        if whole {
            p.exit_code.get_or_insert(code);
            p.killed.get_or_insert(code);
            bump_attention();
            // (siblings spinning in user mode on other harts must be told: with no periodic tick (L13) nothing else would stop them)
            if !cfg!(feature = "fault-skip-kill-ipi") {
                crate::smp::kick_others();
            }
        } else if p.threads_alive == 0 {
            p.exit_code.get_or_insert(code);
        }
        Some((pid, p.detached, p.threads_alive))
    });
    match me {
        Some((pid, _, alive)) if alive > 0 => {
            notify(); // waiters on this thread
                      // siblings: only when the whole process is going (a futex sleeper is on no queue, and a kill must reach it). A plain `thread_exit`
                      // changes nothing a sibling waits on, and waking all of them on every exit is quadratic in the thread count: 2047 exits took
                      // minutes at four harts
            if whole {
                wake_threads_of(pid);
            }
        }
        Some((pid, true, _)) => {
            teardown(pid);
        }
        // the last thread of a kernel-started process: `finish` tears it down, but waiters
        // on this thread must not have to wait for that poll
        _ => notify(),
    }
    thread::exit()
}

/// Whether the calling thread has a pending `thread_interrupt` (for wait conditions; interrupts are off or the lock is held).
pub(super) fn interrupted_now() -> bool {
    state().interrupts.contains(&thread::current())
}

/// Take the calling thread's pending interrupt, if any: the blocking call that reports INTERRUPTED does this.
pub(super) fn take_interrupt() -> bool {
    irq::without(|| state().interrupts.remove(&thread::current()))
}

/// The job the user thread `tid` belongs to (`None` for a kernel thread): who pays for the register state the kernel keeps for it (MM5).
/// Needs the big kernel lock.
pub(crate) fn job_of_thread(tid: Tid) -> Option<JobId> {
    let s = state();
    let pid = s.thread_pid.get(&tid)?;
    Some(s.procs.get(pid)?.job)
}

/// Charge the growth of the calling thread's handle table to its process's job (MM5; `HandleTable::settle_growth`): a job that cannot pay
/// leaves the table unable to grow, so the calls that insert handles say NO_MEMORY.
pub(super) fn settle_handle_table(growth_at_entry: u64) {
    // (the usual syscall grew no table anywhere while it ran: two loads and out, before the big lock is touched. `growth_at_entry` is the
    // count when this call began, kept by the call itself, so a thread that was switched to another hart meanwhile still compares its own)
    if kcore::handle::growth_events() == growth_at_entry && kcore::handle::clamped_tables() == 0 {
        return;
    }
    irq::without(|| {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&thread::current()) else {
            return;
        };
        let Some(p) = s.procs.get_mut(&pid) else {
            return;
        };
        if p.handles.slot_bytes() == p.handles.charged() {
            return; // (the usual case: no growth, nothing to take)
        }
        let job = p.job;
        let j = kmem::jobs();
        p.handles.settle_growth(|n| {
            j.jobs
                .charge(job, kcore::job::Resource::KernelBytes, n)
                .is_ok()
        });
    });
}

/// `take_interrupt` for a caller that already holds the big kernel lock (and the memory locks, so it must not take the big lock again).
pub(super) fn take_interrupt_held() -> bool {
    state().interrupts.remove(&thread::current())
}

/// The exit code the calling thread's process was told to die with (`process_kill`), if any.
pub(super) fn killed_now() -> Option<i32> {
    let s = state();
    let pid = s.thread_pid.get(&thread::current())?;
    s.procs.get(pid)?.killed
}

/// Where a stopped process stops, a suspended thread stops (`thread_suspend`, EX1) and a killed one dies: the calling thread's return to user
/// mode. A thread of a process that `process_suspend` stopped, or whose own suspend count is above 0, sleeps here (woken by id, like a
/// futex sleeper) until `process_resume`, `thread_resume` or a kill; a killed process ends here too. SUSPENDED rises when the thread parks.
/// Nothing is held across the sleep but the big lock, which follows the thread as it does in every blocking call.
pub(super) fn park_while_stopped() {
    loop {
        if let Some(code) = irq::without(killed_now) {
            exit_process(code);
        }
        let (stopped, suspended) = irq::without(|| (stopped_now(), suspended_now()));
        if !stopped && !suspended {
            return;
        }
        if suspended {
            // (the signal rises, once: the thread has stopped)
            irq::without(|| {
                let s = state();
                if let Some(&pid) = s.thread_pid.get(&thread::current()) {
                    ksys::thread_at_return(s, pid, thread::current());
                }
            });
        }
        commit_wake(); // (wakes this trap made are committed before this thread sleeps, as at every blocking site)
        irq::without(|| {
            state().parked_tids.insert(thread::current());
        });
        thread::wait_deadline_private(
            || killed_now().is_some() || (!stopped_now() && !suspended_now()),
            u64::MAX,
        );
        irq::without(|| {
            state().parked_tids.remove(&thread::current());
        });
    }
}

/// Is the calling thread suspended (`thread_suspend`: its count is above 0)? One comparison when nothing is.
pub(super) fn suspended_now() -> bool {
    let s = state();
    !s.suspends.is_empty() && ksys::thread_is_suspended(s, thread::current())
}

/// How many threads have suspend state (a copy of `UserState::suspends.len()`, kept by `sync_suspends`): `thread::block` looks here, with no lock,
/// so that the common case costs one load and takes no lock (a kernel thread may block holding a lock the big lock must not be taken under).
static SUSPEND_STATES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Record the size of the suspend table after a change (under the big lock).
pub(super) fn sync_suspends(s: &UserState) {
    SUSPEND_STATES.store(s.suspends.len(), Ordering::Release);
}

/// Called before a thread blocks in a call: a thread that was suspended after it entered the call has stopped where it is (SUSPENDED rises).
pub(crate) fn suspend_blocks() {
    if SUSPEND_STATES.load(Ordering::Acquire) == 0 {
        return;
    }
    irq::without(|| {
        let s = state();
        if s.suspends.is_empty() {
            return;
        }
        let tid = thread::current();
        let Some(&pid) = s.thread_pid.get(&tid) else {
            return;
        };
        if ksys::thread_blocks(s, pid, tid) {
            commit_wake();
        }
    });
}

/// Is the calling thread's process stopped (`process_suspend`)?
pub(super) fn stopped_now() -> bool {
    let s = state();
    s.thread_pid
        .get(&thread::current())
        .and_then(|pid| s.procs.get(pid))
        .is_some_and(|p| p.stopped)
}
