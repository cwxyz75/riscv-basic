//! The client of `/svc/clock` (FS-SPEC section 7): the wall time in UTC nanoseconds and how far to trust it.
extern crate alloc;

use crate::fs::Fs;
use crate::sys;
use abi::signals;
use abi::status::Status;
use alloc::vec::Vec;
use proto::clock as pc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    Sys(i32),
    TimedOut,
    Closed,
    Garbled,
}

/// What the service says the time is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub nanos: u64,
    /// One of `proto::clock::quality`.
    pub quality: u8,
}

pub struct Clock {
    ch: u64,
    txid: u32,
}

impl Clock {
    /// Open `clock` in `svc` (a view or the registry root), retrying while the registry says the service is not there yet,
    /// until `give_up` (ticks).
    pub fn connect(svc: &mut Fs, give_up: u64) -> Result<Clock, ClockError> {
        let mut wait = 50;
        loop {
            match svc.open(b"clock", 0, proto::file::rights::READ) {
                Ok(f) => return Ok(Clock { ch: f.ch, txid: 0 }),
                Err(_) if sys::clock_get() + wait < give_up => {
                    crate::io::nap(wait);
                    wait = (wait * 2).min(500);
                }
                Err(e) => return Err(ClockError::Sys(e)),
            }
        }
    }

    /// The channel (for a test that sends a malformed request).
    pub fn channel(&self) -> u64 {
        self.ch
    }

    pub fn new(ch: u64) -> Clock {
        Clock { ch, txid: 0 }
    }

    /// Ask for the time, until `deadline` (an absolute tick).
    pub fn now(&mut self, deadline: u64) -> Result<Now, ClockError> {
        self.txid = self.txid.wrapping_add(1);
        let req = pc::encode_now(self.txid);
        let w = sys::channel_write(self.ch, req.as_ptr(), req.len(), core::ptr::null(), 0);
        if w.status != 0 {
            return Err(if w.status == Status::PeerClosed.code() {
                ClockError::Closed
            } else {
                ClockError::Sys(w.status)
            });
        }
        loop {
            let wait =
                sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
            if wait.status != 0 {
                return Err(ClockError::TimedOut);
            }
            let mut buf: Vec<u8> = alloc::vec![0u8; pc::REPLY_LEN];
            let rd = sys::channel_read(
                self.ch,
                buf.as_mut_ptr(),
                buf.len(),
                core::ptr::null_mut(),
                0,
            );
            if rd.status == Status::ShouldWait.code() {
                if wait.a1 as u32 & signals::PEER_CLOSED != 0 {
                    return Err(ClockError::Closed);
                }
                continue;
            }
            if rd.status != 0 {
                return Err(ClockError::Closed);
            }
            let Some((t, st, nanos, quality)) = pc::decode_reply(&buf[..rd.a1]) else {
                return Err(ClockError::Garbled);
            };
            if t != self.txid {
                continue;
            }
            return if st == pc::status::OK {
                Ok(Now { nanos, quality })
            } else {
                Err(ClockError::Garbled)
            };
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        sys::handle_close(self.ch);
    }
}
