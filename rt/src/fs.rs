//! A client of the file protocol (S4-SPEC section 7): one `Fs` is one connection to one node
//! (a directory or a file). Calls are synchronous: one request, wait for its reply.

extern crate alloc;

use crate::sys;
use abi::signals;
use alloc::vec::Vec;
use proto::file::{self, DirEntry, Reply, Request};

/// How long to wait for a reply (ticks).
const REPLY_TIMEOUT: u64 = 30_000;

/// What `stat` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub is_dir: bool,
    pub size: u64,
}

/// A reply with its payload and (for `GetVmo`) a handle.
#[derive(Debug)]
pub struct Answer {
    pub status: i32,
    pub a: u64,
    pub payload: Vec<u8>,
    pub handle: Option<u32>,
}

/// A reply with the transaction id it answers.
struct Tagged {
    txid: u32,
    answer: Answer,
}

pub struct Fs {
    pub ch: u32,
    txid: u32,
}

impl Fs {
    /// Wrap a connection (the channel end the server bound to a node).
    pub fn new(ch: u32) -> Fs {
        Fs { ch, txid: 0 }
    }

    /// Send one raw request (with an optional handle) and read its reply.
    #[allow(clippy::too_many_arguments)] // the wire format's fields, spelled out
    pub fn call(
        &mut self,
        op: u16,
        flags: u16,
        rights: u32,
        a: u64,
        b: u64,
        payload: &[u8],
        send: Option<u32>,
    ) -> Result<Answer, i32> {
        self.txid = self.txid.wrapping_add(1);
        let req = Request {
            txid: self.txid,
            op,
            flags,
            rights,
            a,
            b,
            payload,
        }
        .encode()
        .map_err(|_| abi::status::Status::TooBig.code())?;
        let hs = [send.unwrap_or(0)];
        let n = usize::from(send.is_some());
        let w = sys::channel_write(self.ch, req.as_ptr(), req.len(), hs.as_ptr(), n);
        if w.status != 0 {
            // a failed write moves nothing: the endpoint we meant to send is still ours
            if let Some(h) = send {
                sys::handle_close(h);
            }
            return Err(w.status);
        }
        self.read_reply()
    }

    /// Read the reply to the last request. A reply to an *earlier* request (one that arrived after
    /// its call had given up) is discarded, with any handle it carried, instead of being taken for
    /// this one's answer.
    pub fn read_reply(&mut self) -> Result<Answer, i32> {
        let deadline = sys::clock_get() + REPLY_TIMEOUT;
        loop {
            let a = self.read_reply_any(deadline)?;
            if a.txid == self.txid {
                return Ok(a.answer);
            }
            if let Some(h) = a.answer.handle {
                sys::handle_close(h);
            }
            if a.txid.wrapping_sub(self.txid) as i32 > 0 {
                return Err(-4); // from the future: the server is broken
            }
        }
    }

    /// Read the next reply on this connection whatever request it answers (for callers that
    /// pipeline several requests).
    pub fn read_next_reply(&mut self) -> Result<Answer, i32> {
        Ok(self
            .read_reply_any(sys::clock_get() + REPLY_TIMEOUT)?
            .answer)
    }

    fn read_reply_any(&mut self, deadline: u64) -> Result<Tagged, i32> {
        let wait = sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
        if wait.status != 0 {
            return Err(wait.status);
        }
        let mut buf = alloc::vec![0u8; file::MAX_MESSAGE];
        let mut h = [0u32; 1];
        let r = sys::channel_read(self.ch, buf.as_mut_ptr(), buf.len(), h.as_mut_ptr(), 1);
        if r.status != 0 {
            return Err(r.status);
        }
        let reply = file::decode_reply(&buf[..r.a1]).map_err(|_| -4)?;
        let Reply {
            txid,
            status,
            a,
            payload,
        } = reply;
        Ok(Tagged {
            txid,
            answer: Answer {
                status,
                a,
                payload: payload.to_vec(),
                handle: (r.a2 == 1).then_some(h[0]),
            },
        })
    }

    fn simple(&mut self, op: u16, a: u64, b: u64, payload: &[u8]) -> Result<Answer, i32> {
        let r = self.call(op, 0, 0, a, b, payload, None)?;
        if r.status != 0 {
            if let Some(h) = r.handle {
                sys::handle_close(h);
            }
            return Err(r.status);
        }
        Ok(r)
    }

    /// Open `path` (relative to this directory) with `flags` and `rights`: a new connection.
    pub fn open(&mut self, path: &[u8], flags: u16, rights: u32) -> Result<Fs, i32> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(c.status);
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        // the endpoint travels with the request; the server closes it if it refuses
        let r = self.call(file::op::OPEN, flags, rights, 0, 0, path, Some(theirs));
        match r {
            Ok(a) if a.status == 0 => Ok(Fs::new(mine)),
            Ok(a) => {
                sys::handle_close(mine);
                Err(a.status)
            }
            Err(e) => {
                sys::handle_close(mine);
                Err(e)
            }
        }
    }

    /// A new connection to the same node with equal or fewer `rights`.
    pub fn clone_conn(&mut self, rights: u32) -> Result<Fs, i32> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(c.status);
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        match self.call(file::op::CLONE, 0, rights, 0, 0, &[], Some(theirs)) {
            Ok(a) if a.status == 0 => Ok(Fs::new(mine)),
            Ok(a) => {
                sys::handle_close(mine);
                Err(a.status)
            }
            Err(e) => {
                sys::handle_close(mine);
                Err(e)
            }
        }
    }

    pub fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, i32> {
        Ok(self
            .simple(file::op::READ, offset, len as u64, &[])?
            .payload)
    }

    /// The whole file.
    pub fn read_all(&mut self) -> Result<Vec<u8>, i32> {
        let mut out = Vec::new();
        loop {
            let chunk = self.read(out.len() as u64, file::MAX_IO)?;
            if chunk.is_empty() {
                return Ok(out);
            }
            out.extend_from_slice(&chunk);
        }
    }

    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<usize, i32> {
        Ok(self.simple(file::op::WRITE, offset, 0, data)?.a as usize)
    }

    /// Write all of `data` from `offset`, in protocol-sized pieces.
    pub fn write_all(&mut self, mut offset: u64, mut data: &[u8]) -> Result<(), i32> {
        while !data.is_empty() {
            let n = data.len().min(file::MAX_IO);
            let w = self.write(offset, &data[..n])?;
            offset += w as u64;
            data = &data[w..];
        }
        Ok(())
    }

    pub fn size(&mut self) -> Result<u64, i32> {
        Ok(self.simple(file::op::GETSIZE, 0, 0, &[])?.a)
    }

    pub fn set_size(&mut self, size: u64) -> Result<(), i32> {
        self.simple(file::op::SETSIZE, size, 0, &[]).map(|_| ())
    }

    fn parse_stat(a: &Answer) -> Result<Stat, i32> {
        if a.payload.len() != 9 {
            return Err(-4);
        }
        Ok(Stat {
            is_dir: a.payload[0] == 2,
            size: a.a,
        })
    }

    /// Stat `path` relative to this directory (empty: the directory itself).
    pub fn stat(&mut self, path: &[u8]) -> Result<Stat, i32> {
        let a = self.simple(file::op::STAT, 0, 0, path)?;
        Self::parse_stat(&a)
    }

    pub fn fstat(&mut self) -> Result<Stat, i32> {
        let a = self.simple(file::op::FSTAT, 0, 0, &[])?;
        Self::parse_stat(&a)
    }

    pub fn mkdir(&mut self, name: &[u8]) -> Result<(), i32> {
        self.simple(file::op::MKDIR, 0, 0, name).map(|_| ())
    }

    pub fn unlink(&mut self, name: &[u8]) -> Result<(), i32> {
        self.simple(file::op::UNLINK, 0, 0, name).map(|_| ())
    }

    /// All entries of this directory, in name order (follows the paging cookie).
    pub fn read_dir(&mut self) -> Result<Vec<DirEntry>, i32> {
        let mut all: Vec<DirEntry> = Vec::new();
        loop {
            let cookie: Vec<u8> = all.last().map(|e| e.name.clone()).unwrap_or_default();
            let a = self.simple(file::op::READDIR, 0, 0, &cookie)?;
            let page = file::decode_entries(&a.payload).map_err(|_| -4)?;
            if page.is_empty() {
                return Ok(all);
            }
            all.extend(page);
        }
    }

    /// The boot image VMO (read-only) and the `(offset, length)` of this file in it.
    pub fn get_vmo(&mut self) -> Result<(u32, u64, u64), i32> {
        let a = self.call(file::op::GETVMO, 0, 0, 0, 0, &[], None)?;
        if a.status != 0 {
            if let Some(h) = a.handle {
                sys::handle_close(h);
            }
            return Err(a.status);
        }
        let h = a.handle.ok_or(-4)?;
        if a.payload.len() != 8 {
            sys::handle_close(h);
            return Err(-4);
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&a.payload);
        Ok((h, a.a, u64::from_le_bytes(len)))
    }

    pub fn close(self) {
        sys::handle_close(self.ch);
    }
}
