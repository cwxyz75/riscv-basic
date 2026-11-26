//! Waking the waiters: the signal commit after every syscall, the cleanup of dead objects, the user-mode counters and the kill request.

use super::*;

/// An interrupt source fired (trap context, BKL held): hand it to the interrupt object that owns it.
pub fn irq_fired(irq: u32) -> bool {
    with_state(|s| {
        let Some(&Some(obj)) = s.irq_objs.get(irq as usize) else {
            return false;
        };
        ipc::interrupt_fire(&mut s.objs, obj, irq)
    })
}

/// Wake the waiters whose condition the interrupt path changed, without the cleanup `notify` does.
pub fn wake_signal_waiters() {
    commit_wake();
}

/// Wake exactly the threads whose wait the objects touched since the last call can have ended (slice W,
/// W-SPEC section 3): drain the registry's dirty log (and the handles removed without it), let the wait
/// table decide, and wake those threads by id. Called after every syscall and wherever a signal can have
/// changed outside one.
pub fn commit_wake() {
    commit_wake_with(false);
}

/// The object lock's commit-on-drop (W1', SCHED-SMP-SPEC 3.8, L20): called by `irq::without` at the end of every **outermost** critical section, with the
/// lock still held: if the section touched anything a waiter can see, the wait table decides and the waiters are woken before the lock is let go. Then
/// the trap's tail has nothing to commit and can run without the lock.
pub fn commit_if_dirty() {
    if !super::USER_READY.load(Ordering::Acquire) {
        return;
    }
    if with_state(|s| s.objs.has_pending()) {
        commit_wake_with(false);
    }
}

/// `commit_wake`, with the wakes placed on this hart as a direct hand-off when `sync` is true (a `channel_call` about to block, L24).
fn commit_wake_with(sync: bool) {
    crate::bkl::ensure(|| commit_wake_held(sync));
}

fn commit_wake_held(sync: bool) {
    // (the wakes are made in the same critical section as the commit, and as one batch: every woken thread
    // is runnable before any of them runs, T62)
    with_state(|s| {
        let UserState { objs, waits, .. } = s;
        // bindings made through a handle that was just removed end (L21c); the ports they leave are touched, so the commit below looks at them
        let gone = objs.take_gone();
        if !gone.is_empty() {
            let j = kmem::jobs();
            crate::heap::nofail(|| ipc::end_bindings_made_through(objs, j.jobs, &gone));
        }
        let dirty = objs.take_dirty();
        if dirty.items.is_empty() && !dirty.overflow {
            return;
        }
        let woken = waits.commit(
            |id| ipc::signals_of_object(objs, id),
            |id, f| objs.for_each_binder(id, f),
            |ch, txid| ipc::channel_has_txid(objs, ch, txid),
            &dirty,
        );
        objs.recycle_dirty(dirty);
        // (the scheduler's queues: class N, a wake cannot be refused)
        crate::heap::nofail(|| {
            if sync {
                thread::wake_many_sync(&woken);
            } else {
                thread::wake_many(&woken);
            }
        });
        waits.recycle(woken);
    });
}

/// Clean up what the syscall destroyed and wake exactly the threads whose wait it ended (slice W: the
/// global wait queue that used to be woken here, OD43, is gone).
pub fn notify() {
    notify_with(false);
}

/// `notify` for a `channel_call` that has queued its request and is about to block: the wake of the server is a direct hand-off (L24).
pub fn notify_sync() {
    notify_with(true);
}

fn notify_with(sync: bool) {
    crate::bkl::ensure(|| notify_held(sync));
}

fn notify_held(sync: bool) {
    cleanup_dead();
    // (MM9) a pager whose request channel lost its reader is dead: the waits for its pages fail
    with_state(ksys::pager::reap_closed_channels);
    super::waits::wake_pager_waiters();
    commit_wake_with(sync);
    // the safe point where the kernel heap may grow (heap.rs)
    crate::heap::service();
}

/// Do the kernel work that destroying objects calls for: free the pages of a VMO that has
/// no handle left, and kill a process whose last handle was closed. (The registry cannot
/// reach the frame database or the process table, so it hands the dead objects back.)
///
/// Every thread may clean (each drain takes its objects out of the registry atomically and every step
/// below is its own locked section); what a thread must not do is recurse into itself, so a thread
/// already cleaning skips, and its loop sees the work the nested call would have done (T63: the guard used
/// to be one global flag, so while a cleaner was preempted every other hart skipped the cleanup).
#[allow(clippy::indexing_slicing)] // INVARIANT: the irq slot comes from an interrupt object the kernel made, below the table size; the time counter is far below u64::MAX
pub(super) fn cleanup_dead() {
    crate::bkl::ensure(cleanup_dead_held);
}

#[allow(clippy::indexing_slicing)] // INVARIANT: as above
fn cleanup_dead_held() {
    let me = thread::current();
    if !with_state(|s| s.begin_cleaning(me)) {
        return; // a cleanup is already running further up this thread's stack; it will see our work
    }
    loop {
        let mut dead = with_state(|s| s.objs.drain_dead());
        if dead.is_empty() {
            // (the empty vector goes back too: a drain leaves the twin without capacity until it does, and the next burial would allocate: KT9)
            with_state(|s| {
                s.objs.recycle(dead);
                s.end_cleaning(me)
            });
            return;
        }
        for obj in dead.drain(..) {
            match obj {
                // the object's reference on its VMO goes; a region that still maps it keeps it alive (L15)
                ipc::KObj::Vmo(id) => {
                    let k = kmem::get();
                    let _ = k.vmos.release(k.mm, k.jobs, id);
                }
                ipc::KObj::Process(p) if p.kill_on_close && !p.terminated => {
                    crate::heap::nofail(|| request_kill(p.pid, -2000))
                }
                ipc::KObj::Job(id) => kmem::jobs().jobs.release(id),
                // the last handle of a device domain (IOMMU-SPEC 4.5): its destruction begins (6.3 step 1, under `IOMMU`) and is completed
                // with no lock held at the tail of this call or fault (`iommu::drain`); the object's use of its job goes
                ipc::KObj::IoDomain(d) => {
                    crate::iommu::closed(d.domain);
                    kmem::jobs().jobs.release(d.job);
                }
                // the last handle to an exception was closed without a decision: it is NEXT (EX2)
                ipc::KObj::Exception(e) => crate::heap::nofail(|| {
                    act_on_resolution(with_state(|s| {
                        ksys::exception::exception_dropped(s, kmem::jobs().jobs, &e)
                    }))
                }),
                // the last handle to a pager: every wait for its pages fails, and the kernel's end of its channel goes (MM9)
                ipc::KObj::Pager(id) => {
                    with_state(|s| ksys::pager::pager_destroyed(s, id));
                    super::waits::wake_pager_waiters();
                }
                // (IOMMU7: a vector's revoke begins under the object lock, and `drain` completes it with none; it owns no wired source)
                ipc::KObj::Interrupt(i) if i.vector.is_some() => with_state(|_| {
                    if let Some(v) = i.vector {
                        crate::iommu::vector_closed(v);
                    }
                }),
                ipc::KObj::Interrupt(i) => with_state(|s| {
                    // mask the source and end its route (its identity is retired by its hart) unless a newer object already owns it
                    let owned = matches!(s.irq_objs.get(i.irq as usize), Some(Some(o))
                        if matches!(s.objs.get(*o), Some(ipc::KObj::Interrupt(n)) if n.irq == i.irq));
                    if !owned {
                        if let Some(slot) = s.irq_objs.get_mut(i.irq as usize) {
                            *slot = None;
                        }
                        crate::aia::release(i.irq);
                    }
                }),
                _ => {}
            }
        }
        with_state(|s| s.objs.recycle(dead)); // (the emptied vector goes back, so the next destruction does not allocate: KT9)
    }
}

/// End process `pid` with `code`, from any other thread: a process with no thread yet is
/// torn down at once; a running one is told to die and woken (its thread exits at its next
/// return to user mode).
/// How many harts are in (or just entering) user mode now, and the most there have been at once.
pub(super) static USER_NOW: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub(super) static USER_MAX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// A hart is about to return to user mode.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: USER_NOW counts events, far below u64::MAX
pub fn user_mode_entered() {
    crate::latency::interrupts_on(); // (the return to user mode turns interrupts on: the span ends)
    crate::percpu::this().in_user.store(true, Ordering::SeqCst);
    let now = USER_NOW.fetch_add(1, Ordering::SeqCst) + 1;
    USER_MAX.fetch_max(now, Ordering::SeqCst);
}

/// A hart took a trap from user mode.
pub fn user_mode_left() {
    crate::percpu::this().in_user.store(false, Ordering::SeqCst);
    USER_NOW.fetch_sub(1, Ordering::SeqCst);
}

/// The most harts that were in user mode at the same moment since the last reset.
pub fn max_harts_in_user_mode() -> usize {
    USER_MAX.load(Ordering::SeqCst)
}

pub fn reset_user_mode_high_water() {
    USER_MAX.store(USER_NOW.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// The `time` counter when a whole process last began to end (the kill-latency test).
pub static LAST_EXIT_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// End process `pid` with `code` from kernel code (the tests).
pub fn end_process(pid: u64, code: i32) {
    request_kill(pid, code);
}

/// Wake every blocked thread of process `pid` by id (a kill must reach sleepers that are on no
/// queue: futex sleepers).
pub(super) fn wake_threads_of(pid: u64) {
    crate::bkl::ensure(|| wake_threads_of_held(pid));
}

fn wake_threads_of_held(pid: u64) {
    // (the process's own set of live threads: not a walk of every thread in the system, T191). Sixteen at a time into an array, from the thread
    // after the last one woken: no list is made, so a kill cannot be refused for want of memory (T248); what is left in class N is the wake
    const BATCH: usize = 16;
    let mut after = 0;
    loop {
        let (batch, n) = with_state(|s| {
            let mut batch = [0; BATCH];
            let mut n: usize = 0;
            if let Some(p) = s.procs.get(&pid) {
                for (slot, &t) in batch
                    .iter_mut()
                    .zip(p.live_tids.iter().skip_while(|&&t| t <= after))
                {
                    *slot = t;
                    n = n.saturating_add(1);
                }
            }
            (batch, n)
        });
        if n == 0 {
            return;
        }
        let woken: &[Tid] = batch.get(..n).unwrap_or(&[]);
        after = woken.last().copied().unwrap_or(after);
        crate::heap::nofail(|| thread::wake_many(woken));
    }
}

pub(super) fn request_kill(pid: u64, code: i32) {
    crate::bkl::ensure(|| request_kill_held(pid, code));
}

fn request_kill_held(pid: u64, code: i32) {
    let direct = with_state(|s| {
        let r = match s.procs.get_mut(&pid) {
            Some(p) if p.state == ProcState::Created => {
                p.exit_code = Some(code);
                true
            }
            Some(p) => {
                p.killed.get_or_insert(code);
                bump_attention();
                false
            }
            None => false,
        };
        // A victim running user code on another hart sees the kill at its next trap: the flag is
        // set first and the kick comes in the same locked region, so a hart that handles the IPI
        // always finds the flag (the next timer tick would do it within a millisecond; this makes
        // it prompt).
        if !cfg!(feature = "fault-skip-kill-ipi") {
            crate::smp::kick_others();
        }
        r
    });
    if direct {
        teardown(pid);
    } else {
        wake_threads_of(pid);
    }
}
