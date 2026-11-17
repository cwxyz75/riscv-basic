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
    },
}

/// The machine, as `ish` sees it.
struct Os {
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
    /// There is a console server: interactive mode.
    interactive: bool,
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
            if let Some(ctl::Event::Interrupt) = ctl::decode_event(&rb[..r.a1]) {
                self.interrupt = true;
            }
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
                        if let Some(ctl::Event::Interrupt) = ctl::decode_event(&rb[..r.a1]) {
                            self.interrupt = true;
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
    fn proc_announce(&mut self, pid: u32, h: u32, argv: &[String]) {
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
            msg.extend_from_slice(a.as_bytes());
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
        self.cleanup_tmp(pid);
        self.proc_forget(pid);
        code
    }

    fn add_obj(&mut self, o: Obj) -> Stream {
        let id = self.next_stream;
        self.next_stream += 1;
        self.objs.insert(id, o);
        Stream(id)
    }

    /// The handle to forward to a program for one of its standard streams.
    fn forward_stream(&self, s: Stream, input: bool) -> Option<u32> {
        let (h, want) = match s.0 {
            0 if input => (
                self.tty,
                Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0,
            ),
            1 | 2 if !input => (
                self.tty,
                Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0,
            ),
            _ => match self.objs.get(&s.0)? {
                Obj::PipeR(h) if input => {
                    (*h, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0)
                }
                Obj::PipeW(h) if !input => {
                    (*h, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0)
                }
                _ => return None,
            },
        };
        dup(h, want)
    }

    /// The path as a root-relative path (no leading slash), resolved lexically against the working directory.
    fn rel_path(&self, path: &str) -> Vec<u8> {
        let abs = shell_core::resolve(&self.cwd_path, path.as_bytes());
        abs[1..].to_vec()
    }

    fn start(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        io: ProcIo,
    ) -> Result<u32, HostError> {
        let name = argv[0].as_bytes();
        if !shell_core::valid_command_name(name) {
            return Err(HostError::Other(String::from("bad command name")));
        }
        let path: Vec<u8> = [&b"bin/"[..], name].concat();
        let mut file = self
            .root
            .open(&path, 0, rights::READ)
            .map_err(|_| HostError::NotFound)?;
        let image = file.get_vmo();
        file.close();
        let (vmo, offset, len) = image.map_err(|_| HostError::NotFound)?;
        let want = rights::READ | rights::ENUMERATE;
        let trust = self.trusted.iter().position(|t| t.name == name);
        let trusted = trust.is_some_and(|t| self.trusted[t].root);
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
        let cwd = self.cwd.clone_conn(want);
        let root = if trusted {
            Some(self.root.clone_conn(want))
        } else {
            None
        };
        let mut forward: Vec<u32> = alloc::vec![stdin, stdout, stderr];
        let mut startup = Startup {
            args: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            roles: alloc::vec![(Role::Stdin, 0), (Role::Stdout, 1), (Role::Stderr, 2)],
            ..Startup::default()
        };
        // the environment the shell passes (its `$env` binding, A2), then the working directory as a path: the libc's
        // `getcwd`, and `..` that climbs, start from it
        for (k, v) in env {
            startup.env.push(format!("{k}={v}").into_bytes());
        }
        startup.env.push([&b"PWD="[..], &self.cwd_path].concat());
        // which of the standard streams are the terminal (a program tells a tty from a pipe by this)
        if io.stdin == Stream(0) {
            startup.env.push(b"STDIN=tty".to_vec());
        }
        if io.stdout == Stream(1) {
            startup.env.push(b"STDOUT=tty".to_vec());
        }
        match cwd {
            Ok(c) => {
                startup
                    .namespace
                    .push((b".".to_vec(), forward.len() as u32));
                forward.push(c.ch);
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
        let confined = self.confined.last().cloned();
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
                startup
                    .namespace
                    .push((b"/".to_vec(), forward.len() as u32));
                forward.push(r.ch);
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
                startup
                    .namespace
                    .push((b"/dev".to_vec(), forward.len() as u32));
                forward.push(c.ch);
            }
        }
        if let Some(d) = self.proc_root.as_mut() {
            if let Ok(c) = d.clone_conn(rights::READ | rights::ENUMERATE) {
                startup
                    .namespace
                    .push((b"/proc".to_vec(), forward.len() as u32));
                forward.push(c.ch);
            }
        }
        if self.tmp.is_some() {
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
                startup
                    .namespace
                    .push((b"/tmp".to_vec(), forward.len() as u32));
                forward.push(c.ch);
                self.new_tmp = Some(name);
            }
        }
        // the volumes this program is granted, opened now from the registry (retrying while the volume manager brings a
        // restarted volume server back)
        if let Some(grants) = &confined {
            // `confine`: exactly the grants, opened now, each as narrow as asked
            for (path, rw) in grants {
                match self.open_grant(path, rw) {
                    Ok(f) => {
                        startup
                            .namespace
                            .push((path.as_bytes().to_vec(), forward.len() as u32));
                        forward.push(f.ch);
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
                    startup.namespace.push((mount, forward.len() as u32));
                    forward.push(f.ch);
                }
            }
        }
        self.txid = self.txid.wrapping_add(1);
        let r = rt::launch::spawn_via(self.launch, vmo, offset, len, &startup, &forward, self.txid);
        sys::handle_close(vmo);
        match r {
            Ok(p) => Ok(p),
            Err(e) => {
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
        let started = self.start(argv, env, io);
        let tmp = self.new_tmp.take();
        let id = self.next_pid;
        let h = match started {
            Ok(h) => h,
            Err(e) => {
                if let (Some(n), Some(t)) = (tmp, self.tmp.as_mut()) {
                    remove_tree(t, &n, 0);
                }
                return Err(e);
            }
        };
        self.next_pid += 1;
        self.procs.insert(id, h);
        self.proc_announce(id, h, argv);
        if let Some(n) = tmp {
            self.tmp_of.insert(id, n);
        }
        Ok(Pid(id))
    }

    fn wait(&mut self, pid: Pid) -> i32 {
        let Some(h) = self.procs.remove(&pid.0) else {
            return -1;
        };
        sys::object_wait(h, signals::TERMINATED, u64::MAX);
        self.reap(pid.0, h)
    }

    fn try_wait(&mut self, pid: Pid) -> Option<i32> {
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
        io::read(h, buf).map_err(|_| HostError::Other(String::from("read failed")))
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
            sys::port_wait(port, u64::MAX);
            sys::handle_close(port);
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
        io::write_all(h, bytes).map_err(|e| match e {
            io::IoError::PeerClosed => HostError::Closed,
            _ => HostError::Other(String::from("write failed")),
        })
    }

    fn close(&mut self, s: Stream) {
        if s.0 < 3 {
            return;
        }
        match self.objs.remove(&s.0) {
            Some(Obj::PipeR(h) | Obj::PipeW(h)) => {
                sys::handle_close(h);
            }
            Some(Obj::FileW { path, buf, append }) => {
                let _ = self.store(&path, &buf, append);
            }
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
            })),
            OpenMode::Append => Ok(self.add_obj(Obj::FileW {
                path: String::from(path),
                buf: Vec::new(),
                append: true,
            })),
        }
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostError> {
        let rel = self.rel_path(path);
        let mut f = self.root.open(&rel, 0, rights::READ).map_err(host_err)?;
        let r = f.read_all();
        f.close();
        r.map_err(host_err)
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), HostError> {
        self.store(path, bytes, false)
    }

    fn glob(&mut self, _pattern: &str) -> Vec<String> {
        Vec::new() // (T126: patterns are not expanded in the OS yet)
    }

    fn term_read(&mut self) -> TermIn {
        self.go_raw();
        if !self.inbuf.is_empty() {
            return TermIn::Bytes(core::mem::take(&mut self.inbuf));
        }
        loop {
            let p = sys::port_create();
            if p.status != 0 {
                return TermIn::Gone;
            }
            let port = p.a1 as u32;
            sys::port_bind(port, self.tty, signals::READABLE | signals::PEER_CLOSED, 1);
            sys::port_bind(port, self.ctl, signals::READABLE | signals::PEER_CLOSED, 2);
            let w = sys::port_wait(port, u64::MAX);
            sys::handle_close(port);
            if w.status != 0 {
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
                    Some(ctl::Event::Interrupt) => return TermIn::Interrupt,
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
        // register the job (duplicates with only KILL|WAIT|TRANSFER)
        let job_rights = Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
        let hs: Vec<u32> = pids
            .iter()
            .filter_map(|p| self.procs.get(&p.0).copied())
            .collect();
        let dups: Vec<u32> = hs.iter().filter_map(|&p| dup(p, job_rights)).collect();
        let registered = if dups.len() == hs.len() && !hs.is_empty() {
            self.control(Control::SetForeground, &dups)
        } else {
            close_all(&dups);
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
        if self.interactive {
            self.control(Control::ClearForeground, &[]);
        }
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
        let mut f = self
            .root
            .open(&rel, fl, rights::READ | rights::WRITE)
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
        next_pid: 1,
        killed: false,
        session_env: Vec::new(),
        confined: Vec::new(),
        raw: false,
        interrupt: false,
        interactive: ctl_ch.is_some(),
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
