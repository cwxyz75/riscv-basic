//! The client side of the launcher protocol (S4-SPEC section 6): ask the launcher service to start
//! a program that lives in a VMO.

extern crate alloc;

use crate::sys;
use abi::rights::Rights;
use abi::signals;
use alloc::vec::Vec;
use proto::launch;
use proto::startup::Startup;

/// How long to wait for the launcher's answer (ticks).
const ANSWER_TIMEOUT: u64 = 30_000;

/// Start the ELF at `offset`..`offset + len` of `image_vmo` with `startup`, forwarding `forward`
/// (the handles `startup` refers to, which move to the launcher). `txid` identifies the request.
/// Returns the process handle (rights: WAIT, INSPECT, KILL, DUPLICATE, TRANSFER), or the
/// launcher's status (negative; a kernel status or a `proto::file::status` value).
pub fn spawn_via(
    ch: u32,
    image_vmo: u32,
    offset: u64,
    len: u64,
    startup: &Startup,
    forward: &[u32],
    txid: u32,
) -> Result<u32, i32> {
    let startup_bytes = startup.encode().map_err(|_| -4)?;
    let req = launch::Spawn {
        txid,
        image_offset: offset,
        image_len: len,
        startup: &startup_bytes,
    }
    .encode()
    .map_err(|_| -4)?;
    // The launcher needs its own handle to the image (ours stays ours).
    let d = sys::handle_duplicate(image_vmo, Rights::SAME.0);
    if d.status != 0 {
        return Err(d.status);
    }
    let mut hs: Vec<u32> = Vec::with_capacity(1 + forward.len());
    hs.push(d.a1 as u32);
    hs.extend_from_slice(forward);
    let w = sys::channel_write(ch, req.as_ptr(), req.len(), hs.as_ptr(), hs.len());
    if w.status != 0 {
        sys::handle_close(d.a1 as u32);
        return Err(w.status);
    }
    let wait = sys::object_wait(
        ch,
        signals::READABLE | signals::PEER_CLOSED,
        sys::clock_get() + ANSWER_TIMEOUT,
    );
    if wait.status != 0 {
        return Err(wait.status);
    }
    let mut buf = [0u8; 16];
    let mut h = [0u32; 1];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), h.as_mut_ptr(), h.len());
    if r.status != 0 {
        return Err(r.status);
    }
    match launch::decode_reply(&buf[..r.a1]) {
        Some((id, 0)) if id == txid && r.a2 == 1 => Ok(h[0]),
        Some((id, status)) if id == txid && status != 0 => {
            for &x in &h[..r.a2] {
                sys::handle_close(x);
            }
            Err(status)
        }
        _ => {
            for &x in &h[..r.a2] {
                sys::handle_close(x);
            }
            Err(-4)
        }
    }
}
