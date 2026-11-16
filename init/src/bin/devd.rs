//! `devd`: the `/dev` server (USER-VIEW-SPEC sections 7 and 8, step UV2): a directory of small byte-stream files with a POSIX
//! flavour: `null` (reads end at once, writes vanish), `zero` (reads are zeros, writes vanish) and, if the random service is
//! reachable, `urandom` (bytes from `/svc/random`). It speaks the file protocol like any other server: a program is given a
//! connection to its root at `/dev` in its namespace table. Hardware stays behind services (`devmgr`), not `/dev` nodes.
//!
//! Handles: `ServiceFs` (the root directory connection), optionally `ServiceRegistry` (a view that allows `random`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::file::{self, flags, op, rights as pr, status};
use proto::startup::Role;
use rt::service::{self, Core, Sent, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::Limits;

const POOL: Limits = Limits {
    conns: 128,
    held_bytes: 256 * 1024,
    inflight: 16,
    stream_bytes: 0,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Root,
    Null,
    Zero,
    Urandom,
}

struct Conn {
    node: Node,
    rights: u32,
}

struct Devd {
    core: Core<Conn>,
    random: Option<rt::random::Random>,
}

impl Devd {
    fn names(&self) -> Vec<(&'static [u8], Node)> {
        let mut v = alloc::vec![(&b"null"[..], Node::Null), (&b"zero"[..], Node::Zero)];
        if self.random.is_some() {
            v.push((b"urandom", Node::Urandom));
        }
        v
    }

    fn send(&mut self, key: u64, txid: u32, st: i32, a: u64, payload: &[u8]) {
        let bytes = file::Reply {
            txid,
            status: st,
            a,
            payload,
        }
        .encode();
        match bytes {
            Ok(b) => {
                if self.core.send(key, b, None) == Sent::Closed {
                    self.core.drain_closed();
                }
            }
            Err(_) => self.core.close_conn(key),
        }
    }
}

impl Service for Devd {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], handles: &[u32]) {
        let ep = handles.first().copied();
        let close_ep = |h: Option<u32>| {
            if let Some(h) = h {
                sys::handle_close(h);
            }
        };
        let Ok(req) = file::decode_request(msg) else {
            close_ep(ep);
            match file::peek_txid(msg) {
                Some(t) => self.send(key, t, status::BAD_REQUEST, 0, &[]),
                None => self.core.close_conn(key),
            }
            return;
        };
        let txid = req.txid;
        if op::handles(req.op) != Some(usize::from(ep.is_some())) {
            close_ep(ep);
            return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
        }
        let (node, rights) = {
            let c = self
                .core
                .conn(key)
                .expect("a request came from a live connection");
            (c.node, c.rights)
        };
        let is_dir = node == Node::Root;
        let denied = rt::abi::status::Status::AccessDenied.code();
        let need = |r: u32| rights & r == r;
        // directory-only and file-only operations
        if req.op != op::CLONE && op::is_directory_op(req.op) != is_dir {
            close_ep(ep);
            let st = if is_dir {
                status::IS_DIRECTORY
            } else {
                status::NOT_DIRECTORY
            };
            return self.send(key, txid, st, 0, &[]);
        }
        match req.op {
            op::OPEN | op::CLONE => {
                let path: &[u8] = if req.op == op::OPEN { req.payload } else { &[] };
                let Some(ep) = ep else {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                };
                if (req.op == op::OPEN && !need(pr::READ)) || req.rights & !rights != 0 {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                // nothing here may be created or truncated, whatever the name
                if req.flags & (flags::CREATE | flags::TRUNCATE) != 0 && !path.is_empty() {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                let target = if path.is_empty() {
                    Some(node)
                } else {
                    let found = self
                        .names()
                        .into_iter()
                        .find(|(n, _)| *n == path)
                        .map(|(_, n)| n);
                    match found {
                        None if file::split_path(path).is_err() => {
                            sys::handle_close(ep);
                            return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                        }
                        n => n,
                    }
                };
                let Some(target) = target else {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::NOT_FOUND, 0, &[]);
                };
                let target_is_dir = target == Node::Root;
                if req.flags & flags::DIRECTORY != 0 && !target_is_dir {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::NOT_DIRECTORY, 0, &[]);
                }
                if target_is_dir && req.flags & flags::DIRECTORY == 0 && !path.is_empty() {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::IS_DIRECTORY, 0, &[]);
                }
                let account = self
                    .core
                    .account_of(key)
                    .expect("a live connection has an account");
                if self
                    .core
                    .add_conn(
                        ep,
                        account,
                        Conn {
                            node: target,
                            rights: req.rights,
                        },
                    )
                    .is_none()
                {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::QUOTA, 0, &[]);
                }
                self.send(key, txid, 0, if target_is_dir { 2 } else { 1 }, &[]);
            }
            op::READDIR => {
                if !need(pr::ENUMERATE) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                if !req.payload.is_empty() {
                    // (the directory is short: one page always holds it, so a continuation has nothing more)
                    return self.send(key, txid, 0, 0, &[]);
                }
                let entries: Vec<file::DirEntry> = self
                    .names()
                    .into_iter()
                    .map(|(n, _)| file::DirEntry {
                        is_dir: false,
                        name: n.to_vec(),
                    })
                    .collect();
                let (bytes, n) = file::encode_entries(&entries, file::MAX_IO);
                self.send(key, txid, 0, n as u64, &bytes);
            }
            op::STAT | op::FSTAT => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                let target = if req.op == op::FSTAT || req.payload.is_empty() {
                    Some(node)
                } else {
                    self.names()
                        .into_iter()
                        .find(|(n, _)| *n == req.payload)
                        .map(|(_, n)| n)
                };
                let Some(t) = target else {
                    return self.send(key, txid, status::NOT_FOUND, 0, &[]);
                };
                let mut p = alloc::vec![if t == Node::Root { 2u8 } else { 1u8 }];
                p.extend_from_slice(&0u64.to_le_bytes());
                self.send(key, txid, 0, 0, &p);
            }
            op::GETSIZE => self.send(key, txid, 0, 0, &[]),
            op::SETSIZE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                self.send(key, txid, 0, 0, &[]);
            }
            op::READ => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                let len = req.b as usize;
                if len > file::MAX_IO {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                }
                match node {
                    Node::Null => self.send(key, txid, 0, 0, &[]),
                    Node::Zero => {
                        let z = alloc::vec![0u8; len];
                        self.send(key, txid, 0, len as u64, &z);
                    }
                    Node::Urandom => {
                        let mut buf = alloc::vec![0u8; len];
                        let ok = match self.random.as_mut() {
                            Some(r) => r.fill(&mut buf, sys::clock_get() + 500).is_ok(),
                            None => false,
                        };
                        if ok {
                            self.send(key, txid, 0, len as u64, &buf);
                        } else {
                            self.send(key, txid, status::IO_ERROR, 0, &[]);
                        }
                    }
                    Node::Root => self.send(key, txid, status::IS_DIRECTORY, 0, &[]),
                }
            }
            op::WRITE => {
                if !need(pr::WRITE) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                // everything written to a device file here is accepted and dropped
                match node {
                    Node::Null | Node::Zero => {
                        self.send(key, txid, 0, req.payload.len() as u64, &[])
                    }
                    _ => self.send(key, txid, denied, 0, &[]),
                }
            }
            _ => {
                close_ep(ep);
                self.send(key, txid, status::BAD_REQUEST, 0, &[]);
            }
        }
    }

    fn closed(&mut self, _conn: Conn) {}
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let Some(root_ch) = env.handle(Role::ServiceFs) else {
        return 3;
    };
    // `urandom` exists only if the random service answers
    let random = env.handle(Role::ServiceRegistry).and_then(|h| {
        let mut v = rt::fs::Fs::new(h);
        rt::random::Random::connect(&mut v, sys::clock_get() + 300).ok()
    });
    let p = sys::port_create();
    if p.status != 0 {
        return 4;
    }
    let mut d = Devd {
        core: Core::new(p.a1 as u32, POOL, file::MAX_MESSAGE, 1, 30_000),
        random,
    };
    if d.core
        .add_conn(
            root_ch,
            svc::ROOT,
            Conn {
                node: Node::Root,
                rights: pr::ALL,
            },
        )
        .is_none()
    {
        return 5;
    }
    service::run(&mut d)
}
