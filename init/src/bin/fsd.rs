//! The disk filesystem server (FS-SPEC section 5, step 5c): speaks the file protocol over channels, one connection per open
//! node, on top of the pure `doofs` crate and one block channel (`rt::blk::BlkDevice`). It mounts the volume at start (the
//! newest superblock slot that verifies; the free map is rebuilt by the walk), serves, and commits every few seconds, on
//! `Sync` and when its last client has gone. Single-threaded: one port, every connection bound. A copy of `memfsd`'s
//! request handling on the other filesystem (TIDY-UP T114).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use doofs::{AttrType, Fs, FsError, Limits, NodeId, Term, ROOT};
use memfs::{Error, Kind};
use proto::file::{self, flags, op, rights as pr, status};
use proto::startup::Role;
use rt::abi::status::Status;
use rt::blk::BlkDevice;
use rt::service::{self, Core, Sent, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::Limits as Limits2;

/// The server's total pool (R-SPEC.md, OD84): at most this many live connections, and this many bytes of
/// replies held for clients that have no room (all connections together). With the file quota (8 MiB), the
/// name budget (2 MiB) and this, the worst case fits the heap; a connection that would push the total over is
/// dropped. (Per-account limits come with the `Account` op; every connection is in the root account for now.)
const POOL: Limits2 = Limits2 {
    conns: 200,
    held_bytes: 2 * 1024 * 1024,
    inflight: 16,
    stream_bytes: 0,
};
/// A reply the client has no room for is kept this long (ticks) before the connection is dropped.
const HOLD_TIMEOUT: u64 = 30_000;

/// The bytes of a reply (`proto::file` layout), built without aborting on allocation failure.
fn encode_reply(txid: u32, status: i32, a: u64, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > file::MAX_IO {
        return None;
    }
    let mut v = Vec::new();
    v.try_reserve_exact(file::REPLY_HEADER + payload.len())
        .ok()?;
    v.extend_from_slice(&txid.to_le_bytes());
    v.extend_from_slice(&status.to_le_bytes());
    v.extend_from_slice(&a.to_le_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    Some(v)
}

struct Conn {
    node: NodeId,
    rights: u32,
    is_dir: bool,
    /// A listener channel the registry forwards opens of the published volume to (not a client connection).
    listener: bool,
}

struct Server {
    fs: Fs<BlkDevice>,
    core: Core<Conn>,
    /// The tick of the first change not yet committed (`None`: nothing to commit).
    dirty_since: Option<u64>,
    /// What a forwarded open is connected to, and the most rights it may have (`ro` and `sub=` of the mount table).
    base: NodeId,
    allowed: u32,
}

/// Commit this long (ticks) after the first uncommitted change.
const COMMIT_INTERVAL: u64 = 2_000;

fn err_status(e: Error, name_len: usize) -> i32 {
    match e {
        Error::NotFound => status::NOT_FOUND,
        Error::Exists => status::EXISTS,
        Error::NotEmpty => status::NOT_EMPTY,
        Error::IsDirectory => status::IS_DIRECTORY,
        Error::NotDirectory => status::NOT_DIRECTORY,
        Error::Quota => status::QUOTA,
        Error::TooBig => Status::TooBig.code(),
        Error::ReadOnly => Status::AccessDenied.code(),
        Error::BadName if name_len > file::MAX_COMPONENT => status::NAME_TOO_LONG,
        Error::BadName | Error::BadRange | Error::Invalid => status::BAD_REQUEST,
        // (attributes, indexes, queries and symlinks arrive with the server step; the statuses are defined, nothing sends them yet)
        Error::NoAttr => status::NO_ATTR,
        Error::NoIndex => status::NO_INDEX,
        Error::BadValue => status::BAD_VALUE,
        Error::BadTarget => status::BAD_TARGET,
    }
}

/// The status for a failure of any kind: the filesystem's own errors as `memfsd` reports them, a full volume as `QUOTA`, a
/// failed or damaged disk as `IO_ERROR`.
fn fstat(e: FsError, name_len: usize) -> i32 {
    match e {
        FsError::Fs(e) => err_status(e, name_len),
        FsError::NoSpace => status::QUOTA,
        FsError::Corrupt | FsError::Io => status::IO_ERROR,
    }
}

fn kind_byte(k: Kind) -> u8 {
    if k == Kind::Dir {
        2
    } else {
        1
    }
}

impl Service for Server {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], handles: &[u32]) {
        if self.core.conn(key).is_some_and(|c| c.listener) {
            return self.forwarded_open(msg, handles);
        }
        // (the runtime reads at most one handle: more is a protocol violation it has already ended)
        self.handle_request(key, msg, handles.first().copied());
    }

    fn closed(&mut self, conn: Conn) {
        let _ = self.fs.release(conn.node);
        self.changed();
    }

    fn next_deadline(&mut self) -> Option<u64> {
        self.dirty_since.map(|t| t + COMMIT_INTERVAL)
    }

    fn tick(&mut self) {
        if let Some(t) = self.dirty_since {
            if sys::clock_get() >= t + COMMIT_INTERVAL {
                self.sync();
            }
        }
    }
}

impl Server {
    /// An open the registry forwarded to a listener: the client's endpoint becomes a connection to the volume's base
    /// directory in an account of its own, with the rights it asked for (never more than the mount allows).
    fn forwarded_open(&mut self, msg: &[u8], hs: &[u32]) {
        let ok = file::decode_request(msg)
            .ok()
            .filter(|r| r.op == op::OPEN && r.payload.is_empty() && hs.len() == 1);
        let Some(req) = ok else {
            for &h in hs {
                sys::handle_close(h);
            }
            return;
        };
        let limits = Limits2 {
            conns: 64,
            held_bytes: 256 * 1024,
            inflight: 16,
            stream_bytes: 0,
        };
        let Ok(account) = self.core.accounts().open(svc::ROOT, limits) else {
            sys::handle_close(hs[0]);
            return;
        };
        let rights = req.rights & self.allowed;
        if self
            .add_conn(account, hs[0], self.base, rights, true)
            .is_none()
        {
            sys::handle_close(hs[0]);
            let _ = self.core.accounts().close(account);
        }
    }

    /// Something may have changed on disk: remember when, so the next commit is due.
    fn changed(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(sys::clock_get());
        }
    }

    /// Make everything so far durable.
    fn sync(&mut self) -> i32 {
        match self.fs.commit() {
            Ok(()) => {
                self.dirty_since = None;
                0
            }
            Err(e) => {
                // try again after the interval, not in a busy loop (review finding 3)
                self.dirty_since = Some(sys::clock_get());
                fstat(e, 0)
            }
        }
    }

    /// Start serving `ch` as a connection to `node` in `account`. `None` if the runtime refuses
    /// it (not a channel end we may read and write, both ends of one channel, the pool is full): nothing
    /// changes then, and the handle is still the caller's to close.
    fn add_conn(
        &mut self,
        account: svc::AccountId,
        ch: u32,
        node: NodeId,
        rights: u32,
        is_dir: bool,
    ) -> Option<u64> {
        let key = self.core.add_conn(
            ch,
            account,
            Conn {
                node,
                rights,
                is_dir,
                listener: false,
            },
        )?;
        let _ = self.fs.retain(node);
        self.fs.set_time(sys::clock_get().saturating_mul(1_000_000));
        Some(key)
    }

    /// End a connection and release what it held.
    fn close_conn(&mut self, key: u64) {
        self.core.close_conn(key);
        self.release_closed();
    }

    fn release_closed(&mut self) {
        for c in self.core.drain_closed() {
            let _ = self.fs.release(c.node);
            self.changed();
        }
    }

    /// Send a reply on `key`'s channel; the runtime holds it if the client has no room (backpressure: a
    /// client that never reads stalls itself) and ends the connection if it cannot.
    fn send(
        &mut self,
        key: u64,
        txid: u32,
        status: i32,
        a: u64,
        payload: &[u8],
        handle: Option<u32>,
    ) {
        let Some(bytes) = encode_reply(txid, status, a, payload) else {
            // out of memory (or a payload that cannot be a reply): drop the connection
            if let Some(h) = handle {
                sys::handle_close(h);
            }
            self.close_conn(key);
            return;
        };
        if self.core.send(key, bytes, handle) == Sent::Closed {
            self.release_closed();
        }
    }

    fn handle_request(&mut self, key: u64, msg: &[u8], endpoint: Option<u32>) {
        // file times are the time of the change, not of the last open (review finding 9)
        self.fs.set_time(sys::clock_get().saturating_mul(1_000_000));
        let close_ep = |ep: Option<u32>| {
            if let Some(h) = ep {
                sys::handle_close(h);
            }
        };
        let Ok(req) = file::decode_request(msg) else {
            close_ep(endpoint);
            match file::peek_txid(msg) {
                Some(t) => self.send(key, t, status::BAD_REQUEST, 0, &[], None),
                None => self.close_conn(key),
            }
            return;
        };
        let txid = req.txid;
        let want = op::handles(req.op);
        if want.is_none() || want != Some(usize::from(endpoint.is_some())) {
            close_ep(endpoint);
            self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
            return;
        }
        let (is_dir, conn_rights, node) = {
            let c = self
                .core
                .conn(key)
                .expect("a request came from a live connection");
            (c.is_dir, c.rights, c.node)
        };
        if req.op != op::CLONE
            && req.op != op::ACCOUNT
            && req.op != op::SYNC
            && !matches!(
                req.op,
                op::ATTR_SET | op::ATTR_GET | op::ATTR_REMOVE | op::ATTR_LIST
            )
            && op::is_directory_op(req.op) != is_dir
        {
            close_ep(endpoint);
            let st = if is_dir {
                status::IS_DIRECTORY
            } else {
                status::NOT_DIRECTORY
            };
            self.send(key, txid, st, 0, &[], None);
            return;
        }
        let need = |r: u32| conn_rights & r == r;
        let denied = Status::AccessDenied.code();
        match req.op {
            op::OPEN | op::CLONE => {
                let path: &[u8] = if req.op == op::OPEN { req.payload } else { &[] };
                if (req.op == op::OPEN && !need(pr::READ)) || req.rights & !conn_rights != 0 {
                    close_ep(endpoint);
                    self.send(key, txid, denied, 0, &[], None);
                    return;
                }
                let account = self
                    .core
                    .account_of(key)
                    .expect("a live connection has an account");
                let st = self.open(
                    account,
                    node,
                    conn_rights,
                    path,
                    req.flags,
                    req.rights,
                    endpoint.unwrap(),
                );
                match st {
                    Ok(kind) => self.send(key, txid, 0, u64::from(kind), &[], None),
                    Err(s) => {
                        close_ep(endpoint);
                        self.send(key, txid, s, 0, &[], None);
                    }
                }
            }
            op::ACCOUNT => {
                // A clone that is the root of a new account (R-SPEC.md section 3): `a` is the connection
                // limit (low 32 bits) and the in-flight limit (high 32), `b` the bytes held for slow readers.
                if req.rights & !conn_rights != 0 {
                    close_ep(endpoint);
                    self.send(key, txid, denied, 0, &[], None);
                    return;
                }
                let Ok(held_bytes) = u32::try_from(req.b) else {
                    close_ep(endpoint);
                    self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                    return;
                };
                let limits = Limits2 {
                    conns: req.a as u32,
                    held_bytes,
                    inflight: (req.a >> 32) as u32,
                    stream_bytes: 0,
                };
                let parent = self
                    .core
                    .account_of(key)
                    .expect("a live connection has an account");
                let account = match self.core.accounts().open(parent, limits) {
                    Ok(a) => a,
                    Err(svc::Refuse::Quota) => {
                        close_ep(endpoint);
                        self.send(key, txid, status::QUOTA, 0, &[], None);
                        return;
                    }
                    Err(_) => {
                        // looser than the connection's own account allows
                        close_ep(endpoint);
                        self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                        return;
                    }
                };
                match self.open(
                    account,
                    node,
                    conn_rights,
                    &[],
                    0,
                    req.rights,
                    endpoint.unwrap(),
                ) {
                    Ok(kind) => self.send(key, txid, 0, u64::from(kind), &[], None),
                    Err(s) => {
                        let _ = self.core.accounts().close(account);
                        close_ep(endpoint);
                        self.send(key, txid, s, 0, &[], None);
                    }
                }
            }
            op::READDIR => {
                if !need(pr::ENUMERATE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let cookie = if req.payload.is_empty() {
                    None
                } else if file::valid_name(req.payload).is_ok() {
                    Some(req.payload)
                } else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                match self.fs.read_dir(node, cookie, 128) {
                    Ok(list) => {
                        let entries: Vec<file::DirEntry> = list
                            .into_iter()
                            .map(|(name, k)| file::DirEntry {
                                is_dir: k == Kind::Dir,
                                name,
                            })
                            .collect();
                        let (bytes, n) = file::encode_entries(&entries, file::MAX_IO);
                        self.send(key, txid, 0, n as u64, &bytes, None);
                    }
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::MKDIR | op::UNLINK => {
                self.changed();
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let r = if req.op == op::MKDIR {
                    self.fs.mkdir(node, req.payload).map(|_| ())
                } else {
                    self.fs.unlink(node, req.payload)
                };
                let st = r.map_or_else(|e| fstat(e, req.payload.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::RENAME => {
                self.changed();
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                // `a` bytes of old path, then the new path (the payload holds nothing else)
                let split = usize::try_from(req.a)
                    .ok()
                    .filter(|n| *n <= req.payload.len());
                let Some(n) = split else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let (old, new) = req.payload.split_at(n);
                let (Ok(old), Ok(new)) = (file::split_path(old), file::split_path(new)) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let (old_last, old_parents) =
                    old.split_last().expect("a split path is never empty");
                let (new_last, new_parents) =
                    new.split_last().expect("a split path is never empty");
                let r = self
                    .fs
                    .walk(node, old_parents)
                    .and_then(|from| Ok((from, self.fs.walk(node, new_parents)?)))
                    .and_then(|(from, to)| {
                        // a symlink must not end up where its target would climb above this connection's root (T117 8)
                        let moved = self.fs.lookup(from, old_last)?;
                        if self.fs.kind(moved)? == Kind::Symlink {
                            let target = self.fs.read_link(moved)?;
                            let depth = self.fs.depth_below(to, node)?;
                            file::check_link_target(&target, depth)
                                .map_err(|_| FsError::Fs(Error::BadTarget))?;
                        }
                        self.fs.rename(from, old_last, to, new_last)
                    });
                let st = r.map_or_else(|e| fstat(e, new_last.len().max(old_last.len())), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::STAT => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let target = if req.payload.is_empty() {
                    Ok(node)
                } else {
                    match file::split_path(req.payload) {
                        Ok(comps) => self.fs.walk(node, &comps),
                        Err(_) => return self.send(key, txid, status::BAD_REQUEST, 0, &[], None),
                    }
                };
                self.stat_reply(key, txid, target);
            }
            op::FSTAT => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                self.stat_reply(key, txid, Ok(node));
            }
            op::GETSIZE => {
                // (a writer may learn the size too: `lseek(SEEK_END)` and append need it)
                if !need(pr::READ) && !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.size(node) {
                    Ok(n) => self.send(key, txid, 0, n, &[], None),
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::SETSIZE => {
                self.changed();
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let st = self
                    .fs
                    .set_size(node, req.a)
                    .map_or_else(|e| fstat(e, 0), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::READ => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                if req.b as usize > file::MAX_IO {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                }
                let len = req.b as usize;
                match self.fs.read(node, req.a, len) {
                    Ok(data) => self.send(key, txid, 0, data.len() as u64, &data, None),
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::WRITE => {
                self.changed();
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.write(node, req.a, req.payload) {
                    Ok(n) => self.send(key, txid, 0, n as u64, &[], None),
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::ATTR_SET => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let Ok((ty, name, value)) = file::decode_attr_set(req.payload) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let Some(ty) = AttrType::from_code(ty) else {
                    return self.send(key, txid, status::BAD_VALUE, 0, &[], None);
                };
                self.changed();
                let st = self
                    .fs
                    .attr_set(node, name, ty, value)
                    .map_or_else(|e| fstat(e, name.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::ATTR_GET => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.attr_get(node, req.payload) {
                    Ok((ty, v)) => self.send(key, txid, 0, ty as u64, &v, None),
                    Err(e) => self.send(key, txid, fstat(e, req.payload.len()), 0, &[], None),
                }
            }
            op::ATTR_REMOVE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                self.changed();
                let st = self
                    .fs
                    .attr_remove(node, req.payload)
                    .map_or_else(|e| fstat(e, req.payload.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::ATTR_LIST => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.attr_list(node) {
                    Ok(names) => {
                        // the names after the cookie (`payload`), as many as fit one reply; `a` says whether more remain
                        let rest: Vec<Vec<u8>> = names
                            .into_iter()
                            .filter(|n| req.payload.is_empty() || n.as_slice() > req.payload)
                            .collect();
                        let (bytes, n) = file::encode_names(&rest, file::MAX_IO);
                        self.send(key, txid, 0, u64::from(n < rest.len()), &bytes, None);
                    }
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::MAKE_INDEX => {
                // (the INDEX right is not defined yet, T109(3): writing is what the server asks for)
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let Ok((ty, name)) = file::decode_make_index(req.payload) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let Some(ty) = AttrType::from_code(ty) else {
                    return self.send(key, txid, status::BAD_VALUE, 0, &[], None);
                };
                self.changed();
                let st = self
                    .fs
                    .make_index(name, ty)
                    .map_or_else(|e| fstat(e, name.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::QUERY => {
                if !need(pr::ENUMERATE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let Ok(q) = file::decode_query(req.a, req.b, req.payload) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let term = match q.term {
                    file::QueryTerm::Eq(v) => Term::Eq(v.to_vec()),
                    file::QueryTerm::Range { lo, hi } => Term::Range {
                        lo: lo.map(<[u8]>::to_vec),
                        hi: hi.map(<[u8]>::to_vec),
                    },
                };
                let max = usize::try_from(q.max).unwrap_or(usize::MAX);
                match self.fs.query(node, q.index, &term, q.after, max) {
                    Ok(r) => {
                        let hits: Vec<file::QueryHit> = r
                            .hits
                            .iter()
                            .map(|h| file::QueryHit {
                                key: h.key.clone(),
                                id: h.id,
                                path: h.path.clone(),
                            })
                            .collect();
                        let (bytes, n) = file::encode_hits(&hits, file::MAX_IO);
                        if n == 0 && !hits.is_empty() {
                            // one hit (a very long path) does not fit a reply: say so instead of `more` forever (T117 11)
                            return self.send(key, txid, Status::TooBig.code(), 0, &[], None);
                        }
                        let more = r.more || n < hits.len();
                        self.send(key, txid, 0, u64::from(more), &bytes, None);
                    }
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            op::GETVMO => {
                // (a disk file has no VMO to give until the cache exists, FS-SPEC Q4)
                self.send(key, txid, status::BAD_REQUEST, 0, &[], None)
            }
            op::SYNC => {
                let st = self.sync();
                self.send(key, txid, st, 0, &[], None);
            }
            op::SYMLINK => {
                if !need(pr::CREATE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                // `a` bytes of path, then the target
                let split = usize::try_from(req.a)
                    .ok()
                    .filter(|n| *n <= req.payload.len());
                let Some(n) = split else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let (path, target) = req.payload.split_at(n);
                let Ok(comps) = file::split_path(path) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                let (last, parents) = comps.split_last().expect("a split path is never empty");
                let r = self
                    .fs
                    .walk(node, parents)
                    .and_then(|parent| self.fs.symlink(parent, last, target, node));
                self.changed();
                let st = r.map_or_else(|e| fstat(e, last.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::READLINK => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let Ok(comps) = file::split_path(req.payload) else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[], None);
                };
                match self
                    .fs
                    .walk(node, &comps)
                    .and_then(|id| self.fs.read_link(id))
                {
                    Ok(t) => self.send(key, txid, 0, t.len() as u64, &t, None),
                    Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
                }
            }
            _ => {
                close_ep(endpoint);
                self.send(key, txid, status::BAD_REQUEST, 0, &[], None)
            }
        }
    }

    fn stat_reply(&mut self, key: u64, txid: u32, target: Result<NodeId, FsError>) {
        let r = target.and_then(|id| Ok((self.fs.kind(id)?, self.fs.size(id)?)));
        match r {
            Ok((k, size)) => {
                let mut p = alloc::vec![kind_byte(k)];
                p.extend_from_slice(&size.to_le_bytes());
                self.send(key, txid, 0, size, &p, None);
            }
            Err(e) => self.send(key, txid, fstat(e, 0), 0, &[], None),
        }
    }

    /// Open (or clone, with an empty `path`) a node and bind `endpoint` to it. Returns the kind
    /// byte (1 file, 2 directory) or a status.
    #[allow(clippy::too_many_arguments)]
    fn open(
        &mut self,
        account: svc::AccountId,
        dir: NodeId,
        dir_rights: u32,
        path: &[u8],
        fl: u16,
        want: u32,
        endpoint: u32,
    ) -> Result<u8, i32> {
        let denied = Status::AccessDenied.code();
        self.changed();
        // The connection limit comes first: nothing may be created or truncated for an open that
        // is going to be refused.
        if !self.core.would_admit(account) {
            return Err(status::QUOTA);
        }
        // A clone (or an open of an empty path) names the node it is on: no flag applies.
        let fl = if path.is_empty() { 0 } else { fl };
        let (target, created_new) = if path.is_empty() {
            (dir, false)
        } else {
            let comps = file::split_path(path).map_err(|_| status::BAD_REQUEST)?;
            let (last, parents) = comps.split_last().expect("a split path is never empty");
            let parent = self.fs.walk(dir, parents).map_err(|e| fstat(e, 0))?;
            match self.fs.lookup(parent, last) {
                Ok(id) => {
                    if fl & flags::CREATE != 0 && fl & flags::EXCLUSIVE != 0 {
                        return Err(status::EXISTS);
                    }
                    (id, false)
                }
                Err(FsError::Fs(Error::NotFound)) if fl & flags::CREATE != 0 => {
                    if dir_rights & pr::CREATE == 0 {
                        return Err(denied);
                    }
                    let id = self
                        .fs
                        .create_file(parent, last)
                        .map_err(|e| fstat(e, last.len()))?;
                    (id, true)
                }
                Err(e) => return Err(fstat(e, last.len())),
            }
        };
        let kind = self.fs.kind(target).map_err(|e| fstat(e, 0))?;
        let is_dir = kind == Kind::Dir;
        let undo = |s: &mut Server, st: i32| {
            if created_new {
                // a node we just made for a refused open must not linger
                if let Ok(comps) = file::split_path(path) {
                    if let Some((last, parents)) = comps.split_last() {
                        if let Ok(p) = s.fs.walk(dir, parents) {
                            let _ = s.fs.unlink(p, last);
                        }
                    }
                }
            }
            Err(st)
        };
        if fl & flags::DIRECTORY != 0 && !is_dir {
            return undo(self, status::NOT_DIRECTORY);
        }
        if is_dir && fl & flags::DIRECTORY == 0 && !path.is_empty() {
            return undo(self, status::IS_DIRECTORY);
        }
        // Asking to write something read-only is refused (rights only shrink, and a read-only
        // node never grants WRITE or CREATE).
        let readonly = self.fs.is_readonly(target).unwrap_or(true);
        if readonly && want & (pr::WRITE | pr::CREATE) != 0 {
            return undo(self, denied);
        }
        if !is_dir && want & (pr::ENUMERATE | pr::CREATE) != 0 {
            return undo(self, status::BAD_REQUEST);
        }
        if fl & flags::TRUNCATE != 0 && (is_dir || want & pr::WRITE == 0) {
            return undo(self, status::BAD_REQUEST);
        }
        let Some(key) = self.add_conn(account, endpoint, target, want, is_dir) else {
            // not something the port can watch (an event or a VMO passed as the "endpoint")
            return undo(self, status::BAD_REQUEST);
        };
        // Truncate last: nothing destructive happens unless the open succeeds.
        if fl & flags::TRUNCATE != 0 {
            if let Err(e) = self.fs.set_size(target, 0) {
                self.close_conn(key);
                return Err(fstat(e, 0));
            }
        }
        Ok(kind_byte(kind))
    }
}

/// The limits of a volume: its size bounds data and names; the count of nodes is generous (each is a few hundred bytes).
fn limits_for(blocks: u64) -> Limits {
    let bytes = (blocks * fsfmt::BLOCK as u64).min(usize::MAX as u64 / 2) as usize;
    Limits {
        max_nodes: 1 << 20,
        max_bytes: bytes,
        max_file: bytes,
        max_meta: 64 * 1024 * 1024,
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let Some(blk_ch) = env.handle(Role::BlockDevice) else {
        return 3;
    };
    let root_ch = env.handle(Role::ServiceFs);
    let listeners = [
        env.handle(Role::ServiceListener),
        env.handle(Role::ServiceListenerAlt),
    ];
    if root_ch.is_none() && listeners.iter().all(Option::is_none) {
        return 3;
    }
    // the mount table's options, as arguments: `ro` and `sub=<dir>`
    let (mut readonly, mut sub): (bool, Option<alloc::vec::Vec<u8>>) = (false, None);
    for a in env.startup.args.iter().skip(1) {
        if a.as_slice() == b"ro" {
            readonly = true;
        } else if let Some(p) = a.strip_prefix(b"sub=") {
            sub = Some(p.to_vec());
        }
    }
    let Some(dev) = BlkDevice::open(blk_ch) else {
        return 4;
    };
    let limits = limits_for(fsfmt::Device::blocks(&dev));
    let Ok(vol) = fsfmt::mount(dev) else {
        // not a doofs volume (or damaged beyond the older slots): fail closed
        return 5;
    };
    let Ok(fs) = Fs::open(vol, limits) else {
        return 6;
    };
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut server = Server {
        fs,
        core: Core::new(p.a1 as u32, POOL, file::MAX_MESSAGE, 1, HOLD_TIMEOUT),
        dirty_since: None,
        base: ROOT,
        allowed: if readonly {
            pr::READ | pr::ENUMERATE
        } else {
            pr::ALL
        },
    };
    if let Some(path) = sub {
        let Ok(comps) = file::split_path(&path) else {
            return 10;
        };
        match server.fs.walk(ROOT, &comps) {
            Ok(id) if server.fs.kind(id) == Ok(Kind::Dir) => server.base = id,
            _ => return 10,
        }
    }
    // The service channel (if any) is a connection to the base directory with every right the mount allows.
    if let Some(ch) = root_ch {
        let (base, allowed) = (server.base, server.allowed);
        if server
            .add_conn(svc::ROOT, ch, base, allowed, true)
            .is_none()
        {
            return 9;
        }
    }
    for l in listeners.into_iter().flatten() {
        match server.core.add_conn(
            l,
            svc::ROOT,
            Conn {
                node: ROOT,
                rights: 0,
                is_dir: true,
                listener: true,
            },
        ) {
            Some(_) => {}
            None => return 9,
        }
    }
    let code = service::run(&mut server);
    // the last client has gone: leave the volume committed
    let _ = server.fs.commit();
    code
}
