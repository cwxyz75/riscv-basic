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

pub(super) const WAITABLE_SIGNALS: u32 = abi::signals::WAITABLE;

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

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
pub(super) fn object_wait(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let (h, deadline) = (Handle(a[0] as u32), a[2]);
    let rights = with_proc(|_, p, _| p.handles.get(h))
        .map_err(|e| (e, None, None))?
        .map_err(|e| (e, None, None))?
        .rights;
    if !rights.contains(Rights::WAIT) {
        return fail(Status::AccessDenied);
    }
    if a[1] > u64::from(u32::MAX) || a[1] == 0 || (a[1] as u32) & !WAITABLE_SIGNALS != 0 {
        return fail(Status::InvalidArgs);
    }
    let mask = a[1] as u32;
    let observed = || irq::without(|| signals_now(h));
    if deadline != 0 {
        // A handle that stops naming anything ends the wait (the check below then reports it).
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
    let seen = observed().map_err(|e| (e, None, None))?;
    if seen & mask != 0 {
        Ok((Some(u64::from(seen)), None))
    } else if deadline != 0 && take_interrupt() {
        Err((Status::Interrupted, Some(u64::from(seen)), None))
    } else {
        Err((Status::TimedOut, Some(u64::from(seen)), None))
    }
}

/// `futex_wait(addr, expected, deadline)` (S7b, SYSCALLS.md): sleep on the 32-bit word at `addr`
/// if it still equals `expected`. The check and the enqueue are one step under the big kernel lock,
/// so a wake that follows a user-mode store is never lost.
pub(super) fn futex_wait(a: [u64; 6]) -> Reply {
    let (addr, expected, deadline) = (a[0], a[1], a[2]);
    if expected > u64::from(u32::MAX) {
        return fail(Status::InvalidArgs);
    }
    let tid = thread::current();
    let who = tid as u64;
    // check and enqueue, in one step
    let first = irq::without(|| -> Result<Result<(), Status>, Status> {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&tid) else {
            return Err(Status::BadState);
        };
        let k = kmem::get();
        let UserState { procs, futexes, .. } = s;
        let p = procs.get_mut(&pid).ok_or(Status::BadState)?;
        let word = kcore::usercopy::read_user_u32(&mut p.aspace, &mut k.mm, &mut k.vmos, addr)?;
        if u64::from(word) != expected {
            return Ok(Err(Status::ShouldWait)); // the word changed: do not sleep
        }
        if deadline == 0 {
            return Ok(Err(Status::TimedOut)); // a poll: equal, and no time to wait
        }
        let fresh = futexes.wait((pid, addr), who);
        debug_assert!(fresh, "a thread sleeps on one futex at a time");
        Ok(Ok(()))
    })
    .map_err(|e| (e, None, None))?;
    if let Err(status) = first {
        return fail(status);
    }
    // sleep until woken (removed from the table), killed, or the deadline: not on the global queue,
    // only `futex_wake` and a kill wake this thread (by id)
    thread::wait_deadline_private(
        || killed_now().is_some() || interrupted_now() || !state().futexes.contains(who),
        deadline,
    );
    // still in the table: the deadline (or a kill) ended the wait: leave it
    let timed_out = irq::without(|| state().futexes.forget(who));
    if timed_out && take_interrupt() {
        fail(Status::Interrupted)
    } else if timed_out {
        fail(Status::TimedOut)
    } else {
        Ok((None, None))
    }
}

/// `futex_wake(addr, count)`: wake up to `count` sleepers on `addr` in this process, oldest first.
pub(super) fn futex_wake(a: [u64; 6]) -> (Reply, Vec<u64>) {
    let (addr, count) = (a[0], a[1]);
    if addr & 3 != 0 || addr < abi::user::BASE || addr.saturating_add(4) > abi::user::TOP {
        return (fail(Status::InvalidArgs), Vec::new());
    }
    let tid = thread::current();
    let woken = irq::without(|| {
        let s = state();
        let pid = *s.thread_pid.get(&tid)?;
        Some(s.futexes.wake((pid, addr), count))
    });
    match woken {
        Some(ids) => (Ok((Some(ids.len() as u64), None)), ids),
        None => (Err((Status::BadState, None, None)), Vec::new()),
    }
}

/// `port_wait(port, deadline)`: wait until a binding is ready (the port's READABLE signal),
/// then report one in turn.
pub(super) fn port_wait(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let (h, deadline) = (Handle(a[0] as u32), a[1]);
    let poll = || {
        with_proc(|objs, p, k| ipc::port_poll_in(objs, &p.handles, h, &mut k.vmos.jobs))
            .map_err(|e| (e, None, None))?
            .map_err(|e| (e, None, None))
    };
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
    match got {
        Some((key, observed)) => Ok((Some(key), Some(u64::from(observed)))),
        None if deadline != 0 && take_interrupt() => fail(Status::Interrupted),
        None => fail(Status::TimedOut),
    }
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller passes a constant offset whose four or eight bytes lie inside the fixed-size record it just copied in (the longest, offset 48, is in a record of 56 bytes)
pub(super) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller passes a constant offset whose four or eight bytes lie inside the fixed-size record it just copied in (the longest, offset 48, is in a record of 56 bytes)
pub(super) fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: nhandles <= MAX_MSG_HANDLES was checked first, so nhandles * 4 is small; the record read is of fixed size
pub(super) fn channel_call(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let ch = Handle(a[0] as u32);
    let rec = with_proc(|_, p, k| {
        copy_from_user(
            &mut p.aspace,
            &mut k.mm,
            &mut k.vmos,
            a[1],
            abi::CHANNEL_CALL_ARGS_BYTES,
        )
    })
    .map_err(|e| (e, None, None))?
    .map_err(|e| (e, None, None))?;
    let (req_bytes, req_handles) = (le64(&rec, 0), le64(&rec, 8));
    let (nbytes, nhandles) = (le32(&rec, 16) as usize, le32(&rec, 20) as usize);
    let (rd_bytes, rd_handles) = (le64(&rec, 24), le64(&rec, 32));
    let (rd_max_bytes, rd_max_handles) = (
        (le32(&rec, 40) as usize).min(MAX_MSG_BYTES),
        (le32(&rec, 44) as usize).min(MAX_MSG_HANDLES),
    );
    let deadline = le64(&rec, 48);
    if nbytes > MAX_MSG_BYTES || nhandles > MAX_MSG_HANDLES {
        return fail(Status::TooBig);
    }
    if nbytes < 4 {
        return fail(Status::InvalidArgs);
    }
    // An interrupt already pending ends the call before anything is sent (a retry then sends one request, not two).
    if deadline != 0 && take_interrupt() {
        return fail(Status::Interrupted);
    }
    // Send the request.
    let txid = with_proc(|objs, p, k| -> Result<u32, Status> {
        let entry = p.handles.get(ch)?;
        if !(entry.rights.contains(Rights::READ) && entry.rights.contains(Rights::WRITE)) {
            // (a non-channel handle still reports WRONG_TYPE first, below, via channel_write)
            if !matches!(objs.get(entry.obj), Some(ipc::KObj::Channel(_))) {
                return Err(Status::WrongType);
            }
            return Err(Status::AccessDenied);
        }
        let data = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, req_bytes, nbytes)?;
        let raw = copy_from_user(
            &mut p.aspace,
            &mut k.mm,
            &mut k.vmos,
            req_handles,
            nhandles * 4,
        )?;
        ipc::channel_write_in(
            objs,
            &mut p.handles,
            ch,
            &data,
            &handle_list(&raw),
            &mut kmem::get().vmos.jobs,
        )?;
        Ok(le32(&data, 0))
    })
    .map_err(|e| (e, None, None))?
    .map_err(|e| (e, None, None))?;
    notify();
    // Wait for the reply with that transaction id (or the peer closing, or the deadline).
    let me = thread::current();
    let ready = || {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&thread::current()) else {
            return true;
        };
        let Some(p) = s.procs.get(&pid) else {
            return true;
        };
        if p.killed.is_some() || s.interrupts.contains(&thread::current()) {
            return true;
        }
        !matches!(
            ipc::channel_peek(
                &s.objs,
                &p.handles,
                ch,
                Some(txid),
                rd_max_bytes,
                rd_max_handles
            ),
            Err(ReadError::Status(Status::ShouldWait))
        )
    };
    commit_wake();
    thread::wait_deadline_registered(
        ready,
        || register_wait(me, ch, abi::signals::READABLE | abi::signals::PEER_CLOSED),
        deadline,
    );
    irq::without(|| state().waits.forget(me));
    let out = with_proc(|objs, p, k| -> Reply {
        match ipc::channel_peek(
            objs,
            &p.handles,
            ch,
            Some(txid),
            rd_max_bytes,
            rd_max_handles,
        ) {
            Ok((nb, nh)) => {
                verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_bytes, nb)
                    .map_err(|e| (e, None, None))?;
                verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_handles, nh * 4)
                    .map_err(|e| (e, None, None))?;
                let got = ipc::channel_read_txid_in(
                    objs,
                    &mut p.handles,
                    ch,
                    txid,
                    rd_max_bytes,
                    rd_max_handles,
                    &mut k.vmos.jobs,
                )
                .map_err(|_| (Status::BadState, None, None))?;
                let raw: Vec<u8> = got.handles.iter().flat_map(|h| h.0.to_le_bytes()).collect();
                copy_to_user_verified(&mut p.aspace, &mut k.mm, rd_bytes, &got.data)
                    .map_err(|e| (e, None, None))?;
                copy_to_user_verified(&mut p.aspace, &mut k.mm, rd_handles, &raw)
                    .map_err(|e| (e, None, None))?;
                Ok((Some(got.data.len() as u64), Some(got.handles.len() as u64)))
            }
            Err(ReadError::TooSmall { bytes, handles }) => Err((
                Status::BufferTooSmall,
                Some(bytes as u64),
                Some(handles as u64),
            )),
            Err(ReadError::Status(Status::ShouldWait)) => Err((Status::TimedOut, None, None)),
            Err(ReadError::Status(st)) => Err((st, None, None)),
        }
    })
    .map_err(|e| (e, None, None))?;
    // (a reply that came wins; an interrupt that ended the wait reports INTERRUPTED, the request stays queued)
    match out {
        Err((Status::TimedOut, ..)) if deadline != 0 && take_interrupt() => {
            fail(Status::Interrupted)
        }
        other => other,
    }
}
