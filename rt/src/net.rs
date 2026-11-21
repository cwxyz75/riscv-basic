//! The client of the network stack (S8, NET-SPEC 7.5): a typed request/reply connection over a registry-opened channel
//! (`/svc/net`, `/svc/net-priv`, `/svc/net-config`), and UDP and ICMP-echo endpoints on channels of their own. Every call
//! takes a **deadline** (no forever form). `Net::connect` retries `UNAVAILABLE` (a restarting stack) with a bounded
//! back-off: first retry after 100 ms, doubling to 1 s, giving up at 10 s.

extern crate alloc;

use crate::fs::Fs;
use crate::sys;
use abi::signals;
use abi::status::Status;
use alloc::vec::Vec;
use proto::net::{self as pn, EpEvent, Info, Request};

/// The registry's "the service behind this name is gone, try again" (`proto::file::status::UNAVAILABLE`).
const UNAVAILABLE: i32 = -109;

/// A request/reply connection to the stack.
pub struct Net {
    ch: u64,
    txid: u32,
}

/// What a datagram receive returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub src: [u8; 4],
    pub port: u16,
    pub payload: Vec<u8>,
    /// How many datagrams the stack dropped for this endpoint since the previous one.
    pub dropped: u32,
}

/// An echo reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoReply {
    pub src: [u8; 4],
    pub seq: u16,
    pub ttl: u8,
    pub payload: Vec<u8>,
}

/// Why a call failed: a kernel status, the stack's status, or no answer in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    Sys(i32),
    Stack(i32),
    TimedOut,
    Closed,
    Garbled,
}

impl Net {
    /// Wrap a connection (the channel `Fs::open` returned for the name).
    pub fn new(ch: u64) -> Net {
        Net { ch, txid: 0 }
    }

    pub fn channel(&self) -> u64 {
        self.ch
    }

    /// Open `name` under the `/svc` directory `svc`, retrying `UNAVAILABLE` until `give_up` (ticks).
    pub fn connect(svc: &mut Fs, name: &[u8], give_up: u64) -> Result<Net, NetError> {
        let mut wait = 100;
        loop {
            match svc.open(name, 0, proto::file::rights::READ) {
                Ok(fs) => return Ok(Net::new(fs.ch)),
                Err(UNAVAILABLE) if sys::clock_get() + wait < give_up => {
                    let gate = sys::event_create();
                    if gate.status == 0 {
                        sys::object_wait(gate.a1 as u64, 1 << 24, sys::clock_get() + wait);
                        sys::handle_close(gate.a1 as u64);
                    }
                    wait = (wait * 2).min(1000);
                }
                Err(e) => return Err(NetError::Sys(e)),
            }
        }
    }

    /// One request and its reply, with an optional handle to send. `(stack status, body)`.
    pub fn call(
        &mut self,
        r: &Request,
        send: Option<u64>,
        deadline: u64,
    ) -> Result<(i32, Vec<u8>), NetError> {
        let hs = [send.unwrap_or(0)];
        self.call_with(r, &hs[..usize::from(send.is_some())], deadline)
    }

    /// As `call`, with any number of handles (S9's `Accept` carries two). On a failed send every handle is closed here;
    /// on a sent request the stack owns them.
    pub fn call_with(
        &mut self,
        r: &Request,
        send: &[u64],
        deadline: u64,
    ) -> Result<(i32, Vec<u8>), NetError> {
        self.txid = self.txid.wrapping_add(1);
        let req = pn::encode_request(self.txid, 0, r);
        let w = sys::channel_write(self.ch, req.as_ptr(), req.len(), send.as_ptr(), send.len());
        if w.status != 0 {
            for &h in send {
                sys::handle_close(h);
            }
            return Err(if w.status == Status::PeerClosed.code() {
                NetError::Closed
            } else {
                NetError::Sys(w.status)
            });
        }
        loop {
            let wait =
                sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
            if wait.status != 0 {
                return Err(NetError::TimedOut);
            }
            let mut buf = alloc::vec![0u8; 4096];
            let rd = sys::channel_read(
                self.ch,
                buf.as_mut_ptr(),
                buf.len(),
                core::ptr::null_mut(),
                0,
            );
            if rd.status == Status::ShouldWait.code() {
                if wait.a1 as u32 & signals::PEER_CLOSED != 0 {
                    return Err(NetError::Closed);
                }
                continue;
            }
            if rd.status != 0 {
                return Err(NetError::Closed);
            }
            let Some((txid, status, body)) = pn::decode_reply(&buf[..rd.a1]) else {
                return Err(NetError::Garbled);
            };
            if txid == self.txid {
                return Ok((status, body.to_vec()));
            }
        }
    }

    fn ok(&mut self, r: &Request, send: Option<u64>, deadline: u64) -> Result<Vec<u8>, NetError> {
        match self.call(r, send, deadline)? {
            (0, body) => Ok(body),
            (s, _) => Err(NetError::Stack(s)),
        }
    }

    pub fn info(&mut self, deadline: u64) -> Result<Info, NetError> {
        let b = self.ok(&Request::Info, None, deadline)?;
        pn::decode_info(&b).ok_or(NetError::Garbled)
    }

    pub fn set_config(
        &mut self,
        addr: [u8; 4],
        mask: [u8; 4],
        gateway: [u8; 4],
        dns: [u8; 4],
        deadline: u64,
    ) -> Result<(), NetError> {
        self.ok(
            &Request::SetConfig {
                addr,
                mask,
                gateway,
                dns,
            },
            None,
            deadline,
        )
        .map(|_| ())
    }

    pub fn clear_config(&mut self, deadline: u64) -> Result<(), NetError> {
        self.ok(&Request::ClearConfig, None, deadline).map(|_| ())
    }

    pub fn arp(&mut self, deadline: u64) -> Result<Vec<pn::ArpRow>, NetError> {
        let b = self.ok(&Request::GetArp, None, deadline)?;
        pn::decode_arp(&b).ok_or(NetError::Garbled)
    }

    pub fn stats(&mut self, deadline: u64) -> Result<Vec<(u16, u64)>, NetError> {
        let b = self.ok(&Request::GetStats, None, deadline)?;
        pn::decode_stats(&b).ok_or(NetError::Garbled)
    }

    fn open_endpoint(
        &mut self,
        kind: u16,
        port: u16,
        addr: [u8; 4],
        oflags: u32,
        deadline: u64,
    ) -> Result<(u64, Vec<u8>), NetError> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(NetError::Sys(c.status));
        }
        let (mine, theirs) = (c.a1 as u64, c.a2 as u64);
        match self.ok(
            &Request::Open {
                kind,
                local_port: port,
                local_addr: addr,
                oflags,
            },
            Some(theirs),
            deadline,
        ) {
            Ok(b) => Ok((mine, b)),
            Err(e) => {
                sys::handle_close(mine);
                Err(e)
            }
        }
    }

    /// Bind a UDP endpoint (`port` 0: an ephemeral one).
    pub fn open_udp(&mut self, port: u16, oflags: u32, deadline: u64) -> Result<Udp, NetError> {
        let (ch, body) = self.open_endpoint(pn::kind::UDP, port, [0; 4], oflags, deadline)?;
        let (port, _) = pn::decode_open_reply(&body).ok_or(NetError::Garbled)?;
        Ok(Udp {
            ch,
            port,
            cookie: 0,
        })
    }

    pub fn open_echo(&mut self, deadline: u64) -> Result<Echo, NetError> {
        let (ch, _) = self.open_endpoint(pn::kind::ICMP_ECHO, 0, [0; 4], 0, deadline)?;
        Ok(Echo { ch, cookie: 0 })
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        sys::handle_close(self.ch);
    }
}

/// Read the next endpoint event, waiting until `deadline`.
fn next_event(ch: u64, deadline: u64) -> Result<Vec<u8>, NetError> {
    loop {
        let wait = sys::object_wait(ch, signals::READABLE | signals::PEER_CLOSED, deadline);
        if wait.status != 0 {
            return Err(NetError::TimedOut);
        }
        let mut buf = alloc::vec![0u8; 2048];
        let rd = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
        if rd.status == Status::ShouldWait.code() {
            if wait.a1 as u32 & signals::PEER_CLOSED != 0 {
                return Err(NetError::Closed);
            }
            continue;
        }
        if rd.status != 0 {
            return Err(NetError::Closed);
        }
        buf.truncate(rd.a1);
        return Ok(buf);
    }
}

fn send_msg(ch: u64, b: &[u8]) -> Result<(), NetError> {
    let w = sys::channel_write(ch, b.as_ptr(), b.len(), core::ptr::null(), 0);
    match w.status {
        0 => Ok(()),
        s if s == Status::PeerClosed.code() => Err(NetError::Closed),
        s => Err(NetError::Sys(s)),
    }
}

/// A UDP endpoint.
pub struct Udp {
    ch: u64,
    pub port: u16,
    cookie: u32,
}

impl Udp {
    /// Send a datagram. Fire and forget: a later failure (no route) arrives as a `Notice` from `recv`.
    pub fn send_to(&mut self, dst: [u8; 4], port: u16, payload: &[u8]) -> Result<(), NetError> {
        self.cookie = self.cookie.wrapping_add(1);
        send_msg(
            self.ch,
            &pn::encode_send_udp(self.cookie, dst, port, payload),
        )
    }

    /// The next datagram; notices are returned as `Err(Stack(code))` is not used: they are skipped here (`recv_event` sees them).
    pub fn recv(&mut self, deadline: u64) -> Result<Datagram, NetError> {
        loop {
            let b = next_event(self.ch, deadline)?;
            match pn::decode_ep_event(&b) {
                Some(EpEvent::Recv {
                    dropped,
                    src,
                    port,
                    payload,
                }) => {
                    return Ok(Datagram {
                        src,
                        port,
                        payload: payload.to_vec(),
                        dropped,
                    })
                }
                Some(EpEvent::Notice { .. }) => continue,
                _ => return Err(NetError::Garbled),
            }
        }
    }

    pub fn channel(&self) -> u64 {
        self.ch
    }
}

impl Drop for Udp {
    fn drop(&mut self) {
        sys::handle_close(self.ch);
    }
}

/// An ICMP echo endpoint.
pub struct Echo {
    ch: u64,
    cookie: u32,
}

impl Echo {
    /// Send one echo request and wait for its reply (a reply for a different sequence is skipped); `TimedOut` if none comes.
    pub fn ping(
        &mut self,
        dst: [u8; 4],
        seq: u16,
        payload: &[u8],
        deadline: u64,
    ) -> Result<EchoReply, NetError> {
        self.cookie = self.cookie.wrapping_add(1);
        send_msg(
            self.ch,
            &pn::encode_send_echo(self.cookie, dst, seq, payload),
        )?;
        loop {
            let b = next_event(self.ch, deadline)?;
            match pn::decode_ep_event(&b) {
                Some(EpEvent::EchoReply {
                    src,
                    seq: s,
                    ttl,
                    payload,
                    ..
                }) if s == seq => {
                    return Ok(EchoReply {
                        src,
                        seq: s,
                        ttl,
                        payload: payload.to_vec(),
                    })
                }
                Some(EpEvent::EchoReply { .. }) => continue,
                Some(EpEvent::Notice {
                    code: pn::notice::SEND_FAILED,
                    detail,
                    ..
                }) => return Err(NetError::Stack(detail as i32)),
                Some(EpEvent::Notice { .. }) => continue,
                _ => return Err(NetError::Garbled),
            }
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        sys::handle_close(self.ch);
    }
}

// ---------------------------------------------------------------------------------------------- streams (S9, 8.6)

impl Net {
    /// Open a TCP connection (S9-SPEC 8.6). `snd_buf` and `rcv_buf` of 0 take the stack's defaults; `deadline` bounds the whole
    /// connect (the stack's own SYN limit is 15 s, so a deadline above that sees the stack's answer, not this timeout).
    pub fn connect_stream(
        &mut self,
        dst: [u8; 4],
        port: u16,
        oflags: u32,
        snd_buf: u32,
        rcv_buf: u32,
        deadline: u64,
    ) -> Result<Stream, NetError> {
        let mut s = self.begin_connect(dst, port, oflags, snd_buf, rcv_buf, deadline)?;
        s.finish_connect(deadline)?;
        Ok(s)
    }

    /// Start a TCP connection without waiting for it: the stream exists, `Connect` has been sent, and `finish_connect` (or
    /// `connect_outcome`, which never waits) says how it ended. `deadline` bounds the request to the stack only.
    pub fn begin_connect(
        &mut self,
        dst: [u8; 4],
        port: u16,
        oflags: u32,
        snd_buf: u32,
        rcv_buf: u32,
        deadline: u64,
    ) -> Result<Stream, NetError> {
        let (ch, _) = self.open_endpoint(pn::kind::TCP_STREAM, 0, [0; 4], oflags, deadline)?;
        let pair = sys::socket_create();
        if pair.status != 0 {
            sys::handle_close(ch);
            return Err(NetError::Sys(pair.status));
        }
        let (mine, theirs) = (pair.a1 as u64, pair.a2 as u64);
        let msg = pn::encode_connect(1, dst, port, snd_buf, rcv_buf);
        let w = sys::channel_write(ch, msg.as_ptr(), msg.len(), [theirs].as_ptr(), 1);
        if w.status != 0 {
            sys::handle_close(theirs);
            sys::handle_close(mine);
            sys::handle_close(ch);
            return Err(NetError::Sys(w.status));
        }
        Ok(Stream::new(ch, mine))
    }

    /// Listen on `port` and post `backlog` accepts (at most 16).
    pub fn listen_stream(
        &mut self,
        port: u16,
        backlog: usize,
        oflags: u32,
        deadline: u64,
    ) -> Result<Listener, NetError> {
        let (ch, body) =
            self.open_endpoint(pn::kind::TCP_LISTENER, port, [0; 4], oflags, deadline)?;
        let (port, _) = pn::decode_open_reply(&body).ok_or(NetError::Garbled)?;
        let mut l = Listener {
            ch,
            port,
            posted: Vec::new(),
            next_cookie: 1,
        };
        for _ in 0..backlog.min(pn::limits::POSTED_ACCEPTS_MAX) {
            l.post(0, 0)?;
        }
        Ok(l)
    }
}

/// A listening endpoint with its posted accepts.
pub struct Listener {
    ch: u64,
    pub port: u16,
    /// `(cookie, the new stream's channel, its socket end)` for each accept the stack holds.
    posted: Vec<(u32, u64, u64, u32, u32)>,
    next_cookie: u32,
}

impl Listener {
    /// Post one more accept: a fresh channel and socket pair, whose client ends stay here until a SYN takes them.
    pub fn post(&mut self, snd_buf: u32, rcv_buf: u32) -> Result<(), NetError> {
        let chan = sys::channel_create();
        if chan.status != 0 {
            return Err(NetError::Sys(chan.status));
        }
        let (cmine, ctheirs) = (chan.a1 as u64, chan.a2 as u64);
        let pair = sys::socket_create();
        if pair.status != 0 {
            sys::handle_close(cmine);
            sys::handle_close(ctheirs);
            return Err(NetError::Sys(pair.status));
        }
        let (smine, stheirs) = (pair.a1 as u64, pair.a2 as u64);
        let cookie = self.next_cookie;
        self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
        let msg = pn::encode_accept(cookie, snd_buf, rcv_buf);
        let hs = [ctheirs, stheirs];
        let w = sys::channel_write(self.ch, msg.as_ptr(), msg.len(), hs.as_ptr(), 2);
        if w.status != 0 {
            for h in [cmine, ctheirs, smine, stheirs] {
                sys::handle_close(h);
            }
            return Err(if w.status == Status::PeerClosed.code() {
                NetError::Closed
            } else {
                NetError::Sys(w.status)
            });
        }
        self.posted.push((cookie, cmine, smine, snd_buf, rcv_buf));
        Ok(())
    }

    /// Wait for the next connection and re-post the accept it used.
    pub fn accept(&mut self, deadline: u64) -> Result<Stream, NetError> {
        loop {
            let b = next_event(self.ch, deadline)?;
            match pn::decode_ep_event(&b) {
                Some(EpEvent::ConnInfo {
                    code: pn::notice::ACCEPTED,
                    cookie,
                    local,
                    remote,
                }) => {
                    let Some(i) = self.posted.iter().position(|p| p.0 == cookie) else {
                        continue;
                    };
                    let (_, ch, sock, snd, rcv) = self.posted.swap_remove(i);
                    let mut s = Stream::new(ch, sock);
                    s.connected = Some((local, remote));
                    // (a replacement with the same buffers, so the backlog keeps its size in bytes)
                    let _ = self.post(snd, rcv);
                    return Ok(s);
                }
                // a refused post (quota, a bad handle): forget that accept and give its handles back
                Some(EpEvent::Notice {
                    code: pn::notice::CONNECT_FAILED,
                    cookie,
                    ..
                }) => {
                    if let Some(i) = self.posted.iter().position(|p| p.0 == cookie) {
                        let (_, c, s, _, _) = self.posted.swap_remove(i);
                        sys::handle_close(c);
                        sys::handle_close(s);
                    }
                    continue;
                }
                Some(EpEvent::Notice { .. }) => continue,
                _ => return Err(NetError::Garbled),
            }
        }
    }

    pub fn channel(&self) -> u64 {
        self.ch
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        for (_, c, s, _, _) in self.posted.drain(..) {
            sys::handle_close(c);
            sys::handle_close(s);
        }
        sys::handle_close(self.ch);
    }
}

/// (local address and port, remote address and port) of a connection.
type Tuple4 = (([u8; 4], u16), ([u8; 4], u16));

/// A TCP connection: its endpoint channel (control and the end-of-file rule) and its socket (the bytes).
pub struct Stream {
    ch: u64,
    sock: u64,
    connected: Option<Tuple4>,
    connect_failed: Option<i32>,
    peer_fin: bool,
    reset: Option<u32>,
    chan_closed: bool,
}

impl Stream {
    fn new(ch: u64, sock: u64) -> Stream {
        Stream {
            ch,
            sock,
            connected: None,
            connect_failed: None,
            peer_fin: false,
            reset: None,
            chan_closed: false,
        }
    }

    /// Wait for and apply one channel message.
    fn next_notice(&mut self, deadline: u64) -> Result<(), NetError> {
        let b = next_event(self.ch, deadline).inspect_err(|e| {
            if *e == NetError::Closed {
                self.chan_closed = true;
            }
        })?;
        self.apply(&b);
        Ok(())
    }

    fn apply(&mut self, b: &[u8]) {
        match pn::decode_ep_event(b) {
            Some(EpEvent::ConnInfo {
                code: pn::notice::CONNECTED,
                local,
                remote,
                ..
            }) => {
                self.connected = Some((local, remote));
            }
            Some(EpEvent::Notice {
                code: pn::notice::CONNECT_FAILED,
                detail,
                ..
            }) => {
                self.connect_failed = Some(detail as i32);
            }
            Some(EpEvent::Notice {
                code: pn::notice::PEER_FIN,
                ..
            }) => self.peer_fin = true,
            Some(EpEvent::Notice {
                code: pn::notice::RESET,
                detail,
                ..
            }) => self.reset = Some(detail),
            _ => {}
        }
    }

    /// Apply whatever is waiting on the channel, without blocking.
    fn drain_channel(&mut self) {
        loop {
            let mut buf = alloc::vec![0u8; 256];
            let rd = sys::channel_read(
                self.ch,
                buf.as_mut_ptr(),
                buf.len(),
                core::ptr::null_mut(),
                0,
            );
            if rd.status == Status::ShouldWait.code() {
                if sys::object_signals(self.ch).a1 as u32 & signals::PEER_CLOSED != 0 {
                    self.chan_closed = true;
                }
                return;
            }
            if rd.status != 0 {
                self.chan_closed = true;
                return;
            }
            buf.truncate(rd.a1);
            self.apply(&buf);
        }
    }

    /// Wait (until `deadline`) for the outcome of `begin_connect`.
    pub fn finish_connect(&mut self, deadline: u64) -> Result<(), NetError> {
        loop {
            if let Some(st) = self.connect_failed {
                return Err(NetError::Stack(st));
            }
            if self.connected.is_some() {
                return Ok(());
            }
            self.next_notice(deadline)?;
        }
    }

    /// The outcome of a connect in progress, without waiting: `None` while it is still going on.
    pub fn connect_outcome(&mut self) -> Option<Result<(), i32>> {
        self.drain_channel();
        if let Some(st) = self.connect_failed {
            return Some(Err(st));
        }
        if self.connected.is_some() {
            return Some(Ok(()));
        }
        if self.chan_closed {
            return Some(Err(pn::status::CONN_RESET));
        }
        None
    }

    /// Has the connection failed (a reset arrived, or the channel closed without a clean end)?
    pub fn failed(&mut self) -> bool {
        self.drain_channel();
        self.reset.is_some() || (self.chan_closed && !self.peer_fin)
    }

    /// Did a reset (not a clean close) end the connection?
    pub fn was_reset(&self) -> bool {
        self.reset.is_some() || (self.chan_closed && !self.peer_fin)
    }

    pub fn local(&self) -> Option<([u8; 4], u16)> {
        self.connected.map(|c| c.0)
    }
    pub fn remote(&self) -> Option<([u8; 4], u16)> {
        self.connected.map(|c| c.1)
    }
    pub fn socket(&self) -> u64 {
        self.sock
    }
    pub fn channel(&self) -> u64 {
        self.ch
    }

    /// Read up to `buf.len()` bytes, waiting until `deadline`. `Ok(0)` is a **clean** end of the stream (the peer's FIN
    /// arrived); a reset, a retransmission give-up or the stack's death is `Err(Stack(CONN_RESET))`, never end-of-file
    /// (S9-SPEC 8.5).
    pub fn read(&mut self, buf: &mut [u8], deadline: u64) -> Result<usize, NetError> {
        loop {
            let r = sys::socket_read(self.sock, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(Status::Ok) if r.a1 > 0 => return Ok(r.a1),
                Some(Status::Ok) => return Ok(0),
                Some(Status::ShouldWait) => {
                    let w = sys::object_wait(
                        self.sock,
                        signals::READABLE | signals::PEER_CLOSED | signals::WRITE_CLOSED,
                        deadline,
                    );
                    if w.status != 0 {
                        return Err(NetError::TimedOut);
                    }
                }
                Some(Status::PeerClosed) => return self.end_of_stream(deadline),
                _ => return Err(NetError::Sys(r.status)),
            }
        }
    }

    /// The socket ended: clean only if `PeerFin` arrived on the channel (the stack sends it before it shuts the socket).
    fn end_of_stream(&mut self, deadline: u64) -> Result<usize, NetError> {
        loop {
            self.drain_channel();
            if self.reset.is_some() {
                return Err(NetError::Stack(pn::status::CONN_RESET));
            }
            if self.peer_fin {
                return Ok(0);
            }
            if self.chan_closed {
                return Err(NetError::Stack(pn::status::CONN_RESET));
            }
            // neither yet: the notice may still be on its way; wait for the channel to say something
            let w = sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, deadline);
            if w.status != 0 {
                return Err(NetError::TimedOut);
            }
        }
    }

    /// Write some of `data`, waiting until `deadline` for room. `Err(Stack(CONN_RESET))` if the stack closed its end.
    pub fn write(&mut self, data: &[u8], deadline: u64) -> Result<usize, NetError> {
        if data.is_empty() {
            return Ok(0);
        }
        loop {
            // (one call takes at most the socket's buffer size)
            let n = data.len().min(abi::limits::SOCKET_BUFFER);
            let w = sys::socket_write(self.sock, data.as_ptr(), n);
            match w.status() {
                Some(Status::Ok) if w.a1 > 0 => return Ok(w.a1),
                Some(Status::Ok) | Some(Status::ShouldWait) => {
                    let o = sys::object_wait(
                        self.sock,
                        signals::WRITABLE | signals::PEER_CLOSED,
                        deadline,
                    );
                    if o.status != 0 {
                        return Err(NetError::TimedOut);
                    }
                }
                // our own job's quota is spent: wait a little for the stack to drain what is queued
                Some(Status::NoMemory) => {
                    if sys::clock_get() >= deadline {
                        return Err(NetError::TimedOut);
                    }
                    crate::io::nap(2);
                }
                Some(Status::PeerClosed) => {
                    self.drain_channel();
                    return Err(NetError::Stack(pn::status::CONN_RESET));
                }
                _ => return Err(NetError::Sys(w.status)),
            }
        }
    }

    /// Write all of `data` (until `deadline`).
    pub fn write_all(&mut self, mut data: &[u8], deadline: u64) -> Result<(), NetError> {
        while !data.is_empty() {
            let n = self.write(data, deadline)?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Half-close: we send no more; the stack sends a FIN after the data.
    pub fn shutdown_write(&mut self) {
        sys::socket_shutdown(self.sock);
    }

    /// Reset the connection now.
    pub fn abort(&mut self) {
        let _ = send_msg(self.ch, &pn::encode_abort());
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        sys::handle_close(self.sock);
        sys::handle_close(self.ch);
    }
}
