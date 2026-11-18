//! The shell (S4e, S4-SPEC section 8; step SH4): `ish`, the native shell (`SHELL-SPEC.md`), running on this machine. It reads
//! lines from the console server's terminal socket and runs each with the `ish` evaluator over [`Os`], the host: starting a
//! program means the launcher with only what it needs (STDIN/STDOUT/STDERR, a read-only working directory, `/dev`, a private
//! `/tmp`, and for the programs named in `/etc/trusted` a read-only root and their volumes), and the stages of a pipeline
//! are registered with the console server as the foreground job so Ctrl-C reaches them.
//!
//! The S4e behaviour the suite checks is kept by the evaluator's compatibility profile (`legacy`: `sh:` messages, one
//! `[exit N]` per failed stage, `cd` printing its own errors); TIDY-UP records what it papers over.
//!
//! Known gap (TIDY-UP T31): a Ctrl-C between a stage starting and the registration being acknowledged is handled by reading
//! ahead after the registration, not by the specified start gate.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ish::host::{Host, HostError, OpenMode, Pid, ProcIo, Stream, TermIn, WaitOutcome};
use proto::console::{self as ctl, Control};
use proto::file::{flags, rights};
use proto::startup::{Role, Startup};
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::abi::status::Status;
use rt::fs::Fs;
use rt::io;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

const BANNER: &[u8] = b"riscv-os S4 ready\n";
const HELP: &str =
    "builtins: cd pwd exit help halt\nprograms: ls cat echo probe spin hog bomb ipchog\n";
const KILLED: i32 = -130;

/// What a `Stream` the shell hands to `ish` stands for (0, 1 and 2 are the terminal).
enum Obj {
    PipeR(u32),
    PipeW(u32),
    FileR {
        data: Vec<u8>,
        pos: usize,
    },
    FileW {
        path: String,
        buf: Vec<u8>,
        append: bool,
        /// A program writes it through a socket the shell drains (`pumps_out`): the file is stored when that ends.
        piped: bool,
    },
}

/// A program's standard output redirected to a file: the shell holds the read end of a socket the program writes to and stores
/// what arrives when the program (every writer) is gone (UV8 follow-up, T126 item 2).
struct PumpOut {
    rd: u32,
    path: String,
    append: bool,
    buf: Vec<u8>,
}

/// A program's standard input from a file: the shell feeds the write end of a socket and closes it at the end of the data.
struct PumpIn {
    wr: u32,
    data: Vec<u8>,
    pos: usize,
}

/// The machine, as `ish` sees it.
struct Os {
    pumps_out: Vec<PumpOut>,
    pumps_in: Vec<PumpIn>,
    tty: u32,
    root: Fs,
    cwd: Fs,
    cwd_path: Vec<u8>,
    launch: u32,
    ctl: u32,
    init_ch: u32,
    trusted: Vec<Trust>,
    /// A read connection to the service registry (volumes are opened as `vol-<name>`), if the session has one.
    registry: Option<Fs>,
    /// A writable connection to `/tmp`, where each program gets a private directory for the length of its run, and a
    /// connection to the `/dev` server (UV2).
    tmp: Option<Fs>,
    dev: Option<Fs>,
    /// `/proc` (UV6): a connection to `procd`'s root, and the channel the shell tells it about its programs on.
    proc_root: Option<Fs>,
    proc_ctl: Option<u32>,
    jobs: u32,
    /// The private directory just made for the program being started (taken by `spawn`).
    new_tmp: Option<Vec<u8>>,
    /// Each running program's private directory, removed when it is reaped.
    tmp_of: BTreeMap<u32, Vec<u8>>,
    txid: u32,
    inbuf: Vec<u8>,
    objs: BTreeMap<u32, Obj>,
    next_stream: u32,
    procs: BTreeMap<u32, u32>,
    next_pid: u32,
    killed: bool,
    /// The session's profile (UV5): its program grants replace `/etc/trusted`, and its identity is in every program's environment.
    session_env: Vec<(String, String)>,
    /// `confine` grants, innermost last: while set, programs get exactly these (and `.`, `/dev`, `/tmp`).
    confined: Vec<Vec<(String, String)>>,
    /// The terminal is in raw mode (the shell is editing a line); programs run with it cooked.
    raw: bool,
    /// Ctrl-C was reported while the shell was not looking (an event on the control channel).
    interrupt: bool,
    /// An interrupt event arrived with typed bytes still in the terminal socket: the bytes are read first, then the interrupt.
    late_interrupt: bool,
    /// There is a console server: interactive mode.
    interactive: bool,
    /// The pids of the foreground job (the stages of the running pipeline), for Ctrl-C.
    fg_pids: Vec<u32>,
    /// Programs may start programs (OD118): the default; each program's channel is below.
    spawn_grant: bool,
    brokers: Vec<Broker>,
    /// The broker made for the program being started (attached to its pid by the caller).
    pending_broker: Option<Broker>,
    /// Programs started by programs, by pid.
    desc: BTreeMap<u32, Desc>,
}

/// A volume granted to a trusted program (`/etc/trusted`, FS-SPEC section 6): the volume's name in the mount table, the
/// path it appears at in the program's namespace (`/mnt/<name>` unless the entry says otherwise) and whether it is read-only.
struct Grant {
    mount: Vec<u8>,
    vol: Vec<u8>,
    readonly: bool,
}

/// One line of `/etc/trusted`: `program [volume[:ro] | /path=volume[:ro]]...`. The program gets a read-only root; each volume
/// is opened from the registry as `vol-<volume>` and granted at its mount point.
struct Trust {
    name: Vec<u8>,
    grants: Vec<Grant>,
    /// The program gets a read-only root (`/etc/trusted` always gives one; a profile may not).
    root: bool,
}

/// How a program's standard streams are given: shell streams (a command line), or handles a program passed in a spawn request
/// (which move to the new program).
#[derive(Clone, Copy)]
enum Stdio {
    Streams(ProcIo),
    Handles([u32; 3]),
}

/// A spawn request's handles go away with a start that fails before it uses them.
fn drop_stdio(stdio: Stdio) {
    if let Stdio::Handles(h) = stdio {
        close_all(&h);
    }
}

/// How deep programs may start programs (below the shell's own), and how many a started program may have alive at once.
const SPAWN_DEPTH: u8 = 4;
const SPAWN_LIVE: usize = 32;

/// The shell's end of the spawn channel of one running program (OD118): what the program holds (so a child never holds more),
/// where it was started from, and how deep in the chain it is.
struct Broker {
    /// The program this channel belongs to (a shell pid, or a descendant's).
    pid: u32,
    /// The program the shell's command line started, the root of this chain.
    root: u32,
    ch: u32,
    depth: u8,
    cwd: Fs,
    cwd_path: Vec<u8>,
    /// Each mount in the program's namespace and whether it is writable.
    mounts: Vec<(Vec<u8>, bool)>,
    /// The shell's end of the channel the program receives signals on (S11, OD119).
    sig: u32,
    /// The `confine` grants the program was started under: what its children are confined to, not the shell's present block.
    confined: Option<Vec<(String, String)>>,
    /// The private `/tmp` directory of this chain (its root's: what a program starts, it shares it, so a script's files are
    /// visible to the programs it runs).
    tmp: Option<Vec<u8>>,
    /// The program has said it handles SIGINT (a handler or ignoring it): Ctrl-C is passed on to it as a signal.
    handles_int: bool,
}

/// A program a program started (the shell holds its process handle too: it reaps it and ends it with its chain's root).
struct Desc {
    h: u32,
    root: u32,
}

/// May a program that holds `allow` pass on `name` at these rights? (No `allow` means the shell is starting it.)
fn allowed(allow: Option<&[(Vec<u8>, bool)]>, name: &[u8], writable: bool) -> bool {
    match allow {
        None => true,
        Some(a) => a.iter().any(|(n, w)| n == name && (*w || !writable)),
    }
}

/// Add a namespace entry unless the asking program does not hold it; record it for the new program's own spawn grant.
fn ns_push(
    startup: &mut Startup,
    forward: &mut Vec<u32>,
    mounts: &mut Vec<(Vec<u8>, bool)>,
    allow: Option<&[(Vec<u8>, bool)]>,
    name: &[u8],
    ch: u32,
    writable: bool,
) {
    if !allowed(allow, name, writable) {
        sys::handle_close(ch);
        return;
    }
    startup
        .namespace
        .push((name.to_vec(), forward.len() as u32));
    forward.push(ch);
    mounts.push((name.to_vec(), writable));
}

fn parse_trust(line: &[u8]) -> Option<Trust> {
    let mut words = line
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|w| !w.is_empty());
    let name = words.next()?.to_vec();
    let mut grants = Vec::new();
    for w in words {
        let (path, vol) = match w.iter().position(|&b| b == b'=') {
            Some(i) => (Some(&w[..i]), &w[i + 1..]),
            None => (None, w),
        };
        let (vol, readonly) = match vol.strip_suffix(b":ro") {
            Some(v) => (v, true),
            None => (vol, false),
        };
        if vol.is_empty() {
            return None;
        }
        let mount = match path {
            Some(p) => p.to_vec(),
            None => [&b"/mnt/"[..], vol].concat(),
        };
        if !proto::startup::valid_prefix(&mount) || mount == b"/" || mount == b"." {
            return None;
        }
        grants.push(Grant {
            mount,
            vol: vol.to_vec(),
            readonly,
        });
    }
    Some(Trust {
        name,
        grants,
        root: true,
    })
}

/// Remove the directory `name` of `parent` and everything below it (a dead program's scratch space).
fn remove_tree(parent: &mut Fs, name: &[u8], depth: u32) {
    if depth < 16 {
        let want = rights::READ | rights::WRITE | rights::CREATE | rights::ENUMERATE;
        if let Ok(mut d) = parent.open(name, flags::DIRECTORY, want) {
            if let Ok(list) = d.read_dir() {
                for e in list {
                    if e.is_dir {
                        remove_tree(&mut d, &e.name, depth + 1);
                    } else {
                        let _ = d.unlink(&e.name);
                    }
                }
            }
            d.close();
        }
    }
    let _ = parent.unlink(name);
}

fn say(tty: u32, bytes: &[u8]) {
    let _ = io::write_all(tty, bytes);
}

/// Duplicate with fewer rights (the original stays).
fn dup(h: u32, rights: u32) -> Option<u32> {
    let d = sys::handle_duplicate(h, rights);
    (d.status == 0).then_some(d.a1 as u32)
}

fn close_all(hs: &[u32]) {
    for &h in hs {
        sys::handle_close(h);
    }
}

fn host_err(e: i32) -> HostError {
    if e == proto::file::status::NOT_FOUND {
        HostError::NotFound
    } else {
        HostError::Other(format!("error {e}"))
    }
}

impl Os {
    fn say(&self, bytes: &[u8]) {
        say(self.tty, bytes);
    }

    /// Put the terminal in raw mode, where the shell edits the line itself and Ctrl-C is an event (no echo, no line
    /// editing in the console server).
    fn go_raw(&mut self) {
        if self.interactive && !self.raw {
            self.raw = self.control(Control::SetMode(ctl::mode::ISIG), &[]);
        }
    }

    /// Give the terminal back to the console server's line discipline, for the programs the shell starts.
    fn go_cooked(&mut self) {
        if self.interactive && self.raw {
            self.control(Control::SetMode(ctl::mode::COOKED), &[]);
            self.raw = false;
        }
    }

    /// Look at the control channel without waiting: an interrupt event sets the flag.
    fn poll_ctl(&mut self) {
        if !self.interactive {
            return;
        }
        loop {
            let mut rb = [0u8; 16];
            let r = sys::channel_read(
                self.ctl,
                rb.as_mut_ptr(),
                rb.len(),
                core::ptr::null_mut(),
                0,
            );
            if r.status != 0 {
                return;
            }
            match ctl::decode_event(&rb[..r.a1]) {
                Some(ctl::Event::Interrupt) => self.interrupt = true,
                Some(ctl::Event::JobInterrupt) => self.sigint_foreground(),
                _ => {}
            }
        }
    }

    /// Ctrl-C reached a running job: the console server has ended the programs on the list it was given; the ones that handle
    /// SIGINT were kept off it, and get the signal here.
    fn sigint_foreground(&mut self) {
        // (the console server also leaves the byte 0x03 on the terminal to wake an idle shell: it is swallowed when the job ends)
        self.killed = true;
        for pid in self.fg_pids.clone() {
            if let Some(b) = self.brokers.iter().find(|b| b.pid == pid && b.handles_int) {
                sys::channel_write(b.sig, [2u8].as_ptr(), 1, core::ptr::null(), 0);
            }
        }
    }

    /// The programs of the foreground job that do not handle SIGINT: what the console server may end on Ctrl-C.
    fn register_foreground(&mut self) -> bool {
        let job_rights = Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
        let hs: Vec<u32> = self
            .fg_pids
            .iter()
            .filter(|&&pid| !self.brokers.iter().any(|b| b.pid == pid && b.handles_int))
            .filter_map(|pid| self.procs.get(pid).copied())
            .collect();
        let dups: Vec<u32> = hs.iter().filter_map(|&p| dup(p, job_rights)).collect();
        if dups.len() != hs.len() {
            close_all(&dups);
            return false;
        }
        if dups.is_empty() {
            // (a job runs, and none of it is for the console server to end)
            return self.control(Control::HoldForeground, &[]);
        }
        self.control(Control::SetForeground, &dups)
    }

    /// What the programs have said to the shell on their signal channels (never blocks): that they handle SIGINT, or no longer do.
    fn service_signal_notes(&mut self) {
        let mut changed = false;
        for b in self.brokers.iter_mut() {
            loop {
                let mut m = [0u8; 8];
                let r = sys::channel_read(b.sig, m.as_mut_ptr(), m.len(), core::ptr::null_mut(), 0);
                if r.status != 0 {
                    break;
                }
                if r.a1 == 3 && m[0] == b'H' && m[1] == 2 {
                    let now = m[2] != 0;
                    if now != b.handles_int {
                        b.handles_int = now;
                        changed |= self.fg_pids.contains(&b.pid);
                    }
                }
            }
        }
        if changed && self.interactive {
            self.register_foreground();
        }
    }

    /// Take whatever the terminal has for us right now, without waiting.
    fn read_ahead(&mut self) {
        loop {
            let mut buf = [0u8; 256];
            let r = sys::socket_read(self.tty, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(Status::Ok) if r.a1 > 0 => self.inbuf.extend_from_slice(&buf[..r.a1]),
                _ => return,
            }
        }
    }

    /// After a Ctrl-C killed a job the console server also sends the one-byte 0x03 that wakes an
    /// idle shell; this shell has already reported, so swallow it (bounded wait).
    fn swallow_interrupt(&mut self) {
        let deadline = sys::clock_get() + 500;
        loop {
            if let Some(i) = self.inbuf.iter().position(|&b| b == 0x03) {
                self.inbuf.remove(i);
                return;
            }
            if sys::clock_get() >= deadline {
                return;
            }
            let w = sys::object_wait(self.tty, signals::READABLE | signals::PEER_CLOSED, deadline);
            if w.status != 0 {
                return;
            }
            let mut buf = [0u8; 64];
            let r = sys::socket_read(self.tty, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(Status::Ok) => self.inbuf.extend_from_slice(&buf[..r.a1]),
                Some(Status::ShouldWait) => {}
                _ => return,
            }
        }
    }

    /// Register (or clear, with no handles) the foreground job; true when acknowledged.
    /// `GetMode` and `GetSize`: the console server's answer (its reply carries a value).
    fn control_query(&mut self, what: Control) -> Option<u32> {
        self.txid = self.txid.wrapping_add(1);
        let msg = ctl::encode(self.txid, what);
        if sys::channel_write(self.ctl, msg.as_ptr(), msg.len(), core::ptr::null(), 0).status != 0 {
            return None;
        }
        let deadline = sys::clock_get() + 2000;
        loop {
            let mut rb = [0u8; 16];
            let r = sys::channel_read(
                self.ctl,
                rb.as_mut_ptr(),
                rb.len(),
                core::ptr::null_mut(),
                0,
            );
            if r.status == 0 {
                if let Some((t, st, v)) = ctl::decode_reply_value(&rb[..r.a1]) {
                    if t == self.txid {
                        return (st == 0).then_some(v);
                    }
                    continue;
                }
                match ctl::decode_event(&rb[..r.a1]) {
                    Some(ctl::Event::Interrupt) => self.interrupt = true,
                    Some(ctl::Event::JobInterrupt) => self.sigint_foreground(),
                    _ => {}
                }
                continue;
            }
            if r.status() != Some(Status::ShouldWait)
                || sys::object_wait(self.ctl, signals::READABLE | signals::PEER_CLOSED, deadline)
                    .status
                    != 0
            {
                return None;
            }
        }
    }

    fn control(&mut self, what: Control, handles: &[u32]) -> bool {
        self.txid = self.txid.wrapping_add(1);
        let msg = ctl::encode(self.txid, what);
        let w = sys::channel_write(
            self.ctl,
            msg.as_ptr(),
            msg.len(),
            handles.as_ptr(),
            handles.len(),
        );
        if w.status != 0 {
            close_all(handles);
            return false;
        }
        let deadline = sys::clock_get() + 2000;
        loop {
            let mut rb = [0u8; 16];
            let r = sys::channel_read(
                self.ctl,
                rb.as_mut_ptr(),
                rb.len(),
                core::ptr::null_mut(),
                0,
            );
            if r.status == 0 {
                match ctl::decode_reply(&rb[..r.a1]) {
                    Some((t, st)) if t == self.txid => return st == 0,
                    Some(_) => continue, // a late reply to an earlier call: skip it
                    None => {
                        match ctl::decode_event(&rb[..r.a1]) {
                            Some(ctl::Event::Interrupt) => self.interrupt = true,
                            Some(ctl::Event::JobInterrupt) => self.sigint_foreground(),
                            _ => {}
                        }
                        continue;
                    }
                }
            }
            if r.status() != Some(Status::ShouldWait) {
                return false;
            }
            let w = sys::object_wait(self.ctl, signals::READABLE | signals::PEER_CLOSED, deadline);
            if w.status != 0 {
                return false;
            }
        }
    }

    /// The session's profile: the program grants and the identity it gives every program.
    fn load_profile(&mut self, text: &str) -> bool {
        let Ok(p) = profile::Profile::parse(text) else {
            return false;
        };
        self.trusted = p
            .programs
            .iter()
            .map(|(name, g)| Trust {
                name: name.as_bytes().to_vec(),
                root: g.root,
                grants: g
                    .mounts
                    .iter()
                    .map(|m| Grant {
                        mount: m.path.as_bytes().to_vec(),
                        vol: m.volume.as_bytes().to_vec(),
                        readonly: m.rights == profile::Rights::Read,
                    })
                    .collect(),
            })
            .collect();
        self.session_env = p.session_env();
        self.spawn_grant = p.spawn;
        true
    }

    fn load_trusted(&mut self) {
        if let Ok(mut f) = self.root.open(b"etc/trusted", 0, rights::READ) {
            if let Ok(data) = f.read_all() {
                self.trusted = data
                    .split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty() && l[0] != b'#')
                    .filter_map(parse_trust)
                    .collect();
            }
            f.close();
        }
    }

    /// Tell `procd` about a program (it gets an INSPECT-only duplicate of the process handle).
    fn proc_announce(&mut self, pid: u32, h: u32, argv: &[Vec<u8>]) {
        let Some(ctl) = self.proc_ctl else {
            return;
        };
        let Some(dup_h) = dup(h, Rights::INSPECT.0 | Rights::TRANSFER.0) else {
            return;
        };
        let mut msg = alloc::vec![b'A'];
        msg.extend_from_slice(&pid.to_le_bytes());
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                msg.push(0);
            }
            msg.extend_from_slice(a);
        }
        msg.truncate(500);
        let hs = [dup_h];
        if sys::channel_write(ctl, msg.as_ptr(), msg.len(), hs.as_ptr(), 1).status != 0 {
            sys::handle_close(dup_h);
        }
    }

    /// The program has been waited for: `procd` forgets it.
    fn proc_forget(&mut self, pid: u32) {
        let Some(ctl) = self.proc_ctl else {
            return;
        };
        let mut msg = alloc::vec![b'D'];
        msg.extend_from_slice(&pid.to_le_bytes());
        sys::channel_write(ctl, msg.as_ptr(), msg.len(), core::ptr::null(), 0);
    }

    /// Remove the private `/tmp` directory of a program that has been reaped.
    fn cleanup_tmp(&mut self, pid: u32) {
        if let Some(n) = self.tmp_of.remove(&pid) {
            if let Some(t) = self.tmp.as_mut() {
                remove_tree(t, &n, 0);
            }
        }
    }

    /// A program has ended: its exit code, its handle closed and its `/tmp` removed.
    fn reap(&mut self, pid: u32, h: u32) -> i32 {
        let code = if sys::object_wait(h, signals::TERMINATED, 0).status == 0 {
            sys::process_info(h).a1 as i64 as i32
        } else {
            -1
        };
        if code == KILLED {
            self.killed = true;
        }
        sys::handle_close(h);
        self.end_chain(pid);
        self.drop_broker(pid);
        self.cleanup_tmp(pid);
        self.proc_forget(pid);
        code
    }

    /// Give the broker made for a program just started its pid.
    fn attach_broker(&mut self, pid: u32, root: u32) {
        if let Some(mut b) = self.pending_broker.take() {
            b.pid = pid;
            b.root = root;
            self.brokers.push(b);
        }
    }

    /// A program is gone: its spawn channel closes.
    fn drop_broker(&mut self, pid: u32) {
        if let Some(i) = self.brokers.iter().position(|b| b.pid == pid) {
            let b = self.brokers.remove(i);
            close_all(&[b.ch, b.sig]);
            b.cwd.close();
        }
    }

    /// A program a program started is gone (or ended): forget it everywhere.
    fn forget_desc(&mut self, id: u32) {
        if let Some(d) = self.desc.remove(&id) {
            sys::handle_close(d.h);
        }
        self.cleanup_tmp(id);
        self.proc_forget(id);
        self.drop_broker(id);
    }

    /// The programs that ended on their own.
    fn reap_desc(&mut self) {
        let done: Vec<u32> = self
            .desc
            .iter()
            .filter(|(_, d)| sys::object_wait(d.h, signals::TERMINATED, 0).status == 0)
            .map(|(&id, _)| id)
            .collect();
        for id in done {
            self.forget_desc(id);
        }
    }

    /// A started program has ended: whatever it started and left running ends with it (there are no orphans).
    fn end_chain(&mut self, root: u32) {
        let ids: Vec<u32> = self
            .desc
            .iter()
            .filter(|(_, d)| d.root == root)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            if let Some(d) = self.desc.get(&id) {
                sys::process_kill(d.h, -1);
            }
            self.forget_desc(id);
        }
    }

    /// Answer the spawn requests programs have sent (never blocks; at most a few per program per call, so a program that floods
    /// its channel cannot keep the shell from looking at anything else). A program that closes its channel, or speaks the
    /// protocol wrongly, loses it.
    fn service_brokers(&mut self) {
        if self.brokers.is_empty() {
            return;
        }
        let mut buf = alloc::vec![0u8; proto::spawn::MAX_BYTES];
        let mut i = 0;
        while i < self.brokers.len() {
            let mut served = 0;
            let mut keep = true;
            while served < 4 {
                let mut hs = [0u32; proto::spawn::HANDLES + 1];
                let ch = self.brokers[i].ch;
                let r =
                    sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), hs.len());
                match r.status() {
                    Some(Status::Ok) => {}
                    Some(Status::ShouldWait) => break,
                    _ => {
                        // the program closed it, or sent what does not fit: no more requests from it
                        keep = false;
                        break;
                    }
                }
                served += 1;
                if !self.answer_spawn(i, &buf[..r.a1], &hs[..r.a2]) {
                    keep = false;
                    break;
                }
            }
            if keep {
                i += 1;
            } else {
                let b = self.brokers.remove(i);
                close_all(&[b.ch, b.sig]);
                b.cwd.close();
            }
        }
    }

    /// Serve everything the shell serves while it waits: output pumps, spawn requests, ended programs, and the chains of
    /// programs that have ended (what a program started does not outlive it, whether or not the shell has waited for it yet).
    fn serve(&mut self) {
        self.service_signal_notes();
        self.service_pumps();
        self.service_brokers();
        self.reap_desc();
        let dead: Vec<u32> = self
            .procs
            .iter()
            .filter(|(id, h)| {
                self.desc.values().any(|d| d.root == **id)
                    && sys::object_wait(**h, signals::TERMINATED, 0).status == 0
            })
            .map(|(&id, _)| id)
            .collect();
        for id in dead {
            self.end_chain(id);
        }
    }

    /// Watch what the shell serves on `port`: every program's spawn channel, every program a program started, every output pump.
    /// (A port's keys are unique: each binding gets its own, from 1000 up; a repeated key is refused, and the object is not
    /// watched.)
    fn bind_served(&self, port: u32) {
        let mut key = 1000;
        for b in &self.brokers {
            sys::port_bind(port, b.ch, signals::READABLE | signals::PEER_CLOSED, key);
            key += 1;
            sys::port_bind(port, b.sig, signals::READABLE, key);
            key += 1;
        }
        for d in self.desc.values() {
            sys::port_bind(port, d.h, signals::TERMINATED, key);
            key += 1;
        }
        for q in &self.pumps_out {
            sys::port_bind(port, q.rd, signals::READABLE | signals::PEER_CLOSED, key);
            key += 1;
        }
    }

    /// Wait on `h` for `mask` (or a few milliseconds) while serving; the shell never sits in a blocking read or write while a
    /// program of its own is waiting for it.
    fn serve_wait(&mut self, h: u32, mask: u32) {
        let p = sys::port_create();
        if p.status != 0 {
            io::nap(2);
            return;
        }
        let port = p.a1 as u32;
        sys::port_bind(port, h, mask, 0);
        self.bind_served(port);
        sys::port_wait(port, sys::clock_get() + 20);
        sys::handle_close(port);
        self.serve();
    }

    fn answer_spawn(&mut self, i: usize, msg: &[u8], hs: &[u32]) -> bool {
        let ch = self.brokers[i].ch;
        let txid = proto::file::peek_txid(msg).unwrap_or(0);
        let reply = |status: i32, pid: u32, h: Option<u32>| {
            let m = proto::spawn::encode_reply(txid, status, pid);
            let hv: &[u32] = match &h {
                Some(x) => core::slice::from_ref(x),
                None => &[],
            };
            if sys::channel_write(ch, m.as_ptr(), m.len(), hv.as_ptr(), hv.len()).status != 0 {
                if let Some(x) = h {
                    sys::handle_close(x);
                }
            }
        };
        let req = match proto::spawn::decode_message(msg) {
            Ok(proto::spawn::Message::Spawn(r)) => r,
            Ok(proto::spawn::Message::Term { what, value, .. }) => {
                close_all(hs);
                // the terminal belongs to the foreground job: a program of its chains may read and set the mode and read the
                // size; the shell puts the mode back when the job ends
                let root = self.brokers[i].root;
                let (st, v) = if !self.interactive || !self.fg_pids.contains(&root) {
                    (proto::file::status::BAD_REQUEST, 0)
                } else {
                    match what {
                        proto::spawn::TERM_GET_MODE => self
                            .control_query(Control::GetMode)
                            .map_or((proto::file::status::UNAVAILABLE, 0), |m| (0, m)),
                        proto::spawn::TERM_GET_SIZE => self
                            .control_query(Control::GetSize)
                            .map_or((proto::file::status::UNAVAILABLE, 0), |m| (0, m)),
                        proto::spawn::TERM_SET_MODE if value <= u32::from(ctl::mode::ALL) => {
                            if self.control(Control::SetMode(value as u8), &[]) {
                                (0, 0)
                            } else {
                                (proto::file::status::UNAVAILABLE, 0)
                            }
                        }
                        _ => (proto::file::status::BAD_REQUEST, 0),
                    }
                };
                reply(st, v, None);
                return true;
            }
            Ok(proto::spawn::Message::Signal { pid, sig, .. }) => {
                close_all(hs);
                // a signal goes to a program of the asker's own chain (itself, what it started, what started it): the
                // shell writes the number to the program's signal channel; it is delivered where that program waits
                let root = self.brokers[i].root;
                let target = self
                    .brokers
                    .iter()
                    .find(|b| b.pid == pid && b.root == root)
                    .map(|b| b.sig);
                let st = match target {
                    Some(_) if sig == 9 => {
                        // SIGKILL is not a message: the shell ends the process (it holds the handle of everything in
                        // its chains)
                        match self
                            .procs
                            .get(&pid)
                            .copied()
                            .or_else(|| self.desc.get(&pid).map(|d| d.h))
                        {
                            Some(h) => {
                                sys::process_kill(h, -1);
                                0
                            }
                            None => proto::file::status::NOT_FOUND,
                        }
                    }
                    Some(_) if sig == 0 => 0,
                    Some(t) if (1..=64).contains(&sig) => {
                        // (a full queue drops it: a signal that is already waiting coalesces with this one)
                        sys::channel_write(t, [sig as u8].as_ptr(), 1, core::ptr::null(), 0);
                        0
                    }
                    Some(_) => proto::file::status::BAD_REQUEST,
                    None => proto::file::status::NOT_FOUND,
                };
                reply(st, 0, None);
                return true;
            }
            Err(_) => {
                close_all(hs);
                reply(proto::file::status::BAD_REQUEST, 0, None);
                return false;
            }
        };
        if hs.len() != proto::spawn::HANDLES {
            close_all(hs);
            reply(proto::file::status::BAD_REQUEST, 0, None);
            return false;
        }
        let root = self.brokers[i].root;
        if self.desc.values().filter(|d| d.root == root).count() >= SPAWN_LIVE {
            close_all(hs);
            reply(proto::file::status::QUOTA, 0, None);
            return true;
        }
        // (bytes, not text: a file name given to `xargs` or `find -exec` need not be UTF-8)
        let prog: Vec<u8> = req.prog.to_vec();
        let argv: Vec<Vec<u8>> = req.args.iter().map(|a| a.to_vec()).collect();
        let env: Vec<(Vec<u8>, Vec<u8>)> = req
            .env
            .iter()
            .filter_map(|e| {
                let at = e.iter().position(|&b| b == b'=')?;
                Some((e[..at].to_vec(), e[at + 1..].to_vec()))
            })
            .collect();
        if !shell_core::valid_command_name(&prog) {
            close_all(hs);
            reply(proto::file::status::NOT_FOUND, 0, None);
            return true;
        }
        let mut b = self.brokers.remove(i);
        self.new_tmp = None;
        let id = self.next_pid;
        self.next_pid += 1;
        let started = self.start_with(
            id,
            &prog,
            &argv,
            &env,
            Stdio::Handles([hs[0], hs[1], hs[2]]),
            Some(&mut b),
        );
        self.brokers.insert(i, b);
        let tmp = self.new_tmp.take();
        match started {
            Err(e) => {
                if let Some(t) = tmp {
                    if let Some(tf) = self.tmp.as_mut() {
                        remove_tree(tf, &t, 0);
                    }
                }
                let st = match e {
                    HostError::NotFound => proto::file::status::NOT_FOUND,
                    _ => proto::file::status::UNAVAILABLE,
                };
                reply(st, 0, None);
            }
            Ok(ph) => {
                self.proc_announce(
                    id,
                    ph,
                    if argv.is_empty() {
                        core::slice::from_ref(&prog)
                    } else {
                        &argv
                    },
                );
                self.desc.insert(id, Desc { h: ph, root });
                self.attach_broker(id, root);
                if let Some(t) = tmp {
                    self.tmp_of.insert(id, t);
                }
                let give = dup(
                    ph,
                    Rights::WAIT.0 | Rights::INSPECT.0 | Rights::KILL.0 | Rights::TRANSFER.0,
                );
                match give {
                    Some(g) => reply(0, id, Some(g)),
                    None => {
                        sys::process_kill(ph, -1);
                        self.forget_desc(id);
                        reply(proto::file::status::UNAVAILABLE, 0, None)
                    }
                }
            }
        }
        true
    }

    /// Sleep until something the shell serves happens: a program ends (`extra`, and the ones programs started), a program
    /// sends a spawn request, or `deadline` (capped, so the output pumps are looked at too).
    fn idle(&mut self, extra: &[u32], deadline: u64) {
        let until = deadline.min(sys::clock_get() + if self.has_pumps() { 10 } else { 100 });
        let p = sys::port_create();
        if p.status != 0 {
            io::nap(2);
            return;
        }
        let port = p.a1 as u32;
        for (i, &h) in extra.iter().enumerate() {
            sys::port_bind(port, h, signals::TERMINATED, i as u64);
        }
        self.bind_served(port);
        sys::port_wait(port, until);
        sys::handle_close(port);
    }

    fn add_obj(&mut self, o: Obj) -> Stream {
        let id = self.next_stream;
        self.next_stream += 1;
        self.objs.insert(id, o);
        Stream(id)
    }

    /// The handle to forward to a program for one of its standard streams.
    fn forward_stream(&mut self, s: Stream, input: bool) -> Option<u32> {
        let (h, want) = match s.0 {
            0 if input => (
                self.tty,
                Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
            ),
            1 | 2 if !input => (
                self.tty,
                Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
            ),
            _ => match self.objs.get(&s.0)? {
                Obj::PipeR(h) if input => (
                    *h,
                    Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
                ),
                Obj::PipeW(h) if !input => (
                    *h,
                    Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
                ),
                Obj::FileW { .. } if !input => return self.pump_out(s.0),
                Obj::FileR { .. } if input => return self.pump_in(s.0),
                _ => return None,
            },
        };
        dup(h, want)
    }

    /// A redirected output file for a program: a socket pair, the write end for the program, the read end drained by the shell.
    fn pump_out(&mut self, id: u32) -> Option<u32> {
        let s = sys::socket_create();
        if s.status != 0 {
            return None;
        }
        let (rd, wr) = (s.a1 as u32, s.a2 as u32);
        let fwd = dup(
            wr,
            Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
        );
        let mine = dup(rd, Rights::READ.0 | Rights::WAIT.0);
        close_all(&[rd, wr]);
        let (fwd, mine) = (fwd?, mine?);
        let Some(Obj::FileW {
            path,
            append,
            piped,
            ..
        }) = self.objs.get_mut(&id)
        else {
            close_all(&[fwd, mine]);
            return None;
        };
        *piped = true;
        self.pumps_out.push(PumpOut {
            rd: mine,
            path: path.clone(),
            append: *append,
            buf: Vec::new(),
        });
        Some(fwd)
    }

    /// A redirected input file for a program: a socket pair, the read end for the program, the write end fed by the shell.
    fn pump_in(&mut self, id: u32) -> Option<u32> {
        let s = sys::socket_create();
        if s.status != 0 {
            return None;
        }
        let (rd, wr) = (s.a1 as u32, s.a2 as u32);
        let fwd = dup(
            rd,
            Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
        );
        let mine = dup(wr, Rights::WRITE.0 | Rights::WAIT.0);
        close_all(&[rd, wr]);
        let (fwd, mine) = (fwd?, mine?);
        let Some(Obj::FileR { data, pos }) = self.objs.get_mut(&id) else {
            close_all(&[fwd, mine]);
            return None;
        };
        let rest = data[(*pos).min(data.len())..].to_vec();
        *pos = data.len();
        self.pumps_in.push(PumpIn {
            wr: mine,
            data: rest,
            pos: 0,
        });
        Some(fwd)
    }

    /// Move what the programs have written to their redirected files, store a file whose writers are all gone, and feed the
    /// programs that read a file. Never blocks.
    fn service_pumps(&mut self) {
        let mut i = 0;
        while i < self.pumps_out.len() {
            let mut ended = false;
            loop {
                let mut buf = [0u8; 4096];
                let r = sys::socket_read(self.pumps_out[i].rd, buf.as_mut_ptr(), buf.len());
                match r.status() {
                    Some(Status::Ok) if r.a1 > 0 => {
                        self.pumps_out[i].buf.extend_from_slice(&buf[..r.a1])
                    }
                    Some(Status::ShouldWait) => break,
                    _ => {
                        ended = true;
                        break;
                    }
                }
            }
            if ended {
                let p = self.pumps_out.remove(i);
                sys::handle_close(p.rd);
                self.store_reporting(&p.path, &p.buf, p.append);
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.pumps_in.len() {
            let p = &mut self.pumps_in[i];
            let mut done = false;
            while p.pos < p.data.len() {
                let r = sys::socket_write(p.wr, p.data[p.pos..].as_ptr(), p.data.len() - p.pos);
                match r.status() {
                    Some(Status::Ok) if r.a1 > 0 => p.pos += r.a1,
                    Some(Status::ShouldWait) => break,
                    _ => {
                        done = true; // the program stopped reading
                        break;
                    }
                }
            }
            if done || p.pos >= p.data.len() {
                let p = self.pumps_in.remove(i);
                sys::handle_close(p.wr);
            } else {
                i += 1;
            }
        }
    }

    /// Store a redirected file; a failure is said on the terminal (the command has already run).
    fn store_reporting(&mut self, path: &str, bytes: &[u8], append: bool) {
        if let Err(e) = self.store(path, bytes, append) {
            self.say(format!("sh: {path}: {}\r\n", e.message()).as_bytes());
        }
    }

    fn has_pumps(&self) -> bool {
        !self.pumps_out.is_empty() || !self.pumps_in.is_empty()
    }

    /// The path as a root-relative path (no leading slash), resolved lexically against the working directory.
    fn rel_path(&self, path: &str) -> Vec<u8> {
        let abs = shell_core::resolve(&self.cwd_path, path.as_bytes());
        abs[1..].to_vec()
    }

    fn start(
        &mut self,
        pid: u32,
        argv: &[String],
        env: &[(String, String)],
        io: ProcIo,
    ) -> Result<u32, HostError> {
        let bytes: Vec<Vec<u8>> = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
        let envb: Vec<(Vec<u8>, Vec<u8>)> = env
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        self.start_with(pid, &bytes[0], &bytes, &envb, Stdio::Streams(io), None)
    }

    /// Start a program. `parent` is the broker of the program that asked (a spawn request, OD118): the new program gets
    /// only what that program also holds, at no wider rights, and its working directory.
    fn start_with(
        &mut self,
        pid: u32,
        prog: &[u8],
        argv: &[Vec<u8>],
        env: &[(Vec<u8>, Vec<u8>)],
        stdio: Stdio,
        mut parent: Option<&mut Broker>,
    ) -> Result<u32, HostError> {
        let allow: Option<Vec<(Vec<u8>, bool)>> = parent.as_ref().map(|p| p.mounts.clone());
        let mut mounts: Vec<(Vec<u8>, bool)> = Vec::new();
        let name = prog;
        if !shell_core::valid_command_name(name) {
            drop_stdio(stdio);
            return Err(HostError::Other(String::from("bad command name")));
        }
        let path: Vec<u8> = [&b"bin/"[..], name].concat();
        let mut file = self.root.open(&path, 0, rights::READ).map_err(|_| {
            drop_stdio(stdio);
            HostError::NotFound
        })?;
        let image = file.get_vmo();
        file.close();
        let (vmo, offset, len) = image.map_err(|_| {
            drop_stdio(stdio);
            HostError::NotFound
        })?;
        let want = rights::READ | rights::ENUMERATE;
        let trust = self.trusted.iter().position(|t| t.name == name);
        let trusted = trust.is_some_and(|t| self.trusted[t].root);
        let (stdin, stdout, stderr) = match stdio {
            Stdio::Handles([a, b, c]) => (a, b, c),
            Stdio::Streams(io) => {
                let Some(stdin) = self.forward_stream(io.stdin, true) else {
                    sys::handle_close(vmo);
                    return Err(HostError::Other(String::from(
                        "this input cannot be given to a program",
                    )));
                };
                let Some(stdout) = self.forward_stream(io.stdout, false) else {
                    sys::handle_close(vmo);
                    sys::handle_close(stdin);
                    return Err(HostError::Other(String::from(
                        "this output cannot be given to a program",
                    )));
                };
                let Some(stderr) = self.forward_stream(io.stderr, false) else {
                    sys::handle_close(vmo);
                    close_all(&[stdin, stdout]);
                    return Err(HostError::Other(String::from(
                        "this output cannot be given to a program",
                    )));
                };
                (stdin, stdout, stderr)
            }
        };
        let (cwd, cwd_path) = match parent.as_deref_mut() {
            Some(p) => (p.cwd.clone_conn(want), p.cwd_path.clone()),
            None => (self.cwd.clone_conn(want), self.cwd_path.clone()),
        };
        let root = if trusted {
            Some(self.root.clone_conn(want))
        } else {
            None
        };
        let mut forward: Vec<u32> = alloc::vec![stdin, stdout, stderr];
        let mut startup = Startup {
            args: if argv.is_empty() {
                alloc::vec![prog.to_vec()]
            } else {
                argv.to_vec()
            },
            roles: alloc::vec![(Role::Stdin, 0), (Role::Stdout, 1), (Role::Stderr, 2)],
            ..Startup::default()
        };
        // the environment the shell passes (its `$env` binding, A2), then the working directory as a path: the libc's
        // `getcwd`, and `..` that climbs, start from it
        for (k, v) in env {
            // (the program's pid and its parent's are the shell's to say)
            if k != b"PID" && k != b"PPID" {
                startup.env.push([&k[..], b"=", &v[..]].concat());
            }
        }
        startup.env.push(format!("PID={pid}").into_bytes());
        startup
            .env
            .push(format!("PPID={}", parent.as_ref().map_or(0, |p| p.pid)).into_bytes());
        startup.env.push([&b"PWD="[..], &cwd_path].concat());
        // which of the standard streams are the terminal (a program tells a tty from a pipe by this)
        if let Stdio::Streams(io) = stdio {
            if io.stdin == Stream(0) {
                startup.env.push(b"STDIN=tty".to_vec());
            }
            if io.stdout == Stream(1) {
                startup.env.push(b"STDOUT=tty".to_vec());
            }
        }
        match cwd {
            Ok(c) => {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    None,
                    b".",
                    c.ch,
                    false,
                );
            }
            Err(_) => {
                sys::handle_close(vmo);
                close_all(&forward);
                if let Some(Ok(r)) = root {
                    r.close();
                }
                return Err(HostError::Other(String::from(
                    "could not open the working directory",
                )));
            }
        }
        let confined = match parent.as_ref() {
            Some(p) => p.confined.clone(),
            None => self.confined.last().cloned(),
        };
        let root = if confined.is_some() {
            if let Some(Ok(r)) = root {
                r.close();
            }
            None
        } else {
            root
        };
        match root {
            Some(Ok(r)) => {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    allow.as_deref(),
                    b"/",
                    r.ch,
                    false,
                );
            }
            Some(Err(_)) => {
                sys::handle_close(vmo);
                close_all(&forward);
                return Err(HostError::Other(String::from(
                    "could not open the root directory",
                )));
            }
            None => {}
        }
        // `/dev` for everyone, and a private `/tmp` for the length of this run (USER-VIEW-SPEC section 7): a fresh directory
        // only this program holds a connection to, removed when the job ends
        if let Some(d) = self.dev.as_mut() {
            if let Ok(c) = d.clone_conn(rights::READ | rights::WRITE | rights::ENUMERATE) {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    allow.as_deref(),
                    b"/dev",
                    c.ch,
                    true,
                );
            }
        }
        if let Some(d) = self.proc_root.as_mut() {
            if let Ok(c) = d.clone_conn(rights::READ | rights::ENUMERATE) {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    allow.as_deref(),
                    b"/proc",
                    c.ch,
                    false,
                );
            }
        }
        let mut chain_tmp: Option<Vec<u8>> = parent.as_ref().and_then(|p| p.tmp.clone());
        let shared = chain_tmp.is_some();
        if shared && allowed(allow.as_deref(), b"/tmp", true) {
            // a program started by a program shares its chain's directory (the root's)
            let name = chain_tmp.clone().unwrap_or_default();
            let opened = self.tmp.as_mut().and_then(|t| {
                t.open(
                    &name,
                    flags::DIRECTORY,
                    rights::READ | rights::WRITE | rights::CREATE | rights::ENUMERATE,
                )
                .ok()
            });
            if let Some(c) = opened {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    allow.as_deref(),
                    b"/tmp",
                    c.ch,
                    true,
                );
            }
        } else if self.tmp.is_some() && allowed(allow.as_deref(), b"/tmp", true) {
            self.jobs += 1;
            let name = format!("j{}", self.jobs).into_bytes();
            let opened = self.tmp.as_mut().and_then(|t| {
                t.mkdir(&name).ok()?;
                let c = t.open(
                    &name,
                    flags::DIRECTORY,
                    rights::READ | rights::WRITE | rights::CREATE | rights::ENUMERATE,
                );
                if c.is_err() {
                    let _ = t.unlink(&name);
                }
                c.ok()
            });
            if let Some(c) = opened {
                ns_push(
                    &mut startup,
                    &mut forward,
                    &mut mounts,
                    allow.as_deref(),
                    b"/tmp",
                    c.ch,
                    true,
                );
                chain_tmp = Some(name.clone());
                self.new_tmp = Some(name);
            }
        }
        // the volumes this program is granted, opened now from the registry (retrying while the volume manager brings a
        // restarted volume server back)
        if let Some(grants) = &confined {
            // `confine`: exactly the grants, opened now, each as narrow as asked
            for (path, rw) in grants {
                let writable = rw.contains('w');
                // (`.` and `/tmp` are the program's own, never a grant by name)
                if parent.is_some() && (path == "." || path == "/tmp") {
                    continue;
                }
                if !allowed(allow.as_deref(), path.as_bytes(), writable) {
                    continue;
                }
                match self.open_grant(path, rw) {
                    Ok(f) => {
                        ns_push(
                            &mut startup,
                            &mut forward,
                            &mut mounts,
                            allow.as_deref(),
                            path.as_bytes(),
                            f.ch,
                            writable,
                        );
                    }
                    Err(e) => {
                        sys::handle_close(vmo);
                        close_all(&forward);
                        return Err(HostError::Other(format!(
                            "confine: {path}: {}",
                            e.message()
                        )));
                    }
                }
            }
        }
        if let (Some(t), None) = (trust, &confined) {
            for g in 0..self.trusted[t].grants.len() {
                let (mount, vol, ro) = {
                    let g = &self.trusted[t].grants[g];
                    (g.mount.clone(), g.vol.clone(), g.readonly)
                };
                if !allowed(allow.as_deref(), &mount, !ro) {
                    continue;
                }
                let want = if ro {
                    rights::READ | rights::ENUMERATE
                } else {
                    rights::ALL & !rights::PUBLISH
                };
                let name = [&b"vol-"[..], &vol].concat();
                let opened = match self.registry.as_mut() {
                    Some(r) => r
                        .open_retry(&name, flags::DIRECTORY, want, sys::clock_get() + 4000)
                        .ok(),
                    None => None,
                };
                // a volume that is not there (no disk, or its server is down for good) is not a reason to refuse to run `ls`:
                // the program starts without that mount point and its own errors say what is missing
                if let Some(f) = opened {
                    ns_push(
                        &mut startup,
                        &mut forward,
                        &mut mounts,
                        allow.as_deref(),
                        &mount,
                        f.ch,
                        !ro,
                    );
                }
            }
        }
        // the program's own spawn grant (OD118): a channel to this shell, kept with what the program holds so that what it
        // starts holds no more; absent below the depth limit or when the session turned the grant off
        self.pending_broker = None;
        let depth = parent.as_ref().map_or(1, |p| p.depth + 1);
        if self.spawn_grant && depth <= SPAWN_DEPTH {
            let c = sys::channel_create();
            if c.status == 0 {
                let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
                let cw = match parent {
                    Some(p) => p.cwd.clone_conn(rights::READ | rights::ENUMERATE),
                    None => self.cwd.clone_conn(rights::READ | rights::ENUMERATE),
                };
                let sg = sys::channel_create();
                match cw {
                    Ok(cw) if sg.status == 0 => {
                        startup
                            .roles
                            .push((Role::ServiceSpawn, forward.len() as u32));
                        forward.push(theirs);
                        startup
                            .roles
                            .push((Role::SignalChannel, forward.len() as u32));
                        forward.push(sg.a2 as u32);
                        self.pending_broker = Some(Broker {
                            sig: sg.a1 as u32,
                            pid: 0,
                            root: 0,
                            ch: mine,
                            depth,
                            cwd: cw,
                            cwd_path: cwd_path.clone(),
                            mounts: mounts.clone(),
                            confined: confined.clone(),
                            tmp: chain_tmp.clone(),
                            handles_int: false,
                        });
                    }
                    Ok(cw) => {
                        cw.close();
                        close_all(&[mine, theirs]);
                    }
                    Err(_) => {
                        close_all(&[mine, theirs]);
                        if sg.status == 0 {
                            close_all(&[sg.a1 as u32, sg.a2 as u32]);
                        }
                    }
                }
            }
        }
        self.txid = self.txid.wrapping_add(1);
        let r = rt::launch::spawn_via(self.launch, vmo, offset, len, &startup, &forward, self.txid);
        sys::handle_close(vmo);
        match r {
            Ok(p) => Ok(p),
            Err(e) => {
                if let Some(b) = self.pending_broker.take() {
                    close_all(&[b.ch, b.sig]);
                    b.cwd.close();
                }
                close_all(&forward);
                Err(HostError::Other(format!("could not start ({e})")))
            }
        }
    }
}

impl Host for Os {
    fn stdin(&self) -> Stream {
        Stream(0)
    }
    fn stdout(&self) -> Stream {
        Stream(1)
    }
    fn stderr(&self) -> Stream {
        Stream(2)
    }

    fn spawn(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        io: ProcIo,
    ) -> Result<Pid, HostError> {
        // programs read the terminal through the console server's line editor
        self.go_cooked();
        self.new_tmp = None;
        let id = self.next_pid;
        self.next_pid += 1;
        let started = self.start(id, argv, env, io);
        let tmp = self.new_tmp.take();
        let h = match started {
            Ok(h) => h,
            Err(e) => {
                if let (Some(n), Some(t)) = (tmp, self.tmp.as_mut()) {
                    remove_tree(t, &n, 0);
                }
                return Err(e);
            }
        };
        self.procs.insert(id, h);
        self.attach_broker(id, id);
        let announced: Vec<Vec<u8>> = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
        self.proc_announce(id, h, &announced);
        if let Some(n) = tmp {
            self.tmp_of.insert(id, n);
        }
        Ok(Pid(id))
    }

    fn wait(&mut self, pid: Pid) -> i32 {
        // (the pid stays in `procs` until the program is reaped: a program of its chain may still ask the shell to end it)
        let Some(&h) = self.procs.get(&pid.0) else {
            return -1;
        };
        loop {
            self.serve();
            // (what the console server says about Ctrl-C reaches the programs that handle it; not while the shell edits a line:
            // there the terminal read takes its own events)
            self.poll_ctl();
            if sys::object_wait(h, signals::TERMINATED, 0).status == 0 {
                break;
            }
            self.idle(&[h], u64::MAX);
        }
        self.service_pumps();
        self.procs.remove(&pid.0);
        self.reap(pid.0, h)
    }

    fn try_wait(&mut self, pid: Pid) -> Option<i32> {
        self.serve();
        self.poll_ctl();
        let h = *self.procs.get(&pid.0)?;
        if sys::object_wait(h, signals::TERMINATED, 0).status != 0 {
            return None;
        }
        self.procs.remove(&pid.0);
        Some(self.reap(pid.0, h))
    }

    fn wait_any(&mut self, pids: &[Pid]) -> (usize, i32) {
        // there is no wait-for-many call yet (TIDY-UP): look at each, then sleep a little
        loop {
            for (i, p) in pids.iter().enumerate() {
                if !self.procs.contains_key(&p.0) {
                    return (i, -1);
                }
                if let Some(st) = self.try_wait(*p) {
                    return (i, st);
                }
            }
            io::nap(2);
        }
    }

    fn wait_timeout(&mut self, pid: Pid, ms: u64) -> WaitOutcome {
        let Some(&h) = self.procs.get(&pid.0) else {
            return WaitOutcome::Done(-1);
        };
        let deadline = sys::clock_get() + ms;
        loop {
            self.serve();
            self.poll_ctl();
            if core::mem::take(&mut self.interrupt) {
                return WaitOutcome::Interrupted;
            }
            let now = sys::clock_get();
            let slice = deadline.min(now + 20);
            if sys::object_wait(h, signals::TERMINATED, slice).status == 0 {
                self.procs.remove(&pid.0);
                return WaitOutcome::Done(self.reap(pid.0, h));
            }
            if sys::clock_get() >= deadline {
                return WaitOutcome::Timeout;
            }
        }
    }

    fn pipe(&mut self) -> Result<(Stream, Stream), HostError> {
        let s = sys::socket_create();
        if s.status != 0 {
            return Err(HostError::Other(String::from("could not make a pipe")));
        }
        let (a, b) = (s.a1 as u32, s.a2 as u32);
        let w = dup(
            a,
            Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
        );
        let r = dup(
            b,
            Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
        );
        close_all(&[a, b]);
        let (Some(w), Some(r)) = (w, r) else {
            for h in [w, r].into_iter().flatten() {
                sys::handle_close(h);
            }
            return Err(HostError::Other(String::from("could not make a pipe")));
        };
        Ok((self.add_obj(Obj::PipeR(r)), self.add_obj(Obj::PipeW(w))))
    }

    fn read(&mut self, s: Stream, buf: &mut [u8]) -> Result<usize, HostError> {
        let h = match s.0 {
            0 => self.tty,
            _ => match self.objs.get_mut(&s.0) {
                Some(Obj::PipeR(h)) => *h,
                Some(Obj::FileR { data, pos }) => {
                    let n = buf.len().min(data.len() - *pos);
                    buf[..n].copy_from_slice(&data[*pos..*pos + n]);
                    *pos += n;
                    return Ok(n);
                }
                _ => return Err(HostError::Other(String::from("not readable"))),
            },
        };
        if s.0 == 0 {
            return io::read(h, buf).map_err(|_| HostError::Other(String::from("read failed")));
        }
        // a pipe: the shell keeps serving the programs that feed it while it waits for them
        loop {
            let r = sys::socket_read(h, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(Status::Ok) => return Ok(r.a1),
                Some(Status::ShouldWait) => {
                    self.serve_wait(h, signals::READABLE | signals::PEER_CLOSED)
                }
                _ => return Ok(0), // the end of the stream
            }
        }
    }

    fn poll_read(
        &mut self,
        streams: &[Stream],
        buf: &mut [u8],
    ) -> Result<(usize, usize), HostError> {
        let handles: Vec<Option<u32>> = streams
            .iter()
            .map(|s| match self.objs.get(&s.0) {
                Some(Obj::PipeR(h)) => Some(*h),
                _ => None,
            })
            .collect();
        loop {
            for (i, h) in handles.iter().enumerate() {
                let Some(h) = h else { return Ok((i, 0)) };
                let r = sys::socket_read(*h, buf.as_mut_ptr(), buf.len());
                match r.status() {
                    Some(Status::Ok) => return Ok((i, r.a1)),
                    Some(Status::ShouldWait) => {}
                    _ => return Ok((i, 0)), // PEER_CLOSED: the end of that stream
                }
            }
            let p = sys::port_create();
            if p.status != 0 {
                return Err(HostError::Other(String::from("no port")));
            }
            let port = p.a1 as u32;
            for (i, h) in handles.iter().enumerate() {
                if let Some(h) = h {
                    sys::port_bind(port, *h, signals::READABLE | signals::PEER_CLOSED, i as u64);
                }
            }
            self.bind_served(port);
            sys::port_wait(port, sys::clock_get() + 20);
            sys::handle_close(port);
            self.serve();
        }
    }

    fn write(&mut self, s: Stream, bytes: &[u8]) -> Result<(), HostError> {
        let h = match s.0 {
            1 | 2 => self.tty,
            _ => match self.objs.get_mut(&s.0) {
                Some(Obj::PipeW(h)) => *h,
                Some(Obj::FileW { buf, .. }) => {
                    buf.extend_from_slice(bytes);
                    return Ok(());
                }
                _ => return Err(HostError::Other(String::from("not writable"))),
            },
        };
        if s.0 == 1 || s.0 == 2 {
            return io::write_all(h, bytes).map_err(|e| match e {
                io::IoError::PeerClosed => HostError::Closed,
                _ => HostError::Other(String::from("write failed")),
            });
        }
        let mut at = 0;
        while at < bytes.len() {
            let r = sys::socket_write(h, bytes[at..].as_ptr(), bytes.len() - at);
            match r.status() {
                Some(Status::Ok) => at += r.a1,
                Some(Status::ShouldWait) => {
                    self.serve_wait(h, signals::WRITABLE | signals::PEER_CLOSED)
                }
                Some(Status::PeerClosed) => return Err(HostError::Closed),
                _ => return Err(HostError::Other(String::from("write failed"))),
            }
        }
        Ok(())
    }

    fn close(&mut self, s: Stream) {
        if s.0 < 3 {
            return;
        }
        match self.objs.remove(&s.0) {
            Some(Obj::PipeR(h) | Obj::PipeW(h)) => {
                sys::handle_close(h);
            }
            // (a file a program writes is stored when its pump ends, not here)
            Some(Obj::FileW {
                path,
                buf,
                append,
                piped: false,
            }) => self.store_reporting(&path, &buf, append),
            _ => {}
        }
    }

    fn open(&mut self, path: &str, mode: OpenMode) -> Result<Stream, HostError> {
        match mode {
            OpenMode::Read => {
                let data = self.read_file(path)?;
                Ok(self.add_obj(Obj::FileR { data, pos: 0 }))
            }
            OpenMode::Write => Ok(self.add_obj(Obj::FileW {
                path: String::from(path),
                buf: Vec::new(),
                append: false,
                piped: false,
            })),
            OpenMode::Append => Ok(self.add_obj(Obj::FileW {
                path: String::from(path),
                buf: Vec::new(),
                append: true,
                piped: false,
            })),
        }
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostError> {
        let rel = self.rel_path(path);
        // `/tmp/...` is the shell's own scratch directory (the parent of the programs' private ones), not part of the image
        let mut f = match rel.strip_prefix(b"tmp/") {
            Some(under) => match self.tmp.as_mut() {
                Some(t) => t.open(under, 0, rights::READ).map_err(host_err)?,
                None => return Err(HostError::NotFound),
            },
            None => self.root.open(&rel, 0, rights::READ).map_err(host_err)?,
        };
        let r = f.read_all();
        f.close();
        r.map_err(host_err)
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), HostError> {
        self.store(path, bytes, false)
    }

    fn glob(&mut self, pattern: &str) -> Vec<String> {
        // Expand component by component: a component with `*`, `?` or `[` lists the directory reached so far and keeps the
        // entries that match (a name starting with `.` only when the pattern does); a plain component is appended as it is.
        let absolute = pattern.starts_with('/');
        let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
        let mut paths: Vec<String> = alloc::vec![if absolute {
            String::from("/")
        } else {
            String::new()
        }];
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let mut next: Vec<String> = Vec::new();
            for base in &paths {
                if !comp.contains(['*', '?', '[']) {
                    next.push(format!("{base}{comp}{}", if last { "" } else { "/" }));
                    continue;
                }
                let dir = if base.is_empty() {
                    String::from(".")
                } else {
                    base.clone()
                };
                let Ok(entries) = self.list_dir(&dir) else {
                    continue;
                };
                for (name, is_dir) in entries {
                    if name.starts_with('.') && !comp.starts_with('.') {
                        continue;
                    }
                    if !last && !is_dir {
                        continue;
                    }
                    if ish::builtins::glob_match(comp, &name) {
                        next.push(format!("{base}{name}{}", if last { "" } else { "/" }));
                    }
                }
            }
            paths = next;
        }
        // a pattern with no wildcard names itself: it matches only if it exists
        if !pattern.contains(['*', '?', '[']) {
            return Vec::new();
        }
        paths.retain(|p| !p.is_empty());
        paths.sort();
        paths
    }

    fn term_read(&mut self) -> TermIn {
        self.go_raw();
        if !self.inbuf.is_empty() {
            return TermIn::Bytes(core::mem::take(&mut self.inbuf));
        }
        // an interrupt that came with typed bytes: if the command those bytes made did not take it (`interrupted`, a wait), it is
        // for the line being edited
        if core::mem::take(&mut self.late_interrupt) && core::mem::take(&mut self.interrupt) {
            return TermIn::Interrupt;
        }
        loop {
            let p = sys::port_create();
            if p.status != 0 {
                return TermIn::Gone;
            }
            let port = p.a1 as u32;
            sys::port_bind(port, self.tty, signals::READABLE | signals::PEER_CLOSED, 1);
            sys::port_bind(port, self.ctl, signals::READABLE | signals::PEER_CLOSED, 2);
            // a background job may ask for a program, or write a redirected file, while the shell waits for a key
            self.bind_served(port);
            let w = sys::port_wait(
                port,
                if self.has_pumps() {
                    sys::clock_get() + 20
                } else {
                    u64::MAX
                },
            );
            sys::handle_close(port);
            self.serve();
            if w.status != 0 && w.status != Status::TimedOut.code() {
                return TermIn::Gone;
            }
            // the control channel first: an interrupt event is not typed text
            let mut rb = [0u8; 16];
            let c = sys::channel_read(
                self.ctl,
                rb.as_mut_ptr(),
                rb.len(),
                core::ptr::null_mut(),
                0,
            );
            if c.status == 0 {
                match ctl::decode_event(&rb[..c.a1]) {
                    Some(ctl::Event::Interrupt) => {
                        // what was typed before the Ctrl-C comes first (the event travels on its own channel and can be
                        // read before bytes the console server wrote to the terminal socket ahead of it)
                        self.read_ahead();
                        if self.inbuf.is_empty() {
                            return TermIn::Interrupt;
                        }
                        // (`interrupt` is what a command that runs now sees, so a Ctrl-C typed with the command still ends it)
                        self.interrupt = true;
                        self.late_interrupt = true;
                        return TermIn::Bytes(core::mem::take(&mut self.inbuf));
                    }
                    Some(ctl::Event::Hangup) => return TermIn::Gone,
                    _ => {}
                }
            } else if c.status() != Some(Status::ShouldWait) {
                return TermIn::Gone; // the console server closed the channel
            }
            let mut buf = [0u8; 256];
            let r = sys::socket_read(self.tty, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(Status::Ok) if r.a1 > 0 => return TermIn::Bytes(buf[..r.a1].to_vec()),
                Some(Status::Ok) | Some(Status::ShouldWait) => {}
                _ => return TermIn::Gone,
            }
        }
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<(String, bool)>, HostError> {
        let rel = self.rel_path(path);
        let want = rights::READ | rights::ENUMERATE;
        let mut d = if rel.is_empty() {
            self.root.clone_conn(want)
        } else {
            self.root.open(&rel, flags::DIRECTORY, want)
        }
        .map_err(host_err)?;
        let list = d.read_dir();
        d.close();
        let list = list.map_err(host_err)?;
        Ok(list
            .into_iter()
            .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), e.is_dir))
            .collect())
    }

    fn interrupted(&mut self) -> bool {
        self.poll_ctl();
        core::mem::take(&mut self.interrupt)
    }

    fn started(&mut self, pids: &[Pid]) -> bool {
        if !self.interactive {
            return true; // no console server: nothing to register with
        }
        // register the job (duplicates with only KILL|WAIT|TRANSFER; not the programs that handle SIGINT)
        let hs: Vec<u32> = pids
            .iter()
            .filter_map(|p| self.procs.get(&p.0).copied())
            .collect();
        self.fg_pids = pids.iter().map(|p| p.0).collect();
        let registered = if hs.len() == pids.len() && !hs.is_empty() {
            self.register_foreground()
        } else {
            false
        };
        if !registered {
            // Without a registration Ctrl-C could not reach the job and the wait would never end: do not run it.
            return false;
        }
        // A Ctrl-C typed before the registration was acknowledged reached the console server while it had no job to kill,
        // but its 0x03 byte is already waiting on the terminal: honour it now (this closes the start-gate race, T31).
        self.read_ahead();
        self.poll_ctl();
        let early = self.inbuf.iter().position(|&b| b == 0x03);
        if let Some(i) = early {
            self.inbuf.remove(i);
        }
        if early.is_some() || self.interrupt {
            // (in raw mode the console server reports the interrupt as an event and does not echo it)
            if self.interrupt {
                self.say(b"^C\r\n");
            }
            self.interrupt = false;
            for &p in &hs {
                sys::process_kill(p, KILLED);
            }
        }
        true
    }

    fn finished(&mut self) {
        // a program's redirected output arrives a moment after it ends: let the files be stored before the next command
        let until = sys::clock_get() + 2000;
        while self.has_pumps() && sys::clock_get() < until {
            self.service_pumps();
            if self.has_pumps() {
                io::nap(2);
            }
        }
        if self.interactive {
            self.control(Control::ClearForeground, &[]);
        }
        self.fg_pids.clear();
        if self.killed {
            self.killed = false;
            self.swallow_interrupt();
        }
        self.interrupt = false;
        self.go_raw();
    }

    fn kill(&mut self, pid: Pid) {
        if let Some(&h) = self.procs.get(&pid.0) {
            sys::process_kill(h, -1);
        }
    }

    fn halt(&mut self) {
        // ask init to end the session, then wait to be stopped
        sys::channel_write(self.init_ch, b"halt".as_ptr(), 4, core::ptr::null(), 0);
        io::nap(5000);
    }

    fn table(&self) -> Vec<(String, String)> {
        let always = |v: &mut Vec<(String, String)>| {
            v.push((String::from("."), String::from("r")));
            if self.dev.is_some() {
                v.push((String::from("/dev"), String::from("rw")));
            }
            if self.tmp.is_some() {
                v.push((String::from("/tmp"), String::from("rw")));
            }
            if self.proc_root.is_some() {
                v.push((String::from("/proc"), String::from("r")));
            }
        };
        match self.confined.last() {
            Some(g) => {
                let mut v = g.clone();
                always(&mut v);
                v
            }
            None => {
                let mut v = alloc::vec![(String::from("/"), String::from("r"))];
                always(&mut v);
                if self.registry.is_some() {
                    v.push((String::from("/svc"), String::from("r")));
                }
                v
            }
        }
    }

    fn confine(&mut self, grants: &[(String, String)]) -> Result<(), HostError> {
        let now = self.table();
        for (path, rw) in grants {
            let abs =
                String::from_utf8_lossy(&shell_core::resolve(b"/", path.as_bytes())).into_owned();
            // a grant must lie inside the table the shell has now, with no more rights (SHELL-SPEC SHI2); volumes come
            // from the registry connection
            let is_volume = abs.starts_with("/mnt/") && self.registry.is_some();
            let covered = is_volume
                || now.iter().any(|(e, r)| {
                    (e == "/"
                        || abs == *e
                        || abs
                            .strip_prefix(e.as_str())
                            .is_some_and(|x| x.starts_with('/')))
                        && (r == "rw" || rw == "r")
                });
            if !covered {
                return Err(HostError::Denied);
            }
            // and it must exist, now: an error at `confine`, not later in a program
            let f = self.open_grant(path, rw)?;
            f.close();
        }
        self.confined
            .push(grants.iter().map(|(p, r)| (p.clone(), r.clone())).collect());
        Ok(())
    }

    fn unconfine(&mut self) {
        self.confined.pop();
    }

    fn environ(&self) -> Vec<(String, String)> {
        // the identity the session gives
        let mut v: Vec<(String, String)> = self.session_env.clone();
        v.sort();
        v
    }

    fn cwd(&self) -> String {
        String::from_utf8_lossy(&self.cwd_path).into_owned()
    }

    fn chdir(&mut self, path: &str) -> Result<(), HostError> {
        let abs = shell_core::resolve(&self.cwd_path, path.as_bytes());
        let rel: &[u8] = &abs[1..]; // without the leading slash
        let want = rights::READ | rights::ENUMERATE;
        let opened = if rel.is_empty() {
            self.root.clone_conn(want)
        } else {
            self.root.open(rel, flags::DIRECTORY, want)
        };
        match opened {
            Ok(d) => {
                let old = core::mem::replace(&mut self.cwd, d);
                old.close();
                self.cwd_path = abs;
                Ok(())
            }
            Err(e) if e == proto::file::status::NOT_DIRECTORY => {
                Err(HostError::Other(String::from("not a directory")))
            }
            Err(_) => Err(HostError::NotFound),
        }
    }
}

impl Os {
    /// A connection for a `confine` grant: a path under `/mnt/<volume>` is that volume (from the registry), anything else a
    /// directory of the shell's own root, with the rights asked for (a read-only root cannot give write access).
    fn open_grant(&mut self, path: &str, rw: &str) -> Result<Fs, HostError> {
        let want = if rw == "rw" {
            rights::ALL & !rights::PUBLISH
        } else {
            rights::READ | rights::ENUMERATE
        };
        let abs = shell_core::resolve(b"/", path.as_bytes());
        let rel: &[u8] = &abs[1..];
        let map = |e: i32| match e {
            e if e == proto::file::status::NOT_FOUND => HostError::NotFound,
            e if e == Status::AccessDenied.code() => HostError::Denied,
            e => HostError::Other(format!("error {e}")),
        };
        if let Some(rest) = rel.strip_prefix(b"mnt/") {
            let (vol, sub) = match rest.iter().position(|&b| b == b'/') {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, &b""[..]),
            };
            let name = [&b"vol-"[..], vol].concat();
            let Some(reg) = self.registry.as_mut() else {
                return Err(HostError::NotFound);
            };
            let mut v = reg
                .open_retry(&name, flags::DIRECTORY, want, sys::clock_get() + 2000)
                .map_err(map)?;
            if sub.is_empty() {
                return Ok(v);
            }
            let r = v.open(sub, flags::DIRECTORY, want).map_err(map);
            v.close();
            return r;
        }
        if rel.is_empty() {
            self.root.clone_conn(want).map_err(map)
        } else {
            self.root.open(rel, flags::DIRECTORY, want).map_err(map)
        }
    }

    /// Write a file (create or truncate; or append).
    fn store(&mut self, path: &str, bytes: &[u8], append: bool) -> Result<(), HostError> {
        let rel = self.rel_path(path);
        let fl = if append {
            flags::CREATE
        } else {
            flags::CREATE | flags::TRUNCATE
        };
        // only `/tmp/...` can be written: the image is read-only and the volumes belong to the programs the profile grants them to
        let target = match rel.strip_prefix(b"tmp/") {
            Some(under) => self.tmp.as_mut().map(|t| (t, under.to_vec())),
            None => None,
        };
        let Some((tmp, under)) = target else {
            return Err(HostError::Denied);
        };
        let mut f = tmp
            .open(&under, fl, rights::READ | rights::WRITE)
            .map_err(host_err)?;
        let off = if append { f.size().unwrap_or(0) } else { 0 };
        let r = f.write_all(off, bytes);
        f.close();
        r.map_err(host_err)
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    // without a console server the shell is not on a terminal: it reads a script from standard input (the Tty role's
    // stream) and prints no prompt
    let (Some(tty), Some(root_ch), Some(launch)) = (
        env.handle(Role::Tty),
        env.handle(Role::ServiceFs),
        env.handle(Role::ServiceLaunch),
    ) else {
        return 3;
    };
    let ctl_ch = env.handle(Role::ServiceConsole);
    let init_ch = env.handle(Role::ServiceInit);
    let mut root = Fs::new(root_ch);
    let Ok(cwd) = root.open(b"home", flags::DIRECTORY, rights::READ | rights::ENUMERATE) else {
        return 4;
    };
    let os = Os {
        pumps_out: Vec::new(),
        pumps_in: Vec::new(),
        tty,
        root,
        cwd,
        cwd_path: b"/home".to_vec(),
        launch,
        ctl: ctl_ch.unwrap_or(u32::MAX),
        init_ch: init_ch.unwrap_or(u32::MAX),
        trusted: Vec::new(),
        registry: env.handle(Role::ServiceRegistry).map(Fs::new),
        tmp: env.handle(Role::TmpDir).map(Fs::new),
        dev: env.handle(Role::DevDir).map(Fs::new),
        proc_root: env.handle(Role::ProcDir).map(Fs::new),
        proc_ctl: env.handle(Role::ServiceProc),
        jobs: 0,
        new_tmp: None,
        tmp_of: BTreeMap::new(),
        txid: 0,
        inbuf: Vec::new(),
        objs: BTreeMap::new(),
        next_stream: 3,
        procs: BTreeMap::new(),
        next_pid: 2,
        killed: false,
        session_env: Vec::new(),
        confined: Vec::new(),
        raw: false,
        interrupt: false,
        late_interrupt: false,
        interactive: ctl_ch.is_some(),
        fg_pids: Vec::new(),
        spawn_grant: true,
        brokers: Vec::new(),
        pending_broker: None,
        desc: BTreeMap::new(),
    };
    let mut os = os;
    let profile_loaded = env
        .var(b"PROFILE")
        .and_then(|b| core::str::from_utf8(b).ok())
        .is_some_and(|t| os.load_profile(t));
    if !profile_loaded {
        os.load_trusted();
    }
    // (after the profile: the shell's `$env` starts from the identity it gives)
    let mut sh = ish::Shell::new(os);
    sh.name = String::from("sh");
    if !sh.host.interactive {
        // not on a terminal: the standard messages, no prompt, no echo
        return ish::repl::run_batch(&mut sh);
    }
    sh.legacy = true;
    sh.help = String::from(HELP);
    sh.host.say(BANNER);
    ish::repl::run_interactive(&mut sh)
}
