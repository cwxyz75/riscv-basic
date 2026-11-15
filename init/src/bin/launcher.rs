//! The launcher service (S4b, S4-SPEC section 6): builds and starts processes for its client. It
//! holds no filesystem authority; the caller supplies the program as a VMO extent. One client
//! channel (role `SERVICE_LAUNCH`); one request at a time.
#![no_std]
#![no_main]

extern crate alloc;

use proto::file::status;
use proto::launch::{self, LaunchError};
use proto::startup::{Role, Startup};
use rt::abi;
use rt::service::{self, Core, Service};
use rt::spawn::{self, SpawnError};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

/// What a program started by the launcher may use (the job made for it, S4-SPEC section 6): it
/// is the only process, it may run a few threads, and its memory (image, stack, heap) is capped.
const PROGRAM_PROCESSES: u64 = 1;
const PROGRAM_THREADS: u64 = 4;
// 136 MiB: the runtime heap cap (128 MiB) plus the image, stacks and page tables, so the heap
// refuses first and exhaustion is a failed allocation, not a fault
const PROGRAM_PAGES: u64 = 34816;
/// Kernel objects the program may hold alive (handles are bounded separately), and bytes it may
/// leave queued in the channels and sockets it creates.
const PROGRAM_OBJECTS: u64 = 256;
/// The highest base priority a program may give its own threads (the normal band's upper part).
const PROGRAM_MAX_PRIO: u8 = 20;
const PROGRAM_IPC_BYTES: u64 = 192 * 1024;

/// Request limits (S4-SPEC section 6): handles = image VMO + at most the startup message's 32;
/// bytes = the 28-byte header plus at most a 16 KiB startup message.
const MAX_REQ_BYTES: usize = launch::HEADER + proto::startup::MAX_BYTES;
const MAX_REQ_HANDLES: usize = launch::MAX_HANDLES;

fn close_all(hs: &[u32]) {
    for &h in hs {
        sys::handle_close(h);
    }
}

/// The service's total pool: the one client channel (a few spare slots), nothing held for long.
const POOL: svc::Limits = svc::Limits {
    conns: 4,
    held_bytes: 64 * 1024,
    inflight: 4,
};
/// A reply the client has no room for is kept this long (ticks) before the connection is dropped.
const HOLD_TIMEOUT: u64 = 30_000;

struct Launcher {
    core: Core<()>,
    jobs: Option<u32>,
}

impl Service for Launcher {
    type Conn = ();

    fn core(&mut self) -> &mut Core<()> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        self.spawn_for(key, msg, hs);
    }

    fn closed(&mut self, _conn: ()) {}
}

impl Launcher {
    fn reply(&mut self, key: u64, txid: u32, status: i32, handle: Option<u32>) {
        let _ = self
            .core
            .send(key, launch::encode_reply(txid, status), handle);
    }

    /// Handle one request. The handles are ours to close or forward; a malformed request is answered with
    /// BAD_REQUEST (the runtime already ended a connection whose message did not even fit the buffers).
    fn spawn_for(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        let txid = proto::file::peek_txid(msg).unwrap_or(0);
        let req = match launch::decode_spawn(msg) {
            Ok(q) => q,
            Err(e) => {
                close_all(hs);
                let st = match e {
                    LaunchError::BadImage | LaunchError::BadStartup => status::BAD_REQUEST,
                    _ => status::BAD_REQUEST,
                };
                return self.reply(key, txid, st, None);
            }
        };
        // Element 0 is the image VMO; the rest are forwarded and must all be named by the
        // startup message (an unnamed handle would be authority handed over by mistake).
        if hs.is_empty() || Startup::decode(req.startup, hs.len() - 1).is_err() {
            close_all(hs);
            return self.reply(key, txid, status::BAD_REQUEST, None);
        }
        let image_vmo = hs[0];
        let forwarded = &hs[1..];
        let Some(image) = spawn::read_image(image_vmo, req.image_offset, req.image_len) else {
            close_all(hs);
            return self.reply(key, txid, status::BAD_REQUEST, None);
        };
        sys::handle_close(image_vmo);
        // Every program gets a job of its own under the launcher's (S4.5): it can make no other
        // process, only a few threads, and a bounded amount of memory, whatever it does.
        let result = match self.jobs {
            None => spawn::spawn(&image, req.startup, forwarded),
            Some(parent) => {
                let j = sys::job_create(
                    parent,
                    &[
                        PROGRAM_PROCESSES,
                        PROGRAM_THREADS,
                        PROGRAM_PAGES,
                        0,
                        PROGRAM_OBJECTS,
                        PROGRAM_IPC_BYTES,
                    ],
                );
                if j.status != 0 {
                    close_all(forwarded);
                    drop(image);
                    return self.reply(key, txid, j.status, None);
                }
                let job = j.a1 as u32;
                // Policy (SCHED-SMP-SPEC 4.9): a program runs in the normal band and may raise itself only to 20, with no
                // real-time budget. (Without this every job inherits the root's 63.)
                let _ = sys::job_set_sched(job, PROGRAM_MAX_PRIO, 0, 1024);
                let r = spawn::spawn_in(&image, req.startup, forwarded, job);
                // (the process holds the job now; ours is not needed, and must not be kept: a
                // handle to its own job's MANAGE would let the launcher outlive nothing useful)
                sys::handle_close(job);
                r
            }
        };
        drop(image);
        match result {
            // (the runtime owns the process handle from here: it closes it if the reply cannot go)
            Ok(proc) => self.reply(key, txid, 0, Some(proc)),
            Err(e) => {
                // (`spawn` consumed the forwarded handles)
                let st = match e {
                    SpawnError::BadElf => status::BAD_ELF,
                    SpawnError::TooBig => abi::status::Status::TooBig.code(),
                    SpawnError::Sys(s) => s,
                };
                self.reply(key, txid, st, None)
            }
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let Some(ch) = env.handle(Role::ServiceLaunch) else {
        return 3;
    };
    let jobs = env.handle(Role::Job);
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut launcher = Launcher {
        core: Core::new(
            p.a1 as u32,
            POOL,
            MAX_REQ_BYTES,
            MAX_REQ_HANDLES,
            HOLD_TIMEOUT,
        ),
        jobs,
    };
    if launcher.core.add_conn(ch, svc::ROOT, ()).is_none() {
        return 9;
    }
    // (a violation by the client ends its connection, and with it the service: it has no other client)
    service::run(&mut launcher)
}
