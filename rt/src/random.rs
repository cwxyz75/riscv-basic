//! The client of `/svc/random` (S9 step 5): `fill` bytes from the system's generator.
extern crate alloc;

use crate::fs::Fs;
use crate::sys;
use abi::signals;
use abi::status::Status;
use alloc::vec::Vec;
use proto::random as pr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomError {
    Sys(i32),
    /// The generator is not seeded (no entropy device): there are no bytes to give, and none are made up.
    NotSeeded,
    TimedOut,
    Closed,
    Garbled,
}

pub struct Random {
    ch: u64,
    txid: u32,
}

impl Random {
    /// Open `random` in `svc` (a view or the registry root), retrying while the registry says the service is gone or not
    /// there yet, until `give_up` (ticks).
    pub fn connect(svc: &mut Fs, give_up: u64) -> Result<Random, RandomError> {
        let mut wait = 50;
        loop {
            match svc.open(b"random", 0, proto::file::rights::READ) {
                Ok(f) => return Ok(Random { ch: f.ch, txid: 0 }),
                Err(e) if sys::clock_get() + wait < give_up => {
                    let _ = e;
                    crate::io::nap(wait);
                    wait = (wait * 2).min(500);
                }
                Err(e) => return Err(RandomError::Sys(e)),
            }
        }
    }

    pub fn new(ch: u64) -> Random {
        Random { ch, txid: 0 }
    }

    /// Fill `out` (any length) with random bytes, until `deadline` (an absolute tick).
    pub fn fill(&mut self, out: &mut [u8], deadline: u64) -> Result<(), RandomError> {
        for chunk in out.chunks_mut(pr::MAX_READ) {
            self.txid = self.txid.wrapping_add(1);
            let req = pr::encode_read(self.txid, chunk.len() as u16);
            let w = sys::channel_write(self.ch, req.as_ptr(), req.len(), core::ptr::null(), 0);
            if w.status != 0 {
                return Err(if w.status == Status::PeerClosed.code() {
                    RandomError::Closed
                } else {
                    RandomError::Sys(w.status)
                });
            }
            loop {
                let wait =
                    sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
                if wait.status != 0 {
                    return Err(RandomError::TimedOut);
                }
                let mut buf: Vec<u8> = alloc::vec![0u8; 8 + pr::MAX_READ];
                let rd = sys::channel_read(
                    self.ch,
                    buf.as_mut_ptr(),
                    buf.len(),
                    core::ptr::null_mut(),
                    0,
                );
                if rd.status == Status::ShouldWait.code() {
                    if wait.a1 as u32 & signals::PEER_CLOSED != 0 {
                        return Err(RandomError::Closed);
                    }
                    continue;
                }
                if rd.status != 0 {
                    return Err(RandomError::Closed);
                }
                let Some((t, st, body)) = pr::decode_reply(&buf[..rd.a1]) else {
                    return Err(RandomError::Garbled);
                };
                if t != self.txid {
                    continue;
                }
                match st {
                    pr::status::OK if body.len() == chunk.len() => {
                        chunk.copy_from_slice(body);
                        break;
                    }
                    pr::status::NOT_SEEDED => return Err(RandomError::NotSeeded),
                    _ => return Err(RandomError::Garbled),
                }
            }
        }
        Ok(())
    }
}

impl Drop for Random {
    fn drop(&mut self) {
        sys::handle_close(self.ch);
    }
}
