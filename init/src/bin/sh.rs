//! The shell (S4e, S4-SPEC section 8). Reads lines from the console server's terminal socket,
//! parses them with `shell-core`, and starts each stage of a pipeline through the launcher with
//! only what it needs: STDIN/STDOUT (the terminal or pipe ends, narrowed), a read-only working
//! directory, and, for the programs named in `/etc/trusted`, a read-only root. It registers the
//! stages as the foreground job with the console server, so Ctrl-C reaches them.
//!
//! Known gap (TIDY-UP T31): a Ctrl-C between a stage starting and the registration being
//! acknowledged is lost; the specified start gate is not built.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
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

const PROMPT: &[u8] = b"$ ";
const BANNER: &[u8] = b"riscv-os S4 ready\n";
const HELP: &[u8] = b"builtins: cd pwd exit help halt\nprograms: ls cat echo probe spin\n";
const KILLED: i64 = -130;
const PIPE_EXIT: i64 = 141;

enum Event {
    Line(Vec<u8>),
    Interrupt,
    Eof,
    Gone,
}

struct Shell {
    tty: u32,
    root: Fs,
    cwd: Fs,
    cwd_path: Vec<u8>,
    launch: u32,
    ctl: u32,
    init_ch: u32,
    trusted: Vec<Vec<u8>>,
    txid: u32,
    inbuf: Vec<u8>,
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

impl Shell {
    fn say(&self, bytes: &[u8]) {
        say(self.tty, bytes);
    }

    /// The next thing the user did: a line, Ctrl-C (a lone 0x03) or Ctrl-D (a lone 0x04).
    fn next_event(&mut self) -> Event {
        loop {
            if let Some(&b) = self.inbuf.first() {
                match b {
                    0x03 => {
                        self.inbuf.remove(0);
                        return Event::Interrupt;
                    }
                    0x04 => {
                        self.inbuf.remove(0);
                        return Event::Eof;
                    }
                    _ => {}
                }
            }
            if let Some(i) = self.inbuf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.inbuf.drain(..=i).collect();
                line.pop();
                return Event::Line(line);
            }
            let mut buf = [0u8; 256];
            match io::read(self.tty, &mut buf) {
                Ok(0) | Err(_) => return Event::Gone,
                Ok(n) => self.inbuf.extend_from_slice(&buf[..n]),
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
                return ctl::decode_reply(&rb[..r.a1]) == Some((self.txid, 0));
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

    fn load_trusted(&mut self) {
        if let Ok(mut f) = self.root.open(b"etc/trusted", 0, rights::READ) {
            if let Ok(data) = f.read_all() {
                self.trusted = data
                    .split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty() && l[0] != b'#')
                    .map(|l| l.to_vec())
                    .collect();
            }
            f.close();
        }
    }

    fn cd(&mut self, args: &[Vec<u8>]) {
        if args.len() != 1 {
            self.say(b"cd: usage: cd <dir>\n");
            return;
        }
        let arg = &args[0];
        let show = String::from_utf8_lossy(arg).into_owned();
        let Some(path) = shell_core::resolve(&self.cwd_path, arg) else {
            self.say(format!("cd: {show}: not found\n").as_bytes());
            return;
        };
        let rel: &[u8] = &path[1..]; // without the leading slash
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
                self.cwd_path = path;
            }
            Err(e) => {
                let why = if e == proto::file::status::NOT_DIRECTORY {
                    "not a directory"
                } else {
                    "not found"
                };
                self.say(format!("cd: {show}: {why}\n").as_bytes());
            }
        }
    }

    /// Start one stage; returns the process handle, or the message to print.
    fn spawn_stage(&mut self, words: &[Vec<u8>], stdin: u32, stdout: u32) -> Result<u32, String> {
        let name = &words[0];
        let shown = String::from_utf8_lossy(name).into_owned();
        let path: Vec<u8> = [&b"bin/"[..], name].concat();
        let mut file = match self.root.open(&path, 0, rights::READ) {
            Ok(f) => f,
            Err(_) => {
                close_all(&[stdin, stdout]);
                return Err(format!("sh: {shown}: not found\n"));
            }
        };
        let image = file.get_vmo();
        file.close();
        let Ok((vmo, offset, len)) = image else {
            close_all(&[stdin, stdout]);
            return Err(format!("sh: {shown}: not found\n"));
        };
        let want = rights::READ | rights::ENUMERATE;
        let trusted = self.trusted.iter().any(|t| t == name);
        let cwd = self.cwd.clone_conn(want);
        let root = if trusted {
            Some(self.root.clone_conn(want))
        } else {
            None
        };
        let mut forward: Vec<u32> = alloc::vec![stdin, stdout];
        let mut startup = Startup {
            args: words.to_vec(),
            roles: alloc::vec![(Role::Stdin, 0), (Role::Stdout, 1)],
            ..Startup::default()
        };
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
                return Err(String::from("sh: could not open the working directory\n"));
            }
        }
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
                return Err(String::from("sh: could not open the root directory\n"));
            }
            None => {}
        }
        self.txid = self.txid.wrapping_add(1);
        let r = rt::launch::spawn_via(self.launch, vmo, offset, len, &startup, &forward, self.txid);
        sys::handle_close(vmo);
        match r {
            Ok(p) => Ok(p),
            Err(e) => {
                close_all(&forward);
                Err(format!("sh: {shown}: could not start ({e})\n"))
            }
        }
    }

    fn run_pipeline(&mut self, stages: &[shell_core::Stage]) {
        let n = stages.len();
        // the pipes between stages: pipe i joins stage i's STDOUT to stage i+1's STDIN
        let mut ends: Vec<(u32, u32)> = Vec::new(); // (write end for stage i, read end for i+1)
        for _ in 1..n {
            let s = sys::socket_create();
            if s.status != 0 {
                for &(w, r) in &ends {
                    close_all(&[w, r]);
                }
                self.say(b"sh: could not make a pipe\n");
                return;
            }
            let (a, b) = (s.a1 as u32, s.a2 as u32);
            let w = dup(a, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0);
            let r = dup(b, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0);
            close_all(&[a, b]);
            let (Some(w), Some(r)) = (w, r) else {
                for h in [w, r].into_iter().flatten() {
                    sys::handle_close(h);
                }
                for &(w, r) in &ends {
                    close_all(&[w, r]);
                }
                self.say(b"sh: could not make a pipe\n");
                return;
            };
            ends.push((w, r));
        }
        let tty_in = Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
        let tty_out = Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
        let mut procs: Vec<u32> = Vec::new();
        let mut failure: Option<String> = None;
        // which pipe ends were handed to a stage (a stage consumes its handles even when it
        // fails to start); the rest are closed below
        let mut taken_w = alloc::vec![false; ends.len()];
        let mut taken_r = alloc::vec![false; ends.len()];
        for (i, words) in stages.iter().enumerate() {
            let tty_dup_in = if i == 0 { dup(self.tty, tty_in) } else { None };
            let tty_dup_out = if i + 1 == n {
                dup(self.tty, tty_out)
            } else {
                None
            };
            if (i == 0 && tty_dup_in.is_none()) || (i + 1 == n && tty_dup_out.is_none()) {
                for h in [tty_dup_in, tty_dup_out].into_iter().flatten() {
                    sys::handle_close(h);
                }
                failure = Some(String::from("sh: could not duplicate the terminal\n"));
                break;
            }
            let stdin = match tty_dup_in {
                Some(h) => h,
                None => {
                    taken_r[i - 1] = true;
                    ends[i - 1].1
                }
            };
            let stdout = match tty_dup_out {
                Some(h) => h,
                None => {
                    taken_w[i] = true;
                    ends[i].0
                }
            };
            match self.spawn_stage(words, stdin, stdout) {
                Ok(p) => procs.push(p),
                Err(msg) => {
                    failure = Some(msg);
                    break;
                }
            }
        }
        for (i, &(w, r)) in ends.iter().enumerate() {
            if !taken_w[i] {
                sys::handle_close(w);
            }
            if !taken_r[i] {
                sys::handle_close(r);
            }
        }
        if let Some(msg) = failure {
            for &p in &procs {
                sys::process_kill(p, -1);
                sys::handle_close(p);
            }
            self.say(msg.as_bytes());
            return;
        }
        // register the job (duplicates with only KILL|WAIT|TRANSFER), then wait for every stage
        let job_rights = Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
        let dups: Vec<u32> = procs.iter().filter_map(|&p| dup(p, job_rights)).collect();
        let registered = dups.len() == procs.len() && self.control(Control::SetForeground, &dups);
        if !registered {
            // (`control` closed the handles it was given when the write failed)
            self.say(b"sh: could not register the job\n");
        }
        let port = sys::port_create();
        let mut left = procs.len();
        if port.status == 0 {
            let port = port.a1 as u32;
            for (i, &p) in procs.iter().enumerate() {
                sys::port_bind(port, p, signals::TERMINATED, i as u64);
            }
            // a stage that is already gone is reported at once (level-triggered)
            let mut done = alloc::vec![false; procs.len()];
            while left > 0 {
                let r = sys::port_wait(port, u64::MAX);
                if r.status != 0 {
                    break;
                }
                let k = r.a1;
                if !done[k] {
                    done[k] = true;
                    left -= 1;
                    sys::port_unbind(port, k as u64);
                }
            }
            sys::handle_close(port);
        }
        let mut killed = false;
        let mut report = String::new();
        for &p in &procs {
            let code = sys::process_info(p).a1 as i64;
            if code == KILLED {
                killed = true;
            }
            if code != 0 && code != PIPE_EXIT {
                report.push_str(&format!("[exit {code}]\n"));
            }
        }
        if registered {
            self.control(Control::ClearForeground, &[]);
        }
        close_all(&procs);
        self.say(report.as_bytes());
        if killed {
            self.swallow_interrupt();
        }
    }

    fn run_line(&mut self, line: &[u8]) -> Option<i32> {
        let stages = match shell_core::parse(line) {
            Ok(s) => s,
            Err(e) => {
                self.say(format!("sh: {}\n", e.message()).as_bytes());
                return None;
            }
        };
        if stages.is_empty() {
            return None;
        }
        for st in &stages {
            let name = &st[0];
            if !shell_core::valid_command_name(name) {
                let shown = String::from_utf8_lossy(name).into_owned();
                self.say(format!("sh: {shown}: bad command name\n").as_bytes());
                return None;
            }
        }
        let is_builtin = |n: &[u8]| matches!(n, b"cd" | b"pwd" | b"exit" | b"help" | b"halt");
        if stages.len() > 1 {
            if let Some(st) = stages.iter().find(|st| is_builtin(&st[0])) {
                let shown = String::from_utf8_lossy(&st[0]).into_owned();
                self.say(format!("sh: {shown}: a builtin cannot be piped\n").as_bytes());
                return None;
            }
        }
        let name = stages[0][0].clone();
        if is_builtin(&name) {
            match name.as_slice() {
                b"cd" => self.cd(&stages[0][1..]),
                b"pwd" => {
                    let mut out = self.cwd_path.clone();
                    out.push(b'\n');
                    self.say(&out);
                }
                b"help" => self.say(HELP),
                b"exit" => return Some(0),
                _ => {
                    // halt: ask init to end the session, then wait to be stopped
                    sys::channel_write(self.init_ch, b"halt".as_ptr(), 4, core::ptr::null(), 0);
                    io::nap(5000);
                    return Some(1);
                }
            }
            return None;
        }
        // (a stage that names no program is reported by `spawn_stage`; stages are checked there)
        self.run_pipeline(&stages);
        None
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(tty), Some(root_ch), Some(launch), Some(ctl_ch), Some(init_ch)) = (
        env.handle(Role::Tty),
        env.handle(Role::ServiceFs),
        env.handle(Role::ServiceLaunch),
        env.handle(Role::ServiceConsole),
        env.handle(Role::ServiceInit),
    ) else {
        return 3;
    };
    let mut root = Fs::new(root_ch);
    let Ok(cwd) = root.open(b"home", flags::DIRECTORY, rights::READ | rights::ENUMERATE) else {
        return 4;
    };
    let mut sh = Shell {
        tty,
        root,
        cwd,
        cwd_path: b"/home".to_vec(),
        launch,
        ctl: ctl_ch,
        init_ch,
        trusted: Vec::new(),
        txid: 0,
        inbuf: Vec::new(),
    };
    sh.load_trusted();
    sh.say(BANNER);
    loop {
        sh.say(PROMPT);
        match sh.next_event() {
            Event::Line(line) => {
                if let Some(code) = sh.run_line(&line) {
                    return code;
                }
            }
            Event::Interrupt => {} // the console server already echoed ^C: just prompt again
            Event::Eof => return 0,
            Event::Gone => return 0,
        }
    }
}
