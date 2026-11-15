//! The DHCP client as pure logic (NET-SPEC 8.2): feed it received DHCP payloads and the time, and it says what to send
//! and when to configure or deconfigure the interface. `no_std` plus `alloc`, no `unsafe`, no clock, no randomness of
//! its own beyond a seeded generator (so runs are reproducible), never panics on any input.
//!
//! States (RFC 2131 figure 5): `Init` -> `Selecting` (DISCOVER, wait for OFFER) -> `Requesting` (REQUEST, wait for ACK)
//! -> `Bound` -> `Renewing` (at T1, unicast REQUEST) -> `Rebinding` (at T2, broadcast REQUEST) -> `Init` on expiry or
//! NAK. The client sets the BOOTP broadcast flag (it cannot receive a unicast reply before it has an address) and
//! computes T1 and T2 (half and seven eighths of the lease) when the server sends neither (slirp does not).
//! Retransmission: 4 s, 8 s, 16 s, then 32 s, with jitter of up to a second from the seeded generator, and never more
//! than four packets in any ten seconds while unconfigured.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use netwire::dhcp::{self, msg};
use netwire::{Ip, Mac};

pub type Instant = u64;
pub const SEC: Instant = 1_000_000_000;

/// The most packets sent in any window while unconfigured.
pub const RATE_PACKETS: usize = 4;
pub const RATE_WINDOW: Instant = 10 * SEC;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Init,
    Selecting,
    Requesting,
    Bound,
    Renewing,
    Rebinding,
}

/// What the server granted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lease {
    pub addr: Ip,
    pub mask: Ip,
    pub router: Ip,
    pub dns: Ip,
    pub server: Ip,
    pub lease_s: u32,
    pub t1_s: u32,
    pub t2_s: u32,
}

/// What the client wants done.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Send this DHCP payload to port 67: broadcast (from 0.0.0.0, to 255.255.255.255) when `to` is `None`, else
    /// unicast to that server.
    Send { payload: Vec<u8>, to: Option<Ip> },
    /// Configure the interface with this lease.
    Configure(Lease),
    /// The lease is lost: remove the configuration.
    Deconfigure,
}

pub struct Client {
    mac: Mac,
    state: State,
    xid: u32,
    rng: u64,
    lease: Option<Lease>,
    /// When the current lease was acquired (the ACK's time).
    acquired: Instant,
    /// When the pending offer's REQUEST was first sent / the retransmit count in this state.
    tries: u32,
    next: Option<Instant>,
    offer: Option<(Ip, Ip)>,
    sent: VecDeque<Instant>,
}

impl Client {
    pub fn new(mac: Mac, seed: u64) -> Client {
        Client {
            mac,
            state: State::Init,
            xid: 0,
            rng: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
            lease: None,
            acquired: 0,
            tries: 0,
            next: None,
            offer: None,
            sent: VecDeque::new(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn lease(&self) -> Option<Lease> {
        self.lease
    }
    pub fn xid(&self) -> u32 {
        self.xid
    }
    /// When the client next needs the clock (a retransmission, T1, T2 or expiry).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.next
    }
    /// A test hook: pin the transaction id (a replay of a captured exchange needs the capture's).
    pub fn set_xid(&mut self, xid: u32) {
        self.xid = xid;
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Begin (or restart) the exchange: a new transaction, DISCOVER.
    pub fn start(&mut self, now: Instant) -> Vec<Action> {
        self.state = State::Init;
        self.lease = None;
        self.offer = None;
        self.tries = 0;
        self.xid = self.rand() as u32;
        self.discover(now)
    }

    /// Resume from a stored lease (a restart): the client is `Bound` and renews at the stored times; an expired lease
    /// starts over.
    pub fn resume(&mut self, lease: Lease, acquired: Instant, now: Instant) -> Vec<Action> {
        let expiry = acquired.saturating_add(u64::from(lease.lease_s) * SEC);
        if now >= expiry || lease.lease_s == 0 {
            return self.start(now);
        }
        self.lease = Some(lease);
        self.acquired = acquired;
        self.xid = self.rand() as u32;
        self.state = State::Bound;
        self.next = Some(acquired.saturating_add(u64::from(lease.t1_s) * SEC));
        let mut out = vec![Action::Configure(lease)];
        // already past T1 or T2: the timer below handles it at once
        out.extend(self.on_timer(now));
        out
    }

    fn jittered(&mut self, base: Instant) -> Instant {
        base + self.rand() % SEC
    }

    /// The retransmission delay after `tries` unanswered sends: 4, 8, 16, then 32 s.
    fn backoff(&mut self) -> Instant {
        let base = (4 * SEC) << self.tries.min(3);
        self.jittered(base)
    }

    /// May a packet go out now (at most four in any ten seconds while unconfigured)? If not, when.
    fn rate_ok(&mut self, now: Instant) -> Result<(), Instant> {
        while self
            .sent
            .front()
            .is_some_and(|&t| now.saturating_sub(t) >= RATE_WINDOW)
        {
            self.sent.pop_front();
        }
        if self.sent.len() < RATE_PACKETS {
            Ok(())
        } else {
            Err(self.sent.front().map_or(now, |&t| t + RATE_WINDOW))
        }
    }

    fn send(
        &mut self,
        kind: u8,
        now: Instant,
        to: Option<Ip>,
        ciaddr: Ip,
        requested: Option<Ip>,
        server: Option<Ip>,
    ) -> Vec<Action> {
        let configured = matches!(
            self.state,
            State::Bound | State::Renewing | State::Rebinding
        );
        if !configured {
            if let Err(when) = self.rate_ok(now) {
                self.next = Some(when);
                return Vec::new();
            }
        }
        self.sent.push_back(now);
        let secs = if self.tries == 0 {
            0
        } else {
            (self.tries * 4).min(60) as u16
        };
        let mut b = vec![0u8; 400];
        let flags = if to.is_none() {
            dhcp::FLAG_BROADCAST
        } else {
            0
        };
        let Ok(n) = dhcp::write_client(
            &mut b, kind, self.xid, self.mac, flags, secs, ciaddr, requested, server,
        ) else {
            return Vec::new();
        };
        b.truncate(n);
        vec![Action::Send { payload: b, to }]
    }

    fn discover(&mut self, now: Instant) -> Vec<Action> {
        self.state = State::Selecting;
        let out = self.send(msg::DISCOVER, now, None, [0; 4], None, None);
        if !out.is_empty() {
            let d = self.backoff();
            self.next = Some(now + d);
            self.tries = self.tries.saturating_add(1);
        }
        out
    }

    fn request_offer(&mut self, now: Instant) -> Vec<Action> {
        self.state = State::Requesting;
        let Some((addr, server)) = self.offer else {
            return self.start(now);
        };
        let out = self.send(msg::REQUEST, now, None, [0; 4], Some(addr), Some(server));
        if !out.is_empty() {
            let d = self.backoff();
            self.next = Some(now + d);
            self.tries = self.tries.saturating_add(1);
        }
        out
    }

    fn lease_end(&self) -> Option<Instant> {
        self.lease
            .map(|l| self.acquired.saturating_add(u64::from(l.lease_s) * SEC))
    }

    /// A DHCP payload arrived (the UDP payload to port 68). Anything that does not belong to this client's current
    /// transaction is ignored.
    pub fn on_packet(&mut self, payload: &[u8], now: Instant) -> Vec<Action> {
        let Ok(m) = dhcp::parse(payload) else {
            return Vec::new();
        };
        if m.op != dhcp::OP_REPLY || m.xid != self.xid || m.chaddr != self.mac {
            return Vec::new();
        }
        let s = m.summary();
        match (self.state, s.msg_type) {
            (State::Selecting, Some(msg::OFFER)) => {
                let (Some(server), true) = (s.server_id, m.yiaddr != [0; 4]) else {
                    return Vec::new();
                };
                self.offer = Some((m.yiaddr, server));
                self.tries = 0;
                self.request_offer(now)
            }
            (State::Requesting | State::Renewing | State::Rebinding, Some(msg::ACK)) => {
                let Some(lease) = self.lease_from(&m, &s) else {
                    return Vec::new();
                };
                let was_bound = self.state != State::Requesting;
                self.lease = Some(lease);
                self.acquired = now;
                self.state = State::Bound;
                self.tries = 0;
                self.next = Some(now + u64::from(lease.t1_s) * SEC);
                let _ = was_bound;
                vec![Action::Configure(lease)]
            }
            (State::Requesting | State::Renewing | State::Rebinding, Some(msg::NAK)) => {
                let mut out = Vec::new();
                if self.state != State::Requesting {
                    out.push(Action::Deconfigure);
                }
                self.lease = None;
                self.offer = None;
                self.tries = 0;
                self.xid = self.rand() as u32;
                self.state = State::Init;
                // restart after a pause so a NAK storm cannot spin
                self.next = Some(now + SEC);
                out
            }
            _ => Vec::new(),
        }
    }

    fn lease_from(&self, m: &dhcp::Dhcp<'_>, s: &dhcp::Summary) -> Option<Lease> {
        let server = s
            .server_id
            .or(self.offer.map(|o| o.1))
            .or(self.lease.map(|l| l.server))?;
        let lease_s = s.lease?;
        if lease_s == 0 {
            return None;
        }
        let (mut t1, mut t2) = (
            s.t1.unwrap_or(lease_s / 2),
            s.t2.unwrap_or((u64::from(lease_s) * 7 / 8) as u32),
        );
        // a nonsensical pair is replaced by the defaults
        if !(t1 < t2 && t2 < lease_s) {
            t1 = lease_s / 2;
            t2 = (u64::from(lease_s) * 7 / 8) as u32;
        }
        let addr = if m.yiaddr != [0; 4] {
            m.yiaddr
        } else {
            self.lease?.addr
        };
        Some(Lease {
            addr,
            mask: s.mask.unwrap_or([255, 255, 255, 0]),
            router: s.router.unwrap_or([0; 4]),
            dns: s.dns.unwrap_or([0; 4]),
            server,
            lease_s,
            t1_s: t1,
            t2_s: t2,
        })
    }

    /// The clock reached `next_deadline()` (or later).
    pub fn on_timer(&mut self, now: Instant) -> Vec<Action> {
        let Some(due) = self.next else {
            return Vec::new();
        };
        if now < due {
            return Vec::new();
        }
        match self.state {
            State::Init => self.start(now),
            State::Selecting => self.discover(now),
            State::Requesting => self.request_offer(now),
            State::Bound | State::Renewing | State::Rebinding => self.lease_timer(now),
        }
    }

    fn lease_timer(&mut self, now: Instant) -> Vec<Action> {
        let (Some(lease), Some(end)) = (self.lease, self.lease_end()) else {
            return self.start(now);
        };
        let t1 = self.acquired.saturating_add(u64::from(lease.t1_s) * SEC);
        let t2 = self.acquired.saturating_add(u64::from(lease.t2_s) * SEC);
        if now >= end {
            self.lease = None;
            self.state = State::Init;
            let mut out = vec![Action::Deconfigure];
            out.extend(self.start(now));
            return out;
        }
        if now >= t2 {
            self.state = State::Rebinding;
            let out = self.send(msg::REQUEST, now, None, lease.addr, None, None);
            self.next = Some(self.retry_at(now, end));
            return out;
        }
        if now >= t1 {
            self.state = State::Renewing;
            let out = self.send(
                msg::REQUEST,
                now,
                Some(lease.server),
                lease.addr,
                None,
                None,
            );
            self.next = Some(self.retry_at(now, t2));
            return out;
        }
        self.next = Some(t1);
        Vec::new()
    }

    /// Retry at half the remaining time to `limit` (at least a minute), but never past it.
    fn retry_at(&self, now: Instant, limit: Instant) -> Instant {
        let half = limit.saturating_sub(now) / 2;
        (now + half.max(60 * SEC)).min(limit).max(now + 1)
    }
}
