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
mod job;
mod lifecycle;
mod proc;
mod state;
mod thread;
mod vmo;
mod wait;
pub use create::process_create;
pub use job::{job_call, job_status};
pub use lifecycle::{
    job_kill_victims, job_set_sched, kill_target, stop_target, thread_set_sched_target,
    thread_target,
};
pub use proc::{proc_call, Machine, PCtx, ProcState, Process, SpawnStack};
pub use state::{ThreadStart, UserState};
pub use thread::{
    charge_thread, clamp_prio, credit_thread, new_start, process_start, thread_spawn,
    total_threads, Spawn, KSTACK_PAGES,
};
pub use vmo::vmo_call;
pub use wait::{
    channel_call_ready, channel_call_reply, channel_call_send, futex_wait_check, futex_wait_report,
    futex_wake, object_wait_check, object_wait_report, port_poll, port_poll_many, port_wait_check,
    port_wait_many_check, port_wait_many_report, port_wait_report, signals_of, Call, FutexCheck,
    ObjectWait,
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
use kcore::job::JobId;
use kcore::pagetable::PhysMem;
use kcore::usercopy::{copy_from_user, copy_to_user_verified, verify_writable};
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
    /// The job the process's new objects are charged to (the ledger is `vmos.jobs`).
    pub job: JobId,
    pub aspace: &'a mut AddressSpace,
    pub mm: &'a mut Mm<M>,
    pub vmos: &'a mut Vmos,
    pub faults: Faults,
    /// The processors that are online (`job_info` 7).
    pub harts_online: u64,
    /// The device windows a physical VMO may name (page-aligned `(start, end)` byte ranges, built from the device tree at boot: PL5).
    pub windows: &'a [(u64, u64)],
}

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
        faults,
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
            .close(objs, h(0))
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
                    jobs: &mut vmos.jobs,
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
                    jobs: &mut vmos.jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(x.0), Some(y.0)))
        }
        nr::SOCKET_WRITE => {
            let len = a[2] as usize;
            ipc::socket_write_check(objs, handles, h(0), len).map_err(|e| (e, None, None))?;
            let data = copy_from_user(aspace, mm, vmos, a[1], len).map_err(|e| (e, None, None))?;
            ipc::socket_write_in(objs, handles, h(0), &data, &mut vmos.jobs)
                .map(|n| (Some(n as u64), None))
                .map_err(|e| (e, None, None))
        }
        nr::SOCKET_READ => {
            let max = a[2] as usize;
            // What a read would deliver, checked against the destination before anything is
            // removed, so a bad pointer never loses data.
            let n = ipc::socket_peek(objs, handles, h(0), max).map_err(|e| (e, None, None))?;
            verify_writable(aspace, mm, vmos, a[1], n).map_err(|e| (e, None, None))?;
            let data = ipc::socket_read_in(objs, handles, h(0), max, &mut vmos.jobs)
                .map_err(|e| (e, None, None))?;
            copy_to_user_verified(aspace, mm, a[1], &data).map_err(|e| (e, None, None))?;
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
                    jobs: &mut vmos.jobs,
                    job: Some(job),
                },
            )
            .map_err(|e| (e, None, None))?;
            Ok((Some(port.0), None))
        }
        nr::PORT_BIND => {
            ipc::port_bind_flags_in(objs, handles, h(0), h(1), a[2], a[3], a[4], &mut vmos.jobs)
                .map(|_| (None, None))
                .map_err(|e| (e, None, None))
        }
        nr::PORT_REARM => ipc::port_rearm_in(objs, handles, h(0), a[1], &mut vmos.jobs)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_UNBIND => ipc::port_unbind_in(objs, handles, h(0), a[1], &mut vmos.jobs)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::EVENT_CREATE => {
            let e = ipc::event_create_in(
                objs,
                handles,
                &mut ipc::Quota {
                    jobs: &mut vmos.jobs,
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
            let data =
                copy_from_user(aspace, mm, vmos, a[1], nbytes).map_err(|e| (e, None, None))?;
            let raw = copy_from_user(aspace, mm, vmos, a[3], nhandles * 8)
                .map_err(|e| (e, None, None))?;
            ipc::channel_write_in(
                objs,
                handles,
                h(0),
                &data,
                &handle_list(&raw).map_err(|e| (e, None, None))?,
                &mut vmos.jobs,
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
            verify_writable(aspace, mm, vmos, bytes_at, nbytes).map_err(|e| (e, None, None))?;
            verify_writable(aspace, mm, vmos, handles_at, nhandles * 8)
                .map_err(|e| (e, None, None))?;
            let got = match txid {
                None => ipc::channel_read_in(objs, handles, h(0), mb, mh, &mut vmos.jobs),
                Some(t) => {
                    ipc::channel_read_txid_in(objs, handles, h(0), t, mb, mh, &mut vmos.jobs)
                }
            };
            match got {
                Ok(r) => {
                    // The destinations were verified: these writes allocate nothing (T10), and the handle numbers go out one at a time from the
                    // stack, so nothing here can fail for lack of memory after the message has been taken (KT9).
                    copy_to_user_verified(aspace, mm, bytes_at, &r.data)
                        .map_err(|e| (e, None, None))?;
                    for (i, h) in r.handles.iter().enumerate() {
                        copy_to_user_verified(
                            aspace,
                            mm,
                            handles_at + (i as u64) * 8,
                            &h.0.to_le_bytes(),
                        )
                        .map_err(|e| (e, None, None))?;
                    }
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
        // (not reached: the guard above returned `None` for every call that is not one of these)
        _ => fail(Status::BadSyscall),
    })();
    Some(r)
}
