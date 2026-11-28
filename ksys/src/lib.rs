//! The kernel's syscall layer on the host (TS3, KERNEL-V1-SPEC): argument checks and the calls on handles, events, channels,
//! sockets and ports, written against a small trait for everything the machine does (the user's memory, the job ledger) so that the
//! real code runs under host tests with a fake machine. `kernel/src/user/` keeps the parts that need the machine (address spaces,
//! processes, threads, waits) and calls in here for these.
//!
//! `forbid(unsafe_code)`, `no_std` + `alloc`: nothing here touches hardware.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::panic,
        clippy::unreachable
    )
)]

extern crate alloc;

mod create;
pub mod iodomain;
mod job;
mod lifecycle;
pub mod pager;
mod proc;
mod state;
mod thread;
mod vmo;
mod wait;
pub use create::process_create;
pub mod exception;
pub use job::{job_call, job_status};
pub use lifecycle::{
    job_kill_begin, job_kill_next, job_set_sched, kill_target, stop_target,
    thread_set_sched_target, thread_target,
};
pub use proc::{
    cow_sync, place_anywhere, proc_call, process_protect_begin, process_protect_end,
    process_protect_step, process_unmap_begin, process_unmap_end, process_unmap_step, put_process,
    vmo_range_begin, vmo_range_finish, vmo_range_next_owner, vmo_range_take, vmo_range_unmap_owner,
    LibRange, Machine, NoMap, PCtx, ProcState, ProcTable, Process, ProtectCall, RangeBatch,
    RangeCall, SpawnStack, StartMap, UnmapCall,
};
pub use state::{set_interrupt_in, PendingRegs, ThreadStart, Threads, UThread, UserState};
pub use thread::{
    charge_thread, clamp_prio, credit_thread, initial_general, new_start, process_start,
    thread_at_return, thread_blocks, thread_is_suspended, thread_registered_by_spawner,
    thread_resume, thread_spawn, thread_state_target, thread_stop_pending, thread_suspend,
    thread_suspend_forget, thread_unparked, total_threads, Spawn, KSTACK_PAGES,
};
pub use vmo::{vmo_call, vmo_commit_resolved, vmo_rw_resolved};
pub use wait::{
    channel_call_ready, channel_call_reply, channel_call_reply_deferred, channel_call_send,
    channel_call_send_staged, futex_pi_resolve, futex_wait_check, futex_wait_enqueue,
    futex_wait_report, futex_wait_resolve, futex_wait_resolve_in, futex_wake, futex_wake_args,
    object_wait_check, object_wait_report, port_poll, port_poll_many, port_wait_check,
    port_wait_many_check, port_wait_many_report, port_wait_report, signals_of, Call, FutexCheck,
    FutexWord, ObjectWait, PiWord,
};

use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use alloc::vec::Vec;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, ReadError};
use kcore::job::{JobId, Jobs};
use kcore::pagetable::PhysMem;
use kcore::usercopy::{copy_from_user, copy_to_user, verify_writable};
use kcore::Mm;

/// The failure half of a syscall's reply: the status and the two result registers some errors carry.
pub type ReplyErr = (Status, Option<u64>, Option<u64>);
/// A syscall's reply: the success values (`a1`, `a2`) or a failure.
pub type Reply = Result<(Option<u64>, Option<u64>), ReplyErr>;

/// The negative-control kernels' switches (the `fault-*` features of `kernel`): each is off in every real kernel.
#[derive(Clone, Copy, Default)]
pub struct Faults {
    /// `fault-skip-forced-touch`: a replaced handle is not reported to the wait table.
    pub skip_forced_touch: bool,
}

/// One calling process, as these calls see it: its handles and address space, and the kernel state the calls act on. User memory
/// goes through the real `kcore::usercopy` over whatever `PhysMem` the machine has (the direct map in the kernel, a `Vec` on the
/// host), so pointer rules are the kernel's own rules in both.
pub struct Ctx<'a, M: PhysMem> {
    pub objs: &'a mut Kobjs,
    pub handles: &'a mut HandleTable,
    /// The job the process's new objects are charged to (the ledger is `jobs`).
    pub job: JobId,
    pub aspace: &'a mut AddressSpace,
    pub mm: &'a mut Mm<M>,
    pub vmos: &'a mut Vmos,
    pub jobs: &'a mut Jobs,
    pub faults: Faults,
    /// The processors that are online (`job_info` 7).
    pub harts_online: u64,
    /// What each job's threads have run (SCHED-SMP-SPEC 4.10; `job_info` 9 and 10) and the clock it is read against (nanoseconds since boot).
    pub job_sched: &'a kcore::jobsched::JobSchedTable,
    pub clock: fn() -> u64,
    /// The device windows a physical VMO may name (page-aligned `(start, end)` byte ranges, built from the device tree at boot: PL5).
    pub windows: &'a [(u64, u64)],
    /// The bytes of a big `channel_write` or `socket_write`, copied in before the caller took the object lock (T215); the call uses them in place of its own copy.
    pub staged: Option<Staged>,
    /// Set when the caller copies a big `channel_read` or `socket_read` message out after it let the object lock go (T215): the call leaves the message in
    /// `deferred` instead of copying it.
    pub defer_out: bool,
    pub deferred: Option<Deferred>,
}

/// What a write call's caller copied in for it (T215): the message or stream bytes, and the raw handle numbers of a message.
pub struct Staged {
    pub data: Vec<u8>,
    pub handles: Vec<u8>,
}

/// A big read that a call claimed and left for its caller to finish (T215, OD161): the destinations were checked against what it will take,
/// `data` is a copy of those bytes, and nothing has been taken yet. The caller copies `data` out with the object lock let go
/// (`deferred_copy`), then takes the object lock again and commits (`deferred_commit`).
pub struct Deferred {
    pub bytes_at: u64,
    pub data: Vec<u8>,
    pub handles_at: u64,
    /// How many handles the message carries (their numbers are written at the commit, when they are in the table).
    pub nhandles: usize,
    pub read: DeferredRead,
}

/// What a deferred read claimed.
#[derive(Clone, Copy, Debug)]
pub enum DeferredRead {
    /// The message `claim` names, on the channel `ch` names.
    Channel { ch: Handle, claim: ipc::ReadClaim },
    /// The first `n` bytes of the socket `h` names, as `claim` saw them.
    Socket {
        h: Handle,
        claim: ipc::ReadClaim,
        n: usize,
    },
}

/// Messages and stream writes of at least this many bytes are copied outside the object lock (T215): below it the extra lock round trip costs more than the copy.
pub const OUTSIDE_COPY_MIN: usize = 1024;

pub(crate) fn fail(s: Status) -> Reply {
    Err((s, None, None))
}

/// A 32-bit argument that is not a handle (rights, masks, flags) with high bits set names nothing and grants nothing, instead of being
/// silently truncated. Handles are 64 bits (AB1): a handle argument has no width rule, and a value that names nothing is BAD_HANDLE from the
/// table. `Some(status)`: the call is refused before it runs.
#[allow(clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and `over(i)` is called with constants below 6
pub fn wide_argument_error(n: usize, a: &[u64; 6]) -> Option<Status> {
    let over = |i: usize| a[i] > u64::from(u32::MAX);
    match n {
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE if over(1) => Some(Status::AccessDenied),
        nr::EVENT_SIGNAL if over(1) || over(2) => Some(Status::InvalidArgs),
        nr::CHANNEL_READ_TXID if over(1) => Some(Status::InvalidArgs),
        _ => None,
    }
}

pub(crate) fn handle_list(args: &[u8]) -> Result<Vec<Handle>, Status> {
    let mut v = Vec::new();
    v.try_reserve_exact(args.len() / 8)
        .map_err(|_| Status::NoMemory)?;
    v.extend(
        args.as_chunks::<8>()
            .0
            .iter()
            .map(|c| Handle(u64::from_le_bytes(*c))),
    );
    Ok(v)
}

/// The calls on handles, events, channels, sockets and ports. `None`: `n` is not one of them (the caller handles it). The width of
/// 32-bit arguments is checked by the caller with [`wide_argument_error`].
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and every `h(i)` is called with a constant below 6; sizes were bounded by the limits checked first (nbytes <= MAX_MSG_BYTES, nhandles <= MAX_MSG_HANDLES, so nhandles * 4 is small)
pub fn ipc_call<M: PhysMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let Ctx {
        objs,
        handles,
        job,
        aspace,
        mm,
        vmos,
        jobs,
        faults,
        staged,
        defer_out,
        deferred,
        ..
    } = c;
    let (job, skip_forced_touch) = (*job, faults.skip_forced_touch);
    if !matches!(
        n,
        nr::HANDLE_CLOSE
            | nr::HANDLE_DUPLICATE
            | nr::HANDLE_REPLACE
            | nr::CHANNEL_CREATE
            | nr::SOCKET_CREATE
            | nr::SOCKET_WRITE
            | nr::SOCKET_READ
            | nr::SOCKET_SHUTDOWN
            | nr::PORT_CREATE
            | nr::PORT_BIND
            | nr::PORT_REARM
            | nr::PORT_UNBIND
            | nr::EVENT_CREATE
            | nr::EVENT_SIGNAL
            | nr::OBJECT_INFO
            | nr::OBJECT_SIGNALS
            | nr::CHANNEL_WRITE
            | nr::CHANNEL_READ
            | nr::CHANNEL_READ_TXID
    ) {
        return None;
    }
    let h = |i: usize| Handle(a[i]);
    let r: Reply = (|| match n {
        nr::HANDLE_CLOSE => handles
            .close_bounded(objs, h(0))
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::HANDLE_DUPLICATE => {
            let nh = handles
                .duplicate(objs, h(0), Rights(a[1] as u32))
                .map_err(|e| (e, None, None))?;
            Ok((Some(nh.0), None))
        }
        nr::HANDLE_REPLACE => {
            let nh = handles
                .replace(objs, h(0), Rights(a[1] as u32))
                .map_err(|e| (e, None, None))?;
            // (the negative-control kernel forgets the forced touch: a waiter on the replaced handle
            // must then sleep on, and the test that guards the wake must fail)
            if skip_forced_touch {
                objs.take_dirty();
            }
            Ok((Some(nh.0), None))
        }
        nr::CHANNEL_CREATE => {
            let (x, y) = ipc::channel_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(x.0), Some(y.0)))
        }
        nr::SOCKET_CREATE => {
            let (x, y) = ipc::socket_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(x.0), Some(y.0)))
        }
        nr::SOCKET_WRITE => {
            let len = a[2] as usize;
            ipc::socket_write_check(objs, handles, h(0), len).map_err(|e| (e, None, None))?;
            let data = match staged.take() {
                Some(st) => st.data,
                None => copy_from_user(aspace, mm, vmos, jobs, a[1], len)
                    .map_err(|e| (e, None, None))?,
            };
            ipc::socket_write_in(objs, handles, h(0), &data, jobs)
                .map(|n| (Some(n as u64), None))
                .map_err(|e| (e, None, None))
        }
        nr::SOCKET_READ => {
            let max = a[2] as usize;
            // Peek, copy, commit (OD161): what a read would deliver is checked against the destination and copied out before anything is
            // removed, so a bad pointer or a failed copy never loses data.
            let (claim, n) =
                ipc::socket_claim(objs, handles, h(0), max).map_err(|e| (e, None, None))?;
            verify_writable(aspace, mm, vmos, jobs, a[1], n).map_err(|e| (e, None, None))?;
            if *defer_out && n >= OUTSIDE_COPY_MIN {
                let mut data: Vec<u8> = Vec::new();
                data.try_reserve_exact(n)
                    .map_err(|_| (Status::NoMemory, None, None))?;
                if let Some((x, y)) = ipc::claimed_stream(objs, &claim, n) {
                    data.extend_from_slice(x);
                    data.extend_from_slice(y);
                }
                *deferred = Some(Deferred {
                    bytes_at: a[1],
                    data,
                    handles_at: 0,
                    nhandles: 0,
                    read: DeferredRead::Socket { h: h(0), claim, n },
                });
                return Ok((Some(n as u64), None));
            }
            if let Some((x, y)) = ipc::claimed_stream(objs, &claim, n) {
                copy_to_user(aspace, mm, vmos, jobs, a[1], x).map_err(|e| (e, None, None))?;
                copy_to_user(aspace, mm, vmos, jobs, a[1] + x.len() as u64, y)
                    .map_err(|e| (e, None, None))?;
            }
            match commit_socket(objs, handles, jobs, h(0), &claim, n) {
                Committed::Done(r) => r,
                // (not reached: the object lock was held from the claim to here)
                Committed::Moved => fail(Status::BadState),
            }
        }
        nr::SOCKET_SHUTDOWN => ipc::socket_shutdown(objs, handles, h(0))
            .map(|()| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_CREATE => {
            let port = ipc::port_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(port.0), None))
        }
        nr::PORT_BIND => ipc::port_bind_flags_in(objs, handles, h(0), h(1), a[2], a[3], a[4], jobs)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_REARM => ipc::port_rearm_in(objs, handles, h(0), a[1], jobs)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_UNBIND => ipc::port_unbind_in(objs, handles, h(0), a[1], jobs)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::EVENT_CREATE => {
            let e = ipc::event_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(e.0), None))
        }
        nr::EVENT_SIGNAL => ipc::event_signal(objs, handles, h(0), a[1] as u32, a[2] as u32)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::OBJECT_INFO => ipc::object_info(objs, handles, h(0))
            .map(|(k, peer)| (Some(k), Some(peer)))
            .map_err(|e| (e, None, None)),
        nr::OBJECT_SIGNALS => ipc::signals_of(objs, handles, h(0))
            .map(|s| (Some(u64::from(s)), None))
            .map_err(|e| (e, None, None)),
        nr::CHANNEL_WRITE => {
            let (nbytes, nhandles) = (a[2] as usize, a[4] as usize);
            // Sizes first: they bound everything that follows (SYSCALLS.md).
            if nbytes > MAX_MSG_BYTES || nhandles > MAX_MSG_HANDLES {
                return fail(Status::TooBig);
            }
            // The channel handle next (BAD_HANDLE is the first error after the sizes, SYSCALLS.md): before anything is copied, so a probe with a bad
            // handle and a bad buffer reads BAD_HANDLE (T171). The type and the rights are checked with the rest, after the copies.
            if let Err(e) = handles.get(h(0)) {
                return fail(e);
            }
            let (data, raw) = match staged.take() {
                Some(st) => (st.data, st.handles),
                None => (
                    copy_from_user(aspace, mm, vmos, jobs, a[1], nbytes)
                        .map_err(|e| (e, None, None))?,
                    copy_from_user(aspace, mm, vmos, jobs, a[3], nhandles * 8)
                        .map_err(|e| (e, None, None))?,
                ),
            };
            ipc::channel_write_in(
                objs,
                handles,
                h(0),
                &data,
                &handle_list(&raw).map_err(|e| (e, None, None))?,
                jobs,
            )
            .map(|_| (None, None))
            .map_err(|e| (e, None, None))
        }
        nr::CHANNEL_READ | nr::CHANNEL_READ_TXID => {
            let (txid, bytes_at, max_bytes, handles_at, max_handles) = if n == nr::CHANNEL_READ {
                (None, a[1], a[2] as usize, a[3], a[4] as usize)
            } else {
                (Some(a[1] as u32), a[2], a[3] as usize, a[4], a[5] as usize)
            };
            // A message never exceeds the limits, so larger declared buffers act as the limit.
            let (mb, mh) = (
                max_bytes.min(MAX_MSG_BYTES),
                max_handles.min(MAX_MSG_HANDLES),
            );
            read_message(
                objs,
                handles,
                aspace,
                mm,
                vmos,
                jobs,
                h(0),
                txid,
                bytes_at,
                mb,
                handles_at,
                mh,
                *defer_out,
                deferred,
            )
        }
        // (not reached: the guard above returned `None` for every call that is not one of these)
        _ => fail(Status::BadSyscall),
    })();
    Some(r)
}

/// What a commit came to (OD161): the call's reply, or `Moved` when what the read claimed was taken by another read after the claim, and
/// the read must be made again from the start.
pub enum Committed {
    Done(Reply),
    Moved,
}

/// A channel read, claim to commit (OD161: peek, copy, commit). The destinations are checked against the *actual* message (not the declared
/// capacity) after the read's status is known and before anything is taken, so BUFFER_TOO_SMALL and SHOULD_WAIT win over a bad pointer and a
/// generous `max_bytes` needs no generous mapping. A small message is copied out and taken here, under the caller's locks; a big one with
/// `defer_out` is copied into `deferred` (allocating now, before anything is taken) for the caller to copy out with the object lock let go
/// and then commit (`deferred_commit`). A copy that fails takes nothing: the message stays queued. `channel_call`'s reply uses it too.
#[allow(clippy::too_many_arguments, clippy::arithmetic_side_effects)] // INVARIANT: nhandles <= MAX_MSG_HANDLES, so nhandles * 8 is small
pub(crate) fn read_message<M: PhysMem>(
    objs: &mut Kobjs,
    handles: &mut HandleTable,
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    ch: Handle,
    txid: Option<u32>,
    bytes_at: u64,
    max_bytes: usize,
    handles_at: u64,
    max_handles: usize,
    defer_out: bool,
    deferred: &mut Option<Deferred>,
) -> Reply {
    let (claim, nbytes, nhandles) =
        match ipc::channel_claim(objs, handles, ch, txid, max_bytes, max_handles) {
            Ok(c) => c,
            Err(ReadError::TooSmall { bytes, handles }) => {
                return Err((
                    Status::BufferTooSmall,
                    Some(bytes as u64),
                    Some(handles as u64),
                ))
            }
            Err(ReadError::Status(st)) => return fail(st),
        };
    verify_writable(aspace, mm, vmos, jobs, bytes_at, nbytes).map_err(|e| (e, None, None))?;
    verify_writable(aspace, mm, vmos, jobs, handles_at, nhandles * 8)
        .map_err(|e| (e, None, None))?;
    if defer_out && nbytes >= OUTSIDE_COPY_MIN {
        let mut data: Vec<u8> = Vec::new();
        data.try_reserve_exact(nbytes)
            .map_err(|_| (Status::NoMemory, None, None))?;
        data.extend_from_slice(ipc::claimed_bytes(objs, &claim).unwrap_or(&[]));
        *deferred = Some(Deferred {
            bytes_at,
            data,
            handles_at,
            nhandles,
            read: DeferredRead::Channel { ch, claim },
        });
        return Ok((Some(nbytes as u64), Some(nhandles as u64)));
    }
    let bytes = ipc::claimed_bytes(objs, &claim).unwrap_or(&[]);
    copy_to_user(aspace, mm, vmos, jobs, bytes_at, bytes).map_err(|e| (e, None, None))?;
    match commit_channel(
        objs, handles, aspace, mm, vmos, jobs, ch, &claim, handles_at,
    ) {
        Committed::Done(r) => r,
        // (not reached: the object lock was held from the claim to here)
        Committed::Moved => fail(Status::BadState),
    }
}

/// Write a received message's handle numbers to `at`, as 8-byte little-endian words, in one copy (from the stack: nothing allocates but the
/// copy's own page list, and a failure there is reported before the message is taken).
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i < n <= MAX_MSG_HANDLES, so every slice is inside `words`
fn put_handle_words<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    at: u64,
    hs: &[Handle],
) -> Result<(), Status> {
    let mut words = [0u8; MAX_MSG_HANDLES * 8];
    let n = hs.len().min(MAX_MSG_HANDLES);
    for (i, h) in hs.iter().take(n).enumerate() {
        words[i * 8..i * 8 + 8].copy_from_slice(&h.0.to_le_bytes());
    }
    copy_to_user(aspace, mm, vmos, jobs, at, &words[..n * 8])
}

#[allow(clippy::too_many_arguments)]
fn commit_channel<M: PhysMem>(
    objs: &mut Kobjs,
    handles: &mut HandleTable,
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    ch: Handle,
    claim: &ipc::ReadClaim,
    handles_at: u64,
) -> Committed {
    match ipc::channel_commit(objs, handles, ch, claim, jobs, |hs, jobs| {
        put_handle_words(aspace, mm, vmos, jobs, handles_at, hs)
    }) {
        Ok(r) => Committed::Done(Ok((
            Some(r.data.len() as u64),
            Some(r.handles.len() as u64),
        ))),
        Err(ipc::CommitError::Moved) => Committed::Moved,
        Err(ipc::CommitError::Status(st)) => Committed::Done(fail(st)),
    }
}

fn commit_socket(
    objs: &mut Kobjs,
    handles: &HandleTable,
    jobs: &mut Jobs,
    h: Handle,
    claim: &ipc::ReadClaim,
    n: usize,
) -> Committed {
    match ipc::socket_commit(objs, handles, h, claim, n, jobs) {
        Ok(()) => Committed::Done(Ok((Some(n as u64), None))),
        Err(ipc::CommitError::Moved) => Committed::Moved,
        Err(ipc::CommitError::Status(st)) => Committed::Done(fail(st)),
    }
}

/// Copy `len` bytes of a deferred read's copy, from offset `at`, to the caller's buffer (OD161, the copy of peek, copy, commit): the caller
/// holds the address-space lock and the memory locks, not the object lock, and copies a big message in pieces. A failure takes nothing:
/// the message (or the bytes) stays queued and the call reports the copy's status.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: at + len <= d.data.len() (the slice's own bound), and bytes_at + at stays in the range verify_writable checked
pub fn deferred_copy<M: PhysMem>(
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    d: &Deferred,
    at: usize,
    len: usize,
) -> Result<(), Status> {
    let part = d
        .data
        .get(at..at.saturating_add(len))
        .ok_or(Status::InvalidArgs)?;
    copy_to_user(aspace, mm, vmos, jobs, d.bytes_at + at as u64, part)
}

/// The commit of a deferred read (OD161), with the object lock taken again after the copy: the message the claim named is taken (its handles
/// put in the table and their numbers written out first), or the bytes removed. `Moved`: another read took them in between, and the caller
/// makes the read again from the start (its buffer then holds whatever that read delivers).
#[allow(clippy::too_many_arguments)]
pub fn deferred_commit<M: PhysMem>(
    objs: &mut Kobjs,
    handles: &mut HandleTable,
    aspace: &mut AddressSpace,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    d: &Deferred,
) -> Committed {
    match d.read {
        DeferredRead::Channel { ch, claim } => commit_channel(
            objs,
            handles,
            aspace,
            mm,
            vmos,
            jobs,
            ch,
            &claim,
            d.handles_at,
        ),
        DeferredRead::Socket { h, claim, n } => commit_socket(objs, handles, jobs, h, &claim, n),
    }
}
