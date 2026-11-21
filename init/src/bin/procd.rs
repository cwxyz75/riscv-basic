//! `procd`: the `/proc` server (USER-VIEW-SPEC section 8, step UV6): a read-only, synthesized directory in front of what a session
//! knows about its processes. It holds a process handle (INSPECT only) for each program the session's shell started and told it
//! about, and the session's job; it can show nothing else. So `/proc` lists the session's own programs and no process outside it
//! (UVI3), and the system files report the session's job, not the machine.
//!
//! ```text
//! /proc/<pid>/status   Name, State (R running, Z exited), Pid, ExitCode
//! /proc/<pid>/stat     `pid (name) R|Z`
//! /proc/<pid>/cmdline  the arguments, each ended by a NUL
//! /proc/meminfo        MemTotal, MemFree, MemAvailable in kB: the session job's pages (a compat rendering of `job_info`)
//! /proc/cpuinfo        one `processor : N` stanza per online hart
//! /proc/uptime         seconds since boot, and an idle time of 0.00
//! /proc/loadavg        `0.00 0.00 0.00 <running>/<known> <last pid>`: the averages are not measured (work item T131)
//! ```
//!
//! The shell sends, on the control channel, `A<pid:u32 LE><arguments NUL-separated>` with a duplicate of the process handle,
//! and `D<pid:u32 LE>` when it has waited for the program (the entry then goes).
//!
//! Handles: `ServiceFs` (the root connection), `Job` (the session job, INSPECT), `ServiceProc` (the control channel).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use proto::file::{self, flags, op, rights as pr, status};
use proto::startup::Role;
use rt::abi::signals;
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
const CTL_KEY: u64 = 1 << 63;
const PAGE_KB: u64 = 4;
const MAX_ENTRIES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PidFile {
    Status,
    Stat,
    Cmdline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SysFile {
    Meminfo,
    Cpuinfo,
    Uptime,
    Loadavg,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Root,
    Pid(u32),
    PidFile(u32, PidFile),
    Sys(SysFile),
}

impl Node {
    fn is_dir(self) -> bool {
        matches!(self, Node::Root | Node::Pid(_))
    }
}

const PID_FILES: [(&[u8], PidFile); 3] = [
    (b"status", PidFile::Status),
    (b"stat", PidFile::Stat),
    (b"cmdline", PidFile::Cmdline),
];
const SYS_FILES: [(&[u8], SysFile); 4] = [
    (b"meminfo", SysFile::Meminfo),
    (b"cpuinfo", SysFile::Cpuinfo),
    (b"uptime", SysFile::Uptime),
    (b"loadavg", SysFile::Loadavg),
];

struct Entry {
    pid: u32,
    args: Vec<u8>,
    handle: u64,
}

struct Conn {
    node: Node,
    rights: u32,
}

struct Procd {
    core: Core<Conn>,
    job: Option<u64>,
    ctl: u64,
    entries: Vec<Entry>,
}

fn parse_pid(name: &[u8]) -> Option<u32> {
    if name.is_empty() || name.len() > 9 || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    let mut n = 0u32;
    for &b in name {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n * 10 + u32::from(b - b'0');
    }
    Some(n)
}

impl Procd {
    fn entry(&self, pid: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.pid == pid)
    }

    /// The program's name: the first argument.
    fn name_of(e: &Entry) -> String {
        let first = e.args.split(|&b| b == 0).next().unwrap_or(b"");
        String::from_utf8_lossy(first).into_owned()
    }

    /// `Some(exit code)` once the program has ended.
    fn exited(e: &Entry) -> Option<i32> {
        let r = sys::process_info(e.handle);
        if r.status == 0 {
            Some(r.a1 as i64 as i32)
        } else {
            None
        }
    }

    fn child(&self, node: Node, name: &[u8]) -> Option<Node> {
        match node {
            Node::Root => {
                if let Some(pid) = parse_pid(name) {
                    return self.entry(pid).map(|_| Node::Pid(pid));
                }
                SYS_FILES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, f)| Node::Sys(*f))
            }
            Node::Pid(p) => PID_FILES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, f)| Node::PidFile(p, *f)),
            _ => None,
        }
    }

    /// Resolve a relative path (components separated by `/`) from `node`.
    fn resolve(&self, node: Node, path: &[u8]) -> Option<Node> {
        let mut cur = node;
        for comp in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            cur = self.child(cur, comp)?;
        }
        Some(cur)
    }

    fn render(&self, node: Node) -> Vec<u8> {
        match node {
            Node::PidFile(pid, f) => {
                let Some(e) = self.entry(pid) else {
                    return Vec::new();
                };
                let name = Self::name_of(e);
                let state = match Self::exited(e) {
                    None => "R",
                    Some(_) => "Z",
                };
                match f {
                    PidFile::Status => {
                        let mut s = format!("Name:\t{name}\nState:\t{state}\nPid:\t{pid}\n");
                        if let Some(c) = Self::exited(e) {
                            s.push_str(&format!("ExitCode:\t{c}\n"));
                        }
                        s.into_bytes()
                    }
                    PidFile::Stat => format!("{pid} ({name}) {state}\n").into_bytes(),
                    PidFile::Cmdline => {
                        let mut v = e.args.clone();
                        v.push(0);
                        v
                    }
                }
            }
            Node::Sys(SysFile::Meminfo) => {
                let (total, free) = match self.job {
                    Some(j) => {
                        let r = sys::job_info(j, 2);
                        if r.status == 0 && r.a2 as u64 != u64::MAX {
                            let (used, limit) = (r.a1 as u64, r.a2 as u64);
                            (limit * PAGE_KB, limit.saturating_sub(used) * PAGE_KB)
                        } else {
                            (0, 0)
                        }
                    }
                    None => (0, 0),
                };
                format!("MemTotal:       {total} kB\nMemFree:        {free} kB\nMemAvailable:   {free} kB\n").into_bytes()
            }
            Node::Sys(SysFile::Cpuinfo) => {
                let n = self.job.map_or(1, |j| {
                    let r = sys::job_info(j, 7);
                    if r.status == 0 {
                        (r.a1 as u64).max(1)
                    } else {
                        1
                    }
                });
                let mut s = String::new();
                for i in 0..n {
                    s.push_str(&format!("processor\t: {i}\n\n"));
                }
                s.into_bytes()
            }
            Node::Sys(SysFile::Uptime) => {
                let ms = sys::clock_get();
                format!("{}.{:02} 0.00\n", ms / 1000, (ms % 1000) / 10).into_bytes()
            }
            Node::Sys(SysFile::Loadavg) => {
                let running = self
                    .entries
                    .iter()
                    .filter(|e| Self::exited(e).is_none())
                    .count();
                let last = self.entries.iter().map(|e| e.pid).max().unwrap_or(0);
                format!("0.00 0.00 0.00 {running}/{} {last}\n", self.entries.len()).into_bytes()
            }
            _ => Vec::new(),
        }
    }

    fn dir_entries(&self, node: Node) -> Vec<file::DirEntry> {
        let mut v = Vec::new();
        match node {
            Node::Root => {
                let mut pids: Vec<u32> = self.entries.iter().map(|e| e.pid).collect();
                pids.sort_unstable();
                for p in pids {
                    v.push(file::DirEntry {
                        is_dir: true,
                        is_link: false,
                        name: format!("{p}").into_bytes(),
                    });
                }
                for (n, _) in SYS_FILES {
                    v.push(file::DirEntry {
                        is_dir: false,
                        is_link: false,
                        name: n.to_vec(),
                    });
                }
            }
            Node::Pid(_) => {
                for (n, _) in PID_FILES {
                    v.push(file::DirEntry {
                        is_dir: false,
                        is_link: false,
                        name: n.to_vec(),
                    });
                }
            }
            _ => {}
        }
        v
    }

    fn send(&mut self, key: u64, txid: u32, st: i32, a: u64, payload: &[u8]) {
        match (file::Reply {
            txid,
            status: st,
            a,
            payload,
        })
        .encode()
        {
            Ok(b) => {
                if self.core.send(key, b, None) == Sent::Closed {
                    self.core.drain_closed();
                }
            }
            Err(_) => self.core.close_conn(key),
        }
    }

    /// The shell's control channel: add and remove entries.
    fn control(&mut self) {
        loop {
            let mut buf = [0u8; 600];
            let mut hs = [0u64; 1];
            let r = sys::channel_read(self.ctl, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), 1);
            if r.status != 0 {
                return;
            }
            let msg = &buf[..r.a1];
            let handle = if r.a2 >= 1 { Some(hs[0]) } else { None };
            match (msg.first(), msg.get(1..5)) {
                (Some(b'A'), Some(p)) if msg.len() >= 5 => {
                    let pid = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                    match handle {
                        Some(h)
                            if self.entries.len() < MAX_ENTRIES && self.entry(pid).is_none() =>
                        {
                            self.entries.push(Entry {
                                pid,
                                args: msg[5..].to_vec(),
                                handle: h,
                            });
                        }
                        Some(h) => {
                            sys::handle_close(h);
                        }
                        None => {}
                    }
                }
                (Some(b'D'), Some(p)) if msg.len() == 5 => {
                    let pid = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                    if let Some(i) = self.entries.iter().position(|e| e.pid == pid) {
                        let e = self.entries.remove(i);
                        sys::handle_close(e.handle);
                    }
                    if let Some(h) = handle {
                        sys::handle_close(h);
                    }
                }
                _ => {
                    if let Some(h) = handle {
                        sys::handle_close(h);
                    }
                }
            }
        }
    }
}

impl Service for Procd {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn other_event(&mut self, key: u64, observed: u32) {
        if key == CTL_KEY && observed & signals::READABLE != 0 {
            self.control();
        }
    }

    fn request(&mut self, key: u64, msg: &[u8], handles: &[u64]) {
        // take what the shell has told us first: a program may ask before the port reported the shell's message
        self.control();
        let ep = handles.first().copied();
        let close_ep = |h: Option<u64>| {
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
        let is_dir = node.is_dir();
        let denied = rt::abi::status::Status::AccessDenied.code();
        let need = |r: u32| rights & r == r;
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
                // a read-only tree: nothing is created or truncated, and no connection may write
                if req.flags & (flags::CREATE | flags::TRUNCATE) != 0 || req.rights & pr::WRITE != 0
                {
                    sys::handle_close(ep);
                    return self.send(key, txid, denied, 0, &[]);
                }
                if !path.is_empty() && file::split_path(path).is_err() {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                }
                let Some(target) = self.resolve(node, path) else {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::NOT_FOUND, 0, &[]);
                };
                if req.flags & flags::DIRECTORY != 0 && !target.is_dir() {
                    sys::handle_close(ep);
                    return self.send(key, txid, status::NOT_DIRECTORY, 0, &[]);
                }
                if target.is_dir() && req.flags & flags::DIRECTORY == 0 && !path.is_empty() {
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
                self.send(key, txid, 0, if target.is_dir() { 2 } else { 1 }, &[]);
            }
            op::READDIR => {
                if !need(pr::ENUMERATE) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                if !req.payload.is_empty() {
                    return self.send(key, txid, 0, 0, &[]);
                }
                let entries = self.dir_entries(node);
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
                    self.resolve(node, req.payload)
                };
                let Some(t) = target else {
                    return self.send(key, txid, status::NOT_FOUND, 0, &[]);
                };
                let info = file::StatInfo {
                    kind: if t.is_dir() { 2 } else { 1 },
                    size: if t.is_dir() {
                        0
                    } else {
                        self.render(t).len() as u64
                    },
                    mtime: 0,
                    mode: if t.is_dir() { 0o555 } else { 0o444 },
                    uid: file::ABSENT,
                    gid: file::ABSENT,
                };
                self.send(key, txid, 0, 0, &info.encode_payload());
            }
            op::GETSIZE => {
                let n = self.render(node).len() as u64;
                self.send(key, txid, 0, n, &[]);
            }
            op::READ => {
                if !need(pr::READ) {
                    return self.send(key, txid, denied, 0, &[]);
                }
                let len = req.b as usize;
                if len > file::MAX_IO {
                    return self.send(key, txid, status::BAD_REQUEST, 0, &[]);
                }
                let data = self.render(node);
                let from = (req.a as usize).min(data.len());
                let to = from.saturating_add(len).min(data.len());
                self.send(key, txid, 0, (to - from) as u64, &data[from..to]);
            }
            _ => {
                // writes, size changes, renames, unlinks: this tree is read-only
                close_ep(ep);
                self.send(key, txid, denied, 0, &[]);
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
    let (Some(root_ch), Some(ctl)) = (env.handle(Role::ServiceFs), env.handle(Role::ServiceProc))
    else {
        return 3;
    };
    let p = sys::port_create();
    if p.status != 0 {
        return 4;
    }
    let port = p.a1 as u64;
    let mut d = Procd {
        core: Core::new(port, POOL, file::MAX_MESSAGE, 1, 30_000),
        job: env.handle(Role::Job),
        ctl,
        entries: Vec::new(),
    };
    sys::port_bind(port, ctl, signals::READABLE, CTL_KEY);
    if d.core
        .add_conn(
            root_ch,
            svc::ROOT,
            Conn {
                node: Node::Root,
                rights: pr::READ | pr::ENUMERATE,
            },
        )
        .is_none()
    {
        return 5;
    }
    service::run(&mut d)
}
