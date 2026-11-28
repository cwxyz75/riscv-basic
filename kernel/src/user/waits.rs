//! The blocking syscalls: `object_wait`, `port_wait`, `futex_wait`, `channel_call`.

use super::*;

/// The signal word of `h` in the calling process (0 if it names nothing). Interrupts off.
pub(super) fn signals_now(h: Handle) -> Result<u32, Status> {
    with_state(|s| {
        let pid = s.pid_of(thread::current()).ok_or(Status::BadState)?;
        let p = s.procs.get(&pid).ok_or(Status::BadState)?;
        ipc::signals_of(&s.objs, &p.handles, h)
    })
}

/// Register `thread` as waiting for `mask` on the object the calling process's handle `h` names (just
/// before it sleeps; interrupts off, lock held). A handle that names nothing has no registration (its
/// wait ended already: the predicate was true).
pub(super) fn register_wait(thread: Tid, h: Handle, mask: u32) {
    register_wait_kind(thread, h, mask, false);
}

/// `register_wait` for a thread in `port_wait` or `port_wait_many`: it polls the port when it wakes, so the port wakes one such waiter at a time
/// (L21b). Any other wait on a port (`object_wait` on its READABLE) is released with the rest and takes no baton.
pub(super) fn register_poll_wait(thread: Tid, h: Handle) {
    register_wait_kind(thread, h, abi::signals::READABLE, true);
}

/// The port the calling process's handle `h` names, touched (logged) so that the commit after a wait looks at it again: a thread woken by the baton
/// that cannot poll (a bad buffer, a handle that went) must still pass it on (L21b).
pub(super) fn touch_port_of(h: Handle) {
    locked_state(|s| {
        let Ok(pid) = caller_pid(s) else { return };
        if let Some(entry) = s.procs.get(&pid).and_then(|p| p.handles.get(h).ok()) {
            if matches!(s.objs.get(entry.obj), Some(ipc::KObj::Port(_))) {
                s.objs.touch(entry.obj);
            }
        }
    });
}

/// An R point of the blocking calls (T177): make the calling thread known to the wait table, with room for everything it can register or be woken by.
/// Idempotent; no memory is `NoMemory` and nothing has changed.
pub(super) fn admit_caller() -> Result<(), ReplyErr> {
    locked_state(|s| {
        s.waits
            .admit(thread::current())
            .map_err(|_| Status::NoMemory)
    })
    .map_err(|e| (e, None, None))
}

fn register_wait_kind(thread: Tid, h: Handle, mask: u32, poller: bool) {
    with_state(|s| {
        let Some(pid) = s.pid_of(thread) else {
            return;
        };
        let Some(p) = s.procs.get(&pid) else {
            return;
        };
        if let Ok(entry) = p.handles.get(h) {
            if matches!(s.objs.get(entry.obj), Some(ipc::KObj::Port(_))) {
                // (a port's READABLE follows what it binds: the table remembers which touches can release its waiters, L21a)
                if poller {
                    s.waits.register_poller(thread, entry.obj, mask);
                } else {
                    s.waits.register_port(thread, entry.obj, mask);
                }
            } else {
                s.waits.register(thread, entry.obj, mask);
            }
        }
    })
}

/// Register `thread` to wait for the reply with `txid` on the channel end the calling process's handle `h` names (a `channel_call`, L21d).
pub(super) fn register_call_wait(thread: Tid, h: Handle, txid: u32) {
    with_state(|s| {
        let Some(pid) = s.pid_of(thread) else {
            return;
        };
        let Some(p) = s.procs.get(&pid) else {
            return;
        };
        if let Ok(entry) = p.handles.get(h) {
            s.waits.register_call(thread, entry.obj, txid);
        }
    })
}

/// The calling process and the machine's memory, for a `ksys` function that takes them (interrupts off, lock held).
fn in_process<R>(
    f: impl FnOnce(&mut UserState, &mut kmem::Kmem, u64) -> Result<R, Status>,
) -> Result<R, ReplyErr> {
    locked_state(|s| {
        let me = caller_pid(s)?;
        let _as = AsLocks::one(s, me); // (the caller's address space, before the memory locks)
        f(s, &mut kmem::get(), me)
    })
    .map_err(|e| (e, None, None))
}

pub(super) fn object_wait(a: [u64; 6]) -> Reply {
    let w = in_process(|s, _, me| ksys::object_wait_check(s, me, a))?;
    let (h, mask, deadline) = (w.handle, w.mask, w.deadline);
    if deadline != 0 {
        admit_caller()?;
        // A handle that stops naming anything ends the wait (the report below then says so).
        let me = thread::current();
        // (anything a kernel thread touched without committing goes first: a thread must not sleep
        // while a change it should have seen is still in the log)
        commit_wake();
        // The signal word that ended the wait, read under the lock in the same step that decided it: another thread may clear the signal
        // (an `interrupt_ack`, a read) once the lock is let go, and a wait whose condition held must not then report TIMED_OUT before
        // its deadline (SYSCALLS.md: TIMED_OUT only when the deadline arrived first).
        let mut met: Option<u32> = None;
        thread::wait_deadline_registered(
            || {
                met = None;
                if killed_now().is_some() || interrupted_now() {
                    return true;
                }
                match signals_now(h) {
                    Ok(s) if s & mask != 0 => {
                        met = Some(s);
                        true
                    }
                    Ok(_) => false,
                    Err(_) => true,
                }
            },
            || register_wait(me, h, mask),
            deadline,
        );
        locked_state(|s| s.waits.forget(me));
        if let Some(s) = met {
            return ksys::object_wait_report(Ok(s), &w, take_interrupt);
        }
    }
    // The handle may have gone while we slept: BAD_HANDLE, not a wait that quietly times out.
    let seen = irq::without(|| signals_now(h));
    ksys::object_wait_report(seen, &w, take_interrupt)
}

/// The futex buckets (L14): sixty-four locks of class `Futex`, a key to its bucket by hash, never two at once (SCHED-SMP-SPEC 3.3).
pub(super) struct KBuckets(
    [klock::Spin<kcore::futex::Futexes, crate::bkl::Machine>; kcore::futex::BUCKETS],
);

static FUTEX: KBuckets = KBuckets(
    [const { klock::Spin::new(klock::Class::Futex, 0, kcore::futex::Futexes::new()) };
        kcore::futex::BUCKETS],
);

impl kcore::futex::Buckets for &'static KBuckets {
    #[allow(clippy::indexing_slicing)] // INVARIANT: the table asks for b < BUCKETS, the array's length
    fn with<R>(&self, b: usize, f: impl FnOnce(&mut kcore::futex::Futexes) -> R) -> R {
        f(&mut self.0[b].lock())
    }
}

/// The futex table (`kcore::futex::FutexTable` over the bucket locks). Callable with interrupts on or off, with or without the big kernel
/// lock: each operation takes one bucket's lock.
pub(super) fn futex_table() -> kcore::futex::FutexTable<&'static KBuckets> {
    kcore::futex::FutexTable::new(&FUTEX)
}

/// `futex_wait(addr, expected, deadline)` (S7b, SYSCALLS.md): sleep on the 32-bit word at `addr`
/// if it still equals `expected`. The big kernel lock is held only to translate the word and to queue; the sleep holds nothing (L14). The
/// word is read again under the key's bucket lock, with no memory lock held, and the thread is queued in the same step, so a wake that
/// follows a user-mode store (it takes the same bucket lock) is never lost.
pub(super) fn futex_wait(a: [u64; 6]) -> Reply {
    let who = thread::current() as u64;
    let deadline = a[2];
    let (me, addr) = match locked_state(|s| -> Result<(u64, u64, ksys::FutexCheck), Status> {
        let me = caller_pid(s)?;
        let _as = AsLocks::one(s, me);
        let w = {
            let k = kmem::get();
            ksys::futex_wait_resolve(s, k.mm, k.vmos, k.jobs, me, a)?
        }; // (the memory locks are let go: `FUTEX` is before them in the order)
        let c = ksys::futex_wait_enqueue(&futex_table(), &kmem::DirectMap, me, who, &w);
        Ok((me, w.addr, c))
    }) {
        Ok((me, addr, ksys::FutexCheck::Sleep)) => (me, addr),
        Ok((_, _, ksys::FutexCheck::Done(status))) => return fail(status),
        Err(e) => return fail(e),
    };
    let key = (me, addr);
    // sleep until woken (removed from the table), killed, or the deadline: not on the global queue,
    // only `futex_wake` and a kill wake this thread (by id)
    thread::wait_deadline_futex(
        || killed_now().is_some() || interrupted_now() || !futex_table().contains(key, who),
        deadline,
    );
    // still in the table: the deadline (or a kill) ended the wait: leave it
    irq::without(|| ksys::futex_wait_report(&futex_table(), key, who, take_interrupt))
}

/// Has any PI futex ever been locked with a waiter? A thread's exit looks for PI futexes it owns only after that (one load otherwise).
static PI_USED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn pi_status(e: kcore::pifutex::PiError) -> Status {
    match e {
        kcore::pifutex::PiError::BadState => Status::BadState,
        kcore::pifutex::PiError::NotOwner => Status::AccessDenied,
        kcore::pifutex::PiError::NoId => Status::NotSupported,
        kcore::pifutex::PiError::NoMemory => Status::NoMemory,
    }
}

/// `futex_lock_pi(addr, deadline)` (L23, SYSCALLS.md): own the lock word at `addr`, or wait for it while lending the caller's priority to its owner.
pub(super) fn futex_lock_pi(a: [u64; 6]) -> Reply {
    let who = thread::current();
    let (addr, deadline) = (a[0], a[1]);
    // under the big lock: translate the word (for writing), then, under its bucket lock and the donation graph's, take it, queue, or (a poll)
    // find it held. A poll is decided here and nothing is queued, so a hand-over cannot land between a queue and a cancel.
    let step = locked_state(|s| -> Result<(u64, Option<bool>), Status> {
        let me = caller_pid(s)?;
        let _as = AsLocks::one(s, me);
        let w = {
            let k = kmem::get();
            ksys::futex_pi_resolve(s, k.mm, k.vmos, k.jobs, me, addr)?
        };
        let word = kmem::PhysWord(w.pa);
        let key = (me, w.addr);
        let live = |t: Tid| s.pid_of(t) == Some(me);
        let r = futex_table().with_key(key, |f| {
            let r = thread::pi_op(|d, sink| {
                match f.pi.lock(d, &word, key, who, live, deadline != 0, sink) {
                    Ok(kcore::pifutex::Lock::Acquired) => Ok(Some(true)),
                    Ok(kcore::pifutex::Lock::WouldBlock) => Ok(None),
                    Ok(kcore::pifutex::Lock::Blocked) => Ok(Some(false)),
                    Err(e) => Err(e),
                }
            });
            // (waiters the call ended because the word and the record had come apart: woken with the bucket lock held)
            f.pi.drain_wake_into(thread::wake_many_quiet);
            r
        });
        match r {
            Ok(outcome) => {
                if outcome == Some(false) {
                    PI_USED.store(true, Ordering::Relaxed);
                }
                Ok((me, outcome))
            }
            Err(e) => Err(pi_status(e)),
        }
    })
    .map_err(|e| (e, None, None))?;
    let (me, acquired) = step;
    match acquired {
        Some(true) => return Ok((None, None)),
        None => return fail(Status::TimedOut),
        Some(false) => {}
    }
    let key = (me, addr);
    thread::wait_deadline_futex(
        || {
            killed_now().is_some()
                || interrupted_now()
                || !futex_table().with_key(key, |f| f.pi.still_waiting(who))
        },
        deadline,
    );
    // how it ended: the owner handed the lock over or died (an outcome waits for us), or we give up (a deadline, a kill, an interrupt)
    let ended = irq::without(|| {
        futex_table().with_key(key, |f| {
            if f.pi.still_waiting(who) {
                thread::pi_op(|d, sink| f.pi.cancel(d, who, sink));
                None
            } else {
                f.pi.take_outcome(who)
            }
        })
    });
    match ended {
        Some(kcore::pifutex::Outcome::Granted) => Ok((None, None)),
        Some(kcore::pifutex::Outcome::Died) => fail(Status::BadState),
        None if take_interrupt() => fail(Status::Interrupted),
        None => fail(Status::TimedOut),
    }
}

/// `futex_unlock_pi(addr)`: hand the lock to the waiter with the best effective priority (waking it), or free it.
pub(super) fn futex_unlock_pi(a: [u64; 6]) -> Reply {
    let who = thread::current();
    let addr = a[0];
    locked_state(|s| -> Reply {
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let _as = AsLocks::one(s, me);
        let w = {
            let k = kmem::get();
            ksys::futex_pi_resolve(s, k.mm, k.vmos, k.jobs, me, addr)
                .map_err(|e| (e, None, None))?
        };
        let word = kmem::PhysWord(w.pa);
        let key = (me, w.addr);
        let r = futex_table().with_key(key, |f| {
            let r = thread::pi_op(|d, sink| match f.pi.unlock(d, &word, key, who, sink) {
                Ok(kcore::pifutex::Unlock::Handed { to }) => {
                    // (the wake is issued with the bucket lock held, as `futex_wake` does)
                    thread::wake_many_quiet(&[to]);
                    Ok(())
                }
                Ok(kcore::pifutex::Unlock::Free) => Ok(()),
                Err(e) => Err(e),
            });
            f.pi.drain_wake_into(thread::wake_many_quiet);
            r
        });
        r.map(|()| (None, None))
            .map_err(|e| (pi_status(e), None, None))
    })
}

/// A user thread has gone (big lock held): the PI futexes it owned that had waiters are poisoned (`OWNER_DIED`) and their waiters woken to report
/// BAD_STATE; and it leaves the donation graph.
pub(super) fn pi_thread_gone(s: &mut UserState, tid: Tid, pid: u64) {
    if PI_USED.load(Ordering::Relaxed) {
        // the PI futexes with waiters that it owns, one at a time (nothing is collected): what the word says now is read outside the bucket
        // locks (the memory locks are after them), the record ends under its bucket's lock, and the word is poisoned outside it again
        loop {
            let mut next = None;
            futex_table().for_each_bucket(|f| {
                if next.is_none() {
                    next = f.pi.first_owned_key(tid);
                }
            });
            let Some(key) = next else {
                break;
            };
            let (pa, value) = {
                let _as = AsLocks::one(s, pid);
                let k = kmem::get();
                match ksys::futex_pi_resolve(s, k.mm, k.vmos, k.jobs, pid, key.1) {
                    Ok(w) => (
                        Some(w.pa),
                        Some(kcore::pifutex::Word::load(&kmem::PhysWord(w.pa))),
                    ),
                    Err(_) => {
                        // (a page of a pager's VMO that is not there sets the slot a syscall's copy reads: this thread is leaving and reads nothing)
                        let _ = k.vmos.take_need();
                        (None, None)
                    }
                }
            };
            let mut poison = false;
            futex_table().for_each_bucket(|f| {
                if f.pi.first_owned_key(tid) == Some(key) {
                    poison = thread::pi_op(|d, sink| f.pi.owner_died_one(d, tid, key, value, sink));
                    f.pi.drain_wake_into(thread::wake_many_quiet);
                }
            });
            if poison {
                if let Some(pa) = pa {
                    kcore::pifutex::PiFutexes::poison(&kmem::PhysWord(pa));
                }
            }
        }
    }
    thread::pi_gone(tid);
}

/// `futex_wake(addr, count)` for the thread of process `pid`: wake up to `count` sleepers on `addr`, oldest first, the wakes issued with the
/// bucket lock held. Needs no big kernel lock (L14); interrupts off.
pub fn futex_wake(pid: u64, a: [u64; 6]) -> Reply {
    if pid == 0 {
        return fail(Status::BadState);
    }
    match ksys::futex_wake(&futex_table(), pid, a, thread::wake_many_quiet) {
        Ok(n) => Ok((Some(n as u64), None)),
        Err(e) => fail(e),
    }
}

/// Wake the threads a pager step ended the wait of, in batches: each batch is taken under the state lock and woken after it (the wake takes the
/// scheduler's locks, which come after it). A wake cannot be refused: class N.
pub(super) fn wake_pager_waiters() {
    loop {
        let mut batch = [0; 16];
        let n = any_state(|s| s.pagers.take_woken(&mut batch));
        let Some(w) = batch.get(..n).filter(|w| !w.is_empty()) else {
            return;
        };
        crate::heap::nofail(|| thread::wake_many(w));
    }
}

/// Wait for a page of a pager's VMO (MM9, KERNEL-V1-SPEC 4.5, PG1): register as a waiter (the request goes to the pager once per page), let go of every
/// lock, and sleep until the pager supplies the page, fails it, dies, or this thread is killed or interrupted. `Ok` when the page was supplied
/// (the caller tries again) or the wait was cut short by a kill or an interrupt (the return to user mode ends the process, or the access is
/// tried again); `Err` with the pager's status when the page cannot be had.
pub(super) fn pager_wait(need: kcore::aspace::PagerNeed, in_call: bool) -> Result<(), Status> {
    let me = thread::current();
    // (the room for this thread's wait was made when it was: T248; a pager found dead while registering ends the others' waits too)
    let begun = any_state(|s| {
        let k = kmem::get();
        ksys::pager::pager_wait_begin(s, k.vmos, k.jobs, me, need)
    });
    wake_pager_waiters();
    begun?;
    // (the page came in since the call found it missing: nothing was registered)
    if !any_state(|s| s.pagers.waiting(me)) {
        return Ok(());
    }
    // (the request is in the pager's channel: its reader must see it)
    commit_wake();
    loop {
        // a request that did not fit in a full channel is tried again every 10 ms, by the waiters of that pager
        let unsent = any_state(|s| s.pagers.has_unsent(need.pager));
        let deadline = if unsent {
            thread::now_ns().saturating_add(10_000_000)
        } else {
            u64::MAX
        };
        thread::wait_deadline_private(
            || {
                killed_now().is_some()
                    || (in_call && interrupted_now())
                    || any_state(|s| ksys::pager::pager_wait_done(s, me))
            },
            deadline,
        );
        let ended = any_state(|s| {
            ksys::pager::pager_wait_done(s, me)
                || killed_in(s).is_some()
                || (in_call && interrupted_in(s))
        });
        if ended {
            break;
        }
        with_state(|s| {
            let k = kmem::get();
            ksys::pager::pager_flush(s, k.vmos, k.jobs, need.pager);
        });
        wake_pager_waiters();
        commit_wake();
    }
    // a fault ignores `thread_interrupt` (the flag stays for the next blocking call); a call that was interrupted or killed while it waited
    // reports INTERRUPTED and takes the flag
    let stopped = in_call && (crate::bkl::ensure(|| killed_now().is_some()) || take_interrupt());
    match any_state(|s| ksys::pager::pager_wait_end(s, me)) {
        Some(Err(e)) => Err(e),
        Some(Ok(())) => Ok(()),
        None if stopped => Err(Status::Interrupted),
        None => Ok(()),
    }
}

/// After a call that failed with SHOULD_WAIT because a page of a pager's VMO was not there (MM9): wait for the page, and make the call's answer
/// what the wait came to: SHOULD_WAIT again (the page is there now, try again) or the pager's failure status.
pub(super) fn pager_after_call(frame: &mut crate::trap::TrapFrame) {
    // (one load when no pager page was missing: the common case takes no lock)
    if !kcore::aspace::PAGER_NEED_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let need = irq::without(|| kmem::get().vmos.take_need());
    let Some(need) = need else {
        return;
    };
    if frame.arg(10) as i64 != i64::from(Status::ShouldWait.code()) {
        return; // (the call went on without the page: nothing to wait for)
    }
    if let Err(status) = pager_wait(need, true) {
        status_ret(frame, status);
    }
}

/// `port_wait(port, deadline)`: wait until a binding is ready (the port's READABLE signal),
/// then report one in turn.
pub(super) fn port_wait(a: [u64; 6]) -> Reply {
    let (h, deadline) = ksys::port_wait_check(a).map_err(|e| (e, None, None))?;
    let got = port_wait_until(h, deadline)?;
    ksys::port_wait_report(got, deadline, take_interrupt)
}

/// `port_wait`'s polls and sleeps: the binding that became ready, or none by the deadline.
#[inline(never)]
fn port_wait_until(h: Handle, deadline: u64) -> Result<Option<(u64, u32)>, ReplyErr> {
    let poll = || in_process(|s, k, me| ksys::port_poll(s, k.jobs, me, h));
    // Poll, and while nothing is ready and the deadline has not passed, sleep until the port
    // is READABLE and poll again (the readiness seen by the sleep check can be gone by the time
    // we poll, so one wait is not enough).
    let mut got = poll()?;
    while got.is_none() && port_may_wait(deadline) {
        port_block(h, deadline)?;
        got = poll()?;
    }
    Ok(got)
}

/// Whether a port wait that found nothing ready sleeps: a deadline that has not passed, and no kill or interrupt pending.
#[inline(never)]
fn port_may_wait(deadline: u64) -> bool {
    deadline != 0
        && thread::now_ns() < deadline
        && irq::without(killed_now).is_none()
        && !irq::without(interrupted_now)
}

/// One sleep of `port_wait` and `port_wait_many` on port `h`, until it is ready, the thread is killed or interrupted, or the deadline.
#[inline(never)]
fn port_block(h: Handle, deadline: u64) -> Result<(), ReplyErr> {
    admit_caller()?;
    // (the poll before may have touched the port; tell the wait table before sleeping, so that no
    // thread goes to sleep, and the lock is released, with signal changes still in the log)
    commit_wake();
    // Ready, killed, or the handle stopped naming a port (the poll then says so). The thread is
    // registered on the port: the wait table re-evaluates a port's signal (which follows its
    // bindings) when an object it binds, or the port, was touched (L21a).
    let me = thread::current();
    thread::wait_deadline_registered(
        || {
            killed_now().is_some()
                || interrupted_now()
                || signals_now(h).map_or(true, |s| s & abi::signals::READABLE != 0)
        },
        || register_poll_wait(me, h),
        deadline,
    );
    locked_state(|s| s.waits.forget(me));
    touch_port_of(h); // (the wake may have been the baton: it is passed on whatever the poll does)
    Ok(())
}

/// `port_wait_many(port, buffer, capacity, deadline)` (AB4): `port_wait`'s wait, returning up to `capacity` ready bindings as records in
/// `buffer` and their count in `a1`.
pub(super) fn port_wait_many(a: [u64; 6]) -> Reply {
    let (h, buf, cap, deadline) = ksys::port_wait_many_check(a).map_err(|e| (e, None, None))?;
    let got = port_wait_many_until(h, buf, cap, deadline)?;
    ksys::port_wait_many_report(got, deadline, take_interrupt)
}

/// `port_wait_many`'s polls and sleeps: how many records it wrote.
#[inline(never)]
fn port_wait_many_until(h: Handle, buf: u64, cap: usize, deadline: u64) -> Result<usize, ReplyErr> {
    let poll =
        || in_process(|s, k, me| ksys::port_poll_many(s, k.mm, k.vmos, k.jobs, me, h, buf, cap));
    let mut got = poll()?;
    while got == 0 && port_may_wait(deadline) {
        port_block(h, deadline)?;
        got = poll()?;
    }
    Ok(got)
}

pub(super) fn channel_call(a: [u64; 6]) -> Reply {
    // (T215: a big request is copied in before the object lock, a big reply is copied out after it)
    let staged = super::trap::stage_call_in(a);
    let r = channel_call_inner(a, staged);
    // the call is over, whichever way: the priority it lent to the server that read its request goes back, and the record of its wait goes (L22)
    let me = thread::current();
    thread::push_owed(); // (the server the request woke for a hand-off is on this hart: if the caller did not block, other harts are offered it, L24)
    locked_state(|s| {
        // (one section with the server's donation, `lend_to_reader`: after it, no donation for this call can be made)
        if let Some(t) = s.thread_mut(me) {
            t.call = None;
        }
        thread::pi_revoke(me);
    });
    r
}

fn channel_call_inner(a: [u64; 6], staged: Option<ksys::Staged>) -> Reply {
    admit_caller()?; // (before anything is sent: the wait below registers without allocating)
                     // the request: read the record, check it, send (an interrupt already pending ends the call before anything is sent)
    let call = in_process(|s, k, me| {
        ksys::channel_call_send_staged(
            s,
            k.mm,
            k.vmos,
            k.jobs,
            me,
            thread::current() as u64,
            a,
            staged,
            take_interrupt_in,
        )
    })?;
    notify_sync(); // (the request is queued and this thread blocks next: the server it wakes runs on this hart, L24)
                   // Wait for the reply with that transaction id (or the peer closing, or the deadline).
    call_reply(&call)
}

/// The wait for `channel_call`'s reply and its taking: a reply that came wins; an interrupt that ended the wait reports INTERRUPTED, and the request
/// stays queued.
#[inline(never)]
fn call_reply(call: &ksys::Call) -> Reply {
    let me = thread::current();
    let ready = || {
        with_state(|s| {
            let Some(pid) = s.pid_of(thread::current()) else {
                return true;
            };
            let interrupted = s.interrupt_pending(thread::current());
            ksys::channel_call_ready(s, pid, call, interrupted)
        })
    };
    let mut wait = true;
    loop {
        if core::mem::take(&mut wait) {
            commit_wake();
            thread::wait_deadline_registered(
                &ready,
                || register_call_wait(me, call.ch, call.txid),
                call.deadline,
            );
            locked_state(|s| s.waits.forget(me));
        }
        let mut deferred = None;
        let mut r = locked_state(|s| {
            let _as = AsLocks::caller(s);
            let k = kmem::get();
            match caller_pid(s) {
                Ok(pid) => ksys::channel_call_reply_deferred(
                    s,
                    k.mm,
                    k.vmos,
                    k.jobs,
                    pid,
                    call,
                    &mut deferred,
                    true,
                    take_interrupt_in,
                ),
                Err(e) => Err((e, None, None)),
            }
        });
        if let Some(d) = deferred {
            // (OD161, as in `trap.rs`: the reply is claimed, not taken; the copy and the commit follow, and another read that took the reply in
            // between makes this one look for it again)
            crate::chaos::point(crate::chaos::Point::CopyOut);
            match super::trap::finish_read(&d) {
                Some(done) => r = done,
                None => {
                    // (another thread of the caller read the reply off the channel in between: the call waits again, to its deadline, rather than
                    // report TIMED_OUT before the deadline has passed)
                    wait = true;
                    continue;
                }
            }
        }
        // (MM9) a reply buffer in a page of a pager's VMO that is not there: the reply stays queued; wait for the page here and take the reply
        // again, since a retry by the caller would send the request a second time
        if matches!(&r, Err((Status::ShouldWait, _, _))) {
            if let Some(need) = irq::without(|| kmem::get().vmos.take_need()) {
                match pager_wait(need, true) {
                    Ok(()) => continue,
                    Err(e) => return Err((e, None, None)),
                }
            }
        }
        return r;
    }
}
