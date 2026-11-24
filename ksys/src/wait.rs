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
use kcore::futex::{Buckets, FutexTable};
use kcore::ipc::{self, ReadError};
use kcore::job::Jobs;
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

/// `object_wait(handle, mask, deadline)`: the handle carries WAIT, and the mask is a non-empty set of waitable signals.
pub fn object_wait_check(us: &UserState, me: u64, a: [u64; 6]) -> Result<ObjectWait, Status> {
    let (handle, deadline) = (Handle(a[0]), a[2]);
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
pub fn signals_of(us: &UserState, me: u64, h: Handle) -> Result<u32, Status> {
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

/// `port_wait(port, deadline)`.
pub fn port_wait_check(a: [u64; 6]) -> Result<(Handle, u64), Status> {
    Ok((Handle(a[0]), a[1]))
}

/// One poll of the port: the key and observed signals of a ready binding, or `None`.
pub fn port_poll(
    us: &mut UserState,
    jobs: &mut Jobs,
    me: u64,
    h: Handle,
) -> Result<Option<(u64, u32)>, Status> {
    let UserState { objs, procs, .. } = us;
    let p = procs.get(&me).ok_or(Status::BadState)?;
    ipc::port_poll_in(objs, &p.handles, h, jobs)
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

/// `port_wait_many(port, buffer, capacity, deadline)` (AB4): the handle and deadline, after the capacity (1 to `PORT_PACKETS_MAX`) is checked.
pub fn port_wait_many_check(a: [u64; 6]) -> Result<(Handle, u64, usize, u64), Status> {
    let cap = usize::try_from(a[2]).map_err(|_| Status::InvalidArgs)?;
    if cap == 0 || cap > abi::limits::PORT_PACKETS_MAX {
        return Err(Status::InvalidArgs);
    }
    Ok((Handle(a[0]), a[1], cap, a[3]))
}

/// One poll of the port for up to `cap` ready bindings, written as records at `buf` (`key: u64`, `observed: u32`, four zero bytes). The buffer
/// is verified writable before anything is dequeued (INVALID_ARGS, nothing changes), and the records are written in the same locked step that
/// dequeues them, so a packet is never taken and then lost. The number of records written (0: nothing ready).
#[allow(clippy::too_many_arguments, clippy::arithmetic_side_effects)] // INVARIANT: cap is at most PORT_PACKETS_MAX (checked by `port_wait_many_check`)
pub fn port_poll_many<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    h: Handle,
    buf: u64,
    cap: usize,
) -> Result<usize, Status> {
    let UserState { objs, procs, .. } = us;
    let p = procs.get_mut(&me).ok_or(Status::BadState)?;
    let len = cap * abi::limits::PORT_PACKET_BYTES;
    verify_writable(p.aspace.held_mut(), mm, vmos, jobs, buf, len).map_err(|e| match e {
        // (MM9) a page of a pager's VMO that is not there: the kernel waits for it and the call says "try again"
        Status::ShouldWait => Status::ShouldWait,
        _ => Status::InvalidArgs,
    })?;
    // (the buffer is reserved before the poll takes anything: NO_MEMORY consumes no packet, T248)
    let mut bytes: Vec<u8> = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| Status::NoMemory)?;
    let got = ipc::port_poll_many_in(objs, &p.handles, h, jobs, cap)?;
    for &(key, observed) in &got {
        bytes.extend_from_slice(&key.to_le_bytes());
        bytes.extend_from_slice(&observed.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
    }
    copy_to_user_verified(p.aspace.held_mut(), mm, buf, &bytes)?;
    Ok(got.len())
}

/// How `port_wait_many` reports: the count, else INTERRUPTED if the interrupt flag was taken, else TIMED_OUT.
pub fn port_wait_many_report(
    count: usize,
    deadline: u64,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    if count > 0 {
        return Ok((Some(count as u64), None));
    }
    if deadline != 0 && take_interrupt() {
        return fail(Status::Interrupted);
    }
    fail(Status::TimedOut)
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

/// The word `futex_wait` is about: where it is in physical memory, resolved and made resident (L14).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct FutexWord {
    /// The user address (the futex's key, with the process).
    pub addr: u64,
    /// The physical address of the word's 32 bits.
    pub pa: u64,
    pub expected: u32,
    pub deadline: u64,
}

/// First half of `futex_wait(addr, expected, deadline)` (the big kernel lock held, and the memory locks, which are let go before the second
/// half): validate the arguments and translate the word, committing a lazily backed page, so that the word is in a frame. The frame cannot
/// go while the big lock is held (every unmap, decommit and teardown takes it), which is what keeps it from the second half's read.
pub fn futex_wait_resolve<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    a: [u64; 6],
) -> Result<FutexWord, Status> {
    let p = us.procs.get_mut(&me).ok_or(Status::BadState)?;
    futex_wait_resolve_in(p.aspace.held_mut(), mm, vmos, jobs, a)
}

/// `futex_wait_resolve` on an address space the caller holds the lock of (the quiet path of L19 has no `UserState`: it reaches the space through
/// the hart's record of its running process).
pub fn futex_wait_resolve_in<M: PhysMem>(
    space: &mut kcore::aspace::AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    a: [u64; 6],
) -> Result<FutexWord, Status> {
    let (addr, expected, deadline) = (a[0], a[1], a[2]);
    if expected > u64::from(u32::MAX) {
        return Err(Status::InvalidArgs);
    }
    let pa = kcore::usercopy::resolve_user_word(space, mm, vmos, jobs, addr)?;
    Ok(FutexWord {
        addr,
        pa,
        expected: expected as u32,
        deadline,
    })
}

/// Read the 32-bit word at physical address `pa` with one aligned 64-bit load (as `read_user_u32`: a store by another hart is seen whole).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: (pa & 4) * 8 is 0 or 32, below the shift width of a u64
fn read_word<M: PhysMem>(mem: &M, pa: u64) -> u32 {
    (mem.read_u64(pa & !7) >> ((pa & 4) * 8)) as u32
}

/// Second half: under the key's bucket lock (no memory lock held: `FUTEX` is before them), compare the word with `expected` and queue the
/// thread, one step, so that a wake after a user-mode store (which takes the same lock) is never lost. `who` is the thread's id.
pub fn futex_wait_enqueue<M: PhysMem, B: Buckets>(
    table: &FutexTable<B>,
    mem: &M,
    me: u64,
    who: u64,
    w: &FutexWord,
) -> FutexCheck {
    if w.deadline == 0 {
        // a poll: equal, and no time to wait (not queued)
        return FutexCheck::Done(
            if u64::from(read_word(mem, w.pa)) != u64::from(w.expected) {
                Status::ShouldWait
            } else {
                Status::TimedOut
            },
        );
    }
    match table.wait_if((me, w.addr), who, || read_word(mem, w.pa) == w.expected) {
        Ok(true) => FutexCheck::Sleep,
        Ok(false) => FutexCheck::Done(Status::ShouldWait), // the word changed: do not sleep
        // (KT9: the queue entry could not be made, so the thread is not asleep: a call that decides to sleep is where running out is reported)
        Err(_) => FutexCheck::Done(Status::NoMemory),
    }
}

/// `futex_wait` in one call, for the host tests (the kernel calls the halves, letting go of the memory locks between them).
#[allow(clippy::too_many_arguments)]
pub fn futex_wait_check<M: PhysMem, B: Buckets>(
    us: &mut UserState,
    table: &FutexTable<B>,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    who: u64,
    a: [u64; 6],
) -> Result<FutexCheck, Status> {
    let w = futex_wait_resolve(us, mm, vmos, jobs, me, a)?;
    Ok(futex_wait_enqueue(table, &mm.mem, me, who, &w))
}

/// How `futex_wait` reports once the sleep is over: still in the table means the deadline (or a kill or an interrupt) ended it.
pub fn futex_wait_report<B: Buckets>(
    table: &FutexTable<B>,
    key: (u64, u64),
    who: u64,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    let timed_out = table.forget(key, who);
    if timed_out && take_interrupt() {
        fail(Status::Interrupted)
    } else if timed_out {
        fail(Status::TimedOut)
    } else {
        Ok((None, None))
    }
}

/// The lock word of a PI futex, resolved and made resident for writing (L23).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PiWord {
    pub addr: u64,
    pub pa: u64,
}

/// First half of `futex_lock_pi` and `futex_unlock_pi` (the big kernel lock held, and the caller's `AS` lock and the memory locks, which are let go
/// before the second half): translate the word for writing, committing a page that is lazily backed or copy-on-write. The frame cannot go while
/// the big lock is held.
pub fn futex_pi_resolve<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    addr: u64,
) -> Result<PiWord, Status> {
    let p = us.procs.get_mut(&me).ok_or(Status::BadState)?;
    let pa =
        kcore::usercopy::resolve_user_word_for_write(p.aspace.held_mut(), mm, vmos, jobs, addr)?;
    Ok(PiWord { addr, pa })
}

/// The arguments of `futex_wake(addr, count)`, checked: the word's address and how many to wake. Nothing here touches shared state (L14: the
/// call runs without the big kernel lock).
pub fn futex_wake_args(a: [u64; 6]) -> Result<(u64, u64), Status> {
    let (addr, count) = (a[0], a[1]);
    if addr & 3 != 0 || addr < abi::user::BASE || addr.saturating_add(4) > abi::user::TOP {
        return Err(Status::InvalidArgs);
    }
    Ok((addr, count))
}

/// `futex_wake(addr, count)`: wake up to `count` sleepers on `addr` in process `me`, oldest first. `wake` is run on the thread ids taken from the
/// queue with the bucket lock still held. Returns how many.
pub fn futex_wake<B: Buckets>(
    table: &FutexTable<B>,
    me: u64,
    a: [u64; 6],
    wake: impl FnOnce(&[u64]),
) -> Result<usize, Status> {
    let (addr, count) = futex_wake_args(a)?;
    table
        .wake_with((me, addr), count, wake)
        .map_err(|_| Status::NoMemory)
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

/// `channel_call(channel, record)`, the request half (`caller` is the calling thread's id): read the record, check it, and send the request. `Err` is the call's result (nothing was
/// sent unless the error came after the send, which none does). `interrupted` is whether a `thread_interrupt` is pending (it ends the call before
/// anything is sent, so a retry sends one request, not two) and is taken here.
#[allow(clippy::arithmetic_side_effects, clippy::too_many_arguments)] // INVARIANT: nhandles <= MAX_MSG_HANDLES was checked first, so nhandles * 4 is small; the record read is of fixed size
pub fn channel_call_send<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    caller: u64,
    a: [u64; 6],
    take_interrupt: impl FnOnce() -> bool,
) -> Result<Call, Status> {
    channel_call_send_staged(us, mm, vmos, jobs, me, caller, a, None, take_interrupt)
}

/// `channel_call_send` with the request's bytes and handle words already copied in by the caller before it took the object lock (T215): `staged` is used in place
/// of the two copies (the record is read here as always, so every check and its order are unchanged).
#[allow(clippy::arithmetic_side_effects, clippy::too_many_arguments)] // INVARIANT: as `channel_call_send`
pub fn channel_call_send_staged<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    caller: u64,
    a: [u64; 6],
    mut staged: Option<crate::Staged>,
    take_interrupt: impl FnOnce() -> bool,
) -> Result<Call, Status> {
    let ch = Handle(a[0]);
    let UserState {
        objs,
        procs,
        threads,
        next_call,
        ..
    } = us;
    let p = procs.get_mut(&me).ok_or(Status::BadState)?;
    let rec = copy_from_user(
        p.aspace.held_mut(),
        mm,
        vmos,
        jobs,
        a[1],
        abi::CHANNEL_CALL_ARGS_BYTES,
    )?;
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
    let (data, raw) = match staged.take() {
        Some(st) if st.data.len() == nbytes && st.handles.len() == nhandles * 8 => {
            (st.data, st.handles)
        }
        _ => (
            copy_from_user(p.aspace.held_mut(), mm, vmos, jobs, req_bytes, nbytes)?,
            Vec::new(),
        ),
    };
    let call = *next_call;
    *next_call = next_call.wrapping_add(1);
    let raw = if raw.is_empty() && nhandles != 0 {
        copy_from_user(
            p.aspace.held_mut(),
            mm,
            vmos,
            jobs,
            req_handles,
            nhandles * 8,
        )?
    } else {
        raw
    };
    ipc::channel_write_from(
        objs,
        &mut p.handles,
        ch,
        &data,
        &crate::handle_list(&raw)?,
        jobs,
        Some((caller, call)),
    )?;
    // (from here until the call ends, a server that reads the request lends it the caller's priority: L22)
    if let Some(t) = threads.get_mut(&caller) {
        t.call = Some(call);
    }
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
pub fn channel_call_ready(us: &UserState, me: u64, c: &Call, interrupted: bool) -> bool {
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
    jobs: &mut Jobs,
    me: u64,
    c: &Call,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    channel_call_reply_deferred(us, mm, vmos, jobs, me, c, &mut None, false, take_interrupt)
}

/// `channel_call_reply` that, when `defer_out` is set and the reply is `OUTSIDE_COPY_MIN` bytes or more, leaves the message in `deferred` for the caller to copy out
/// after it let the object lock go (T215): the destinations were checked and the message taken, only the copy is left.
#[allow(clippy::arithmetic_side_effects, clippy::too_many_arguments)] // INVARIANT: as `channel_call_reply`
pub fn channel_call_reply_deferred<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    me: u64,
    c: &Call,
    deferred: &mut Option<crate::Deferred>,
    defer_out: bool,
    take_interrupt: impl FnOnce() -> bool,
) -> Reply {
    let UserState { objs, procs, .. } = us;
    objs.clear_dequeued_calls(); // (the reply is read here, not by a server: a note an earlier read left must not be attributed to a later one, L22)
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
            verify_writable(p.aspace.held_mut(), mm, vmos, jobs, c.rd_bytes, nb)
                .map_err(|e| (e, None, None))?;
            verify_writable(p.aspace.held_mut(), mm, vmos, jobs, c.rd_handles, nh * 8)
                .map_err(|e| (e, None, None))?;
            let got = ipc::channel_read_txid_in(
                objs,
                &mut p.handles,
                c.ch,
                c.txid,
                c.rd_max_bytes,
                c.rd_max_handles,
                jobs,
            )
            .map_err(|_| (Status::BadState, None, None))?;
            if defer_out && got.data.len() >= crate::OUTSIDE_COPY_MIN {
                let (nb, nh) = (got.data.len() as u64, got.handles.len() as u64);
                *deferred = Some(crate::Deferred {
                    bytes_at: c.rd_bytes,
                    data: got.data,
                    handles_at: c.rd_handles,
                    handles: got.handles,
                });
                return Ok((Some(nb), Some(nh)));
            }
            copy_to_user_verified(p.aspace.held_mut(), mm, c.rd_bytes, &got.data)
                .map_err(|e| (e, None, None))?;
            // (the handle numbers go out one at a time from the stack: nothing here allocates after the message has been taken: KT9)
            for (i, h) in got.handles.iter().enumerate() {
                copy_to_user_verified(
                    p.aspace.held_mut(),
                    mm,
                    c.rd_handles + (i as u64) * 8,
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
