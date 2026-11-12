//! The console server (S4d, S4-SPEC section 8): owns the serial socket, runs the line
//! discipline (`proto::tty`, over `console::LineEditor`) and serves one terminal session.
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
/// Bytes held for the serial line, and for the client.
const OUT_CAP: usize = 16 * 1024;
const DELIVER_CAP: usize = 4 * 1024;

const K_SERIAL: u64 = 1;
const K_SERIAL_W: u64 = 2;
const K_TTY: u64 = 3;
const K_TTY_W: u64 = 4;
const K_CTL: u64 = 5;

struct Console {
    port: u32,
    serial: u32,
    tty_sock: u32,
    ctl_ch: Option<u32>,
    tty: Tty,
    /// Bytes taken from `tty` that the serial socket has not accepted yet.
    serial_buf: Vec<u8>,
    /// Lines (and 0x03/0x04) waiting for the client's socket.
    deliver: Vec<u8>,
    lost_input: u64,
    jobs: Vec<u32>,
    serial_w_bound: bool,
    tty_w_bound: bool,
    /// A write failed for lack of kernel memory: retry on a timer, not on a signal.
    retry: bool,
}

impl Console {
    fn bind(&self, key: u64, obj: u32, mask: u32) {
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
        // no IPC: this works with the shared IPC budget exhausted; BAD_STATE (already dead) is fine
        for &h in &self.jobs {
            sys::process_kill(h, KILLED);
        }
    }

    /// Push queued output to the serial socket. Returns false when the serial line is gone.
    fn pump_serial(&mut self) -> bool {
        self.retry = false;
        loop {
            if self.serial_buf.is_empty() {
                if !self.tty.has_output() {
                    break;
                }
                self.serial_buf = self.tty.take_out(512);
            }
            let r = sys::socket_write(self.serial, self.serial_buf.as_ptr(), self.serial_buf.len());
            match r.status() {
                Some(Status::Ok) => {
                    self.serial_buf.drain(..r.a1);
                }
                Some(Status::ShouldWait) => {
                    if !self.serial_w_bound {
                        self.bind(
                            K_SERIAL_W,
                            self.serial,
                            signals::WRITABLE | signals::PEER_CLOSED,
                        );
                        self.serial_w_bound = true;
                    }
                    return true;
                }
                Some(Status::NoMemory) => {
                    self.retry = true;
                    return true;
                }
                _ => return false, // PEER_CLOSED or worse
            }
        }
        if self.serial_w_bound {
            self.unbind(K_SERIAL_W);
            self.serial_w_bound = false;
        }
        true
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
        let r = sys::socket_read(self.serial, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => {
                let input = self.tty.serial_in(&buf[..r.a1]);
                if input.kill {
                    self.kill_jobs();
                }
                if self.deliver.len() + input.deliver.len() <= DELIVER_CAP {
                    self.deliver.extend_from_slice(&input.deliver);
                } else {
                    self.lost_input += input.deliver.len() as u64;
                }
                true
            }
            Some(Status::ShouldWait) => true,
            _ => false, // the serial line is gone (PEER_CLOSED) or broken
        }
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

    fn reply(&self, ch: u32, txid: u32, status: i32) {
        let bytes = ctl::encode_reply(txid, status);
        // (a full or dead channel loses the reply; the client's call then fails: its problem)
        sys::channel_write(ch, bytes.as_ptr(), bytes.len(), core::ptr::null(), 0);
    }

    fn on_ctl(&mut self) {
        let Some(ch) = self.ctl_ch else { return };
        let mut bytes = [0u8; 16];
        let mut handles = [0u32; ctl::MAX_JOBS + 1];
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
                self.jobs.extend_from_slice(hs);
                self.reply(ch, txid, 0);
            }
            Ok((txid, Control::ClearForeground)) => {
                self.clear_jobs();
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

    fn drop_ctl(&mut self) {
        if let Some(ch) = self.ctl_ch.take() {
            self.unbind(K_CTL);
            sys::handle_close(ch);
        }
        self.clear_jobs();
    }

    /// Best effort at the end of the session: let the serial line take what is queued.
    fn flush_at_end(&mut self) {
        let deadline = sys::clock_get() + 1000;
        while sys::clock_get() < deadline {
            self.pump_serial();
            if self.serial_buf.is_empty() && !self.tty.has_output() {
                return;
            }
            sys::object_wait(
                self.serial,
                signals::WRITABLE | signals::PEER_CLOSED,
                sys::clock_get() + 20,
            );
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
        port: p.a1 as u32,
        serial,
        tty_sock,
        ctl_ch: Some(ctl_ch),
        tty: Tty::new(OUT_CAP),
        serial_buf: Vec::new(),
        deliver: Vec::new(),
        lost_input: 0,
        jobs: Vec::new(),
        serial_w_bound: false,
        tty_w_bound: false,
        retry: false,
    };
    c.bind(K_SERIAL, serial, signals::READABLE | signals::PEER_CLOSED);
    c.bind(K_TTY, tty_sock, signals::READABLE | signals::PEER_CLOSED);
    c.bind(K_CTL, ctl_ch, signals::READABLE | signals::PEER_CLOSED);
    loop {
        let deadline = if c.retry {
            sys::clock_get() + 10
        } else {
            u64::MAX
        };
        let r = sys::port_wait(c.port, deadline);
        let alive = match r.status() {
            Some(Status::Ok) => {
                let (key, observed) = (r.a1 as u64, r.a2 as u32);
                match key {
                    K_SERIAL => c.on_serial(),
                    K_TTY => c.on_tty(),
                    K_CTL => {
                        if observed & signals::READABLE != 0 {
                            c.on_ctl();
                        } else {
                            c.drop_ctl();
                        }
                        true
                    }
                    // K_SERIAL_W / K_TTY_W: the pumps below retry
                    _ => observed & signals::PEER_CLOSED == 0,
                }
            }
            Some(Status::TimedOut) => true,
            _ => return 5,
        };
        if !alive {
            c.flush_at_end();
            c.clear_jobs();
            return 0;
        }
        let tty_ok = c.pump_tty();
        let serial_ok = c.pump_serial();
        if !serial_ok || !tty_ok {
            c.flush_at_end();
            c.clear_jobs();
            return 0;
        }
    }
}
