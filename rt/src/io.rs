//! Blocking helpers over sockets (S4-SPEC section 9): `read`/`write_all` that wait on the
//! right signal. Stdio waits have no deadline (a slow pipe reader must not break a
//! pipeline); only `NO_MEMORY` from the kernel's shared IPC budget is retried for a bounded
//! time and then returned.

use crate::sys;
use abi::limits::SOCKET_BUFFER;
use abi::signals;
use abi::status::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// The other end is gone (writing), or the stream is broken.
    PeerClosed,
    /// The shared IPC budget stayed exhausted for a full second.
    NoMemory,
    Other(i32),
}

/// Wait for `mask` on `h`. A wait that *fails* (a handle without WAIT, a bad handle) must end
/// the operation with that error: ignoring it would turn the caller's loop into a busy spin.
fn wait(h: u32, mask: u32, deadline: u64) -> Result<(), IoError> {
    let r = sys::object_wait(h, mask, deadline);
    match r.status() {
        Some(Status::Ok) | Some(Status::TimedOut) => Ok(()),
        _ => Err(IoError::Other(r.status)),
    }
}

/// Read up to `buf.len()` bytes; `Ok(0)` is end of stream (the peer closed after the data
/// drained). Blocks until at least one byte or the end.
pub fn read(h: u32, buf: &mut [u8]) -> Result<usize, IoError> {
    if buf.is_empty() {
        return Ok(0);
    }
    loop {
        let r = sys::socket_read(h, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => return Ok(r.a1),
            Some(Status::PeerClosed) => return Ok(0),
            Some(Status::ShouldWait) => {
                wait(h, signals::READABLE | signals::PEER_CLOSED, u64::MAX)?
            }
            _ => return Err(IoError::Other(r.status)),
        }
    }
}

/// Write all of `data`, waiting for room as needed.
pub fn write_all(h: u32, mut data: &[u8]) -> Result<(), IoError> {
    let mut nomem_since: Option<u64> = None;
    while !data.is_empty() {
        // The kernel refuses a single write larger than one socket buffer (TOO_BIG).
        let n = data.len().min(SOCKET_BUFFER);
        let r = sys::socket_write(h, data.as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => {
                nomem_since = None;
                data = &data[r.a1..];
            }
            Some(Status::PeerClosed) => return Err(IoError::PeerClosed),
            Some(Status::ShouldWait) => {
                wait(h, signals::WRITABLE | signals::PEER_CLOSED, u64::MAX)?
            }
            Some(Status::NoMemory) => {
                let now = sys::clock_get();
                let since = *nomem_since.get_or_insert(now);
                if now.saturating_sub(since) >= 1000 {
                    return Err(IoError::NoMemory);
                }
                nap(10);
            }
            _ => return Err(IoError::Other(r.status)),
        }
    }
    Ok(())
}

/// Sleep about `ticks` milliseconds (a timed wait on an event nobody signals).
pub fn nap(ticks: u64) {
    let e = sys::event_create();
    if e.status == 0 {
        let h = e.a1 as u32;
        sys::object_wait(h, signals::USER_MASK, sys::clock_get() + ticks);
        sys::handle_close(h);
    }
}
