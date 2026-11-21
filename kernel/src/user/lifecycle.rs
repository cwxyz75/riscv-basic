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
    let (ts, token, killed) = {
        let s = state();
        let ts = s
            .starts
            .remove(&token)
            .expect("a thread start was registered");
        // Registered here, by the thread itself, before it can make a syscall: the spawner
        // cannot be preempted into leaving a window where the thread is unknown.
        s.thread_pid.insert(thread::current(), ts.pid);
        let p = s
            .procs
            .get_mut(&ts.pid)
            .expect("a process lives while one of its threads does");
        if let Some(o) = ts.obj {
            p.thread_objs.insert(thread::current(), o);
        }
        if let Some(st) = ts.stack {
            p.stacks.insert(thread::current(), st);
        }
        if ts.rr {
            s.sched_flags.insert(thread::current(), 1);
        }
        let token = aspace_token_of(p);
        // A thread that was created before its job's maximum was lowered has not been seen by the clamp (it was in
        // no table yet): clamp it now (PR1; review of task 8).
        let max = kmem::get()
            .vmos
            .jobs
            .sched(p.job)
            .map_or(63, |j| j.max_prio)
            .max(1);
        let late_clamp = thread::priority_of(thread::current()).is_some_and(|now| now > max);
        if late_clamp {
            thread::set_priority(thread::current(), max);
        }
        (ts, token, p.killed)
    };
    // Killed before it ever ran: it never runs user code.
    if let Some(code) = killed {
        crate::bkl::unlock();
        irq::enable();
        leave(code, true);
    }
    // A thread created in a stopped process waits here: it runs no user code before `process_resume` (or dies with the
    // process).
    park_while_stopped();
    // (the return to user mode is a defined point for a switch a wake asked for: X7)
    thread::preempt_point();
    // The marks above were read before `preempt_point`, which can switch and let a kill or stop land (and another thread on this hart
    // record the new epoch) before this thread resumes: its first trap takes the full path, which looks again (PR 267 review)
    crate::percpu::this()
        .attention_seen
        .store(0, Ordering::SeqCst);
    // The lock is released here, with interrupts still off: only `enter_user`'s own instructions
    // (loads from this thread's stack and CSR writes) run before `sret`.
    crate::bkl::unlock();
    user_mode_entered();
    // SAFETY: `enter_user` switches to the process's address space and drops to U-mode at
    // its entry point with the stack and argument chosen above; it never returns.
    unsafe {
        crate::arch::enter_user(
            ts.entry,
            ts.sp,
            ts.arg,
            token,
            top,
            ts.tp,
            if ts.spawned { thread::current() } else { 0 },
        )
    }
}

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
    REAP_DIRTY.store(true, Ordering::SeqCst); // its mappings are gone: pending VMOs may go
    notify(); // peers of what this process held now see PEER_CLOSED; waiters see TERMINATED
    code
}

// (the three steps of `teardown` are functions of their own, never merged into one frame: the optimised kernel's stack budget, KT13, T173)

#[inline(never)]
pub(super) fn teardown_take(pid: u64) -> Option<Box<Process>> {
    irq::without(|| {
        let s = state();
        s.thread_pid.retain(|_, p| *p != pid);
        // nobody of this process sleeps on a futex any more (a killed sleeper has normally left
        // already; this is the backstop)
        s.futexes.forget_process(pid);
        s.procs.remove(&pid)
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

#[inline(never)]
pub(super) fn teardown_memory(process: Box<Process>) {
    irq::without(|| {
        let k = kmem::get();
        let mut process = process;
        process.aspace.destroy_user(&mut k.mm);
        for id in core::mem::take(&mut process.vmos) {
            let _ = k.vmos.destroy(&mut k.mm, id);
        }
        // the stacks `thread_spawn` mapped for threads that were still alive (their mappings went with the address space)
        for st in process.stacks.values() {
            let _ = k.vmos.destroy(&mut k.mm, st.vmo);
        }
        let tables: u64 = process.table_charge.values().sum();
        // the address space and its page tables are gone: no hart may keep translations through them
        crate::smp::shootdown();
        k.vmos.jobs.credit(process.job, Resource::Pages, tables);
        k.vmos.jobs.credit(process.job, Resource::Processes, 1);
        k.vmos.jobs.release(process.job);
    });
}

/// Unmap and free the stack `thread_spawn` gave thread `tid`, if it has one. The thread is in the kernel, on its
/// kernel stack, so the stack it stood on in user mode is nobody's; the shootdown comes before the frames can be reused.
pub(super) fn free_spawn_stack(p: &mut Process, tid: Tid) {
    let Some(st) = p.stacks.remove(&tid) else {
        return;
    };
    let k = kmem::get();
    let _ = p.aspace.unmap_region(&mut k.mm, st.region);
    if let Some(tables) = p.table_charge.remove(&st.region) {
        k.vmos.jobs.credit(p.job, Resource::Pages, tables);
    }
    // other harts may cache translations of the stack: they go before the frames can be reused
    crate::smp::shootdown();
    kmem::flush_tlb();
    let _ = k.vmos.destroy(&mut k.mm, st.vmo);
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
        let pid = s.thread_pid.remove(&tid)?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        // A code the process was already given by a kill wins over this thread's own.
        let code = p.killed.unwrap_or(code);
        if let Some(o) = p.thread_objs.remove(&tid) {
            ipc::thread_mark_terminated(objs, o, code);
        }
        p.threads_alive -= 1;
        credit_thread(&mut kmem::get().vmos.jobs, p.job);
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
            wake_threads_of(pid); // siblings (a futex sleeper is on no queue)
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

/// The exit code the calling thread's process was told to die with (`process_kill`), if any.
pub(super) fn killed_now() -> Option<i32> {
    let s = state();
    let pid = s.thread_pid.get(&thread::current())?;
    s.procs.get(pid)?.killed
}

/// Where a stopped process stops and a killed one dies: the calling thread's return to user mode. A thread of a process
/// that `process_suspend` stopped sleeps here (woken by id, like a futex sleeper) until `process_resume` or a kill; a
/// killed process ends here too. Nothing is held across the sleep but the big lock, which follows the thread as it does
/// in every blocking call.
pub(super) fn park_while_stopped() {
    loop {
        if let Some(code) = irq::without(killed_now) {
            exit_process(code);
        }
        if !irq::without(stopped_now) {
            return;
        }
        commit_wake(); // (wakes this trap made are committed before this thread sleeps, as at every blocking site)
        thread::wait_deadline_private(|| killed_now().is_some() || !stopped_now(), u64::MAX);
    }
}

/// Is the calling thread's process stopped (`process_suspend`)?
pub(super) fn stopped_now() -> bool {
    let s = state();
    s.thread_pid
        .get(&thread::current())
        .and_then(|pid| s.procs.get(pid))
        .is_some_and(|p| p.stopped)
}
