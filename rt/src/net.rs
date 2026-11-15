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
    ch: u32,
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
    pub fn new(ch: u32) -> Net {
        Net { ch, txid: 0 }
    }

    pub fn channel(&self) -> u32 {
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
                        sys::object_wait(gate.a1 as u32, 1 << 24, sys::clock_get() + wait);
                        sys::handle_close(gate.a1 as u32);
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
        send: Option<u32>,
        deadline: u64,
    ) -> Result<(i32, Vec<u8>), NetError> {
        self.txid = self.txid.wrapping_add(1);
        let req = pn::encode_request(self.txid, 0, r);
        let hs = [send.unwrap_or(0)];
        let w = sys::channel_write(
            self.ch,
            req.as_ptr(),
            req.len(),
            hs.as_ptr(),
            usize::from(send.is_some()),
        );
        if w.status != 0 {
            if let Some(h) = send {
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

    fn ok(&mut self, r: &Request, send: Option<u32>, deadline: u64) -> Result<Vec<u8>, NetError> {
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

    pub fn arp(&mut self, deadline: u64) -> Result<Vec<([u8; 4], [u8; 6], u32)>, NetError> {
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
    ) -> Result<(u32, Vec<u8>), NetError> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(NetError::Sys(c.status));
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
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
fn next_event(ch: u32, deadline: u64) -> Result<Vec<u8>, NetError> {
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

fn send_msg(ch: u32, b: &[u8]) -> Result<(), NetError> {
    let w = sys::channel_write(ch, b.as_ptr(), b.len(), core::ptr::null(), 0);
    match w.status {
        0 => Ok(()),
        s if s == Status::PeerClosed.code() => Err(NetError::Closed),
        s => Err(NetError::Sys(s)),
    }
}

/// A UDP endpoint.
pub struct Udp {
    ch: u32,
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

    pub fn channel(&self) -> u32 {
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
    ch: u32,
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
