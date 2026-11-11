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
use rt::spawn::{self, SpawnError};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

/// Request limits (S4-SPEC section 6): handles = image VMO + at most the startup message's 32;
/// bytes = the 28-byte header plus at most a 16 KiB startup message.
const MAX_REQ_BYTES: usize = launch::HEADER + proto::startup::MAX_BYTES;
const MAX_REQ_HANDLES: usize = launch::MAX_HANDLES;

fn reply(ch: u32, txid: u32, status: i32, handle: Option<u32>) -> bool {
    let bytes = launch::encode_reply(txid, status);
    let hs = [handle.unwrap_or(0)];
    let n = usize::from(handle.is_some());
    let r = sys::channel_write(ch, bytes.as_ptr(), bytes.len(), hs.as_ptr(), n);
    r.status == 0
}

fn close_all(hs: &[u32]) {
    for &h in hs {
        sys::handle_close(h);
    }
}

/// Handle one request; returns false when the client is gone or violated the protocol.
fn serve_one(ch: u32) -> bool {
    let mut bytes = alloc::vec![0u8; MAX_REQ_BYTES];
    let mut handles = [0u32; MAX_REQ_HANDLES];
    // Exact buffers: a message that does not fit is a protocol violation, not something to retry
    // (it would sit at the head of the queue for ever).
    let r = sys::channel_read(
        ch,
        bytes.as_mut_ptr(),
        bytes.len(),
        handles.as_mut_ptr(),
        handles.len(),
    );
    match r.status() {
        Some(abi::status::Status::Ok) => {}
        Some(abi::status::Status::ShouldWait) => {
            let w = sys::object_wait(
                ch,
                abi::signals::READABLE | abi::signals::PEER_CLOSED,
                u64::MAX,
            );
            return w.status == 0;
        }
        _ => return false, // PEER_CLOSED, BUFFER_TOO_SMALL (violation) or an error
    }
    let (nb, nh) = (r.a1, r.a2);
    let hs = &handles[..nh];
    let txid = proto::file::peek_txid(&bytes[..nb]).unwrap_or(0);
    let req = match launch::decode_spawn(&bytes[..nb]) {
        Ok(q) => q,
        Err(e) => {
            close_all(hs);
            let st = match e {
                LaunchError::BadImage | LaunchError::BadStartup => status::BAD_REQUEST,
                _ => status::BAD_REQUEST,
            };
            return reply(ch, txid, st, None);
        }
    };
    // Element 0 is the image VMO; the rest are forwarded and must all be named by the
    // startup message (an unnamed handle would be authority handed over by mistake).
    if hs.is_empty() || Startup::decode(req.startup, nh - 1).is_err() {
        close_all(hs);
        return reply(ch, txid, status::BAD_REQUEST, None);
    }
    let image_vmo = hs[0];
    let forwarded = &hs[1..];
    let Some(image) = spawn::read_image(image_vmo, req.image_offset, req.image_len) else {
        close_all(hs);
        return reply(ch, txid, status::BAD_REQUEST, None);
    };
    sys::handle_close(image_vmo);
    let result = spawn::spawn(&image, req.startup, forwarded);
    drop(image);
    match result {
        Ok(proc) => {
            let ok = reply(ch, txid, 0, Some(proc));
            if !ok {
                sys::handle_close(proc);
            }
            ok
        }
        Err(e) => {
            // (`spawn` consumed the forwarded handles)
            let st = match e {
                SpawnError::BadElf => status::BAD_ELF,
                SpawnError::TooBig => abi::status::Status::TooBig.code(),
                SpawnError::Sys(s) => s,
            };
            reply(ch, txid, st, None)
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
    while serve_one(ch) {}
    0
}
