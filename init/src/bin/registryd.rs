//! The service registry (slice SD, SD-SPEC.md, OD97): the directory `/svc`. It serves the file protocol as a
//! directory: `Readdir` lists the names a connection's **view** allows, `Open(name)+endpoint` forwards the
//! client's endpoint to the service's **listener** channel, `Publish`/`Unpublish` (the `PUBLISH` right) manage
//! names, `View` makes a narrower connection. A listener that closes makes its entry **stale**: clients then get
//! `UNAVAILABLE` until the service republishes. The registry never sees a conversation, only the first message
//! of each connection.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::file::{self, op, rights as pr, status};
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::{Directory, Lookup, View};

/// The registry's total pool (OD84): connections (clients, views, and the published listeners' own slots are
/// not connections) and held bytes.
const POOL: svc::Limits = svc::Limits {
    conns: 256,
    held_bytes: 256 * 1024,
    inflight: 16,
    stream_bytes: 0,
};
const HOLD_TIMEOUT: u64 = 30_000;
/// Port keys with this bit set are listeners the registry watches (the runtime's connection keys count up
/// from 1 and never reach it).
const LISTENER_KEY: u64 = 1 << 63;

struct Conn {
    view: View,
    rights: u32,
}

struct Registry {
    core: Core<Conn>,
    /// name -> the listener channel handle
    dir: Directory<u32>,
}

fn encode_reply(txid: u32, status: i32, a: u64, payload: &[u8]) -> Option<Vec<u8>> {
    file::Reply {
        txid,
        status,
        a,
        payload,
    }
    .encode()
    .ok()
}

impl Registry {
    fn send(&mut self, key: u64, txid: u32, status: i32, a: u64, payload: &[u8]) {
        match encode_reply(txid, status, a, payload) {
            Some(bytes) => {
                let _ = self.core.send(key, bytes, None);
            }
            None => self.core.close_conn(key),
        }
    }

    fn drop_listener(&self, h: u32) {
        sys::port_unbind(self.core.port(), LISTENER_KEY | u64::from(h));
        sys::handle_close(h);
    }

    /// A new connection to this registry for `endpoint`, with `view` and `rights`, charged to the account of
    /// `from`. `None` if the runtime refuses it (not a channel, pool full): the handle stays the caller's.
    fn add(&mut self, from: u64, endpoint: u32, view: View, rights: u32) -> Option<u64> {
        let account = self.core.account_of(from)?;
        self.core.add_conn(endpoint, account, Conn { view, rights })
    }
}

impl Service for Registry {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn closed(&mut self, _conn: Conn) {}

    /// A listener closed: its name goes stale (the entry stays, so a restart can replace it).
    fn other_event(&mut self, key: u64, observed: u32) {
        if key & LISTENER_KEY == 0 || observed & signals::PEER_CLOSED == 0 {
            return;
        }
        let h = (key & !LISTENER_KEY) as u32;
        let names: Vec<Vec<u8>> = self
            .dir
            .listeners()
            .filter(|(_, l)| **l == h)
            .map(|(n, _)| n.to_vec())
            .collect();
        for n in names {
            self.dir.mark_stale(&n);
        }
        // (stop watching: the closed channel would fire again at once)
        sys::port_unbind(self.core.port(), key);
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        let close_all = |hs: &[u32]| {
            for &h in hs {
                sys::handle_close(h);
            }
        };
        let endpoint = (hs.len() == 1).then(|| hs[0]);
        let Ok(req) = file::decode_request(msg) else {
            close_all(hs);
            match file::peek_txid(msg) {
                Some(t) => self.send(key, t, status::BAD_REQUEST, 0, &[]),
                None => self.core.close_conn(key),
            }
            return;
        };
        let txid = req.txid;
        let want = op::handles(req.op);
        if want.is_none() || want != Some(hs.len()) {
            close_all(hs);
            return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
        }
        let denied = Status::AccessDenied.code();
        let (view, rights) = {
            let c = self
                .core
                .conn(key)
                .expect("a request came from a live connection");
            (c.view.clone(), c.rights)
        };
        match req.op {
            op::OPEN => {
                let ep = endpoint.expect("OPEN carries one handle");
                if rights & pr::READ == 0 || req.rights & !rights != 0 {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                let Ok(name) = core::str::from_utf8(req.payload).map(|s| s.as_bytes()) else {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                };
                let listener = match self.dir.lookup(&view, name) {
                    Ok(l) => *l,
                    Err(Lookup::NotFound) => {
                        sys::handle_close(ep);
                        return self.send(key, txid, status::NOT_FOUND, 0, &[]);
                    }
                    Err(Lookup::Unavailable) => {
                        sys::handle_close(ep);
                        return self.send(key, txid, status::UNAVAILABLE, 0, &[]);
                    }
                };
                // forward the client's request, with its endpoint, to the service: an Open with an empty path
                let fwd = file::Request {
                    txid: 0,
                    op: op::OPEN,
                    flags: req.flags,
                    rights: req.rights,
                    a: 0,
                    b: 0,
                    payload: &[],
                }
                .encode();
                let Ok(bytes) = fwd else {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                };
                let hs1 = [ep];
                let r = sys::channel_write(listener, bytes.as_ptr(), bytes.len(), hs1.as_ptr(), 1);
                match r.status() {
                    Some(Status::Ok) => self.send(key, txid, 0, 1, &[]),
                    Some(Status::PeerClosed) => {
                        // the service is gone: its name goes stale and the client may retry later
                        sys::handle_close(ep);
                        self.dir.mark_stale(name);
                        self.send(key, txid, status::UNAVAILABLE, 0, &[]);
                    }
                    _ => {
                        // the listener's queue is full (or out of memory): nothing is queued here
                        sys::handle_close(ep);
                        self.send(key, txid, status::UNAVAILABLE, 0, &[]);
                    }
                }
            }
            op::READDIR => {
                if rights & pr::ENUMERATE == 0 {
                    return self.send(key, txid, denied, 0, &[]);
                }
                let after = req.payload;
                let entries: Vec<file::DirEntry> = self
                    .dir
                    .list(&view)
                    .into_iter()
                    .filter(|n| after.is_empty() || n.as_slice() > after)
                    .take(128)
                    .map(|name| file::DirEntry {
                        is_dir: false,
                        name,
                    })
                    .collect();
                let (bytes, n) = file::encode_entries(&entries, file::MAX_IO);
                self.send(key, txid, 0, n as u64, &bytes);
            }
            op::CLONE => {
                let ep = endpoint.expect("CLONE carries one handle");
                if req.rights & !rights != 0 {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                match self.add(key, ep, view, req.rights) {
                    Some(_) => self.send(key, txid, 0, 2, &[]),
                    None => {
                        sys::handle_close(ep);
                        self.send(key, txid, status::QUOTA, 0, &[]);
                    }
                }
            }
            op::VIEW => {
                let ep = endpoint.expect("VIEW carries one handle");
                if req.rights & !rights != 0 {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                let names: Vec<&[u8]> = if req.payload.is_empty() {
                    Vec::new()
                } else {
                    req.payload.split(|&b| b == 0).collect()
                };
                let Ok(narrower) = view.restrict(&names) else {
                    // a name that is not allowed (or not valid): a view cannot grant more than its parent
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                };
                match self.add(key, ep, narrower, req.rights) {
                    Some(_) => self.send(key, txid, 0, 2, &[]),
                    None => {
                        sys::handle_close(ep);
                        self.send(key, txid, status::QUOTA, 0, &[]);
                    }
                }
            }
            op::PUBLISH => {
                let ep = endpoint.expect("PUBLISH carries one handle");
                if rights & pr::PUBLISH == 0 {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                if !svc::directory::valid_name(req.payload) {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                }
                // the listener must be something we can watch for closing (a channel end)
                let probe =
                    sys::channel_read(ep, core::ptr::null_mut(), 0, core::ptr::null_mut(), 0);
                if matches!(
                    probe.status(),
                    Some(Status::WrongType) | Some(Status::AccessDenied) | Some(Status::BadHandle)
                ) || sys::port_bind(
                    self.core.port(),
                    ep,
                    signals::PEER_CLOSED,
                    LISTENER_KEY | u64::from(ep),
                )
                .status
                    != 0
                {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                }
                match self.dir.publish(req.payload, ep) {
                    Ok(old) => {
                        if let Some(o) = old {
                            self.drop_listener(o);
                        }
                        self.send(key, txid, 0, 0, &[]);
                    }
                    Err(e) => {
                        sys::port_unbind(self.core.port(), LISTENER_KEY | u64::from(ep));
                        sys::handle_close(ep);
                        let st = match e {
                            svc::Refuse::Quota => status::QUOTA,
                            _ => status::EXISTS,
                        };
                        self.send(key, txid, st, 0, &[]);
                    }
                }
            }
            op::UNPUBLISH => {
                if rights & pr::PUBLISH == 0 {
                    return self.send(key, txid, denied, 0, &[]);
                }
                match self.dir.unpublish(req.payload) {
                    Some(l) => {
                        self.drop_listener(l);
                        self.send(key, txid, 0, 0, &[]);
                    }
                    None => self.send(key, txid, status::NOT_FOUND, 0, &[]),
                }
            }
            _ => {
                close_all(hs);
                self.send(key, txid, status::BAD_REQUEST, 0, &[]);
            }
        }
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let Some(root_ch) = env.handle(Role::ServiceRegistry) else {
        return 3;
    };
    let p = sys::port_create();
    if p.status != 0 {
        return 7;
    }
    let mut reg = Registry {
        core: Core::new(p.a1 as u32, POOL, file::MAX_MESSAGE, 1, HOLD_TIMEOUT),
        dir: Directory::new(),
    };
    // the service channel is the full directory connection, with every right
    if reg
        .core
        .add_conn(
            root_ch,
            svc::ROOT,
            Conn {
                view: View::all(),
                rights: pr::ALL,
            },
        )
        .is_none()
    {
        return 9;
    }
    service::run(&mut reg)
}
