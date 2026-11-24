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
        with_state(|s| {
            let ts = s
                .starts
                .remove(&token)
                .expect("a thread start was registered");
            // (KT9: the spawner registered the thread, with its record, its register image and the donation graph's node, in the section that
            // made it: all that is left here is what only the thread's own first slice can do)
            let tid = thread::current();
            // (its CPU time is charged to its process's job from its first slice on: SCHED-SMP-SPEC 4.10)
            if let Some(p) = s.procs.get(&ts.pid) {
                thread::set_job(tid, thread::job_sched().tag(p.job.index()));
            }
            let p = s
                .procs
                .get_mut(&ts.pid)
                .expect("a process lives while one of its threads does");
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
        })
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
        if killed_now().is_none() && !stopped_now() && !suspended_now() {
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
    let token = with_state(|s| {
        let UserState { asids, procs, .. } = s;
        let p = procs
            .get_mut(&ts.pid)
            .expect("a process lives while one of its threads does");
        crate::percpu::this().cur_as.store(
            alloc::sync::Arc::as_ptr(&p.aspace).cast_mut(),
            Ordering::Release,
        );
        crate::percpu::this()
            .cur_lib_start
            .store(p.libsys.text_start, Ordering::Relaxed);
        crate::percpu::this()
            .cur_lib_end
            .store(p.libsys.text_end, Ordering::Relaxed);
        install_token_of(asids, p)
    });
    crate::percpu::this()
        .active_aspace
        .store(token, Ordering::SeqCst);
    crate::percpu::this()
        .cur_pid
        .store(ts.pid, Ordering::Relaxed); // (the lock-free calls read it: L14)
                                           // The lock is released here, with interrupts still off: only `enter_user`'s own instructions
                                           // (loads from this thread's stack and CSR writes) run before `sret`.
                                           // (EX1) the image the thread starts from (the spawner's or an earlier `thread_write_state`'s, or the one made at registration), with its FP and
                                           // vector state loaded if a write gave it any, in this hart's entry frame
    let entry_frame = {
        let image = with_state(|s| {
            s.thread_mut(thread::current())
                .and_then(|t| t.initial_regs.take())
                .map(kcore::fbox::FBox::into_inner)
        });
        let image = image.unwrap_or_else(|| {
            let mut regs = ksys::initial_general(
                ts.entry,
                ts.sp,
                ts.arg,
                ts.tp,
                if ts.spawned { thread::current() } else { 0 },
            );
            regs[11] = ts.a2;
            regs
        });
        let mut frame = crate::trap::TrapFrame::from_general(&image);
        crate::fp::mark_present(thread::current(), &mut frame);
        crate::fp::return_to_user_held(&mut frame);
        let hart = crate::percpu::this().index.load(Ordering::Relaxed);
        // SAFETY: the pointer is to this hart's one element, made without a reference to the array (the others' elements may be read by their harts
        // meanwhile: a hart reads its own in `enter_user_frame`, after it dropped the lock), and the hart index is below the array's length (one element per hart). This hart alone writes it:
        // the object lock is held and interrupts are off, the hart index was read after the last switch point of this thread, and the element is read
        // again only by this hart's own `enter_user_frame` (T242).
        unsafe {
            let slot = ENTRY_FRAMES
                .0
                .get()
                .cast::<crate::trap::TrapFrame>()
                .add(hart);
            *slot = frame;
            slot as *const crate::trap::TrapFrame
        }
    };
    crate::bkl::unlock();
    user_mode_entered();
    // SAFETY: the contract of `enter_user_frame`: `entry_frame` is this hart's static entry frame (it does not overlap the stack), `token` was taken under the
    // object lock above, with no switch point after it, and names the live root of this thread's own process (upper half the kernel's), `top` is the top of this
    // thread's own kernel stack, interrupts are off and no lock is held (the object lock was dropped just above); the call switches to that space and drops to
    // U-mode with the image, and never returns (T242).
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
    while locked_state(|s| s.procs.get(&pid).map_or(0, |p| p.threads_alive)) > 0 {
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
    // The process is private to this thread now: let the big lock go **completely** for the two long steps. A thread that comes here from a trap holds it at
    // depth 1 (and a nested critical section is at depth 2, which never reaches a switch point), so without this the slices below would be slices of
    // one hold. The slices take it themselves, one at a time (`irq::without` ends in a switch point at depth 1); nothing here holds a borrow of the state.
    let depth = crate::bkl::release_all();
    teardown_objects(&mut process, code);
    teardown_memory(process);
    crate::bkl::reacquire(depth);
    kmem::flush_tlb();
    notify(); // peers of what this process held now see PEER_CLOSED; waiters see TERMINATED
    code
}

// (the three steps of `teardown` are functions of their own, never merged into one frame: the optimised kernel's stack budget, KT13, T173)

#[inline(never)]
pub(super) fn teardown_take(pid: u64) -> Option<kcore::fbox::FBox<Process>> {
    any_state(|s| {
        // (the process's own set of live threads says which entries go: no walk of every thread in the system, T191)
        let process = s.procs.remove(&pid);
        if let Some(p) = process.as_ref() {
            if !p.live_tids.is_empty() {
                thread::charge_running(); // (the threads that end with the process may be running on any hart)
            }
            for tid in p.live_tids.iter() {
                // (the threads that end with the process: their CPU time goes to their objects and the process's total, AB6)
                let tobj = s.unregister_thread(*tid).and_then(|t| t.obj);
                ipc::thread_cpu_final(&mut s.objs, tobj, p.obj, thread::cpu_ns(*tid));
            }
        }
        // nobody of this process sleeps on a futex any more (a killed sleeper has normally left
        // already; this is the backstop)
        futex_table().forget_process(pid);
        process
    })
}

/// The handles one slice of a teardown closes, and the objects one slice of its sweep looks at (T206: SCHED-SMP-SPEC X4, no section proportional to
/// what the process held or to the live objects of the system).
const TEARDOWN_HANDLES: usize = 256;
const TEARDOWN_SWEEP: usize = 256;

#[inline(never)]
pub(super) fn teardown_objects(process: &mut Process, code: i32) {
    // 1. the handles, a slice at a time: each slice is its own critical section, so the big lock is let go (and a switch taken) between them
    loop {
        let done = locked_state(|s| {
            let done = process.handles.drain_some(&mut s.objs, TEARDOWN_HANDLES);
            if done {
                if let Some(end) = process.kernel_end.take() {
                    let _ = s.ktable.close(&mut s.objs, end);
                }
                if let Some(end) = process.exc_channel.take() {
                    let _ = s.ktable.close(&mut s.objs, end);
                }
                // (the process's handles were drained: a job exception channel it held the user end of is reaped now)
                ksys::exception::reap_closed_job_channels(s);
            }
            done
        });
        if done {
            break;
        }
    }
    // 2. anything still alive that nothing can reach (reference cycles): the sweep as the state machine of MM10, a slice per critical section. The roots
    // are shaded a process table per slice (T206), the kernel's own table last; a handle made or moved once the sweep has begun marks its object (that is
    // what the machine's barrier is for). A sweep another
    // call started (with older roots) is finished first, a slice at a time.
    loop {
        let began = locked_state(|s| {
            if s.objs.sweep_active() {
                s.sweep_slice(TEARDOWN_SWEEP);
                return false;
            }
            s.sweep_start()
        });
        if began {
            break;
        }
    }
    while locked_state(|s| {
        s.objs.sweep_active()
            && !matches!(
                s.sweep_slice(TEARDOWN_SWEEP),
                kcore::object::SweepStep::Done(_)
            )
    }) {}
    // 3. the process is over
    locked_state(|s| {
        if let Some(obj) = process.obj {
            ipc::process_mark_terminated(&mut s.objs, obj, code);
        }
        if !s.line.is_empty() {
            let line = core::mem::take(&mut s.line);
            let _ = writeln!(Console, "{line}");
        }
    });
}

/// The pages one chunk of a teardown unmaps or gives back (SCHED-SMP-SPEC X4: 64 pages).
const TEARDOWN_CHUNK: u64 = 64;

/// Free the memory of a process that is gone from the table (L19). **No big kernel lock**: the process is private to this thread (nothing can fault in
/// its space, no call can name it), so what is left is the memory locks, taken for one chunk of at most `TEARDOWN_CHUNK` pages at a time and let go
/// between chunks, which are switch points even for a caller whose trap left interrupts off, so that no section is proportional to what the process
/// mapped or owned. Each chunk is shot down before its locks are let go (TLB2).
#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[inline(never)]
pub(super) fn teardown_memory(process: kcore::fbox::FBox<Process>) {
    let mut process = process.into_inner();
    // (KT9, T177: the address-space cell goes back to the pool when the space is out of it)
    let mut cell = process.aspace;
    let mut aspace = klock::aslock::AsCell::vacate(&mut cell);
    // 1. the mappings, a chunk at a time. Each chunk is unmapped and shot down in one interrupts-off section under the memory locks (TLB2: nothing it
    // frees, a table or a VMO's last frame, can be reused before every hart has dropped what it cached; X6: the hart cannot change under the shootdown);
    // then a switch point (X4: a trap from user mode arrives here with interrupts off, so the chunk boundary is where a tick's switch is taken)
    loop {
        let was = irq::save_and_disable();
        let done = {
            let k = kmem::mem();
            let done = aspace.destroy_user_chunk(k.mm, k.vmos, k.jobs, TEARDOWN_CHUNK);
            crate::smp::shootdown();
            done
        };
        irq::restore(was);
        irq::without(|| {});
        if done {
            break;
        }
    }
    {
        // the tables go with a shootdown of their own, inside the locks, so no hart's walk cache outlives them
        let was = irq::save_and_disable();
        {
            let k = kmem::mem();
            aspace.finish_destroy_user(k.mm, k.jobs);
            crate::smp::shootdown();
        }
        irq::restore(was);
    }
    // 2. the VMOs the process owned: the pages of one that nothing else holds go back in chunks, then the VMO itself
    let ids: alloc::vec::Vec<u64> = core::mem::take(&mut process.vmos)
        .into_iter()
        .chain(process.stacks.values().map(|st| st.vmo))
        .collect();
    for id in ids {
        irq::without(|| {}); // a switch point (X4)
        {
            let k = kmem::mem();
            if k.vmos.is_unshared_plain(id) {
                let pages = k.vmos.pages(id).unwrap_or(0);
                drop(k);
                let mut first = 0;
                while first < pages {
                    {
                        let k = kmem::mem();
                        let _ = k.vmos.decommit(
                            k.mm,
                            k.jobs,
                            id,
                            first,
                            TEARDOWN_CHUNK.min(pages.saturating_sub(first)),
                        );
                    }
                    first = first.saturating_add(TEARDOWN_CHUNK);
                    irq::without(|| {});
                }
            }
        }
        let k = kmem::mem();
        let _ = k.vmos.release(k.mm, k.jobs, id);
    }
    // 3. what the process was charged
    let k = kmem::mem();
    let tables: u64 = process.table_charge.values().sum();
    k.jobs.credit(process.job, Resource::Pages, tables);
    k.jobs.credit(
        process.job,
        Resource::KernelBytes,
        process.handles.charged(),
    );
    k.jobs.credit(process.job, Resource::Processes, 1);
    k.jobs.release(process.job);
    drop(k);
    any_state(|s| s.return_as_cell(cell));
}

/// Unmap and free the stack `thread_spawn` gave thread `tid`, if it has one. The thread is in the kernel, on its
/// kernel stack, so the stack it stood on in user mode is nobody's; the shootdown comes before the frames can be reused.
pub(super) fn free_spawn_stack(p: &mut Process, tid: Tid) {
    let Some(st) = p.stacks.remove(&tid) else {
        return;
    };
    let _as = klock::aslock::AsCell::hold(&p.aspace); // (before the memory locks: AS comes first)
    let k = kmem::get();
    let _ = p
        .aspace
        .held_mut()
        .unmap_region(k.mm, k.vmos, k.jobs, st.region);
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
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn leave(code: i32, whole: bool) -> ! {
    if whole {
        LAST_EXIT_CYCLES.store(crate::timer::now(), Ordering::SeqCst);
    }
    let me = any_state(|s| {
        let tid = thread::current();
        crate::fp::thread_gone(tid);
        // (the thread's record goes, and with it its flags, its suspend state, its register images and its pending interrupt)
        let rec = s.unregister_thread(tid);
        sync_interrupts(s);
        sync_suspends(s);
        let rec = rec?;
        let pid = rec.pid;
        pi_thread_gone(s, tid, pid); // (a PI futex it owned is poisoned, and it leaves the donation graph: L23)
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        p.live_tids.remove(&tid);
        // A code the process was already given by a kill wins over this thread's own.
        let code = p.killed.unwrap_or(code);
        let cpu = thread::charge_and_cpu_ns(); // (this thread's last interval counts; it runs here)
        let tobj = rec.obj;
        ipc::thread_cpu_final(objs, tobj, p.obj, cpu);
        if let Some(o) = tobj {
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
    with_state(|s| s.interrupt_pending(thread::current()))
}

/// `interrupted_now` for a caller that has the state open already.
pub(super) fn interrupted_in(s: &UserState) -> bool {
    s.interrupt_pending(thread::current())
}

/// Take the calling thread's pending interrupt, if any: the blocking call that reports INTERRUPTED does this.
pub(super) fn take_interrupt() -> bool {
    any_state(|s| {
        let had = s.set_interrupt(thread::current(), false);
        sync_interrupts(s);
        had
    })
}

/// The job the user thread `tid` belongs to (`None` for a kernel thread): who pays for the register state the kernel keeps for it (MM5).
/// Needs the big kernel lock.
pub(crate) fn job_of_thread(tid: Tid) -> Option<JobId> {
    with_state(|s| {
        let pid = s.pid_of(tid)?;
        Some(s.procs.get(&pid)?.job)
    })
}

/// Charge the growth of the calling thread's handle table to its process's job (MM5; `HandleTable::settle_growth`): a job that cannot pay
/// leaves the table unable to grow, so the calls that insert handles say NO_MEMORY.
pub(super) fn settle_handle_table(growth_at_entry: u64) {
    // (the usual syscall grew no table anywhere while it ran: two loads and out, before the big lock is touched. `growth_at_entry` is the
    // count when this call began, kept by the call itself, so a thread that was switched to another hart meanwhile still compares its own)
    if kcore::handle::growth_events() == growth_at_entry && kcore::handle::clamped_tables() == 0 {
        return;
    }
    locked_state(|s| {
        let Some(pid) = s.pid_of(thread::current()) else {
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
    // (not `with_state`: the caller holds the memory locks, and the object lock comes before them in the order; it holds the object lock too)
    with_state(|s| {
        let had = s.set_interrupt(thread::current(), false);
        sync_interrupts(s);
        had
    })
}

/// The exit code the calling thread's process was told to die with (`process_kill`), if any.
pub(super) fn killed_now() -> Option<i32> {
    with_state(|s| killed_in(s))
}

/// `killed_now` for a caller that has the state open already.
pub(super) fn killed_in(s: &UserState) -> Option<i32> {
    let pid = s.pid_of(thread::current())?;
    s.procs.get(&pid)?.killed
}

/// Where a stopped process stops, a suspended thread stops (`thread_suspend`, EX1) and a killed one dies: the calling thread's return to user
/// mode. A thread of a process that `process_suspend` stopped, or whose own suspend count is above 0, sleeps here (woken by id, like a
/// futex sleeper) until `process_resume`, `thread_resume` or a kill; a killed process ends here too. SUSPENDED rises when the thread parks.
/// Nothing is held across the sleep but the big lock, which follows the thread as it does in every blocking call.
pub(super) fn park_while_stopped() {
    loop {
        // (one lookup of the process for all three questions: this runs on every return to user mode)
        let (killed, stopped, suspended) = any_state(|s| {
            let tid = thread::current();
            let p = s.pid_of(tid).and_then(|pid| s.procs.get(&pid));
            (
                p.and_then(|p| p.killed),
                p.is_some_and(|p| p.stopped),
                s.n_suspended != 0 && ksys::thread_is_suspended(s, tid),
            )
        });
        if let Some(code) = killed {
            exit_process(code);
        }
        if !stopped && !suspended {
            return;
        }
        if suspended {
            // (the signal rises, once: the thread has stopped)
            any_state(|s| {
                if let Some(pid) = s.pid_of(thread::current()) {
                    ksys::thread_at_return(s, pid, thread::current());
                }
            });
        }
        commit_wake(); // (wakes this trap made are committed before this thread sleeps, as at every blocking site)
        any_state(|s| {
            if let Some(t) = s.thread_mut(thread::current()) {
                t.parked = true;
            }
        });
        thread::wait_deadline_private(
            || {
                killed_now().is_some()
                || (!stopped_now() && !suspended_now())
                // (resumed and suspended again before this thread ran: it parks afresh, which raises SUSPENDED)
                || (suspended_now() && with_state(|s| ksys::thread_stop_pending(s, thread::current())))
            },
            u64::MAX,
        );
        any_state(|s| {
            if let Some(t) = s.thread_mut(thread::current()) {
                t.parked = false;
            }
        });
    }
}

/// Is the calling thread suspended (`thread_suspend`: its count is above 0)? One comparison when nothing is.
pub(super) fn suspended_now() -> bool {
    with_state(|s| s.n_suspended != 0 && ksys::thread_is_suspended(s, thread::current()))
}

/// How many threads have suspend state (a copy of `UserState::suspends.len()`, kept by `sync_suspends`): `thread::block` looks here, with no lock,
/// so that the common case costs one load and takes no lock (a kernel thread may block holding a lock the big lock must not be taken under).
pub(super) static SUSPEND_STATES: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// How many threads have a pending interrupt flag (`thread_interrupt`): a copy of `UserState::interrupts.len()`, kept by `sync_interrupts`, read with no
/// lock by the quiet `futex_wait` (L19): a call that would take the flag must take the full path.
pub(super) static INTERRUPT_FLAGS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Record the size of the interrupt set after a change (under the big lock).
pub(super) fn sync_interrupts(s: &UserState) {
    INTERRUPT_FLAGS.store(s.n_interrupts, Ordering::Release);
}

/// Record the size of the suspend table after a change (under the big lock).
pub(super) fn sync_suspends(s: &UserState) {
    SUSPEND_STATES.store(s.n_suspended, Ordering::Release);
}

/// Called before a thread blocks in a call: a thread that was suspended after it entered the call has stopped where it is (SUSPENDED rises).
pub(crate) fn suspend_blocks() {
    if SUSPEND_STATES.load(Ordering::Acquire) == 0 {
        return;
    }
    with_state(|s| {
        if s.n_suspended == 0 {
            return;
        }
        let tid = thread::current();
        let Some(pid) = s.pid_of(tid) else {
            return;
        };
        if ksys::thread_blocks(s, pid, tid) {
            commit_wake();
        }
    });
}

/// Is the calling thread's process stopped (`process_suspend`)?
pub(super) fn stopped_now() -> bool {
    with_state(|s| {
        s.pid_of(thread::current())
            .and_then(|pid| s.procs.get(&pid))
            .is_some_and(|p| p.stopped)
    })
}
