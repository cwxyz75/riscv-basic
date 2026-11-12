//! The memory filesystem server (S4c, S4-SPEC section 7): speaks the file protocol over channels,
//! one connection per open node, on top of the pure `memfs` crate. The boot image supplies
//! `/bin`, `/etc` and `/home` (read-only, backed by the image VMO); `/tmp` is the one writable
//! directory. Single-threaded: one port, every connection bound.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use memfs::{Error, Fs, Kind, Limits, NodeId, ROOT};
use proto::file::{self, flags, op, rights as pr, status, Reply};
use proto::startup::Role;
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::abi::status::Status;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

/// At most this many live connections (one port holds 256 bindings; one is the service channel).
const MAX_CONNECTIONS: usize = 200;
/// A reply the client has no room for is kept this long (ticks) before the connection is dropped.
const PENDING_TIMEOUT: u64 = 30_000;
/// Requests handled per wake-up of one connection (the port rotates, so none starves).
const BATCH: usize = 4;

struct Pending {
    bytes: Vec<u8>,
    handle: Option<u32>,
    deadline: u64,
}

struct Conn {
    ch: u32,
    node: NodeId,
    rights: u32,
    is_dir: bool,
    /// A reply that did not fit the client's queue: no further requests are read until it is sent.
    pending: Option<Pending>,
}

struct Server {
    fs: Fs,
    conns: BTreeMap<u64, Conn>,
    next_key: u64,
    port: u32,
    image_vmo: u32,
    rx: Vec<u8>,
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
        Error::BadName | Error::BadRange => status::BAD_REQUEST,
    }
}

fn kind_byte(k: Kind) -> u8 {
    if k == Kind::Dir {
        2
    } else {
        1
    }
}

impl Server {
    /// Watch a connection for requests, or (`pending`) stop watching it: a connection with a reply
    /// waiting is polled on a timer instead. Its WRITABLE signal cannot be trusted for this: a
    /// channel is "writable" while it has room for *some* message, not for a 32 KiB one, so a
    /// binding on it would be ready all the time and the loop would spin.
    fn bind(&self, key: u64, ch: u32, pending: bool) {
        sys::port_unbind(self.port, key);
        if !pending {
            sys::port_bind(self.port, ch, signals::READABLE | signals::PEER_CLOSED, key);
        }
    }

    /// Start serving `ch` as a connection to `node`. `None` if the port refuses to watch it (the
    /// handle is not a channel, or the port is full): nothing changes then, and the handle is
    /// still the caller's to close.
    fn add_conn(&mut self, ch: u32, node: NodeId, rights: u32, is_dir: bool) -> Option<u64> {
        // Only a channel end we may read and write can be served: probe with a zero-sized read
        // (an event or a VMO answers WRONG_TYPE; a channel says SHOULD_WAIT, PEER_CLOSED or
        // BUFFER_TOO_SMALL and loses nothing).
        let probe = sys::channel_read(ch, core::ptr::null_mut(), 0, core::ptr::null_mut(), 0);
        if matches!(
            probe.status(),
            Some(Status::WrongType) | Some(Status::AccessDenied) | Some(Status::BadHandle)
        ) {
            return None;
        }
        let key = self.next_key;
        let b = sys::port_bind(self.port, ch, signals::READABLE | signals::PEER_CLOSED, key);
        if b.status != 0 {
            return None;
        }
        self.next_key += 1;
        let _ = self.fs.retain(node);
        self.conns.insert(
            key,
            Conn {
                ch,
                node,
                rights,
                is_dir,
                pending: None,
            },
        );
        Some(key)
    }

    fn close_conn(&mut self, key: u64) {
        if let Some(c) = self.conns.remove(&key) {
            sys::port_unbind(self.port, key);
            sys::handle_close(c.ch);
            if let Some(p) = c.pending {
                if let Some(h) = p.handle {
                    sys::handle_close(h);
                }
            }
            self.fs.release(c.node);
        }
    }

    /// Send a reply on `key`'s channel; if the client has no room, hold it and stop reading
    /// that connection until it drains (backpressure: a client that never reads stalls itself).
    fn send(
        &mut self,
        key: u64,
        txid: u32,
        status: i32,
        a: u64,
        payload: &[u8],
        handle: Option<u32>,
    ) {
        let Ok(bytes) = (Reply {
            txid,
            status,
            a,
            payload,
        })
        .encode() else {
            if let Some(h) = handle {
                sys::handle_close(h);
            }
            return;
        };
        let Some(c) = self.conns.get_mut(&key) else {
            if let Some(h) = handle {
                sys::handle_close(h);
            }
            return;
        };
        let hs = [handle.unwrap_or(0)];
        let n = usize::from(handle.is_some());
        let r = sys::channel_write(c.ch, bytes.as_ptr(), bytes.len(), hs.as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => {}
            Some(Status::ShouldWait) | Some(Status::NoMemory) => {
                c.pending = Some(Pending {
                    bytes,
                    handle,
                    deadline: sys::clock_get() + PENDING_TIMEOUT,
                });
                let ch = c.ch;
                self.bind(key, ch, true);
            }
            _ => {
                // the client is gone or the write is hopeless
                if let Some(h) = handle {
                    sys::handle_close(h);
                }
                self.close_conn(key);
            }
        }
    }

    fn retry_pending(&mut self, key: u64) {
        let Some(c) = self.conns.get_mut(&key) else {
            return;
        };
        let Some(p) = c.pending.take() else {
            return;
        };
        let hs = [p.handle.unwrap_or(0)];
        let n = usize::from(p.handle.is_some());
        let r = sys::channel_write(c.ch, p.bytes.as_ptr(), p.bytes.len(), hs.as_ptr(), n);
        match r.status() {
            Some(Status::Ok) => {
                let ch = c.ch;
                self.bind(key, ch, false);
            }
            Some(Status::ShouldWait) | Some(Status::NoMemory) => {
                if sys::clock_get() >= p.deadline {
                    if let Some(h) = p.handle {
                        sys::handle_close(h);
                    }
                    self.close_conn(key);
                } else {
                    c.pending = Some(p);
                }
            }
            _ => {
                if let Some(h) = p.handle {
                    sys::handle_close(h);
                }
                self.close_conn(key);
            }
        }
    }

    fn on_readable(&mut self, key: u64) {
        for _ in 0..BATCH {
            let Some(c) = self.conns.get(&key) else {
                return;
            };
            if c.pending.is_some() {
                return;
            }
            let ch = c.ch;
            let mut hs = [0u32; 1];
            let r = sys::channel_read(ch, self.rx.as_mut_ptr(), self.rx.len(), hs.as_mut_ptr(), 1);
            match r.status() {
                Some(Status::Ok) => {}
                Some(Status::ShouldWait) => return,
                // PEER_CLOSED, BUFFER_TOO_SMALL (a message with more handles than any request
                // may carry, or too many bytes: a protocol violation) or an error
                _ => {
                    self.close_conn(key);
                    return;
                }
            }
            let (nb, nh) = (r.a1, r.a2);
            let endpoint = (nh == 1).then_some(hs[0]);
            let msg = self.rx[..nb].to_vec();
            self.handle_request(key, &msg, endpoint);
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
            let c = &self.conns[&key];
            (c.is_dir, c.rights, c.node)
        };
        if req.op != op::CLONE && op::is_directory_op(req.op) != is_dir {
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
                let st = self.open(
                    key,
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
                match self.fs.read_dir(node, cookie, 1000) {
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
                if !need(pr::READ) {
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
                        let mut buf = alloc::vec![0u8; n];
                        if n > 0 {
                            let r = sys::vmo_read(self.image_vmo, off + start, buf.as_mut_ptr(), n);
                            if r.status != 0 {
                                return self.send(key, txid, r.status, 0, &[], None);
                            }
                        }
                        self.send(key, txid, 0, n as u64, &buf, None);
                    }
                    Ok(_) => match self.fs.read(node, req.a, len) {
                        Ok(data) => {
                            let data = data.to_vec();
                            self.send(key, txid, 0, data.len() as u64, &data, None);
                        }
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

    fn stat_reply(&mut self, key: u64, txid: u32, target: Result<NodeId, Error>) {
        let r = target.and_then(|id| Ok((self.fs.kind(id)?, self.fs.size(id)?)));
        match r {
            Ok((k, size)) => {
                let mut p = alloc::vec![kind_byte(k)];
                p.extend_from_slice(&size.to_le_bytes());
                self.send(key, txid, 0, size, &p, None);
            }
            Err(e) => self.send(key, txid, err_status(e, 0), 0, &[], None),
        }
    }

    /// Open (or clone, with an empty `path`) a node and bind `endpoint` to it. Returns the kind
    /// byte (1 file, 2 directory) or a status.
    #[allow(clippy::too_many_arguments)]
    fn open(
        &mut self,
        _key: u64,
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
        if self.conns.len() >= MAX_CONNECTIONS {
            return Err(status::QUOTA);
        }
        // A clone (or an open of an empty path) names the node it is on: no flag applies.
        let fl = if path.is_empty() { 0 } else { fl };
        let (target, created_new) = if path.is_empty() {
            (dir, false)
        } else {
            let comps = file::split_path(path).map_err(|_| status::BAD_REQUEST)?;
            let (last, parents) = comps.split_last().expect("a split path is never empty");
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
        let Some(key) = self.add_conn(endpoint, target, want, is_dir) else {
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

fn build_tree(fs: &mut Fs, entries: &[proto::bootimg::Entry]) -> Option<()> {
    let tmp = fs.mkdir(ROOT, b"tmp").ok()?;
    let _ = tmp;
    for e in entries {
        let comps = file::split_path(&e.name).ok()?;
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
    if build_tree(&mut fs, &entries).is_none() {
        return 6;
    }
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut server = Server {
        fs,
        conns: BTreeMap::new(),
        next_key: 1,
        port: p.a1 as u32,
        image_vmo,
        rx: alloc::vec![0u8; file::MAX_MESSAGE],
    };
    // The service channel is the root directory connection with every right.
    if server.add_conn(root_ch, ROOT, pr::ALL, true).is_none() {
        return 9;
    }
    loop {
        let any_pending = server.conns.values().any(|c| c.pending.is_some());
        let deadline = if any_pending {
            sys::clock_get() + 10
        } else {
            u64::MAX
        };
        let r = sys::port_wait(server.port, deadline);
        match r.status() {
            Some(Status::Ok) => {
                let (key, observed) = (r.a1 as u64, r.a2 as u32);
                if observed & signals::READABLE != 0 {
                    server.on_readable(key);
                } else if observed & signals::PEER_CLOSED != 0 {
                    server.close_conn(key);
                }
                if server.conns.is_empty() {
                    return 0; // every client is gone, the root connection included
                }
            }
            Some(Status::TimedOut) => {}
            _ => return 8,
        }
        // retry replies that were waiting, drop the ones that waited too long
        let keys: Vec<u64> = server
            .conns
            .iter()
            .filter(|(_, c)| c.pending.is_some())
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            server.retry_pending(k);
        }
    }
}
