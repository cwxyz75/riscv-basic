//! The blocking syscalls: `object_wait`, `port_wait`, `futex_wait`, `channel_call`.

use super::*;

/// The signal word of `h` in the calling process (0 if it names nothing). Interrupts off.
pub(super) fn signals_now(h: Handle) -> Result<u32, Status> {
    let s = state();
    let pid = s
        .thread_pid
        .get(&thread::current())
        .ok_or(Status::BadState)?;
    let p = s.procs.get(pid).ok_or(Status::BadState)?;
    ipc::signals_of(&s.objs, &p.handles, h)
}

/// Register `thread` as waiting for `mask` on the object the calling process's handle `h` names (just
/// before it sleeps; interrupts off, lock held). A handle that names nothing has no registration (its
/// wait ended already: the predicate was true).
pub(super) fn register_wait(thread: Tid, h: Handle, mask: u32) {
    let s = state();
    let Some(pid) = s.thread_pid.get(&thread).copied() else {
        return;
    };
    let Some(p) = s.procs.get(&pid) else {
        return;
    };
    if let Ok(entry) = p.handles.get(h) {
        s.waits.register(thread, entry.obj, mask);
    }
}

/// The calling process and the machine's memory, for a `ksys` function that takes them (interrupts off, lock held).
fn in_process<R>(
    f: impl FnOnce(&mut UserState, &mut kmem::Kmem, u64) -> Result<R, Status>,
) -> Result<R, ReplyErr> {
    irq::without(|| {
        let s = state();
        let me = caller_pid(s)?;
        f(s, &mut kmem::get(), me)
    })
    .map_err(|e| (e, None, None))
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn object_wait(a: [u64; 6]) -> Reply {
    let w = in_process(|s, _, me| ksys::object_wait_check(s, me, a))?;
    let (h, mask, deadline) = (w.handle, w.mask, w.deadline);
    if deadline != 0 {
        // A handle that stops naming anything ends the wait (the report below then says so).
        let me = thread::current();
        // (anything a kernel thread touched without committing goes first: a thread must not sleep
        // while a change it should have seen is still in the log)
        commit_wake();
        thread::wait_deadline_registered(
            || {
                killed_now().is_some()
                    || interrupted_now()
                    || signals_now(h).map_or(true, |s| s & mask != 0)
            },
            || register_wait(me, h, mask),
            deadline,
        );
        irq::without(|| state().waits.forget(me));
    }
    // The handle may have gone while we slept: BAD_HANDLE, not a wait that quietly times out.
    let seen = irq::without(|| signals_now(h));
    ksys::object_wait_report(seen, &w, take_interrupt)
}

/// `futex_wait(addr, expected, deadline)` (S7b, SYSCALLS.md): sleep on the 32-bit word at `addr`
/// if it still equals `expected`. The check and the enqueue are one step under the big kernel lock,
/// so a wake that follows a user-mode store is never lost.
pub(super) fn futex_wait(a: [u64; 6]) -> Reply {
    let who = thread::current() as u64;
    let deadline = a[2];
    match in_process(|s, k, me| ksys::futex_wait_check(s, k.mm, k.vmos, k.jobs, me, who, a))? {
        ksys::FutexCheck::Done(status) => return fail(status),
        ksys::FutexCheck::Sleep => {}
    }
    // sleep until woken (removed from the table), killed, or the deadline: not on the global queue,
    // only `futex_wake` and a kill wake this thread (by id)
    thread::wait_deadline_private(
        || killed_now().is_some() || interrupted_now() || !state().futexes.contains(who),
        deadline,
    );
    // still in the table: the deadline (or a kill) ended the wait: leave it
    irq::without(|| ksys::futex_wait_report(state(), who, take_interrupt))
}

/// `futex_wake(addr, count)`: wake up to `count` sleepers on `addr` in this process, oldest first.
pub(super) fn futex_wake(a: [u64; 6]) -> (Reply, Vec<u64>) {
    match in_process(|s, _, me| ksys::futex_wake(s, me, a)) {
        Ok(ids) => (Ok((Some(ids.len() as u64), None)), ids),
        Err(e) => (Err(e), Vec::new()),
    }
}

/// `port_wait(port, deadline)`: wait until a binding is ready (the port's READABLE signal),
/// then report one in turn.
pub(super) fn port_wait(a: [u64; 6]) -> Reply {
    let (h, deadline) = ksys::port_wait_check(a).map_err(|e| (e, None, None))?;
    let poll = || in_process(|s, k, me| ksys::port_poll(s, k.jobs, me, h));
    // Poll, and while nothing is ready and the deadline has not passed, sleep until the port
    // is READABLE and poll again (the readiness seen by the sleep check can be gone by the time
    // we poll, so one wait is not enough).
    let mut got = poll()?;
    while got.is_none()
        && deadline != 0
        && thread::now_ns() < deadline
        && irq::without(killed_now).is_none()
        && !irq::without(interrupted_now)
    {
        // (the poll above may have touched the port; tell the wait table before sleeping, so that no
        // thread goes to sleep, and the lock is released, with signal changes still in the log)
        commit_wake();
        // Ready, killed, or the handle stopped naming a port (the poll then says so). The thread is
        // registered on the port: the wait table re-evaluates a port's signal (which follows its
        // bindings) whenever anything was touched.
        let me = thread::current();
        thread::wait_deadline_registered(
            || {
                killed_now().is_some()
                    || interrupted_now()
                    || signals_now(h).map_or(true, |s| s & abi::signals::READABLE != 0)
            },
            || register_wait(me, h, abi::signals::READABLE),
            deadline,
        );
        irq::without(|| state().waits.forget(me));
        got = poll()?;
    }
    ksys::port_wait_report(got, deadline, take_interrupt)
}

/// `port_wait_many(port, buffer, capacity, deadline)` (AB4): `port_wait`'s wait, returning up to `capacity` ready bindings as records in
/// `buffer` and their count in `a1`.
pub(super) fn port_wait_many(a: [u64; 6]) -> Reply {
    let (h, buf, cap, deadline) = ksys::port_wait_many_check(a).map_err(|e| (e, None, None))?;
    let poll =
        || in_process(|s, k, me| ksys::port_poll_many(s, k.mm, k.vmos, k.jobs, me, h, buf, cap));
    let mut got = poll()?;
    while got == 0
        && deadline != 0
        && thread::now_ns() < deadline
        && irq::without(killed_now).is_none()
        && !irq::without(interrupted_now)
    {
        commit_wake();
        let me = thread::current();
        thread::wait_deadline_registered(
            || {
                killed_now().is_some()
                    || interrupted_now()
                    || signals_now(h).map_or(true, |s| s & abi::signals::READABLE != 0)
            },
            || register_wait(me, h, abi::signals::READABLE),
            deadline,
        );
        irq::without(|| state().waits.forget(me));
        got = poll()?;
    }
    ksys::port_wait_many_report(got, deadline, take_interrupt)
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn channel_call(a: [u64; 6]) -> Reply {
    // the request: read the record, check it, send (an interrupt already pending ends the call before anything is sent)
    let call = in_process(|s, k, me| {
        ksys::channel_call_send(s, k.mm, k.vmos, k.jobs, me, a, take_interrupt_held)
    })?;
    notify();
    // Wait for the reply with that transaction id (or the peer closing, or the deadline).
    let me = thread::current();
    let ready = || {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&thread::current()) else {
            return true;
        };
        let interrupted = s.interrupts.contains(&thread::current());
        ksys::channel_call_ready(s, pid, &call, interrupted)
    };
    commit_wake();
    thread::wait_deadline_registered(
        ready,
        || {
            register_wait(
                me,
                call.ch,
                abi::signals::READABLE | abi::signals::PEER_CLOSED,
            )
        },
        call.deadline,
    );
    irq::without(|| state().waits.forget(me));
    // the reply (a reply that came wins; an interrupt that ended the wait reports INTERRUPTED, the request stays queued)
    irq::without(|| {
        let s = state();
        let k = kmem::get();
        match caller_pid(s) {
            Ok(pid) => {
                ksys::channel_call_reply(s, k.mm, k.vmos, k.jobs, pid, &call, take_interrupt_held)
            }
            Err(e) => Err((e, None, None)),
        }
    })
}
