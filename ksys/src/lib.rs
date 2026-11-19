//! The kernel's syscall layer on the host (TS3, KERNEL-V1-SPEC): argument checks and the calls on handles, events, channels,
//! sockets and ports, written against a small trait for everything the machine does (the user's memory, the job ledger) so that the
//! real code runs under host tests with a fake machine. `kernel/src/user.rs` keeps the parts that need the machine (address spaces,
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

use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use alloc::vec::Vec;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, ReadError};
use kcore::job::{JobId, Jobs};

/// The failure half of a syscall's reply: the status and the two result registers some errors carry.
pub type ReplyErr = (Status, Option<u64>, Option<u64>);
/// A syscall's reply: the success values (`a1`, `a2`) or a failure.
pub type Reply = Result<(Option<u64>, Option<u64>), ReplyErr>;

/// What the calls in this crate need from the machine: the calling process's user memory and the job ledger the objects are charged
/// to. The kernel implements it over the address space, the page allocator and the VMO table; the host tests over a `Vec`.
pub trait UserMem {
    /// Copy `len` bytes from user address `ptr` into a kernel buffer.
    fn copy_in(&mut self, ptr: u64, len: usize) -> Result<Vec<u8>, Status>;
    /// Check that `len` bytes at `ptr` can be written, committing what it must, so a later `copy_out_verified` allocates nothing and
    /// fails for no reason (a bad pointer must never lose a message).
    fn verify_writable(&mut self, ptr: u64, len: usize) -> Result<(), Status>;
    /// Copy to a destination `verify_writable` has passed.
    fn copy_out_verified(&mut self, ptr: u64, data: &[u8]) -> Result<(), Status>;
    /// The job ledger.
    fn jobs(&mut self) -> &mut Jobs;
}

/// One calling process, as these calls see it.
pub struct Ctx<'a, M: UserMem> {
    pub objs: &'a mut Kobjs,
    pub handles: &'a mut HandleTable,
    /// The job the process's new objects are charged to.
    pub job: JobId,
    pub mem: &'a mut M,
    /// The negative-control kernel `fault-skip-forced-touch`: a replaced handle is not reported to the wait table.
    pub skip_forced_touch: bool,
}

fn fail(s: Status) -> Reply {
    Err((s, None, None))
}

/// Handles are 32 bits wide everywhere (SYSCALLS.md): a handle or 32-bit argument with high bits set names nothing and grants nothing,
/// instead of being silently truncated. `Some(status)`: the call is refused before it runs.
#[allow(clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and `over(i)` is called with constants below 6
pub fn wide_argument_error(n: usize, a: &[u64; 6]) -> Option<Status> {
    let over = |i: usize| a[i] > u64::from(u32::MAX);
    match n {
        nr::HANDLE_CLOSE | nr::OBJECT_SIGNALS | nr::OBJECT_INFO | nr::CHANNEL_WRITE if over(0) => {
            Some(Status::BadHandle)
        }
        nr::CHANNEL_READ | nr::CHANNEL_READ_TXID if over(0) => Some(Status::BadHandle),
        nr::VMO_READ | nr::VMO_WRITE | nr::VMO_GET_SIZE | nr::VMO_PHYS_ADDR | nr::VMO_COMMIT
            if over(0) =>
        {
            Some(Status::BadHandle)
        }
        nr::VMO_CREATE_PHYSICAL
        | nr::VMO_CREATE_DMA
        | nr::RESOURCE_DTB
        | nr::INTERRUPT_CREATE
        | nr::INTERRUPT_ACK
            if over(0) =>
        {
            Some(Status::BadHandle)
        }
        nr::VMO_CREATE if over(1) => Some(Status::BadHandle),
        nr::SOCKET_WRITE
        | nr::SOCKET_READ
        | nr::SOCKET_SHUTDOWN
        | nr::PORT_UNBIND
        | nr::PORT_REARM
            if over(0) =>
        {
            Some(Status::BadHandle)
        }
        nr::PORT_BIND if over(0) || over(1) => Some(Status::BadHandle),
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE if over(0) => Some(Status::BadHandle),
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE if over(1) => Some(Status::AccessDenied),
        nr::EVENT_SIGNAL if over(0) => Some(Status::BadHandle),
        nr::EVENT_SIGNAL if over(1) || over(2) => Some(Status::InvalidArgs),
        nr::CHANNEL_READ_TXID if over(1) => Some(Status::InvalidArgs),
        _ => None,
    }
}

fn handle_list(args: &[u8]) -> Vec<Handle> {
    args.as_chunks::<4>()
        .0
        .iter()
        .map(|c| Handle(u32::from_le_bytes(*c)))
        .collect()
}

/// The calls on handles, events, channels, sockets and ports. `None`: `n` is not one of them (the caller handles it). The width of
/// 32-bit arguments is checked by the caller with [`wide_argument_error`].
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and every `h(i)` is called with a constant below 6; sizes were bounded by the limits checked first (nbytes <= MAX_MSG_BYTES, nhandles <= MAX_MSG_HANDLES, so nhandles * 4 is small)
pub fn ipc_call<M: UserMem>(n: usize, a: [u64; 6], c: &mut Ctx<'_, M>) -> Option<Reply> {
    let Ctx {
        objs,
        handles,
        job,
        mem,
        skip_forced_touch,
    } = c;
    let (job, skip_forced_touch) = (*job, *skip_forced_touch);
    let h = |i: usize| Handle(a[i] as u32);
    let r: Reply = (|| match n {
        nr::HANDLE_CLOSE => handles
            .close(objs, h(0))
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::HANDLE_DUPLICATE => {
            let nh = handles
                .duplicate(objs, h(0), Rights(a[1] as u32))
                .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(nh.0)), None))
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
            Ok((Some(u64::from(nh.0)), None))
        }
        nr::CHANNEL_CREATE => {
            let (x, y) = ipc::channel_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs: mem.jobs(),
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(x.0)), Some(u64::from(y.0))))
        }
        nr::SOCKET_CREATE => {
            let (x, y) = ipc::socket_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs: mem.jobs(),
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(x.0)), Some(u64::from(y.0))))
        }
        nr::SOCKET_WRITE => {
            let len = a[2] as usize;
            ipc::socket_write_check(objs, handles, h(0), len).map_err(|e| (e, None, None))?;
            let data = mem.copy_in(a[1], len).map_err(|e| (e, None, None))?;
            ipc::socket_write_in(objs, handles, h(0), &data, mem.jobs())
                .map(|n| (Some(n as u64), None))
                .map_err(|e| (e, None, None))
        }
        nr::SOCKET_READ => {
            let max = a[2] as usize;
            // What a read would deliver, checked against the destination before anything is
            // removed, so a bad pointer never loses data.
            let n = ipc::socket_peek(objs, handles, h(0), max).map_err(|e| (e, None, None))?;
            mem.verify_writable(a[1], n).map_err(|e| (e, None, None))?;
            let data = ipc::socket_read_in(objs, handles, h(0), max, mem.jobs())
                .map_err(|e| (e, None, None))?;
            mem.copy_out_verified(a[1], &data)
                .map_err(|e| (e, None, None))?;
            Ok((Some(data.len() as u64), None))
        }
        nr::SOCKET_SHUTDOWN => ipc::socket_shutdown(objs, handles, h(0))
            .map(|()| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_CREATE => {
            let port = ipc::port_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs: mem.jobs(),
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(port.0)), None))
        }
        nr::PORT_BIND => {
            ipc::port_bind_flags_in(objs, handles, h(0), h(1), a[2], a[3], a[4], mem.jobs())
                .map(|_| (None, None))
                .map_err(|e| (e, None, None))
        }
        nr::PORT_REARM => ipc::port_rearm_in(objs, handles, h(0), a[1], mem.jobs())
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_UNBIND => ipc::port_unbind_in(objs, handles, h(0), a[1], mem.jobs())
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::EVENT_CREATE => {
            let e = ipc::event_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs: mem.jobs(),
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(e.0)), None))
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
            let data = mem.copy_in(a[1], nbytes).map_err(|e| (e, None, None))?;
            let raw = mem
                .copy_in(a[3], nhandles * 4)
                .map_err(|e| (e, None, None))?;
            ipc::channel_write_in(objs, handles, h(0), &data, &handle_list(&raw), mem.jobs())
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
            // Find out what this read would deliver, and check the destinations against the
            // *actual* message (not the declared capacity) before dequeuing anything, so a
            // bad pointer can never lose a message and a generous `max_bytes` needs no
            // generous mapping.
            let (nbytes, nhandles) = match ipc::channel_peek(objs, handles, h(0), txid, mb, mh) {
                Ok(sizes) => sizes,
                Err(ReadError::TooSmall { bytes, handles }) => {
                    return Err((
                        Status::BufferTooSmall,
                        Some(bytes as u64),
                        Some(handles as u64),
                    ))
                }
                Err(ReadError::Status(st)) => return fail(st),
            };
            mem.verify_writable(bytes_at, nbytes)
                .map_err(|e| (e, None, None))?;
            mem.verify_writable(handles_at, nhandles * 4)
                .map_err(|e| (e, None, None))?;
            let got = match txid {
                None => ipc::channel_read_in(objs, handles, h(0), mb, mh, mem.jobs()),
                Some(t) => ipc::channel_read_txid_in(objs, handles, h(0), t, mb, mh, mem.jobs()),
            };
            match got {
                Ok(r) => {
                    let raw: Vec<u8> = r.handles.iter().flat_map(|h| h.0.to_le_bytes()).collect();
                    // The destinations were verified: these writes allocate nothing (T10).
                    mem.copy_out_verified(bytes_at, &r.data)
                        .map_err(|e| (e, None, None))?;
                    mem.copy_out_verified(handles_at, &raw)
                        .map_err(|e| (e, None, None))?;
                    Ok((Some(r.data.len() as u64), Some(r.handles.len() as u64)))
                }
                Err(ReadError::TooSmall { bytes, handles }) => Err((
                    Status::BufferTooSmall,
                    Some(bytes as u64),
                    Some(handles as u64),
                )),
                Err(ReadError::Status(st)) => Err((st, None, None)),
            }
        }
        _ => Err((Status::Ok, None, None)), // (the marker for "not one of these": see below)
    })();
    if matches!(r, Err((Status::Ok, None, None))) {
        return None;
    }
    Some(r)
}
