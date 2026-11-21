//! The console server (S4d, S4-SPEC section 8): owns the serial socket, runs the line
//! discipline (`proto::tty`, over `console::LineEditor`) and serves one terminal session.
//!
//! Terminal modes, the window size and events (USER-VIEW-SPEC section 9, step UV4): the owner on the control channel
//! sets the mode and size and gets `Interrupt`, `Hangup` and `Resize` events pushed to it (`proto::console`).
//!
//! Handles: `Serial` (the serial socket), `Tty` (the session's socket: the client's bytes go
//! to the serial line, lines come back), `ServiceConsole` (the control channel on which the
//! shell registers the foreground job). Authority beyond the three: `KILL|WAIT|TRANSFER`
//! duplicates of the foreground job's processes, nothing else.
//!
//! Fully event-driven on one port, never blocking in a write: output the serial line cannot
//! take yet waits in `Tty`'s bounded queue (oldest dropped and counted); a line the client
//! cannot take yet waits in a bounded buffer. The session ends (exit 0) when the client closes
//! its terminal socket or the serial line goes away.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use proto::console::{self as ctl, Control};
use proto::startup::Role;
use proto::tty::Tty;
use rt::abi::signals;
use rt::abi::status::Status;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

/// Exit code given to a job killed by Ctrl-C (128 + SIGINT).
const KILLED: i32 = -130;
/// What Ctrl-\\ ends a job with: killed by SIGQUIT (`-2000 - 3`, as the C library's default action does).
const QUIT_KILLED: i32 = -2003;
/// How long a hangup lets the foreground programs act on SIGHUP before the console server ends them.
const HANGUP_GRACE_MS: u64 = 500;
/// Bytes held for the serial line, and for the client.
const OUT_CAP: usize = 16 * 1024;
const DELIVER_CAP: usize = 4 * 1024;

/// How long a size query waits for the host terminal's report.
const QUERY_MS: u64 = 2000;

const K_SERIAL: u64 = 1;
const K_SERIAL_W: u64 = 2;
const K_TTY: u64 = 3;
const K_TTY_W: u64 = 4;
const K_CTL: u64 = 5;
const K_TERM: u64 = 6;
/// What the terminal remembers of its recent output, to show a backend that attaches later.
const SCROLL_CAP: usize = 8 * 1024;

struct Console {
    port: u64,
    /// The backend's socket: the serial line, or what a provider attached. `None` while detached.
    serial: Option<u64>,
    /// A backend that closes is a detach, not the end (UV7): the terminal keeps its job and its recent output.
    detachable: bool,
    term_ch: Option<u64>,
    scroll: Vec<u8>,
    tty_sock: u64,
    ctl_ch: Option<u64>,
    tty: Tty,
    /// Bytes taken from `tty` that the serial socket has not accepted yet.
    serial_buf: Vec<u8>,
    /// Lines (and 0x03/0x04) waiting for the client's socket.
    deliver: Vec<u8>,
    lost_input: u64,
    jobs: Vec<u64>,
    /// A foreground job is declared (`SetForeground` or `HoldForeground`): Ctrl-C is reported to the owner as `JobInterrupt`.
    fg_active: bool,
    serial_w_bound: bool,
    tty_w_bound: bool,
    /// A write failed for lack of kernel memory: retry on a timer, not on a signal.
    retry: bool,
    /// When the pending size query gives up.
    query_deadline: Option<u64>,
}

impl Console {
    fn bind(&self, key: u64, obj: u64, mask: u32) {
        sys::port_bind(self.port, obj, mask, key);
    }

    fn unbind(&self, key: u64) {
        sys::port_unbind(self.port, key);
    }

    fn clear_jobs(&mut self) {
        for h in self.jobs.drain(..) {
            sys::handle_close(h);
        }
    }

    fn kill_jobs(&self) {
        self.kill_jobs_with(KILLED);
    }

    fn kill_jobs_with(&self, code: i32) {
        // no IPC: this works with the shared IPC budget exhausted; BAD_STATE (already dead) is fine
        for &h in &self.jobs {
            sys::process_kill(h, code);
        }
    }

    /// Remember output for a backend that attaches later (the newest `SCROLL_CAP` bytes).
    fn remember(&mut self, bytes: &[u8]) {
        self.scroll.extend_from_slice(bytes);
        if self.scroll.len() > SCROLL_CAP {
            let cut = self.scroll.len() - SCROLL_CAP;
            self.scroll.drain(..cut);
        }
    }

    /// Push queued output to the backend. Returns false when the backend is gone and cannot be detached.
    fn pump_serial(&mut self) -> bool {
        let Some(serial) = self.serial else {
            // detached: the output is kept for whoever attaches next, and the job runs on
            while self.tty.has_output() {
                let out = self.tty.take_out(512);
                self.remember(&out);
            }
            return true;
        };
        loop {
            if self.serial_buf.is_empty() {
                if !self.tty.has_output() {
                    break;
                }
                self.serial_buf = self.tty.take_out(512);
                let copy = self.serial_buf.clone();
                self.remember(&copy);
            }
            let r = sys::socket_write(serial, self.serial_buf.as_ptr(), self.serial_buf.len());
            match r.status() {
                Some(Status::Ok) => {
                    self.serial_buf.drain(..r.a1);
                }
                Some(Status::ShouldWait) => {
                    if !self.serial_w_bound {
                        self.bind(K_SERIAL_W, serial, signals::WRITABLE | signals::PEER_CLOSED);
                        self.serial_w_bound = true;
                    }
                    return true;
                }
                Some(Status::NoMemory) => {
                    self.retry = true;
                    if self.serial_w_bound {
                        // (a level-triggered binding would fire at once and spin)
                        self.unbind(K_SERIAL_W);
                        self.serial_w_bound = false;
                    }
                    return true;
                }
                _ => {
                    // PEER_CLOSED or worse
                    if self.detachable {
                        self.detach();
                        return true;
                    }
                    return false;
                }
            }
        }
        if self.serial_w_bound {
            self.unbind(K_SERIAL_W);
            self.serial_w_bound = false;
        }
        true
    }

    /// Let go of the backend; the terminal, its job and its recent output stay.
    fn detach(&mut self) {
        if let Some(s) = self.serial.take() {
            self.unbind(K_SERIAL);
            if self.serial_w_bound {
                self.unbind(K_SERIAL_W);
                self.serial_w_bound = false;
            }
            sys::handle_close(s);
        }
        self.serial_buf.clear();
    }

    /// A new backend: the old one is closed, the new one is shown what the terminal last displayed.
    fn attach(&mut self, socket: u64) {
        self.detach();
        self.serial = Some(socket);
        self.bind(K_SERIAL, socket, signals::READABLE | signals::PEER_CLOSED);
        self.serial_buf = self.scroll.clone();
    }

    /// Push waiting lines to the client. Returns false when the client is gone.
    fn pump_tty(&mut self) -> bool {
        while !self.deliver.is_empty() {
            let r = sys::socket_write(self.tty_sock, self.deliver.as_ptr(), self.deliver.len());
            match r.status() {
                Some(Status::Ok) => {
                    self.deliver.drain(..r.a1);
                }
                Some(Status::ShouldWait) => {
                    if !self.tty_w_bound {
                        self.bind(
                            K_TTY_W,
                            self.tty_sock,
                            signals::WRITABLE | signals::PEER_CLOSED,
                        );
                        self.tty_w_bound = true;
                    }
                    return true;
                }
                Some(Status::NoMemory) => {
                    self.retry = true;
                    if self.tty_w_bound {
                        self.unbind(K_TTY_W);
                        self.tty_w_bound = false;
                    }
                    return true;
                }
                _ => return false,
            }
        }
        if self.tty_w_bound {
            self.unbind(K_TTY_W);
            self.tty_w_bound = false;
        }
        true
    }

    fn on_serial(&mut self) -> bool {
        let mut buf = [0u8; 256];
        let Some(serial) = self.serial else {
            return true;
        };
        let r = sys::socket_read(serial, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => {
                let input = self.tty.serial_in(&buf[..r.a1]);
                if self.deliver.len() + input.deliver.len() <= DELIVER_CAP {
                    self.deliver.extend_from_slice(&input.deliver);
                } else {
                    self.lost_input += input.deliver.len() as u64;
                }
                // the bytes typed before a Ctrl-C reach the client before the event does (the shell, in raw mode, echoes what it
                // reads: an event that overtook the byte before it printed `^C` ahead of that byte)
                self.pump_tty();
                self.after_input(&input);
                true
            }
            Some(Status::ShouldWait) => true,
            _ if self.detachable => {
                // the backend closed: a detach, not a hangup (UV7)
                self.detach();
                true
            }
            _ => {
                // the serial line is gone (PEER_CLOSED) or broken: the owner is told before the session ends
                self.event(ctl::Event::Hangup);
                false
            }
        }
    }

    /// What a chunk of input asks of the jobs and the owner.
    fn after_input(&mut self, input: &proto::tty::Input) {
        if input.kill {
            self.kill_jobs();
            if !input.interrupt && self.fg_active {
                // canonical mode, a job running: the shell passes SIGINT on to the programs it kept off the list (they handle it)
                self.event(ctl::Event::JobInterrupt);
            }
        }
        if input.interrupt {
            self.event(ctl::Event::Interrupt);
        }
        if input.quit && self.fg_active {
            // Ctrl-\\: the jobs it was given end; the shell ends the rest of the foreground job (it does not ask the program)
            self.kill_jobs_with(QUIT_KILLED);
            self.event(ctl::Event::JobQuit);
        }
        if input.stop && self.fg_active {
            // Ctrl-Z: the shell stops the job (it holds the handles of everything it started); nothing is ended here
            self.event(ctl::Event::JobStop);
        }
        if let Some((cols, rows)) = input.resized {
            self.query_deadline = None;
            self.event(ctl::Event::Resize { cols, rows });
        }
    }

    /// Push an event to the owner (best effort: a full or dead channel loses it).
    fn event(&self, e: ctl::Event) {
        if let Some(ch) = self.ctl_ch {
            let bytes = ctl::encode_event(e);
            sys::channel_write(ch, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
        }
    }

    /// The size query got no answer: what was held is ordinary input after all.
    fn query_timed_out(&mut self) {
        self.query_deadline = None;
        let input = self.tty.cancel_query();
        if self.deliver.len() + input.deliver.len() <= DELIVER_CAP {
            self.deliver.extend_from_slice(&input.deliver);
        } else {
            self.lost_input += input.deliver.len() as u64;
        }
        self.pump_tty();
        self.after_input(&input);
    }

    fn on_tty(&mut self) -> bool {
        let mut buf = [0u8; 256];
        let r = sys::socket_read(self.tty_sock, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => {
                self.tty.client_out(&buf[..r.a1]);
                true
            }
            Some(Status::ShouldWait) => true,
            _ => false, // the session is over
        }
    }

    fn reply(&self, ch: u64, txid: u32, status: i32) {
        let bytes = ctl::encode_reply(txid, status);
        // (a full or dead channel loses the reply; the client's call then fails: its problem)
        sys::channel_write(ch, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
    }

    fn reply_value(&self, ch: u64, txid: u32, value: u32) {
        let bytes = ctl::encode_reply_value(txid, 0, value);
        sys::channel_write(ch, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
    }

    fn on_ctl(&mut self) {
        let Some(ch) = self.ctl_ch else { return };
        let mut bytes = [0u8; 16];
        let mut handles = [0u64; ctl::MAX_JOBS + 1];
        let r = sys::channel_read(
            ch,
            bytes.as_mut_ptr(),
            bytes.len(),
            handles.as_mut_ptr(),
            handles.len(),
        );
        match r.status() {
            Some(Status::Ok) => {}
            Some(Status::ShouldWait) => return,
            _ => {
                // closed, or a message too big for the protocol: stop listening, forget the job
                self.drop_ctl();
                return;
            }
        }
        let (nb, nh) = (r.a1, r.a2);
        let hs = &handles[..nh];
        match ctl::decode(&bytes[..nb], nh) {
            Ok((txid, Control::SetForeground)) => {
                self.clear_jobs();
                self.fg_active = true;
                self.jobs.extend_from_slice(hs);
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::ClearForeground)) => {
                self.clear_jobs();
                self.fg_active = false;
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::HoldForeground)) => {
                self.clear_jobs();
                self.fg_active = true;
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::SetMode(m))) => {
                self.tty.set_mode(m);
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::GetMode)) => self.reply_value(ch, txid, u32::from(self.tty.mode())),
            Ok((txid, Control::SetSize { cols, rows })) => {
                self.tty.set_size(cols, rows);
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::GetSize)) => {
                let (cols, rows) = self.tty.size();
                self.reply_value(ch, txid, u32::from(cols) | u32::from(rows) << 16);
            }
            Ok((txid, Control::QuerySize)) => {
                self.tty.query_size();
                self.query_deadline = Some(sys::clock_get() + QUERY_MS);
                self.reply(ch, txid, 0);
            }
            Err(_) => {
                for &h in hs {
                    sys::handle_close(h);
                }
                let txid = proto::file::peek_txid(&bytes[..nb]).unwrap_or(0);
                self.reply(ch, txid, proto::file::status::BAD_REQUEST);
            }
        }
    }

    /// The backend provider's channel (UV7). Returns false when the terminal ends (a hangup).
    fn on_term(&mut self) -> bool {
        let Some(ch) = self.term_ch else {
            return true;
        };
        let mut bytes = [0u8; 16];
        let mut handles = [0u64; 2];
        let r = sys::channel_read(
            ch,
            bytes.as_mut_ptr(),
            bytes.len(),
            handles.as_mut_ptr(),
            handles.len(),
        );
        match r.status() {
            Some(Status::Ok) => {}
            Some(Status::ShouldWait) => return true,
            _ => {
                // the provider is gone: with no backend and no one to attach one the terminal carries on
                self.unbind(K_TERM);
                sys::handle_close(ch);
                self.term_ch = None;
                return true;
            }
        }
        let (nb, nh) = (r.a1, r.a2);
        match proto::console::term::decode(&bytes[..nb], nh) {
            Some((txid, proto::console::term::OP_ATTACH)) => {
                self.attach(handles[0]);
                self.reply(ch, txid, 0);
                true
            }
            Some((txid, proto::console::term::OP_DETACH)) => {
                self.detach();
                self.reply(ch, txid, 0);
                true
            }
            Some((txid, _hangup)) => {
                self.reply(ch, txid, 0);
                self.event(ctl::Event::Hangup);
                // SIGHUP reaches the programs through the shell (it has just been told): they get a moment to run their handlers
                // or end by the default action before what is left is ended (the terminal is going, nothing else waits for this)
                if !self.jobs.is_empty() {
                    rt::io::nap(HANGUP_GRACE_MS);
                }
                self.kill_jobs();
                false
            }
            None => {
                for &h in &handles[..nh] {
                    sys::handle_close(h);
                }
                let txid = proto::file::peek_txid(&bytes[..nb]).unwrap_or(0);
                self.reply(ch, txid, proto::file::status::BAD_REQUEST);
                true
            }
        }
    }

    fn drop_ctl(&mut self) {
        if let Some(ch) = self.ctl_ch.take() {
            self.unbind(K_CTL);
            sys::handle_close(ch);
        }
        self.clear_jobs();
        self.fg_active = false;
        // nobody owns the terminal any more: back to the plain line discipline
        self.tty.set_mode(proto::console::mode::COOKED);
    }

    /// Best effort at the end of the session: let the serial line take what is queued.
    fn flush_at_end(&mut self) {
        let Some(serial) = self.serial else {
            return;
        };
        let deadline = sys::clock_get() + 1000;
        while sys::clock_get() < deadline {
            if !self.pump_serial() {
                return; // the serial line is gone: nothing more can be flushed
            }
            if self.serial_buf.is_empty() && !self.tty.has_output() {
                return;
            }
            sys::object_wait(serial, signals::WRITABLE, sys::clock_get() + 20);
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
    let (Some(serial), Some(tty_sock), Some(ctl_ch)) = (
        env.handle(Role::Serial),
        env.handle(Role::Tty),
        env.handle(Role::ServiceConsole),
    ) else {
        return 3;
    };
    let p = sys::port_create();
    if p.status != 0 {
        return 4;
    }
    let mut c = Console {
        port: p.a1 as u64,
        serial: Some(serial),
        detachable: env.var(b"DETACHABLE") == Some(b"1"),
        term_ch: env.handle(Role::ServiceTerm),
        scroll: Vec::new(),
        tty_sock,
        ctl_ch: Some(ctl_ch),
        tty: Tty::new(OUT_CAP),
        serial_buf: Vec::new(),
        deliver: Vec::new(),
        lost_input: 0,
        jobs: Vec::new(),
        fg_active: false,
        serial_w_bound: false,
        tty_w_bound: false,
        retry: false,
        query_deadline: None,
    };
    c.bind(K_SERIAL, serial, signals::READABLE | signals::PEER_CLOSED);
    c.bind(K_TTY, tty_sock, signals::READABLE | signals::PEER_CLOSED);
    c.bind(K_CTL, ctl_ch, signals::READABLE | signals::PEER_CLOSED);
    if let Some(t) = c.term_ch {
        c.bind(K_TERM, t, signals::READABLE | signals::PEER_CLOSED);
    }
    loop {
        let mut deadline = if c.retry {
            sys::clock_get() + 10
        } else {
            u64::MAX
        };
        if let Some(q) = c.query_deadline {
            deadline = deadline.min(q);
        }
        let r = sys::port_wait(c.port, deadline);
        let alive = match r.status() {
            Some(Status::Ok) => {
                let (key, observed) = (r.a1 as u64, r.a2 as u32);
                match key {
                    K_SERIAL => c.on_serial(),
                    K_TTY => c.on_tty(),
                    K_TERM => c.on_term(),
                    K_CTL => {
                        if observed & signals::READABLE != 0 {
                            c.on_ctl();
                        } else {
                            c.drop_ctl();
                        }
                        true
                    }
                    K_SERIAL_W if c.detachable => {
                        if observed & signals::PEER_CLOSED != 0 {
                            c.detach();
                        }
                        true
                    }
                    // K_SERIAL_W / K_TTY_W: the pumps below retry
                    _ => observed & signals::PEER_CLOSED == 0,
                }
            }
            Some(Status::TimedOut) => {
                if c.query_deadline.is_some_and(|q| sys::clock_get() >= q) {
                    c.query_timed_out();
                }
                true
            }
            _ => return 5,
        };
        if !alive {
            c.flush_at_end();
            c.clear_jobs();
            return 0;
        }
        c.retry = false; // (each pump sets it again when its write needs a retry)
        let tty_ok = c.pump_tty();
        let serial_ok = c.pump_serial();
        if !serial_ok || !tty_ok {
            c.flush_at_end();
            c.clear_jobs();
            return 0;
        }
    }
}
