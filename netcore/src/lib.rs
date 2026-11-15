//! The network stack's pure core (NET-SPEC section 6): one interface with an address, an ARP cache and its pending
//! queues, UDP and ICMP-echo endpoints, token-bucket rate limits, timers and counters. `no_std` (plus `alloc` for the
//! bounded tables), no `unsafe`, **no clock** (time is an opaque monotonic nanosecond [`Instant`] the caller passes in),
//! no blocking, and every packet is validated before it is used: nothing here allocates in proportion to a value a
//! packet claims (a frame is copied only up to the bytes actually present, at most one MTU).
//!
//! The adapter (`netd`) feeds frames to [`Core::ingress`], applies the returned [`Event`]s, calls [`Core::egress`] to
//! push built frames into free transmit slots through the [`Device`] trait, and sleeps until [`Core::next_deadline`].
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeSet, VecDeque};
use alloc::vec;
use alloc::vec::Vec;
use netwire::{arp, dhcp, eth, icmp, ipv4, udp, Ip, Mac, WireError, BROADCAST_IP, BROADCAST_MAC};

pub mod counters;
pub mod tcp;
pub use counters::Counters;
pub use tcp::*;

/// A monotonic time in nanoseconds (opaque to the core).
pub type Instant = u64;

pub const MS: Instant = 1_000_000;
pub const SEC: Instant = 1_000_000_000;

pub const MTU: usize = 1500;
/// UDP payload maximum: MTU less the IP and UDP headers.
pub const UDP_MAX: usize = MTU - 20 - 8;
/// ICMP echo data maximum.
pub const ECHO_MAX: usize = MTU - 20 - 8;

pub const ARP_CACHE_MAX: usize = 64;
pub const ARP_ENTRY_LIFE: Instant = 60 * SEC;
pub const ARP_PENDING_PER_ENTRY: usize = 4;
pub const ARP_PENDING_TOTAL: usize = 16;
pub const ARP_RETRY: [Instant; 3] = [SEC, SEC, SEC];
pub const ECHO_REPLIES_PER_SEC: u32 = 100;
pub const UNREACHABLES_PER_SEC: u32 = 10;
/// The first and last ephemeral ports.
pub const EPHEMERAL: (u16, u16) = (49152, 65535);
/// Frames waiting for transmit slots (beyond this, new ones are dropped and counted).
pub const TX_QUEUE_MAX: usize = 64;

/// Status codes shared with `proto::net` (NET-SPEC 7.1).
pub mod status {
    pub const OK: i32 = 0;
    pub const REFUSED: i32 = 2;
    pub const ADDR_IN_USE: i32 = 3;
    pub const TOO_BIG: i32 = 4;
    pub const NO_ROUTE: i32 = 5;
    pub const QUOTA: i32 = 6;
    pub const NOT_CONFIGURED: i32 = 7;
    pub const NO_BUFFERS: i32 = 8;
}

/// `Notice` codes (NET-SPEC 7.2).
pub mod notice {
    pub const SEND_FAILED: u16 = 1;
    pub const LINK_DOWN: u16 = 2;
    pub const LINK_UP: u16 = 3;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub addr: Ip,
    pub mask: Ip,
    pub gateway: Ip,
    pub dns: Ip,
}

impl Config {
    fn on_link(&self, ip: Ip) -> bool {
        (0..4).all(|i| ip[i] & self.mask[i] == self.addr[i] & self.mask[i])
    }
    fn subnet_broadcast(&self) -> Ip {
        let mut b = [0u8; 4];
        for (i, o) in b.iter_mut().enumerate() {
            *o = self.addr[i] | !self.mask[i];
        }
        b
    }
}

/// Where to put frames: the transmit side of the driver rings.
pub trait Device {
    /// Hand a complete Ethernet frame (at most 1514 bytes) to the device. False: no free slot, try again later.
    fn tx(&mut self, frame: &[u8]) -> bool;
}

/// An endpoint handle: stable until closed, never reused while live.
pub type EpId = u32;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    Deliver {
        ep: EpId,
        src: Ip,
        src_port: u16,
        payload: Vec<u8>,
    },
    EchoReply {
        ep: EpId,
        src: Ip,
        seq: u16,
        ttl: u8,
        payload: Vec<u8>,
    },
    /// `code` is a [`notice`] constant; `cookie` names the send; `detail` is a [`status`] for `SEND_FAILED`.
    Notice {
        ep: EpId,
        code: u16,
        cookie: u32,
        detail: u32,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SendError {
    Refused,
    TooBig,
    NoRoute,
    NotConfigured,
    NoBuffers,
    NoSuchEndpoint,
}

impl SendError {
    pub fn status(self) -> i32 {
        match self {
            SendError::Refused | SendError::NoSuchEndpoint => status::REFUSED,
            SendError::TooBig => status::TOO_BIG,
            SendError::NoRoute => status::NO_ROUTE,
            SendError::NotConfigured => status::NOT_CONFIGURED,
            SendError::NoBuffers => status::NO_BUFFERS,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindError {
    AddrInUse,
    NoPorts,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Udp { port: u16, addr: Option<Ip> },
    Echo { ident: u16 },
}

struct Endpoint {
    id: EpId,
    kind: Kind,
    /// May send the limited broadcast and (while unconfigured) from 0.0.0.0.
    privileged: bool,
}

struct ArpEntry {
    ip: Ip,
    mac: Mac,
    expires: Instant,
}

struct Pending {
    packet: Vec<u8>,
    cookie: Option<(EpId, u32)>,
}

struct Resolving {
    ip: Ip,
    requests: u8,
    next: Instant,
    queue: VecDeque<Pending>,
}

struct Bucket {
    capacity: u32,
    tokens: u32,
    /// The time of the last full second boundary for the refill.
    last: Instant,
}

impl Bucket {
    fn new(per_sec: u32) -> Bucket {
        Bucket {
            capacity: per_sec,
            tokens: per_sec,
            last: 0,
        }
    }
    fn take(&mut self, now: Instant) -> bool {
        if now.saturating_sub(self.last) >= SEC {
            self.last = now;
            self.tokens = self.capacity;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// One interface and everything that hangs on it.
pub struct Core {
    mac: Mac,
    cfg: Option<Config>,
    link: bool,
    arp: Vec<ArpEntry>,
    resolving: Vec<Resolving>,
    endpoints: Vec<Endpoint>,
    next_ep: EpId,
    next_port: u16,
    next_ident: u16,
    idents: BTreeSet<u16>,
    ip_id: u16,
    outq: VecDeque<Vec<u8>>,
    echo_bucket: Bucket,
    unreach_bucket: Bucket,
    counters: Counters,
    tcp: tcp::Tcp,
}

impl Core {
    pub fn new(mac: Mac) -> Core {
        Core {
            mac,
            cfg: None,
            link: false,
            arp: Vec::new(),
            resolving: Vec::new(),
            endpoints: Vec::new(),
            next_ep: 1,
            next_port: EPHEMERAL.0,
            next_ident: 1,
            idents: BTreeSet::new(),
            ip_id: 1,
            outq: VecDeque::new(),
            echo_bucket: Bucket::new(ECHO_REPLIES_PER_SEC),
            unreach_bucket: Bucket::new(UNREACHABLES_PER_SEC),
            counters: Counters::default(),
            tcp: tcp::Tcp::new(),
        }
    }

    pub fn mac(&self) -> Mac {
        self.mac
    }
    /// The interface's hardware address (learned from the driver at attach).
    pub fn set_mac(&mut self, mac: Mac) {
        self.mac = mac;
    }
    pub fn config(&self) -> Option<Config> {
        self.cfg
    }
    pub fn link_is_up(&self) -> bool {
        self.link
    }
    pub fn counters(&self) -> &Counters {
        &self.counters
    }
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }
    pub fn arp_len(&self) -> usize {
        self.arp.len()
    }
    pub fn pending_len(&self) -> usize {
        self.resolving.iter().map(|r| r.queue.len()).sum()
    }
    pub fn tx_queue_len(&self) -> usize {
        self.outq.len()
    }
    /// The ARP table as `(ip, mac, age_ms)` for `GetArp`.
    pub fn arp_table(&self, now: Instant) -> Vec<(Ip, Mac, u32)> {
        self.arp
            .iter()
            .map(|e| {
                let age = ARP_ENTRY_LIFE.saturating_sub(e.expires.saturating_sub(now)) / MS;
                (e.ip, e.mac, age.min(u64::from(u32::MAX)) as u32)
            })
            .collect()
    }

    // ---- configuration and link ----

    /// Set the address. A gratuitous ARP announces it once.
    pub fn configure(&mut self, cfg: Config, now: Instant) {
        let changed = self.cfg != Some(cfg);
        self.cfg = Some(cfg);
        if changed && self.link {
            let g = arp::Arp {
                op: arp::OP_REQUEST,
                sha: self.mac,
                spa: cfg.addr,
                tha: [0; 6],
                tpa: cfg.addr,
            };
            self.send_arp(BROADCAST_MAC, &g);
        }
        let _ = now;
    }

    pub fn clear_config(&mut self) {
        self.cfg = None;
        self.arp.clear();
        self.resolving.clear();
    }

    /// The link went up or down. Down: everything queued is dropped. Returns a `LinkDown`/`LinkUp` notice for every
    /// endpoint.
    pub fn link(&mut self, up: bool) -> Vec<Event> {
        if self.link == up {
            return Vec::new();
        }
        self.link = up;
        if !up {
            self.outq.clear();
            self.resolving.clear();
        }
        let code = if up {
            notice::LINK_UP
        } else {
            notice::LINK_DOWN
        };
        self.endpoints
            .iter()
            .map(|e| Event::Notice {
                ep: e.id,
                code,
                cookie: 0,
                detail: 0,
            })
            .collect()
    }

    // ---- endpoints ----

    fn port_in_use(&self, port: u16) -> bool {
        self.endpoints
            .iter()
            .any(|e| matches!(e.kind, Kind::Udp { port: p, .. } if p == port))
    }

    /// Bind a UDP endpoint. Port 0 allocates an ephemeral port (scanning from a rotating start so a freed port is not
    /// reused at once). Privilege is the caller's policy (`netd` decides who may bind below 1024); the flag here is for
    /// broadcast and unconfigured-source sends.
    pub fn open_udp(
        &mut self,
        port: u16,
        addr: Option<Ip>,
        privileged: bool,
    ) -> Result<(EpId, u16), BindError> {
        let port = if port == 0 {
            let span = u32::from(EPHEMERAL.1 - EPHEMERAL.0) + 1;
            let mut found = None;
            for i in 0..span {
                let p = EPHEMERAL.0 + ((u32::from(self.next_port - EPHEMERAL.0) + i) % span) as u16;
                if !self.port_in_use(p) {
                    found = Some(p);
                    break;
                }
            }
            let p = found.ok_or(BindError::NoPorts)?;
            self.next_port = if p == EPHEMERAL.1 { EPHEMERAL.0 } else { p + 1 };
            p
        } else if self.port_in_use(port) {
            return Err(BindError::AddrInUse);
        } else {
            port
        };
        let id = self.fresh_id();
        self.endpoints.push(Endpoint {
            id,
            kind: Kind::Udp { port, addr },
            privileged,
        });
        Ok((id, port))
    }

    /// An ICMP echo endpoint. Returns its id and the ICMP identifier the core chose (unique among live endpoints).
    pub fn open_echo(&mut self, privileged: bool) -> Result<(EpId, u16), BindError> {
        let mut ident = None;
        for _ in 0..=u16::MAX {
            let c = self.next_ident;
            self.next_ident = self.next_ident.wrapping_add(1).max(1);
            if self.idents.insert(c) {
                ident = Some(c);
                break;
            }
        }
        let ident = ident.ok_or(BindError::NoPorts)?;
        let id = self.fresh_id();
        self.endpoints.push(Endpoint {
            id,
            kind: Kind::Echo { ident },
            privileged,
        });
        Ok((id, ident))
    }

    fn fresh_id(&mut self) -> EpId {
        loop {
            let id = self.next_ep;
            self.next_ep = self.next_ep.wrapping_add(1).max(1);
            if !self.endpoints.iter().any(|e| e.id == id) {
                return id;
            }
        }
    }

    /// Close an endpoint: the port or identifier is free at once and any send still waiting for ARP is forgotten.
    pub fn close(&mut self, ep: EpId) {
        if let Some(Kind::Echo { ident }) = self.endpoint(ep).map(|e| e.kind) {
            self.idents.remove(&ident);
        }
        self.endpoints.retain(|e| e.id != ep);
        for r in &mut self.resolving {
            r.queue.retain(|p| p.cookie.is_none_or(|(e, _)| e != ep));
        }
        self.resolving.retain(|r| !r.queue.is_empty());
    }

    fn endpoint(&self, ep: EpId) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.id == ep)
    }

    // ---- sending ----

    /// Send a UDP datagram from `ep`. Fire and forget: later failures (no route after ARP gives up) come back as a
    /// `Notice` naming `cookie`.
    pub fn send_udp(
        &mut self,
        ep: EpId,
        dst: Ip,
        dst_port: u16,
        payload: &[u8],
        cookie: u32,
        now: Instant,
    ) -> Result<(), SendError> {
        let (port, privileged) = match self.endpoint(ep).map(|e| (e.kind, e.privileged)) {
            Some((Kind::Udp { port, .. }, p)) => (port, p),
            _ => return Err(SendError::NoSuchEndpoint),
        };
        if payload.len() > UDP_MAX {
            return Err(SendError::TooBig);
        }
        let broadcast =
            dst == BROADCAST_IP || self.cfg.is_some_and(|c| dst == c.subnet_broadcast());
        if broadcast && !privileged {
            return Err(SendError::Refused);
        }
        let src = match self.cfg {
            Some(c) => c.addr,
            None if privileged && dst == BROADCAST_IP => [0; 4],
            None => return Err(SendError::NotConfigured),
        };
        let mut d = vec![0u8; udp::HEADER + payload.len()];
        let n =
            udp::write(&mut d, src, dst, port, dst_port, payload).map_err(|_| SendError::TooBig)?;
        d.truncate(n);
        self.send_ip(ep, cookie, src, dst, ipv4::PROTO_UDP, d, broadcast, now)
    }

    /// Send an ICMP echo request from an echo endpoint (the identifier is the endpoint's).
    pub fn send_echo(
        &mut self,
        ep: EpId,
        dst: Ip,
        seq: u16,
        payload: &[u8],
        cookie: u32,
        now: Instant,
    ) -> Result<(), SendError> {
        let ident = match self.endpoint(ep).map(|e| e.kind) {
            Some(Kind::Echo { ident }) => ident,
            _ => return Err(SendError::NoSuchEndpoint),
        };
        if payload.len() > ECHO_MAX {
            return Err(SendError::TooBig);
        }
        let src = self.cfg.ok_or(SendError::NotConfigured)?.addr;
        let mut m = vec![0u8; icmp::HEADER + payload.len()];
        let n =
            icmp::write_echo(&mut m, false, ident, seq, payload).map_err(|_| SendError::TooBig)?;
        m.truncate(n);
        self.send_ip(ep, cookie, src, dst, ipv4::PROTO_ICMP, m, false, now)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_ip(
        &mut self,
        ep: EpId,
        cookie: u32,
        src: Ip,
        dst: Ip,
        proto: u8,
        payload: Vec<u8>,
        broadcast: bool,
        now: Instant,
    ) -> Result<(), SendError> {
        let packet = self.ip_packet(src, dst, proto, 64, &payload);
        if broadcast {
            self.queue_frame(BROADCAST_MAC, eth::ETHERTYPE_IPV4, &packet);
            return Ok(());
        }
        let cfg = self.cfg.ok_or(SendError::NotConfigured)?;
        let next_hop = if cfg.on_link(dst) {
            dst
        } else if cfg.gateway != [0; 4] {
            cfg.gateway
        } else {
            return Err(SendError::NoRoute);
        };
        self.route(next_hop, packet, Some((ep, cookie)), now);
        Ok(())
    }

    fn ip_packet(&mut self, src: Ip, dst: Ip, proto: u8, ttl: u8, payload: &[u8]) -> Vec<u8> {
        let id = self.ip_id;
        self.ip_id = self.ip_id.wrapping_add(1);
        let h = ipv4::Header {
            tos: 0,
            id,
            flags_frag: ipv4::FLAG_DF,
            ttl,
            proto,
            src,
            dst,
        };
        let mut p = vec![0u8; ipv4::HEADER + payload.len()];
        if ipv4::write_header(&mut p, &h, payload.len()).is_ok() {
            p[ipv4::HEADER..].copy_from_slice(payload);
        }
        p
    }

    /// Deliver an IP packet to the next hop: use the cache, or queue it and start resolving.
    fn route(&mut self, next_hop: Ip, packet: Vec<u8>, cookie: Option<(EpId, u32)>, now: Instant) {
        if let Some(mac) = self.lookup(next_hop, now) {
            self.queue_frame(mac, eth::ETHERTYPE_IPV4, &packet);
            return;
        }
        let pos = match self.resolving.iter().position(|r| r.ip == next_hop) {
            Some(p) => p,
            None => {
                self.resolving.push(Resolving {
                    ip: next_hop,
                    requests: 0,
                    next: now,
                    queue: VecDeque::new(),
                });
                self.resolving.len() - 1
            }
        };
        // bounded queues: 4 per entry, 16 in all; the oldest is dropped
        if self.resolving[pos].queue.len() >= ARP_PENDING_PER_ENTRY {
            self.resolving[pos].queue.pop_front();
            self.counters.arp_queue_dropped += 1;
        }
        let total: usize = self.resolving.iter().map(|r| r.queue.len()).sum();
        if total >= ARP_PENDING_TOTAL {
            if let Some(r) = self.resolving.iter_mut().find(|r| !r.queue.is_empty()) {
                r.queue.pop_front();
                self.counters.arp_queue_dropped += 1;
            }
        }
        let pos = match self.resolving.iter().position(|r| r.ip == next_hop) {
            Some(p) => p,
            None => {
                self.resolving.push(Resolving {
                    ip: next_hop,
                    requests: 0,
                    next: now,
                    queue: VecDeque::new(),
                });
                self.resolving.len() - 1
            }
        };
        self.resolving[pos]
            .queue
            .push_back(Pending { packet, cookie });
        if self.resolving[pos].requests == 0 {
            self.arp_request(pos, now);
        }
    }

    fn arp_request(&mut self, pos: usize, now: Instant) {
        let Some(cfg) = self.cfg else { return };
        let ip = self.resolving[pos].ip;
        let r = &mut self.resolving[pos];
        let idx = usize::from(r.requests).min(ARP_RETRY.len() - 1);
        r.requests = r.requests.saturating_add(1);
        r.next = now + ARP_RETRY[idx];
        let req = arp::Arp {
            op: arp::OP_REQUEST,
            sha: self.mac,
            spa: cfg.addr,
            tha: [0; 6],
            tpa: ip,
        };
        self.counters.arp_requests_sent += 1;
        self.send_arp(BROADCAST_MAC, &req);
    }

    fn send_arp(&mut self, dst: Mac, a: &arp::Arp) {
        let mut p = [0u8; arp::LEN];
        if arp::write(&mut p, a).is_ok() {
            self.queue_frame(dst, eth::ETHERTYPE_ARP, &p);
        }
    }

    fn queue_frame(&mut self, dst: Mac, ethertype: u16, payload: &[u8]) {
        if !self.link {
            self.counters.tx_link_down += 1;
            return;
        }
        if self.outq.len() >= TX_QUEUE_MAX {
            self.counters.tx_queue_full += 1;
            return;
        }
        let len = (eth::HEADER + payload.len()).max(60);
        if len > eth::FRAME_MAX {
            self.counters.tx_too_big += 1;
            return;
        }
        let mut f = vec![0u8; len];
        if eth::write_header(&mut f, dst, self.mac, ethertype).is_ok() {
            f[eth::HEADER..eth::HEADER + payload.len()].copy_from_slice(payload);
            self.outq.push_back(f);
        }
    }

    // ---- the ARP cache ----

    fn lookup(&mut self, ip: Ip, now: Instant) -> Option<Mac> {
        self.arp.retain(|e| e.expires > now);
        self.arp.iter().find(|e| e.ip == ip).map(|e| e.mac)
    }

    fn learn(&mut self, ip: Ip, mac: Mac, now: Instant) -> Vec<Event> {
        let Some(cfg) = self.cfg else {
            return Vec::new();
        };
        // only addresses on our subnet, never a broadcast or multicast sender
        if !cfg.on_link(ip)
            || ip == [0; 4]
            || ip == BROADCAST_IP
            || ip == cfg.subnet_broadcast()
            || mac[0] & 1 == 1
        {
            return Vec::new();
        }
        self.arp.retain(|e| e.expires > now);
        if let Some(e) = self.arp.iter_mut().find(|e| e.ip == ip) {
            e.mac = mac;
            e.expires = now + ARP_ENTRY_LIFE;
        } else {
            if self.arp.len() >= ARP_CACHE_MAX {
                if let Some(i) = (0..self.arp.len()).min_by_key(|&i| self.arp[i].expires) {
                    self.arp.swap_remove(i);
                    self.counters.arp_evictions += 1;
                }
            }
            self.arp.push(ArpEntry {
                ip,
                mac,
                expires: now + ARP_ENTRY_LIFE,
            });
        }
        // anything waiting for this address goes out now
        if let Some(pos) = self.resolving.iter().position(|r| r.ip == ip) {
            let r = self.resolving.swap_remove(pos);
            for p in r.queue {
                self.queue_frame(mac, eth::ETHERTYPE_IPV4, &p.packet);
            }
        }
        Vec::new()
    }

    // ---- timers and egress ----

    /// The earliest time something needs the core again (an ARP retry), if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.resolving.iter().map(|r| r.next).min(),
            self.tcp_next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Run the timers due at `now` (ARP retries; after the third unanswered request the queue is dropped and each
    /// cookie-tagged send gets a `NoRoute` notice).
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.tcp_poll(now);
        let mut events = Vec::new();
        let mut i = 0;
        while i < self.resolving.len() {
            if self.resolving[i].next > now {
                i += 1;
                continue;
            }
            if usize::from(self.resolving[i].requests) >= ARP_RETRY.len() {
                let r = self.resolving.swap_remove(i);
                self.counters.arp_failed += 1;
                for p in r.queue {
                    if let Some((ep, cookie)) = p.cookie {
                        events.push(Event::Notice {
                            ep,
                            code: notice::SEND_FAILED,
                            cookie,
                            detail: status::NO_ROUTE as u32,
                        });
                    }
                }
            } else {
                self.arp_request(i, now);
                i += 1;
            }
        }
        events
    }

    /// Push at most `max` built frames into the device, running the timers first. Returns how many went out.
    pub fn egress(
        &mut self,
        dev: &mut impl Device,
        now: Instant,
        max: usize,
    ) -> (usize, Vec<Event>) {
        let events = self.poll(now);
        let mut sent = 0;
        while sent < max {
            let Some(f) = self.outq.front() else { break };
            if !dev.tx(f) {
                break;
            }
            self.counters.tx_frames += 1;
            self.outq.pop_front();
            sent += 1;
        }
        (sent, events)
    }

    // ---- receive ----

    /// Process one received frame. The first failing check drops it and counts exactly one counter (6.3).
    pub fn ingress(&mut self, frame: &[u8], now: Instant) -> Vec<Event> {
        self.counters.rx_frames += 1;
        if !self.link || frame.len() < eth::HEADER {
            self.counters.rx_runt += 1;
            return Vec::new();
        }
        let Ok(e) = eth::parse(frame) else {
            self.counters.rx_runt += 1;
            return Vec::new();
        };
        if e.dst != self.mac && e.dst != BROADCAST_MAC {
            self.counters.rx_not_for_us += 1;
            return Vec::new();
        }
        match e.ethertype {
            eth::ETHERTYPE_ARP => self.rx_arp(e.payload, now),
            eth::ETHERTYPE_IPV4 => self.rx_ipv4(e.src, e.dst == BROADCAST_MAC, e.payload, now),
            _ => {
                self.counters.rx_unknown_ethertype += 1;
                Vec::new()
            }
        }
    }

    fn rx_arp(&mut self, p: &[u8], now: Instant) -> Vec<Event> {
        let Ok(a) = arp::parse(p) else {
            self.counters.rx_arp_malformed += 1;
            return Vec::new();
        };
        let Some(cfg) = self.cfg else {
            self.counters.rx_arp_unconfigured += 1;
            return Vec::new();
        };
        // learn from anything addressed to us, requests and replies alike
        if a.tpa == cfg.addr {
            self.learn(a.spa, a.sha, now);
        }
        if a.op == arp::OP_REQUEST && a.tpa == cfg.addr && a.spa != cfg.addr {
            let reply = arp::Arp {
                op: arp::OP_REPLY,
                sha: self.mac,
                spa: cfg.addr,
                tha: a.sha,
                tpa: a.spa,
            };
            self.counters.arp_replies_sent += 1;
            self.send_arp(a.sha, &reply);
        }
        Vec::new()
    }

    fn rx_ipv4(&mut self, src_mac: Mac, l2_broadcast: bool, p: &[u8], now: Instant) -> Vec<Event> {
        let ip = match ipv4::parse(p) {
            Ok(ip) => ip,
            Err(WireError::BadChecksum) => {
                self.counters.rx_ip_checksum += 1;
                return Vec::new();
            }
            Err(WireError::Fragment) => {
                self.counters.rx_fragment += 1;
                return Vec::new();
            }
            Err(_) => {
                self.counters.rx_ip_malformed += 1;
                return Vec::new();
            }
        };
        let h = ip.header;
        if h.ttl == 0 {
            self.counters.rx_ttl += 1;
            return Vec::new();
        }
        let accepted = match self.cfg {
            Some(c) => h.dst == c.addr || h.dst == BROADCAST_IP || h.dst == c.subnet_broadcast(),
            // no address yet: only DHCP traffic (UDP to port 68 sent to a broadcast address)
            None => {
                h.proto == ipv4::PROTO_UDP
                    && (h.dst == BROADCAST_IP || l2_broadcast)
                    && ip.payload.get(2..4) == Some(&dhcp::PORT_CLIENT.to_be_bytes()[..])
            }
        };
        if !accepted {
            self.counters.rx_ip_not_for_us += 1;
            return Vec::new();
        }
        match h.proto {
            ipv4::PROTO_ICMP => self.rx_icmp(src_mac, &h, ip.payload, now),
            ipv4::PROTO_UDP => self.rx_udp(
                src_mac,
                &h,
                &p[..ip.header_len + ip.payload.len()],
                ip.header_len,
                now,
            ),
            // the dispatch point for TCP (S9). Until a TCP layer exists a segment is, honestly, an unknown protocol.
            ipv4::PROTO_TCP => {
                self.rx_tcp_segment(&h, ip.payload, now);
                Vec::new()
            }
            _ => {
                self.counters.rx_unknown_protocol += 1;
                Vec::new()
            }
        }
    }

    /// A destination-unreachable quotes the start of a packet *we sent*: the IP header and the first 8 bytes of its
    /// payload. Pass it to the transport named by the quoted protocol, which is the only layer that knows what the
    /// quoted ports mean. In S8 the UDP and TCP hooks only count; S9's TCP uses its hook for hard errors (RFC 5927).
    fn rx_icmp_error(&mut self, code: u8, quoted: &[u8]) -> Vec<Event> {
        let Some(cfg) = self.cfg else {
            return Vec::new();
        };
        let ihl = usize::from(quoted.first().map_or(0, |b| b & 0x0f)) * 4;
        // the quoted packet must look like one of ours: IPv4, a whole header, our source address, and room for the ports
        let ours = quoted.first().is_some_and(|b| b >> 4 == 4)
            && ihl >= ipv4::HEADER
            && quoted.len() >= ihl + 4
            && quoted[12..16] == cfg.addr;
        if !ours {
            self.counters.icmp_error_malformed += 1;
            return Vec::new();
        }
        let ports = &quoted[ihl..ihl + 4];
        let (src_port, dst_port) = (
            u16::from_be_bytes([ports[0], ports[1]]),
            u16::from_be_bytes([ports[2], ports[3]]),
        );
        let dst: Ip = [quoted[16], quoted[17], quoted[18], quoted[19]];
        match quoted[9] {
            ipv4::PROTO_UDP => self.udp_error(src_port, dst, dst_port, code),
            ipv4::PROTO_TCP => self.tcp_error(src_port, dst, dst_port, code),
            _ => {
                self.counters.icmp_error_other_protocol += 1;
                Vec::new()
            }
        }
    }

    /// The UDP layer's hook: our datagram from `src_port` to `dst:dst_port` drew an unreachable of `code`.
    fn udp_error(&mut self, _src_port: u16, _dst: Ip, _dst_port: u16, _code: u8) -> Vec<Event> {
        self.counters.icmp_error_udp += 1;
        Vec::new()
    }

    /// The TCP layer's hook (S9 fills it in).
    fn tcp_error(&mut self, src_port: u16, dst: Ip, dst_port: u16, _code: u8) -> Vec<Event> {
        self.counters.icmp_error_tcp += 1;
        self.tcp_soft_error(src_port, dst, dst_port);
        Vec::new()
    }

    fn rx_icmp(&mut self, src_mac: Mac, h: &ipv4::Header, body: &[u8], now: Instant) -> Vec<Event> {
        let m = match icmp::parse(body) {
            Ok(m) => m,
            Err(_) => {
                self.counters.rx_icmp_malformed += 1;
                return Vec::new();
            }
        };
        match m.kind {
            icmp::TYPE_ECHO_REQUEST => {
                // only unicast to our own address is answered, never a broadcast (no amplification)
                let Some(cfg) = self.cfg else {
                    return Vec::new();
                };
                if h.dst != cfg.addr || m.body.len() > ECHO_MAX {
                    self.counters.icmp_echo_ignored += 1;
                    return Vec::new();
                }
                if !self.echo_bucket.take(now) {
                    self.counters.icmp_echo_rate_limited += 1;
                    return Vec::new();
                }
                let mut r = vec![0u8; icmp::HEADER + m.body.len()];
                if let Ok(n) = icmp::write_echo(&mut r, true, m.id, m.seq, m.body) {
                    r.truncate(n);
                    let packet = self.ip_packet(cfg.addr, h.src, ipv4::PROTO_ICMP, 64, &r);
                    self.counters.icmp_echo_replies_sent += 1;
                    self.queue_frame(src_mac, eth::ETHERTYPE_IPV4, &packet);
                }
                Vec::new()
            }
            icmp::TYPE_ECHO_REPLY => {
                let ep = self
                    .endpoints
                    .iter()
                    .find(|e| e.kind == Kind::Echo { ident: m.id })
                    .map(|e| e.id);
                match ep {
                    Some(ep) => vec![Event::EchoReply {
                        ep,
                        src: h.src,
                        seq: m.seq,
                        ttl: h.ttl,
                        payload: m.body.to_vec(),
                    }],
                    None => {
                        self.counters.icmp_unmatched += 1;
                        Vec::new()
                    }
                }
            }
            icmp::TYPE_UNREACHABLE => {
                self.counters.icmp_unreachable_received += 1;
                self.rx_icmp_error(m.code, m.body)
            }
            _ => {
                self.counters.icmp_other += 1;
                Vec::new()
            }
        }
    }

    fn rx_udp(
        &mut self,
        src_mac: Mac,
        h: &ipv4::Header,
        whole: &[u8],
        header_len: usize,
        now: Instant,
    ) -> Vec<Event> {
        let payload = whole.get(header_len..).unwrap_or(&[]);
        let u = match udp::parse(payload, h.src, h.dst) {
            Ok(u) => u,
            Err(_) => {
                self.counters.rx_udp_malformed += 1;
                return Vec::new();
            }
        };
        let ep = self.endpoints.iter().find(|e| match e.kind {
            Kind::Udp { port, addr } => port == u.dst_port && addr.is_none_or(|a| a == h.dst),
            _ => false,
        });
        if let Some(ep) = ep {
            if u.payload.len() > UDP_MAX {
                self.counters.rx_udp_malformed += 1;
                return Vec::new();
            }
            self.counters.udp_delivered += 1;
            return vec![Event::Deliver {
                ep: ep.id,
                src: h.src,
                src_port: u.src_port,
                payload: u.payload.to_vec(),
            }];
        }
        // no endpoint: a port unreachable, rate limited, never for a broadcast destination
        self.counters.udp_no_port += 1;
        let Some(cfg) = self.cfg else {
            return Vec::new();
        };
        let broadcast = h.dst == BROADCAST_IP || h.dst == cfg.subnet_broadcast();
        if broadcast {
            return Vec::new();
        }
        if !self.unreach_bucket.take(now) {
            self.counters.icmp_unreachable_rate_limited += 1;
            return Vec::new();
        }
        let quoted = whole.get(..whole.len().min(header_len + 8)).unwrap_or(&[]);
        let mut m = [0u8; icmp::HEADER + 28];
        if let Ok(n) = icmp::write_unreachable(&mut m, icmp::CODE_PORT_UNREACHABLE, quoted) {
            let packet = self.ip_packet(cfg.addr, h.src, ipv4::PROTO_ICMP, 64, &m[..n]);
            self.counters.icmp_unreachable_sent += 1;
            self.queue_frame(src_mac, eth::ETHERTYPE_IPV4, &packet);
        }
        Vec::new()
    }
}
