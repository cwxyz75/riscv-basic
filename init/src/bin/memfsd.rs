//! The memory filesystem server (S4c, S4-SPEC section 7): speaks the file protocol over channels,
//! one connection per open node, on top of the pure `memfs` crate. The boot image supplies
//! `/bin`, `/etc` and `/home` (read-only, backed by the image VMO); `/tmp` is the one writable
//! directory. Single-threaded: one port, every connection bound.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use memfs::{AttrType, Error, Fs, Kind, Limits, NodeId, ROOT};
use proto::file::{self, flags, op, rights as pr, status};
use proto::startup::Role;
use rt::abi::rights::Rights;
use rt::abi::status::Status;
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
}

struct Server {
    fs: Fs,
    core: Core<Conn>,
    image_vmo: u32,
    /// A walk met a symlink: (components consumed, target text) for the reply (`status::SYMLINK`).
    link_stop: Option<(usize, Vec<u8>)>,
}

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
        Error::BadEncoding => status::BAD_NAME,
        Error::BadName | Error::BadRange | Error::Invalid => status::BAD_REQUEST,
        // (attributes, indexes, queries and symlinks arrive with the server step; the statuses are defined, nothing sends them yet)
        Error::NoAttr => status::NO_ATTR,
        Error::NoIndex => status::NO_INDEX,
        Error::BadValue => status::BAD_VALUE,
        Error::BadTarget => status::BAD_TARGET,
    }
}

fn kind_byte(k: Kind) -> u8 {
    match k {
        Kind::Dir => 2,
        Kind::Symlink => 3,
        _ => 1,
    }
}

impl Service for Server {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], handles: &[u32]) {
        // (the runtime reads at most one handle: more is a protocol violation it has already ended)
        self.handle_request(key, msg, handles.first().copied());
    }

    fn closed(&mut self, conn: Conn) {
        self.fs.release(conn.node);
    }
}

impl Server {
    /// Does walking `comps` from `node` meet a symlink? (See `fsd`: a link in the middle always stops the walk, the last
    /// component only if `follow_last`; the client follows, never the server.)
    fn link_on_path(&mut self, node: NodeId, comps: &[&[u8]], follow_last: bool) -> bool {
        let mut at = node;
        for (i, c) in comps.iter().enumerate() {
            let Ok(id) = self.fs.lookup(at, c) else {
                return false;
            };
            match self.fs.kind(id) {
                Ok(Kind::Symlink) if i + 1 < comps.len() || follow_last => {
                    if let Ok(t) = self.fs.read_link(id) {
                        self.link_stop = Some((i + 1, t));
                        return true;
                    }
                    return false;
                }
                Ok(Kind::Dir) => at = id,
                _ => return false,
            }
        }
        false
    }

    fn send_status(&mut self, key: u64, txid: u32, st: i32) {
        if st == status::SYMLINK {
            if let Some((consumed, target)) = self.link_stop.take() {
                return self.send(key, txid, st, consumed as u64, &target, None);
            }
        }
        self.send(key, txid, st, 0, &[], None);
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
            },
        )?;
        let _ = self.fs.retain(node);
        Some(key)
    }

    /// End a connection and release what it held.
    fn close_conn(&mut self, key: u64) {
        self.core.close_conn(key);
        self.release_closed();
    }

    fn release_closed(&mut self) {
        for c in self.core.drain_closed() {
            self.fs.release(c.node);
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
        if req.op != op::CLONE && req.op != op::ACCOUNT && op::is_directory_op(req.op) != is_dir {
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
                        self.send_status(key, txid, s);
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
                        self.send_status(key, txid, s);
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
                                is_link: k == Kind::Symlink,
                                name,
                            })
                            .collect();
                        let (bytes, n) = file::encode_entries(&entries, file::MAX_IO);
                        self.send(key, txid, 0, n as u64, &bytes, None);
                    }
                    Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                }
            }
            op::MKDIR | op::UNLINK => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let r = if req.op == op::MKDIR {
                    self.fs.mkdir(node, req.payload).map(|_| ())
                } else {
                    self.fs.unlink(node, req.payload)
                };
                let st = r.map_or_else(|e| err_status(e, req.payload.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::RENAME => {
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
                let (old, new) = match (file::split_path(old), file::split_path(new)) {
                    (Ok(o), Ok(n)) => (o, n),
                    (Err(e), _) | (_, Err(e)) => {
                        return self.send(key, txid, file::path_status(e), 0, &[], None)
                    }
                };
                let (old_last, old_parents) =
                    old.split_last().expect("a split path is never empty");
                let (new_last, new_parents) =
                    new.split_last().expect("a split path is never empty");
                let r = self
                    .fs
                    .walk(node, old_parents)
                    .and_then(|from| Ok((from, self.fs.walk(node, new_parents)?)))
                    .and_then(|(from, to)| self.fs.rename(from, old_last, to, new_last));
                let st =
                    r.map_or_else(|e| err_status(e, new_last.len().max(old_last.len())), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            // memory has nothing to commit: a sync succeeds (SQLite syncs its journal)
            op::SYNC => self.send(key, txid, 0, 0, &[], None),
            op::STAT => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let target = if req.payload.is_empty() {
                    Ok(node)
                } else {
                    match file::split_path(req.payload) {
                        Ok(comps) => {
                            if self.link_on_path(node, &comps, req.flags & flags::NOFOLLOW == 0) {
                                return self.send_status(key, txid, status::SYMLINK);
                            }
                            self.fs.walk(node, &comps)
                        }
                        Err(e) => return self.send(key, txid, file::path_status(e), 0, &[], None),
                    }
                };
                self.stat_reply(key, txid, target);
            }
            op::FSTAT => {
                // (a descriptor opened for writing may describe itself, as POSIX `fstat` allows: `tar` does it on its archive)
                if !need(pr::READ) && !need(pr::WRITE) {
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
                    Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                }
            }
            op::SETSIZE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let st = self
                    .fs
                    .set_size(node, req.a)
                    .map_or_else(|e| err_status(e, 0), |_| 0);
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
                match self.fs.kind(node) {
                    Ok(Kind::Image) => {
                        let (off, total) = self.fs.image_extent(node).unwrap_or((0, 0));
                        let start = req.a.min(total);
                        let n = (total - start).min(len as u64) as usize;
                        let mut buf: Vec<u8> = Vec::new();
                        if buf.try_reserve_exact(n).is_err() {
                            return self.send(key, txid, status::QUOTA, 0, &[], None);
                        }
                        buf.resize(n, 0);
                        if n > 0 {
                            let r = sys::vmo_read(self.image_vmo, off + start, buf.as_mut_ptr(), n);
                            if r.status != 0 {
                                return self.send(key, txid, r.status, 0, &[], None);
                            }
                        }
                        self.send(key, txid, 0, n as u64, &buf, None);
                    }
                    Ok(_) => match self.fs.read(node, req.a, len) {
                        Ok(data) => self.send(key, txid, 0, data.len() as u64, &data, None),
                        Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                    },
                    Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                }
            }
            op::WRITE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.write(node, req.a, req.payload) {
                    Ok(n) => self.send(key, txid, 0, n as u64, &[], None),
                    Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                }
            }
            op::READLINK => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let comps = match file::split_path(req.payload) {
                    Ok(c) => c,
                    Err(e) => return self.send(key, txid, file::path_status(e), 0, &[], None),
                };
                // (the link is the last component: only links in the middle stop the walk)
                if self.link_on_path(node, &comps, false) {
                    return self.send_status(key, txid, status::SYMLINK);
                }
                match self
                    .fs
                    .walk(node, &comps)
                    .and_then(|id| self.fs.read_link(id))
                {
                    Ok(t) => self.send(key, txid, 0, t.len() as u64, &t, None),
                    Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
                }
            }
            // (the advisory POSIX attributes: `chmod`, `chown` and their descriptor forms; the files of /tmp can carry them)
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
                let st = self
                    .fs
                    .attr_set(node, name, ty, value)
                    .map_or_else(|e| err_status(e, name.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::ATTR_GET => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.attr_get(node, req.payload) {
                    Ok((ty, v)) => self.send(key, txid, 0, ty as u64, &v, None),
                    Err(e) => self.send(key, txid, err_status(e, req.payload.len()), 0, &[], None),
                }
            }
            op::ATTR_REMOVE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                let st = self
                    .fs
                    .attr_remove(node, req.payload)
                    .map_or_else(|e| err_status(e, req.payload.len()), |_| 0);
                self.send(key, txid, st, 0, &[], None);
            }
            op::GETVMO => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[], None);
                }
                match self.fs.image_extent(node) {
                    Ok((off, len)) => {
                        // The whole boot image, read-only; the extent says where the file is.
                        let d = sys::handle_duplicate(
                            self.image_vmo,
                            Rights::READ.0
                                | Rights::MAP.0
                                | Rights::DUPLICATE.0
                                | Rights::TRANSFER.0,
                        );
                        if d.status != 0 {
                            return self.send(key, txid, d.status, 0, &[], None);
                        }
                        self.send(key, txid, 0, off, &len.to_le_bytes(), Some(d.a1 as u32));
                    }
                    Err(_) => self.send(key, txid, status::BAD_REQUEST, 0, &[], None),
                }
            }
            _ => self.send(key, txid, status::BAD_REQUEST, 0, &[], None),
        }
    }

    /// What `STAT` says about `id`: the kind, size, time and the advisory POSIX attributes (USER-VIEW-SPEC section 6). The
    /// programs of the boot image (`/bin/*`, read-only image files that cannot carry attributes) are executable by default.
    fn stat_info(&self, id: NodeId) -> Result<file::StatInfo, Error> {
        let kind = self.fs.kind(id)?;
        let attr = |name: &[u8]| self.fs.attr_get(id, name).ok().map(|(_, v)| v);
        let mut mode = file::attr_u32(attr(file::ATTR_POSIX_MODE).as_deref());
        if mode == file::ABSENT && kind == Kind::Image {
            let in_bin = self
                .fs
                .lookup(ROOT, b"bin")
                .is_ok_and(|bin| self.fs.parent(id) == Ok(bin));
            mode = if in_bin { 0o755 } else { 0o644 };
        }
        Ok(file::StatInfo {
            kind: kind_byte(kind),
            size: self.fs.size(id)?,
            mtime: self.fs.mtime(id)?,
            mode,
            uid: file::attr_u32(attr(file::ATTR_POSIX_UID).as_deref()),
            gid: file::attr_u32(attr(file::ATTR_POSIX_GID).as_deref()),
        })
    }

    fn stat_reply(&mut self, key: u64, txid: u32, target: Result<NodeId, Error>) {
        match target.and_then(|id| self.stat_info(id)) {
            Ok(s) => self.send(key, txid, 0, s.size, &s.encode_payload(), None),
            Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
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
            let comps = file::split_path(path).map_err(file::path_status)?;
            let (last, parents) = comps.split_last().expect("a split path is never empty");
            let follow = fl & flags::NOFOLLOW == 0 && fl & flags::EXCLUSIVE == 0;
            if self.link_on_path(dir, &comps, follow) {
                return Err(status::SYMLINK);
            }
            let parent = self.fs.walk(dir, parents).map_err(|e| err_status(e, 0))?;
            match self.fs.lookup(parent, last) {
                Ok(id) => {
                    if fl & flags::CREATE != 0 && fl & flags::EXCLUSIVE != 0 {
                        return Err(status::EXISTS);
                    }
                    (id, false)
                }
                Err(Error::NotFound) if fl & flags::CREATE != 0 => {
                    if dir_rights & pr::CREATE == 0 {
                        return Err(denied);
                    }
                    let id = self
                        .fs
                        .create_file(parent, last)
                        .map_err(|e| err_status(e, last.len()))?;
                    (id, true)
                }
                Err(e) => return Err(err_status(e, last.len())),
            }
        };
        let kind = self.fs.kind(target).map_err(|e| err_status(e, 0))?;
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
                return Err(err_status(e, 0));
            }
        }
        Ok(kind_byte(kind))
    }
}

/// The image names a session's root is built from, and where each appears: `home_src` (a directory of the image, `home` by
/// default) is `/home`; the image's `homes/` (every user's own directory) and `etc/profiles/` (the profiles) are never part of
/// a session's root, so no session can reach another's home or read the profiles (USER-VIEW-SPEC section 10, step UV5).
fn session_name(name: &[u8], home_src: &[u8]) -> Option<Vec<u8>> {
    let under =
        |dir: &[u8]| name.len() > dir.len() && name.starts_with(dir) && name[dir.len()] == b'/';
    // the session's own home first (it may be a directory below `homes/`)
    if home_src != b"home" && under(home_src) {
        return Some([&b"home"[..], &name[home_src.len()..]].concat());
    }
    if under(b"homes") || under(b"etc/profiles") {
        return None;
    }
    if home_src != b"home" && under(b"home") {
        return None;
    }
    Some(name.to_vec())
}

fn build_tree(fs: &mut Fs, entries: &[proto::bootimg::Entry], home_src: &[u8]) -> Option<()> {
    let tmp = fs.mkdir(ROOT, b"tmp").ok()?;
    let _ = tmp;
    // `/home` exists even when the profile's home has nothing in it
    match fs.mkdir(ROOT, b"home") {
        Ok(_) | Err(Error::Exists) => {}
        Err(_) => return None,
    }
    for e in entries {
        let Some(name) = session_name(&e.name, home_src) else {
            continue;
        };
        let comps = file::split_path(&name).ok()?;
        // `tmp` is the one writable directory and is made above: an image name may not use it
        if comps[0] == b"tmp" {
            return None;
        }
        let (last, parents) = comps.split_last()?;
        let mut at = ROOT;
        for c in parents {
            at = match fs.lookup(at, c) {
                Ok(id) => id,
                Err(Error::NotFound) => fs.mkdir(at, c).ok()?,
                Err(_) => return None,
            };
        }
        fs.add_image_file(at, last, u64::from(e.offset), u64::from(e.size))
            .ok()?;
    }
    // The skeleton (USER-VIEW-SPEC section 7, UV2): the directories a session's namespace hangs things on, empty here (`/dev`,
    // `/mnt` and `/svc` are mount points other servers are bound over in a program's table; `/lib`, `/include` and `/share`
    // wait for what is installed), and `/usr`, a real directory of compatibility links to the real ones (ported software
    // hardcodes `/usr/bin/env`, `/usr/lib`, ...; nothing is installed under it).
    for name in [&b"dev"[..], b"mnt", b"svc", b"lib", b"include", b"share"] {
        match fs.mkdir(ROOT, name) {
            Ok(_) | Err(Error::Exists) => {}
            Err(_) => return None,
        }
    }
    let usr = fs.mkdir(ROOT, b"usr").ok()?;
    for (name, target) in [
        (&b"bin"[..], &b"../bin"[..]),
        (b"lib", b"../lib"),
        (b"include", b"../include"),
        (b"share", b"../share"),
    ] {
        fs.symlink(usr, name, target, ROOT).ok()?;
    }
    // Everything that came from the boot image is read-only; so is the root (no new top-level
    // names); `/tmp` stays writable.
    fn lock(fs: &mut Fs, dir: NodeId) {
        let names: Vec<(Vec<u8>, Kind)> = fs.read_dir(dir, None, usize::MAX).unwrap_or_default();
        for (name, k) in names {
            if let Ok(id) = fs.lookup(dir, &name) {
                if name != b"tmp" || dir != ROOT {
                    if k == Kind::Dir {
                        lock(fs, id);
                    }
                    let _ = fs.set_readonly(id);
                }
            }
        }
    }
    lock(fs, ROOT);
    let _ = fs.set_readonly(ROOT);
    Some(())
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(root_ch), Some(image_vmo)) =
        (env.handle(Role::ServiceFs), env.handle(Role::BootImage))
    else {
        return 3;
    };
    // Only the table is needed to build the tree: read a prefix of the image.
    let total = rt::sys::vmo_get_size(image_vmo).a1;
    let table_len = total.min(128 * 1024);
    let Some(prefix) = rt::spawn::read_image(image_vmo, 0, table_len as u64) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse_prefix(&prefix, total) else {
        return 5;
    };
    drop(prefix);
    let mut fs = Fs::new(Limits::default());
    // the profile's home: a directory of the image that is `/home` in this session (default `home`)
    let home_src = env.var(b"HOME_SRC").unwrap_or(b"home").to_vec();
    if build_tree(&mut fs, &entries, &home_src).is_none() {
        return 6;
    }
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut server = Server {
        fs,
        core: Core::new(p.a1 as u32, POOL, file::MAX_MESSAGE, 1, HOLD_TIMEOUT),
        image_vmo,
        link_stop: None,
    };
    // The service channel is the root directory connection with every right.
    if server
        .add_conn(svc::ROOT, root_ch, ROOT, pr::ALL, true)
        .is_none()
    {
        return 9;
    }
    service::run(&mut server)
}
