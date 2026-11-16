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
    /// Everything `STAT` said: the kind (including a symlink), time and the advisory POSIX attributes.
    pub info: file::StatInfo,
}

impl Stat {
    /// A `Stat` with no times or attributes (for a caller that has only a kind and a size).
    pub fn basic(is_dir: bool, size: u64) -> Stat {
        Stat {
            is_dir,
            size,
            info: file::StatInfo {
                kind: if is_dir { 2 } else { 1 },
                size,
                mtime: 0,
                mode: file::ABSENT,
                uid: file::ABSENT,
                gid: file::ABSENT,
            },
        }
    }
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

/// Why a path operation did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The walk met a symlink after consuming `consumed` components of the path (the link included); `target` is its text.
    Link {
        consumed: usize,
        target: Vec<u8>,
    },
    Status(i32),
}

impl Stopped {
    fn of(a: Answer) -> Stopped {
        if a.status == file::status::SYMLINK {
            Stopped::Link {
                consumed: a.a as usize,
                target: a.payload,
            }
        } else {
            Stopped::Status(a.status)
        }
    }
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

    /// Wrap a connection, numbering requests from `txid`: several short-lived views of one shared
    /// connection must not reuse numbers, or a late reply to one would pass for the answer to the next.
    pub fn new_at(ch: u32, txid: u32) -> Fs {
        Fs { ch, txid }
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

    /// `open`, retrying while the service behind a registry name is `UNAVAILABLE` (its server is down and about to be
    /// restarted), with growing naps, until `give_up` (an absolute tick). Any other error is returned at once.
    pub fn open_retry(
        &mut self,
        path: &[u8],
        flags: u16,
        rights: u32,
        give_up: u64,
    ) -> Result<Fs, i32> {
        let mut wait = 50;
        loop {
            match self.open(path, flags, rights) {
                Err(e) if e == file::status::UNAVAILABLE && sys::clock_get() + wait < give_up => {
                    crate::io::nap(wait);
                    wait = (wait * 2).min(500);
                }
                r => return r,
            }
        }
    }

    /// Open `path` (relative to this directory) with `flags` and `rights`: a new connection.
    pub fn open(&mut self, path: &[u8], flags: u16, rights: u32) -> Result<Fs, i32> {
        self.open_stop(path, flags, rights).map_err(|s| match s {
            Stopped::Link { .. } => file::status::SYMLINK,
            Stopped::Status(e) => e,
        })
    }

    /// `open`, but a symlink in the way is reported with what the server said about it (how many components it consumed, and the
    /// target text) instead of as a bare status: the caller follows it (`pathres`).
    pub fn open_stop(&mut self, path: &[u8], flags: u16, rights: u32) -> Result<Fs, Stopped> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(Stopped::Status(c.status));
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        // the endpoint travels with the request; the server closes it if it refuses
        let r = self.call(file::op::OPEN, flags, rights, 0, 0, path, Some(theirs));
        match r {
            Ok(a) if a.status == 0 => Ok(Fs::new(mine)),
            Ok(a) => {
                sys::handle_close(mine);
                Err(Stopped::of(a))
            }
            Err(e) => {
                sys::handle_close(mine);
                Err(Stopped::Status(e))
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

    /// A new connection to the same node that is the root of a **new account** with these limits (slice R):
    /// everything opened or cloned from it is charged to it, and to this connection's own account above it.
    /// The limits may not exceed this connection's account's (BAD_REQUEST).
    pub fn account(
        &mut self,
        conns: u32,
        inflight: u32,
        held_bytes: u32,
        rights: u32,
    ) -> Result<Fs, i32> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(c.status);
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        let a = u64::from(conns) | (u64::from(inflight) << 32);
        match self.call(
            file::op::ACCOUNT,
            0,
            rights,
            a,
            u64::from(held_bytes),
            &[],
            Some(theirs),
        ) {
            Ok(r) if r.status == 0 => Ok(Fs::new(mine)),
            Ok(r) => {
                sys::handle_close(mine);
                Err(r.status)
            }
            Err(e) => {
                sys::handle_close(mine);
                Err(e)
            }
        }
    }

    /// Registry: publish `name`, handing the registry `listener` (the end of a channel the service listens
    /// on). The registry owns it once the call succeeds; on failure it is still the caller's.
    pub fn publish(&mut self, name: &[u8], listener: u32) -> Result<(), i32> {
        match self.call(file::op::PUBLISH, 0, 0, 0, 0, name, Some(listener)) {
            Ok(r) if r.status == 0 => Ok(()),
            Ok(r) => Err(r.status),
            Err(e) => Err(e),
        }
    }

    /// Registry: remove a published name.
    pub fn unpublish(&mut self, name: &[u8]) -> Result<(), i32> {
        self.simple(file::op::UNPUBLISH, 0, 0, name).map(|_| ())
    }

    /// Registry: a new connection whose view allows only `names` (each already allowed here).
    pub fn view(&mut self, names: &[&[u8]], rights: u32) -> Result<Fs, i32> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(c.status);
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        let mut payload = Vec::new();
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                payload.push(0);
            }
            payload.extend_from_slice(n);
        }
        match self.call(file::op::VIEW, 0, rights, 0, 0, &payload, Some(theirs)) {
            Ok(r) if r.status == 0 => Ok(Fs::new(mine)),
            Ok(r) => {
                sys::handle_close(mine);
                Err(r.status)
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
        let info = file::StatInfo::decode(a.a, &a.payload).map_err(|_| -4)?;
        Ok(Stat {
            is_dir: info.kind == 2,
            size: info.size,
            info,
        })
    }

    /// Stat `path` relative to this directory (empty: the directory itself).
    pub fn stat(&mut self, path: &[u8]) -> Result<Stat, i32> {
        self.stat_stop(path, 0).map_err(|s| match s {
            Stopped::Link { .. } => file::status::SYMLINK,
            Stopped::Status(e) => e,
        })
    }

    /// `stat` with `flags` (`NOFOLLOW` to describe a link itself), reporting a symlink in the way as [`Stopped::Link`].
    pub fn stat_stop(&mut self, path: &[u8], flags: u16) -> Result<Stat, Stopped> {
        let r = self
            .call(file::op::STAT, flags, 0, 0, 0, path, None)
            .map_err(Stopped::Status)?;
        if r.status != 0 {
            if let Some(h) = r.handle {
                sys::handle_close(h);
            }
            return Err(Stopped::of(r));
        }
        Self::parse_stat(&r).map_err(Stopped::Status)
    }

    pub fn fstat(&mut self) -> Result<Stat, i32> {
        let a = self.simple(file::op::FSTAT, 0, 0, &[])?;
        Self::parse_stat(&a)
    }

    /// The target text of the symlink `path` (relative to this directory; the link itself is the last component).
    pub fn readlink(&mut self, path: &[u8]) -> Result<Vec<u8>, i32> {
        self.simple(file::op::READLINK, 0, 0, path)
            .map(|a| a.payload)
    }

    /// Make a symlink `path` (relative to this directory) pointing at `target` (needs `CREATE`; the server refuses an absolute
    /// target or one that climbs above this connection's root).
    pub fn symlink(&mut self, path: &[u8], target: &[u8]) -> Result<(), i32> {
        let (a, payload) = file::encode_symlink(path, target);
        let r = self.call(file::op::SYMLINK, 0, 0, a, 0, &payload, None)?;
        if r.status == 0 {
            Ok(())
        } else {
            Err(r.status)
        }
    }

    /// Make everything written so far durable (`fsync`): the reply comes when the server has committed.
    pub fn sync(&mut self) -> Result<(), i32> {
        self.simple(file::op::SYNC, 0, 0, &[]).map(|_| ())
    }

    /// Set attribute `name` of this connection's node (`ty`: 0 bytes, 1 text, 2 u64, 3 i64; the value as on the wire).
    pub fn attr_set(&mut self, ty: u8, name: &[u8], value: &[u8]) -> Result<(), i32> {
        self.simple(
            file::op::ATTR_SET,
            0,
            0,
            &file::encode_attr_set(ty, name, value),
        )
        .map(|_| ())
    }

    /// The type and value of an attribute.
    pub fn attr_get(&mut self, name: &[u8]) -> Result<(u8, Vec<u8>), i32> {
        let a = self.simple(file::op::ATTR_GET, 0, 0, name)?;
        Ok((a.a as u8, a.payload))
    }

    pub fn attr_remove(&mut self, name: &[u8]) -> Result<(), i32> {
        self.simple(file::op::ATTR_REMOVE, 0, 0, name).map(|_| ())
    }

    /// The names of all attributes (follows the paging cookie).
    pub fn attr_list(&mut self) -> Result<Vec<Vec<u8>>, i32> {
        let mut all: Vec<Vec<u8>> = Vec::new();
        loop {
            let cookie = all.last().cloned().unwrap_or_default();
            let a = self.simple(file::op::ATTR_LIST, 0, 0, &cookie)?;
            let names = file::decode_names(&a.payload)
                .map_err(|_| abi::status::Status::InvalidArgs.code())?;
            let empty = names.is_empty();
            all.extend(names);
            if a.a == 0 || empty {
                return Ok(all);
            }
        }
    }

    /// Create an index on a user attribute (this connection is a directory).
    pub fn make_index(&mut self, ty: u8, name: &[u8]) -> Result<(), i32> {
        self.simple(
            file::op::MAKE_INDEX,
            0,
            0,
            &file::encode_make_index(ty, name),
        )
        .map(|_| ())
    }

    /// One page of a query below this directory: the hits and whether there are more.
    pub fn query(&mut self, q: &file::Query<'_>) -> Result<(Vec<file::QueryHit>, bool), i32> {
        let (flags, payload) = q.encode();
        let a = self.call(file::op::QUERY, 0, 0, q.max, flags, &payload, None)?;
        if a.status != 0 {
            return Err(a.status);
        }
        let hits =
            file::decode_hits(&a.payload).map_err(|_| abi::status::Status::InvalidArgs.code())?;
        Ok((hits, a.a != 0))
    }

    pub fn mkdir(&mut self, name: &[u8]) -> Result<(), i32> {
        self.simple(file::op::MKDIR, 0, 0, name).map(|_| ())
    }

    /// Move `old` to `new` (paths relative to this directory); an existing file or empty directory
    /// at `new` is replaced.
    pub fn rename(&mut self, old: &[u8], new: &[u8]) -> Result<(), i32> {
        let mut both = old.to_vec();
        both.extend_from_slice(new);
        self.simple(file::op::RENAME, old.len() as u64, 0, &both)
            .map(|_| ())
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
