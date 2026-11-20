//! The blocking syscalls, without the blocking (TS3d): what a wait checks before it sleeps, the test that ends the sleep, and how it reports when it
//! wakes. The kernel does the sleeping between the two halves (it registers the thread, blocks, and forgets the registration), so these run on the host.
//!
//! Every function takes `me`, the calling process, and the state it acts on; `interrupted` and `killed` are the kernel's facts about the calling
//! thread (a pending `thread_interrupt`, a kill), which the caller reads under the same lock.

use crate::{Reply, UserState};
use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::vec::Vec;
use kcore::aspace::Vmos;
use kcore::ipc::{self, ReadError};
use kcore::pagetable::PhysMem;
use kcore::usercopy::{copy_from_user, copy_to_user_verified, verify_writable};
use kcore::Mm;

fn fail(s: Status) -> Reply {
    Err((s, None, None))
}

// ---- object_wait ----

/// What `object_wait` checks before it may sleep.
#[derive(Debug, PartialEq, Eq)]
pub struct ObjectWait {
    pub handle: Handle,
    pub mask: u32,
    pub deadline: u64,
}

/// `object_wait(handle, mask, deadline)`: the handle is 32 bits, carries WAIT, and the mask is a non-empty set of waitable signals.
pub fn object_wait_check(us: &UserState, me: u32, a: [u64; 6]) -> Result<ObjectWait, Status> {
    if a[0] > u64::from(u32::MAX) {
        return Err(Status::BadHandle);
    }
    let (handle, deadline) = (Handle(a[0] as u32), a[2]);
    let p = us.procs.get(&me).ok_or(Status::BadState)?;
    if !p.handles.get(handle)?.rights.contains(Rights::WAIT) {
        return Err(Status::AccessDenied);
    }
    if a[1] > u64::from(u32::MAX) || a[1] == 0 || (a[1] as u32) & !abi::signals::WAITABLE != 0 {
        return Err(Status::InvalidArgs);
    }
    Ok(ObjectWait {
        handle,
        mask: a[1] as u32,
        deadline,
    })
}

/// The signal word of the process's handle (the error is BAD_HANDLE, WRONG_TYPE... as `signals_of` says; a handle that names nothing).
pub fn signals_of(us: &UserState, me: u32, h: Handle) -> Result<u32, Status> {
    let p = us.procs.get(&me).ok_or(Status::BadState)?;
    ipc::signals_of(&us.objs, &p.handles, h)
}

/// How `object_wait` reports once the sleep is over: the signals seen (an error if the handle went away), whether the thread was interrupted
/// (and took the flag), and the wait it made.
pub fn object_wait_report(
    seen: Result<u32, Status>,
    w: &ObjectWait,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    let seen = seen.map_err(|e| (e, None, None))?;
    if seen & w.mask != 0 {
        Ok((Some(u64::from(seen)), None))
    } else if w.deadline != 0 && take_interrupt() {
        Err((Status::Interrupted, Some(u64::from(seen)), None))
    } else {
        Err((Status::TimedOut, Some(u64::from(seen)), None))
    }
}

// ---- port_wait ----

/// `port_wait(port, deadline)`: the handle must be 32 bits wide.
pub fn port_wait_check(a: [u64; 6]) -> Result<(Handle, u64), Status> {
    if a[0] > u64::from(u32::MAX) {
        return Err(Status::BadHandle);
    }
    Ok((Handle(a[0] as u32), a[1]))
}

/// One poll of the port: the key and observed signals of a ready binding, or `None`.
pub fn port_poll(
    us: &mut UserState,
    vmos: &mut Vmos,
    me: u32,
    h: Handle,
) -> Result<Option<(u64, u32)>, Status> {
    let UserState { objs, procs, .. } = us;
    let p = procs.get(&me).ok_or(Status::BadState)?;
    ipc::port_poll_in(objs, &p.handles, h, &mut vmos.jobs)
}

/// How `port_wait` reports: the binding it found, else INTERRUPTED if the interrupt flag was taken, else TIMED_OUT.
pub fn port_wait_report(
    got: Option<(u64, u32)>,
    deadline: u64,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    match got {
        Some((key, observed)) => Ok((Some(key), Some(u64::from(observed)))),
        None if deadline != 0 && take_interrupt() => fail(Status::Interrupted),
        None => fail(Status::TimedOut),
    }
}

// ---- futex_wait and futex_wake ----

/// What `futex_wait` decided before sleeping.
#[derive(Debug, PartialEq, Eq)]
pub enum FutexCheck {
    /// The thread is on the futex's queue: sleep until it is removed, a kill, an interrupt or the deadline.
    Sleep,
    /// Do not sleep: the call's result.
    Done(Status),
}

/// `futex_wait(addr, expected, deadline)`: compare the word with `expected` and enqueue the thread, one step under the lock, so that a wake
/// after a user-mode store is never lost. `who` is the thread's id.
pub fn futex_wait_check<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    me: u32,
    who: u64,
    a: [u64; 6],
) -> Result<FutexCheck, Status> {
    let (addr, expected, deadline) = (a[0], a[1], a[2]);
    if expected > u64::from(u32::MAX) {
        return Err(Status::InvalidArgs);
    }
    let UserState { procs, futexes, .. } = us;
    let p = procs.get_mut(&me).ok_or(Status::BadState)?;
    let word = kcore::usercopy::read_user_u32(&mut p.aspace, mm, vmos, addr)?;
    if u64::from(word) != expected {
        return Ok(FutexCheck::Done(Status::ShouldWait)); // the word changed: do not sleep
    }
    if deadline == 0 {
        return Ok(FutexCheck::Done(Status::TimedOut)); // a poll: equal, and no time to wait
    }
    let fresh = futexes.wait((me, addr), who);
    debug_assert!(fresh, "a thread sleeps on one futex at a time");
    Ok(FutexCheck::Sleep)
}

/// How `futex_wait` reports once the sleep is over: still in the table means the deadline (or a kill or an interrupt) ended it.
pub fn futex_wait_report(
    us: &mut UserState,
    who: u64,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    let timed_out = us.futexes.forget(who);
    if timed_out && take_interrupt() {
        fail(Status::Interrupted)
    } else if timed_out {
        fail(Status::TimedOut)
    } else {
        Ok((None, None))
    }
}

/// `futex_wake(addr, count)`: wake up to `count` sleepers on `addr` in this process, oldest first. Returns the thread ids the caller must wake.
pub fn futex_wake(us: &mut UserState, me: u32, a: [u64; 6]) -> Result<Vec<u64>, Status> {
    let (addr, count) = (a[0], a[1]);
    if addr & 3 != 0 || addr < abi::user::BASE || addr.saturating_add(4) > abi::user::TOP {
        return Err(Status::InvalidArgs);
    }
    if !us.procs.contains_key(&me) {
        return Err(Status::BadState);
    }
    Ok(us.futexes.wake((me, addr), count))
}

// ---- channel_call ----

/// What `channel_call` keeps between its request and its reply.
#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    pub ch: Handle,
    pub txid: u32,
    pub rd_bytes: u64,
    pub rd_handles: u64,
    pub rd_max_bytes: usize,
    pub rd_max_handles: usize,
    pub deadline: u64,
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller passes a constant offset whose four or eight bytes lie inside the fixed-size record it just copied in (the longest, offset 48, is in a record of 56 bytes)
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: as `le32`
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// `channel_call(channel, record)`, the request half: read the record, check it, and send the request. `Err` is the call's result (nothing was
/// sent unless the error came after the send, which none does). `interrupted` is whether a `thread_interrupt` is pending (it ends the call before
/// anything is sent, so a retry sends one request, not two) and is taken here.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: nhandles <= MAX_MSG_HANDLES was checked first, so nhandles * 4 is small; the record read is of fixed size
pub fn channel_call_send<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    me: u32,
    a: [u64; 6],
    take_interrupt: impl FnOnce() -> bool,
) -> Result<Call, Status> {
    if a[0] > u64::from(u32::MAX) {
        return Err(Status::BadHandle);
    }
    let ch = Handle(a[0] as u32);
    let UserState { objs, procs, .. } = us;
    let p = procs.get_mut(&me).ok_or(Status::BadState)?;
    let rec = copy_from_user(&mut p.aspace, mm, vmos, a[1], abi::CHANNEL_CALL_ARGS_BYTES)?;
    let (req_bytes, req_handles) = (le64(&rec, 0), le64(&rec, 8));
    let (nbytes, nhandles) = (le32(&rec, 16) as usize, le32(&rec, 20) as usize);
    let (rd_bytes, rd_handles) = (le64(&rec, 24), le64(&rec, 32));
    let (rd_max_bytes, rd_max_handles) = (
        (le32(&rec, 40) as usize).min(MAX_MSG_BYTES),
        (le32(&rec, 44) as usize).min(MAX_MSG_HANDLES),
    );
    let deadline = le64(&rec, 48);
    if nbytes > MAX_MSG_BYTES || nhandles > MAX_MSG_HANDLES {
        return Err(Status::TooBig);
    }
    if nbytes < 4 {
        return Err(Status::InvalidArgs);
    }
    // An interrupt already pending ends the call before anything is sent (a retry then sends one request, not two).
    if deadline != 0 && take_interrupt() {
        return Err(Status::Interrupted);
    }
    // Send the request.
    let entry = p.handles.get(ch)?;
    if !(entry.rights.contains(Rights::READ) && entry.rights.contains(Rights::WRITE)) {
        // (a non-channel handle still reports WRONG_TYPE first)
        if !matches!(objs.get(entry.obj), Some(ipc::KObj::Channel(_))) {
            return Err(Status::WrongType);
        }
        return Err(Status::AccessDenied);
    }
    let data = copy_from_user(&mut p.aspace, mm, vmos, req_bytes, nbytes)?;
    let raw = copy_from_user(&mut p.aspace, mm, vmos, req_handles, nhandles * 4)?;
    ipc::channel_write_in(
        objs,
        &mut p.handles,
        ch,
        &data,
        &crate::handle_list(&raw)?,
        &mut vmos.jobs,
    )?;
    Ok(Call {
        ch,
        txid: le32(&data, 0),
        rd_bytes,
        rd_handles,
        rd_max_bytes,
        rd_max_handles,
        deadline,
    })
}

/// The test that ends a call's sleep: the thread was killed or interrupted, or a reply with its transaction id (or the peer closing) is there.
/// A thread that is not a user thread, or a process that is gone, ends it too.
pub fn channel_call_ready(us: &UserState, me: u32, c: &Call, interrupted: bool) -> bool {
    let Some(p) = us.procs.get(&me) else {
        return true;
    };
    if p.killed.is_some() || interrupted {
        return true;
    }
    !matches!(
        ipc::channel_peek(
            &us.objs,
            &p.handles,
            c.ch,
            Some(c.txid),
            c.rd_max_bytes,
            c.rd_max_handles
        ),
        Err(ReadError::Status(Status::ShouldWait))
    )
}

/// `channel_call`, the reply half: take the reply with the call's transaction id and copy it out (the destinations are checked before the message
/// is taken, so a bad pointer loses nothing), or report TIMED_OUT, BUFFER_TOO_SMALL... An interrupt that ended the wait reports INTERRUPTED (the
/// request stays queued).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: nh <= MAX_MSG_HANDLES and the index of a handle in the message is below it, so nh * 4 and handle_index * 4 are small; rd_handles + offset stays in the user range that `verify_writable` checked
pub fn channel_call_reply<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    me: u32,
    c: &Call,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    let UserState { objs, procs, .. } = us;
    let p = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
    let out: Reply = match ipc::channel_peek(
        objs,
        &p.handles,
        c.ch,
        Some(c.txid),
        c.rd_max_bytes,
        c.rd_max_handles,
    ) {
        Ok((nb, nh)) => (|| {
            verify_writable(&mut p.aspace, mm, vmos, c.rd_bytes, nb)
                .map_err(|e| (e, None, None))?;
            verify_writable(&mut p.aspace, mm, vmos, c.rd_handles, nh * 4)
                .map_err(|e| (e, None, None))?;
            let got = ipc::channel_read_txid_in(
                objs,
                &mut p.handles,
                c.ch,
                c.txid,
                c.rd_max_bytes,
                c.rd_max_handles,
                &mut vmos.jobs,
            )
            .map_err(|_| (Status::BadState, None, None))?;
            copy_to_user_verified(&mut p.aspace, mm, c.rd_bytes, &got.data)
                .map_err(|e| (e, None, None))?;
            // (the handle numbers go out one at a time from the stack: nothing here allocates after the message has been taken: KT9)
            for (i, h) in got.handles.iter().enumerate() {
                copy_to_user_verified(
                    &mut p.aspace,
                    mm,
                    c.rd_handles + (i as u64) * 4,
                    &h.0.to_le_bytes(),
                )
                .map_err(|e| (e, None, None))?;
            }
            Ok((Some(got.data.len() as u64), Some(got.handles.len() as u64)))
        })(),
        Err(ReadError::TooSmall { bytes, handles }) => Err((
            Status::BufferTooSmall,
            Some(bytes as u64),
            Some(handles as u64),
        )),
        Err(ReadError::Status(Status::ShouldWait)) => Err((Status::TimedOut, None, None)),
        Err(ReadError::Status(st)) => Err((st, None, None)),
    };
    match out {
        Err((Status::TimedOut, ..)) if c.deadline != 0 && take_interrupt() => {
            fail(Status::Interrupted)
        }
        other => other,
    }
}
