//! Waking the waiters: the signal commit after every syscall, the cleanup of dead objects, the user-mode counters and the kill request.

use super::*;

/// An interrupt source fired (trap context, BKL held): hand it to the interrupt object that owns it.
pub fn irq_fired(irq: u32) -> bool {
    let s = state();
    let Some(&Some(obj)) = s.irq_objs.get(irq as usize) else {
        return false;
    };
    ipc::interrupt_fire(&mut s.objs, obj, irq)
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
    // (the wakes are made in the same critical section as the commit, and as one batch: every woken thread
    // is runnable before any of them runs, T62)
    irq::without(|| {
        let s = state();
        let UserState { objs, waits, .. } = s;
        let dirty = objs.take_dirty();
        if dirty.items.is_empty() && !dirty.overflow {
            return;
        }
        let woken = waits.commit(
            |id| ipc::signals_of_object(objs, id),
            |id| matches!(objs.get(id), Some(ipc::KObj::Port(_))),
            &dirty,
        );
        objs.recycle_dirty(dirty);
        thread::wake_many(&woken);
    });
}

/// Clean up what the syscall destroyed and wake exactly the threads whose wait it ended (slice W: the
/// global wait queue that used to be woken here, OD43, is gone).
pub fn notify() {
    cleanup_dead();
    commit_wake();
    // the safe point where the kernel heap may grow (heap.rs)
    irq::without(crate::heap::service);
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
    let me = thread::current();
    if !irq::without(|| state().cleaning.insert(me)) {
        return; // a cleanup is already running further up this thread's stack; it will see our work
    }
    loop {
        let mut dead = irq::without(|| state().objs.drain_dead());
        if dead.is_empty() {
            irq::without(|| state().cleaning.remove(&me));
            return;
        }
        for obj in dead.drain(..) {
            match obj {
                // the object's reference on its VMO goes; a region that still maps it keeps it alive (L15)
                ipc::KObj::Vmo(id) => irq::without(|| {
                    let k = kmem::get();
                    let _ = k.vmos.release(&mut k.mm, &mut k.jobs, id);
                }),
                ipc::KObj::Process(p) if p.kill_on_close && !p.terminated => {
                    request_kill(p.pid, -2000)
                }
                ipc::KObj::Job(id) => irq::without(|| kmem::get().jobs.release(id)),
                ipc::KObj::Interrupt(i) => irq::without(|| {
                    // mask the source unless a newer object already owns it
                    let s = state();
                    let owned = matches!(s.irq_objs.get(i.irq as usize), Some(Some(o))
                        if matches!(s.objs.get(*o), Some(ipc::KObj::Interrupt(n)) if n.irq == i.irq));
                    if !owned {
                        if let Some(slot) = s.irq_objs.get_mut(i.irq as usize) {
                            *slot = None;
                        }
                        crate::aia::disable(i.irq);
                    }
                }),
                _ => {}
            }
        }
        irq::without(|| state().objs.recycle(dead)); // (the emptied vector goes back, so the next destruction does not allocate: KT9)
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
    // (the process's own set of live threads: not a walk of every thread in the system, T191)
    let tids: Vec<Tid> = irq::without(|| {
        state()
            .procs
            .get(&pid)
            .map(|p| p.live_tids.iter().copied().collect())
            .unwrap_or_default()
    });
    thread::wake_many(&tids);
}

pub(super) fn request_kill(pid: u64, code: i32) {
    let direct = irq::without(|| {
        let s = state();
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
