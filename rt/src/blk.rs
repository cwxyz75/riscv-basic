//! A client of the block protocol (`proto::blk`) as an `fsfmt::Device`: a filesystem's 4 KiB blocks over one channel to a
//! block driver. Calls are synchronous (one request, wait for its reply). Any failure of the channel or the driver is an I/O
//! error: the filesystem above never retries (FS-SPEC section 3).

extern crate alloc;

use crate::sys;
use abi::signals;
use alloc::vec;
use alloc::vec::Vec;
use fsfmt::{DevError, Device, BLOCK};
use proto::blk::{self, Op, Status, SECTOR};

/// How long to wait for the driver to answer one request (ticks).
const REPLY_TIMEOUT: u64 = 10_000;
/// Sectors in one filesystem block.
const PER_BLOCK: u32 = (BLOCK / SECTOR) as u32;

pub struct BlkDevice {
    ch: u32,
    txid: u32,
    blocks: u64,
    read_only: bool,
}

impl BlkDevice {
    /// Ask the driver on `ch` for its size and take the device over (the channel is the caller's no longer).
    pub fn open(ch: u32) -> Option<BlkDevice> {
        let mut d = BlkDevice {
            ch,
            txid: 0,
            blocks: 0,
            read_only: false,
        };
        let (st, body) = d.call(Op::Info).ok()?;
        if st != Status::Ok {
            return None;
        }
        let info = blk::decode_info(&body)?;
        if info.sector_size as usize != SECTOR {
            return None;
        }
        d.blocks = info.sectors / u64::from(PER_BLOCK);
        d.read_only = info.read_only;
        Some(d)
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The channel, to hand the device back (or close it).
    pub fn channel(&self) -> u32 {
        self.ch
    }

    fn call(&mut self, op: Op<'_>) -> Result<(Status, Vec<u8>), DevError> {
        self.txid = self.txid.wrapping_add(1);
        let txid = self.txid;
        let req = blk::Request { txid, op }.encode();
        if sys::channel_write(self.ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
            return Err(DevError::Io);
        }
        let deadline = sys::clock_get() + REPLY_TIMEOUT;
        loop {
            let w = sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
            if w.status != 0 {
                return Err(DevError::Io);
            }
            let mut buf = vec![0u8; blk::REPLY_HEADER + blk::MAX_IO_SECTORS as usize * SECTOR];
            let r = sys::channel_read(
                self.ch,
                buf.as_mut_ptr(),
                buf.len(),
                core::ptr::null_mut(),
                0,
            );
            if r.status != 0 {
                // nothing to read: the peer closed, or the wait was for the close
                return Err(DevError::Io);
            }
            buf.truncate(r.a1);
            let Some((t, status, body)) = blk::decode_reply(&buf) else {
                return Err(DevError::Io);
            };
            if t != txid {
                continue; // a late answer to an earlier request: not ours
            }
            return Ok((status, body.to_vec()));
        }
    }
}

impl Device for BlkDevice {
    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn read(&mut self, addr: u64, buf: &mut [u8; BLOCK]) -> Result<(), DevError> {
        if addr >= self.blocks {
            return Err(DevError::Range);
        }
        let (st, body) = self.call(Op::Read {
            sector: addr * u64::from(PER_BLOCK),
            count: PER_BLOCK,
        })?;
        if st != Status::Ok || body.len() != BLOCK {
            return Err(DevError::Io);
        }
        buf.copy_from_slice(&body);
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8; BLOCK]) -> Result<(), DevError> {
        if addr >= self.blocks {
            return Err(DevError::Range);
        }
        let (st, _) = self.call(Op::Write {
            sector: addr * u64::from(PER_BLOCK),
            count: PER_BLOCK,
            data,
        })?;
        if st == Status::Ok {
            Ok(())
        } else {
            Err(DevError::Io)
        }
    }

    fn flush(&mut self) -> Result<(), DevError> {
        let (st, _) = self.call(Op::Flush)?;
        if st == Status::Ok {
            Ok(())
        } else {
            Err(DevError::Io)
        }
    }
}
