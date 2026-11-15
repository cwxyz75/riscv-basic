//! `netd`: the network stack (S8, NET-SPEC 6 to 8). One thread, one port, built on the service runtime: it feeds received
//! frames to the pure `netcore`, serves clients over the native protocol (`proto::net`), and moves frames to and from the
//! driver through the shared rings. It never blocks on a client (a datagram for a slow endpoint is dropped and counted)
//! and never waits for the driver except on its own port with a deadline.
//!
//! Handles (startup message): `NetRing`, `NetRxPool` (read-only, no DMA right), `NetTxPool`, the doorbell events
//! `NetEventToStack` (wait) and `NetEventToDriver` (signal), `NetCtl` (the driver's control channel), `ServiceRegistry` (a
//! connection with `PUBLISH`, to publish `/svc/net`, `/svc/net-priv` and `/svc/net-config`) and `ServiceInit` (a channel
//! from `netsup`, which sends a fresh control channel whenever it restarts the driver).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;

#[path = "../netblob.rs"]
mod netblob;
#[path = "../netshm.rs"]
mod netshm;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use netcore::{
    Config, ConnId, Core as Net, Device, EpId, Event, FailReason, ListenerId, ResetReason,
    TcpConfig, TcpEvent,
};
use netring::{
    ctl, msg, must_signal, stat, state, validate, Entry, Pool, Ring, RingId, Slot, FRAME_MAX,
    HDR_LEN, SLOTS, SLOT_SIZE,
};
use netshm::Shm;
use proto::net::{self as pn, Class, EpRequest, Request};
use proto::startup::Role;
use rt::abi::signals;
use rt::abi::status::Status as Sys;
use rt::service::{self, Core, Sent, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::{AccountId, Limits};

const RING_VA: u64 = 0x510_0000;
const RX_VA: u64 = 0x520_0000;
const TX_VA: u64 = 0x530_0000;
const BLOB_VA: u64 = 0x540_0000;
const PF_R: u64 = 1;
const PF_RW: u64 = 3;

/// Loop bounds (NET-SPEC 8.1): at most this many receive frames, client messages and transmit frames per iteration.
const N_RX: usize = 32;
const M_TX: usize = 32;
/// The safety poll: the loop wakes at least this often (ticks of 1 ms).
const POLL_TICKS: u64 = 10;

/// Keys of events that are not connections (top bit set, so they never collide with a connection's).
const KEY_DOORBELL: u64 = 1 << 63 | 1;
const KEY_DRIVER: u64 = 1 << 63 | 2;
const KEY_SUP: u64 = 1 << 63 | 3;
/// Keys of a stream's socket bindings: bit 62 set, the stream's key above bit 1, which of the two in bit 0.
const KEY_SOCK: u64 = 1 << 62;
const SOCK_RD: u64 = 0;
const SOCK_WR: u64 = 1;
/// Streams served per pass and bytes moved per call (S9-SPEC 7.2: P = 32, 16 KiB).
const P_STREAMS: usize = 32;
const PIECE: usize = 16 * 1024;
/// The MSS we announce (MTU 1500 less 40).
const TCP_MSS: u16 = 1460;

/// What this stack's connections are.
enum Conn {
    /// A published name's listener: forwarded opens arrive here.
    Listener(Class),
    /// A client's service connection (its own account).
    Service { class: Class },
    /// An endpoint channel: a UDP or echo endpoint of the stack, owned by this connection.
    Endpoint { ep: EpId, dropped: u32 },
    /// A TCP stream's endpoint channel (S9): control and notices; the bytes travel on the socket it names.
    Stream(Box<StreamState>),
    /// A TCP listener's endpoint channel: posted accepts arrive here, `Accepted` notices leave.
    TcpListener { lid: ListenerId },
}

/// Where a stream is in its life.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Phase {
    /// Opened; no `Connect` yet.
    Fresh,
    /// A posted accept of a listener, waiting for a SYN.
    Posted,
    /// `Connect` accepted, handshake in progress.
    Connecting,
    Established,
    /// Nothing more will happen on the socket (reset, failed, or finished and shut down).
    Over,
}

struct StreamState {
    phase: Phase,
    core: Option<ConnId>,
    sock: Option<u32>,
    account: AccountId,
    /// Bytes reserved against the account for this stream's buffers (S9-SPEC 6.2).
    reserved: u32,
    /// The client's cookie (for `Accepted`/`Connected` notices); and the core's accept cookie while posted.
    cookie: u32,
    accept_cookie: u32,
    listener: Option<u64>,
    nodelay: bool,
    rd_bound: bool,
    rd_armed: bool,
    wr_bound: bool,
    wr_armed: bool,
    /// The listener's id while posted.
    lid: Option<ListenerId>,
    /// The configuration or link went away: report any reset as such.
    lost: bool,
    /// The peer's FIN has arrived (in order) and was reported.
    fin_seen: bool,
    /// The client shut its write side down and we told the core.
    client_eof: bool,
    sock_shut: bool,
    /// A write into the socket was refused for want of the client's quota: retry at this tick.
    retry_at: Option<u64>,
    retry_delay: u64,
}

impl StreamState {
    fn new(account: AccountId) -> StreamState {
        StreamState {
            phase: Phase::Fresh,
            core: None,
            sock: None,
            account,
            reserved: 0,
            cookie: 0,
            accept_cookie: 0,
            listener: None,
            nodelay: false,
            rd_bound: false,
            rd_armed: false,
            wr_bound: false,
            wr_armed: false,
            lid: None,
            lost: false,
            fin_seen: false,
            client_eof: false,
            sock_shut: false,
            retry_at: None,
            retry_delay: 20,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Link {
    Detached,
    Attached(u64),
}

struct Netd {
    core: Core<Conn>,
    net: Net,
    shm: Shm,
    to_driver: u32,
    to_stack: u32,
    ctl: Option<u32>,
    link: Link,
    last_generation: u64,
    rx: Pool,
    tx: Pool,
    tx_free: Vec<u32>,
    eps: BTreeMap<EpId, u64>,
    /// Core connection id to the stream's key; listener id to its channel's key; core accept cookie to the stream's key.
    streams: BTreeMap<ConnId, u64>,
    listeners: BTreeMap<ListenerId, u64>,
    accepts: BTreeMap<u32, u64>,
    next_accept: u32,
    /// Koids of sockets the stack holds (a socket may serve one connection only: S9-SPEC 8.3).
    held_socks: Vec<(u64, u64)>,
    /// What each live core connection reserved against its account (released when the connection leaves the table).
    charged: BTreeMap<ConnId, (AccountId, u32)>,
    stream_rr: usize,
    more: bool,
    blob: Option<netblob::Blob>,
    /// Negative controls (`FAULT=` in the environment): never signal the driver; crash on a marked datagram.
    fault_no_doorbell: bool,
    fault_crash: bool,
}

fn now_ns() -> u64 {
    sys::clock_get().saturating_mul(1_000_000)
}

fn rx_byte(off: usize) -> u8 {
    // SAFETY: RX_VA maps the receive pool read-only for the life of the process; callers pass offsets inside it.
    unsafe { core::ptr::read_volatile((RX_VA as usize + off) as *const u8) }
}

fn tx_write(off: usize, b: u8) {
    // SAFETY: TX_VA maps the transmit pool read-write; callers pass offsets inside it.
    unsafe { core::ptr::write_volatile((TX_VA as usize + off) as *mut u8, b) }
}

/// The transmit side of the device, as the core sees it.
struct TxDev<'a> {
    shm: Shm,
    free: &'a mut Vec<u32>,
    view: &'a mut Pool,
    published: bool,
    was_empty: bool,
}

impl Device for TxDev<'_> {
    fn tx(&mut self, frame: &[u8]) -> bool {
        let Some(&slot) = self.free.last() else {
            self.shm.count(stat::TX_DROPPED_NO_SLOT);
            return false;
        };
        let ring = Ring::new(&self.shm, RingId::TxReady);
        if self.was_empty && !self.published {
            self.was_empty = ring.is_empty();
        }
        let base = slot as usize * SLOT_SIZE;
        for i in 0..HDR_LEN {
            tx_write(base + i, 0); // the virtio-net header is all zero: no offloads were negotiated
        }
        for (i, b) in frame.iter().enumerate() {
            tx_write(base + HDR_LEN + i, *b);
        }
        if ring
            .push(Entry {
                slot,
                len: frame.len() as u16,
            })
            .is_err()
        {
            return false;
        }
        self.free.pop();
        let _ = self.view.mv(slot, Slot::StackFree, Slot::DeviceHas);
        self.published = true;
        true
    }
}

impl Netd {
    fn ring(&self, id: RingId) -> Ring<'_, Shm> {
        Ring::new(&self.shm, id)
    }

    fn info(&self) -> pn::Info {
        let c = self.net.config();
        let z = [0u8; 4];
        pn::Info {
            addr: c.map_or(z, |c| c.addr),
            mask: c.map_or(z, |c| c.mask),
            gateway: c.map_or(z, |c| c.gateway),
            dns: c.map_or(z, |c| c.dns),
            mac: self.net.mac(),
            mtu: netcore::MTU as u16,
            link: self.net.link_is_up(),
            configured: c.is_some(),
        }
    }

    // ---- the driver side ----

    fn detach(&mut self) {
        if self.link != Link::Detached {
            self.link = Link::Detached;
            let ev = self.net.link(false);
            self.apply(ev);
        }
    }

    /// The control channel: READY starts a new generation; its closing detaches.
    fn poll_ctl(&mut self) {
        let Some(ch) = self.ctl else { return };
        loop {
            let mut b = [0u8; 64];
            let r = sys::channel_read(ch, b.as_mut_ptr(), b.len(), core::ptr::null_mut(), 0);
            match r.status() {
                Some(Sys::Ok) => match msg::decode(&b[..r.a1]) {
                    Some((
                        _,
                        msg::Msg::Ready {
                            generation,
                            mac,
                            link,
                        },
                    )) => {
                        if generation <= self.last_generation || self.link != Link::Detached {
                            // a generation that is not newer, or a second READY: a violation by the driver (it is told to go)
                            self.violation();
                            continue;
                        }
                        // forget every outstanding slot, rebuild the free list, treat it as a link flap
                        self.last_generation = generation;
                        self.rx = Pool::rx_posted();
                        self.tx = Pool::tx_free();
                        self.tx_free = (0..SLOTS as u32).rev().collect();
                        self.net.set_mac(mac);
                        let at = msg::encode_attach(1, generation);
                        let _ = sys::channel_write(ch, at.as_ptr(), at.len(), core::ptr::null(), 0);
                        self.link = Link::Attached(generation);
                        let ev = self.net.link(link);
                        self.apply(ev);
                    }
                    Some((_, msg::Msg::Link { link })) => {
                        let ev = self.net.link(link);
                        self.apply(ev);
                    }
                    _ => self.violation(),
                },
                Some(Sys::ShouldWait) => return,
                _ => {
                    // the control channel is closed: the driver is gone until netsup sends a new one
                    self.detach();
                    sys::handle_close(ch);
                    self.ctl = None;
                    return;
                }
            }
        }
    }

    /// `netsup` sends the stack end of a fresh control channel when it restarts the driver.
    fn poll_sup(&mut self, sup: u32) {
        loop {
            let mut b = [0u8; 16];
            let mut hs = [0u32; 4];
            let r = sys::channel_read(sup, b.as_mut_ptr(), b.len(), hs.as_mut_ptr(), hs.len());
            if r.status() != Some(Sys::Ok) {
                return;
            }
            if r.a2 >= 1 {
                if let Some(old) = self.ctl.replace(hs[0]) {
                    sys::handle_close(old);
                }
                self.detach();
                let _ = sys::port_bind(
                    self.core.port(),
                    hs[0],
                    signals::READABLE | signals::PEER_CLOSED,
                    KEY_DRIVER,
                );
            }
            for &h in hs.iter().take(r.a2).skip(1) {
                sys::handle_close(h);
            }
        }
    }

    /// One pass over the data path: receive, deliver, client messages are served by the runtime, transmit, doorbells.
    fn pump(&mut self) {
        self.poll_ctl();
        let attached = matches!(self.link, Link::Attached(g) if g == self.shm.load64(ctl::GENERATION) && netring::Mem::load(&self.shm, ctl::STATE) != state::NONE);
        if !attached {
            if self.link != Link::Detached {
                // the driver restarted underneath us (state NONE, or a newer generation)
                self.detach();
            }
            // nothing to move: sleep until an event or the next timer (not a busy loop while there is no driver)
            let ev = self.net.poll(now_ns());
            self.apply(ev);
            self.drain_tcp();
            let moved = self.pump_streams();
            self.drain_tcp();
            // (loopback streams work with no driver: keep looping while bytes move)
            self.more = moved;
            return;
        }
        netring::Mem::store(&self.shm, ctl::NET_WAITING, 0);
        let mut moved = false;
        let now = now_ns();
        // reclaim finished transmits
        loop {
            match self.ring(RingId::TxDone).pop() {
                Ok(Some(e)) => {
                    let ok = validate(RingId::TxDone, e).is_ok()
                        && self.tx.mv(e.slot, Slot::DeviceHas, Slot::StackFree).is_ok();
                    if !ok {
                        self.violation();
                        return;
                    }
                    self.tx_free.push(e.slot);
                    moved = true;
                }
                Ok(None) => break,
                Err(_) => {
                    self.violation();
                    return;
                }
            }
        }
        // received frames, at most N per pass
        let mut returned = false;
        let rx_free_was_empty = self.ring(RingId::RxFree).is_empty();
        for _ in 0..N_RX {
            let e = match self.ring(RingId::RxUsed).pop() {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => {
                    self.violation();
                    return;
                }
            };
            let Ok(e) = validate(RingId::RxUsed, e) else {
                self.violation();
                return;
            };
            if self.rx.mv(e.slot, Slot::DeviceHas, Slot::StackHas).is_err() {
                self.violation();
                return;
            }
            let base = e.slot as usize * SLOT_SIZE;
            // the virtio-net header: no offload, no GSO (anything else is dropped and counted)
            let (flags, gso) = (rx_byte(base), rx_byte(base + 1));
            if flags & 1 != 0 || gso != 0 {
                self.shm.count(stat::RX_BAD_HEADER);
            } else {
                let mut f = [0u8; FRAME_MAX];
                let n = usize::from(e.len).min(FRAME_MAX);
                for (i, b) in f.iter_mut().enumerate().take(n) {
                    *b = rx_byte(base + HDR_LEN + i);
                }
                let ev = self.net.ingress(&f[..n], now);
                self.apply(ev);
            }
            if self
                .ring(RingId::RxFree)
                .push(Entry {
                    slot: e.slot,
                    len: 0,
                })
                .is_err()
                || self.rx.mv(e.slot, Slot::StackHas, Slot::DeviceHas).is_err()
            {
                self.violation();
                return;
            }
            returned = true;
            moved = true;
        }
        // streams: bytes between the core and the clients' sockets, then what that made the core want to send
        self.drain_tcp();
        if self.pump_streams() {
            moved = true;
        }
        // frames to send, at most M per pass
        let was_empty = self.ring(RingId::TxReady).is_empty();
        let (published, ev) = {
            let mut dev = TxDev {
                shm: self.shm,
                free: &mut self.tx_free,
                view: &mut self.tx,
                published: false,
                was_empty,
            };
            let (_, ev) = self.net.egress(&mut dev, now, M_TX);
            (dev.published, ev)
        };
        self.apply(ev);
        self.drain_tcp();
        if published {
            moved = true;
        }
        if (published && must_signal(&self.shm, ctl::DRV_WAITING, was_empty))
            || (returned && must_signal(&self.shm, ctl::DRV_WAITING, rx_free_was_empty))
        {
            if !self.fault_no_doorbell {
                let _ = sys::event_signal(self.to_driver, 0, 1 << 24);
            }
            self.shm.count(stat::DOORBELLS_SENT);
        } else if published || returned {
            self.shm.count(stat::DOORBELLS_SUPPRESSED);
        }
        // announce, fence, re-check (5.5): only then may the runtime sleep
        let again = moved
            || netring::announce_waiting(&self.shm, ctl::NET_WAITING, RingId::RxUsed)
            || !self.ring(RingId::TxDone).is_empty();
        if again && netring::Mem::load(&self.shm, ctl::NET_WAITING) == 1 {
            netring::Mem::store(&self.shm, ctl::NET_WAITING, 0);
        }
        self.more = again;
    }

    fn violation(&mut self) {
        self.shm.count(stat::RING_VIOLATIONS);
        rt::println!("netd: ring violation: detaching from the driver");
        let at = msg::encode_detach(2);
        if let Some(ch) = self.ctl {
            let _ = sys::channel_write(ch, at.as_ptr(), at.len(), core::ptr::null(), 0);
        }
        self.detach();
    }

    // ---- delivering to clients ----

    fn apply(&mut self, events: Vec<Event>) {
        for ev in events {
            match ev {
                Event::Deliver {
                    ep,
                    src,
                    src_port,
                    payload,
                } => self.deliver(ep, |d| pn::encode_recv(d, src, src_port, &payload)),
                Event::EchoReply {
                    ep,
                    src,
                    seq,
                    ttl,
                    payload,
                } => self.deliver(ep, |d| pn::encode_echo_reply(d, src, seq, ttl, &payload)),
                Event::Notice {
                    ep,
                    code,
                    cookie,
                    detail,
                } => {
                    if let Some(&key) = self.eps.get(&ep) {
                        let _ = self
                            .core
                            .send_datagram(key, &pn::encode_notice(code, cookie, detail));
                    }
                }
            }
        }
    }

    /// One datagram to an endpoint's channel: if the client has no room it is dropped and counted, never waited for.
    fn deliver(&mut self, ep: EpId, encode: impl FnOnce(u32) -> Vec<u8>) {
        let Some(&key) = self.eps.get(&ep) else {
            return;
        };
        let dropped = match self.core.conn(key) {
            Some(Conn::Endpoint { dropped, .. }) => *dropped,
            _ => return,
        };
        let bytes = encode(dropped);
        match self.core.send_datagram(key, &bytes) {
            Sent::Sent => {
                if let Some(Conn::Endpoint { dropped, .. }) = self.core.conn_mut(key) {
                    *dropped = 0;
                }
            }
            Sent::Held => {
                if let Some(Conn::Endpoint { dropped, .. }) = self.core.conn_mut(key) {
                    *dropped = dropped.saturating_add(1);
                }
            }
            Sent::Closed => {}
        }
    }

    // ---- clients ----

    fn reply(&mut self, key: u64, txid: u32, status: i32, body: &[u8]) {
        let _ = self
            .core
            .send(key, pn::encode_reply(txid, status, body), None);
    }

    fn open_conn(&mut self, key: u64, class: Class, msg: &[u8], hs: &[u32]) {
        let close_all = |hs: &[u32]| {
            for &h in hs {
                sys::handle_close(h);
            }
        };
        let (txid, _, req) = match pn::decode_request(msg) {
            Ok(x) => x,
            Err(m) => {
                close_all(hs);
                if let Some(t) = m.txid {
                    self.reply(key, t, pn::status::BAD_REQUEST, &[]);
                }
                return;
            }
        };
        if !matches!(req, Request::Open { .. }) && !hs.is_empty() {
            close_all(hs);
            return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
        }
        match req {
            Request::Info => self.reply(key, txid, pn::status::OK, &pn::encode_info(&self.info())),
            Request::Open {
                kind,
                local_port,
                local_addr,
                oflags,
            } => {
                if class == Class::Config {
                    close_all(hs);
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                // an unknown kind, a wrong handle count (a request may carry more than one: S9's `Accept` does, on an
                // endpoint channel) or unknown flags are malformed. Every handle passed is closed either way.
                let okind = pn::classify_open_kind(kind);
                let stream_kind =
                    matches!(okind, pn::OpenKind::TcpStream | pn::OpenKind::TcpListener);
                let bad_flags = if stream_kind {
                    oflags & !pn::oflags::NODELAY != 0
                } else {
                    oflags & !(pn::oflags::BROADCAST | pn::oflags::SRC_ANY) != 0
                };
                if hs.len() != 1 || bad_flags || okind == pn::OpenKind::Invalid {
                    close_all(hs);
                    return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
                }
                let privileged = class == Class::Privileged;
                if stream_kind {
                    return self.open_stream_endpoint(
                        key, txid, okind, class, local_port, local_addr, oflags, hs[0],
                    );
                }
                if (oflags != 0 && !privileged)
                    || (local_port != 0
                        && local_port < pn::limits::FIRST_UNPRIVILEGED_PORT
                        && !privileged)
                {
                    close_all(hs);
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                let addr = (local_addr != [0; 4]).then_some(local_addr);
                let made = if kind == pn::kind::UDP {
                    self.net.open_udp(local_port, addr, oflags != 0)
                } else {
                    self.net.open_echo(false).map(|(e, _)| (e, 0))
                };
                let (ep, port) = match made {
                    Ok(x) => x,
                    Err(netcore::BindError::AddrInUse) => {
                        close_all(hs);
                        return self.reply(key, txid, pn::status::ADDR_IN_USE, &[]);
                    }
                    Err(netcore::BindError::NoPorts) => {
                        close_all(hs);
                        return self.reply(key, txid, pn::status::NO_BUFFERS, &[]);
                    }
                };
                let account = self.core.account_of(key).unwrap_or(svc::ROOT);
                match self
                    .core
                    .add_conn(hs[0], account, Conn::Endpoint { ep, dropped: 0 })
                {
                    Some(k) => {
                        self.eps.insert(ep, k);
                        self.reply(
                            key,
                            txid,
                            pn::status::OK,
                            &pn::encode_open_reply(port, [0; 4]),
                        );
                    }
                    None => {
                        self.net.close(ep);
                        close_all(hs);
                        self.reply(key, txid, pn::status::QUOTA, &[]);
                    }
                }
            }
            Request::SetConfig {
                addr,
                mask,
                gateway,
                dns,
            } => {
                if class != Class::Config {
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                if addr == [0; 4] || mask == [0; 4] {
                    return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
                }
                self.net.configure(
                    Config {
                        addr,
                        mask,
                        gateway,
                        dns,
                    },
                    now_ns(),
                );
                if let Some(b) = &self.blob {
                    b.write_iface(Some(netblob::Iface {
                        addr,
                        mask,
                        gateway,
                        dns,
                    }));
                }
                self.reply(key, txid, pn::status::OK, &[]);
                self.more = true;
            }
            Request::ClearConfig => {
                if class != Class::Config {
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                self.net.clear_config();
                self.reset_all_streams();
                if let Some(b) = &self.blob {
                    b.write_iface(None);
                }
                self.reply(key, txid, pn::status::OK, &[]);
            }
            Request::GetArp => {
                if class != Class::Config {
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                let t = self.net.arp_table(now_ns());
                self.reply(key, txid, pn::status::OK, &pn::encode_arp(&t));
            }
            Request::GetStats => {
                if class != Class::Config {
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                let mut pairs: Vec<(u16, u64)> = self
                    .net
                    .counters()
                    .pairs()
                    .into_iter()
                    .map(|(i, _, v)| (i, v))
                    .collect();
                let base = pairs.len() as u16;
                for (i, s) in [
                    stat::RX_FRAMES,
                    stat::TX_FRAMES,
                    stat::RX_BAD_HEADER,
                    stat::RING_VIOLATIONS,
                    stat::DOORBELLS_SENT,
                    stat::DOORBELLS_SUPPRESSED,
                    stat::TX_DROPPED_NO_SLOT,
                ]
                .into_iter()
                .enumerate()
                {
                    pairs.push((base + i as u16, self.shm.load64(ctl::STATS + 8 * s)));
                }
                self.reply(key, txid, pn::status::OK, &pn::encode_stats(&pairs));
            }
        }
    }

    fn endpoint_request(&mut self, key: u64, ep: EpId, msg: &[u8], hs: &[u32]) {
        for &h in hs {
            sys::handle_close(h);
        }
        let Some(req) = pn::decode_ep_request(msg).filter(|_| hs.is_empty()) else {
            return self.core.violation(key);
        };
        let now = now_ns();
        let (cookie, r) = match req {
            EpRequest::SendUdp {
                cookie,
                dst,
                port,
                payload,
            } => {
                if self.fault_crash && payload.starts_with(b"##crash") {
                    panic!("fault-crash-on-magic");
                }
                (
                    cookie,
                    self.net.send_udp(ep, dst, port, payload, cookie, now),
                )
            }
            EpRequest::SendEcho {
                cookie,
                dst,
                seq,
                payload,
            } => (
                cookie,
                self.net.send_echo(ep, dst, seq, payload, cookie, now),
            ),
            // stream operations on a datagram endpoint: a protocol violation
            _ => return self.core.violation(key),
        };
        if let Err(e) = r {
            let _ = self.core.send_datagram(
                key,
                &pn::encode_notice(pn::notice::SEND_FAILED, cookie, e.status() as u32),
            );
        }
        self.more = true;
    }

    fn publish_all(&mut self, registry: u32) -> bool {
        let mut fs = rt::fs::Fs::new(registry);
        for (name, class) in Class::NAMES {
            let c = sys::channel_create();
            if c.status != 0 {
                return false;
            }
            let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
            // (a stale entry from a previous incarnation is replaced; a live one is a conflict)
            if fs.publish(name, theirs).is_err() {
                sys::handle_close(mine);
                return false;
            }
            if self
                .core
                .add_conn(mine, svc::ROOT, Conn::Listener(class))
                .is_none()
            {
                return false;
            }
        }
        // the registry connection itself stays open (we keep it: publishing is a standing authority of netsup's)
        let _ = fs;
        true
    }
}

// ---------------------------------------------------------------------------------------------------- TCP streams (S9)

fn status_of_connect_error(e: netcore::ConnectError) -> i32 {
    match e {
        netcore::ConnectError::NotConfigured => pn::status::NOT_CONFIGURED,
        netcore::ConnectError::NoRoute => pn::status::NO_ROUTE,
        netcore::ConnectError::NoPorts | netcore::ConnectError::TooMany => pn::status::NO_BUFFERS,
        netcore::ConnectError::AddrInUse => pn::status::ADDR_IN_USE,
    }
}

/// The buffer sizes a request asks for, clamped to what the stack allows (0: the default).
fn clamp_bufs(snd: u32, rcv: u32) -> (usize, usize) {
    let f = |v: u32, max: u32| {
        if v == 0 {
            pn::limits::STREAM_BUF_DEFAULT
        } else {
            v.clamp(pn::limits::STREAM_BUF_MIN, max)
        }
    };
    (
        f(snd, pn::limits::STREAM_BUF_MAX) as usize,
        f(rcv, pn::limits::STREAM_RCV_BUF_MAX) as usize,
    )
}

impl Netd {
    fn account_of_key(&self, key: u64) -> AccountId {
        self.core.account_of(key).unwrap_or(svc::ROOT)
    }

    fn notice(&mut self, key: u64, code: u16, cookie: u32, detail: u32) {
        let _ = self
            .core
            .send(key, pn::encode_notice(code, cookie, detail), None);
    }

    /// Is `h` a socket we may serve (S9-SPEC 8.3)? A channel, a socket without READ, WRITE or WAIT, a socket we already hold
    /// and the peer of one we hold are refused; an admitted socket is recorded.
    fn admit_socket(&mut self, h: u32) -> bool {
        let mut b = [0u8; 1];
        for x in [
            sys::socket_read(h, b.as_mut_ptr(), 0),
            sys::socket_write(h, b.as_ptr(), 0),
        ] {
            match x.status() {
                Some(Sys::Ok) | Some(Sys::ShouldWait) | Some(Sys::PeerClosed) => {}
                _ => return false,
            }
        }
        let port = sys::port_create();
        if port.status != 0 {
            return false;
        }
        let p = port.a1 as u32;
        let bindable =
            sys::port_bind_flags(p, h, signals::READABLE, 0, rt::abi::port_flags::ONESHOT).status
                == 0;
        sys::handle_close(p);
        if !bindable {
            return false;
        }
        let info = sys::object_info(h);
        if info.status != 0 {
            return false;
        }
        let (koid, peer) = (info.a1 as u64, info.a2 as u64);
        if self
            .held_socks
            .iter()
            .any(|&(k, p2)| k == koid || p2 == koid || k == peer)
        {
            return false;
        }
        self.held_socks.push((koid, peer));
        true
    }

    fn forget_socket(&mut self, h: u32) {
        let info = sys::object_info(h);
        if info.status == 0 {
            let koid = info.a1 as u64;
            self.held_socks.retain(|&(k, _)| k != koid);
        }
    }

    /// `Open` of kind 3 (a stream endpoint) or 4 (a listener).
    #[allow(clippy::too_many_arguments)]
    fn open_stream_endpoint(
        &mut self,
        key: u64,
        txid: u32,
        kind: pn::OpenKind,
        class: Class,
        local_port: u16,
        local_addr: [u8; 4],
        oflags: u32,
        ch: u32,
    ) {
        let account = self.account_of_key(key);
        match kind {
            pn::OpenKind::TcpStream => {
                // a stream binds no port at open: the local port is chosen at connect
                if local_port != 0 || local_addr != [0; 4] {
                    sys::handle_close(ch);
                    return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
                }
                let mut st = StreamState::new(account);
                st.nodelay = oflags & pn::oflags::NODELAY != 0;
                match self.core.add_conn(ch, account, Conn::Stream(Box::new(st))) {
                    Some(_) => {
                        self.reply(key, txid, pn::status::OK, &pn::encode_open_reply(0, [0; 4]))
                    }
                    None => {
                        sys::handle_close(ch);
                        self.reply(key, txid, pn::status::QUOTA, &[]);
                    }
                }
            }
            _ => {
                if local_port == 0 {
                    sys::handle_close(ch);
                    return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
                }
                if local_port < pn::limits::FIRST_UNPRIVILEGED_PORT && class != Class::Privileged {
                    sys::handle_close(ch);
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                let addr = (local_addr != [0; 4]).then_some(local_addr);
                let lid = match self.net.tcp_listen(addr, local_port) {
                    Ok(l) => l,
                    Err(_) => {
                        sys::handle_close(ch);
                        return self.reply(key, txid, pn::status::ADDR_IN_USE, &[]);
                    }
                };
                match self.core.add_conn(ch, account, Conn::TcpListener { lid }) {
                    Some(k) => {
                        self.listeners.insert(lid, k);
                        self.reply(
                            key,
                            txid,
                            pn::status::OK,
                            &pn::encode_open_reply(local_port, [0; 4]),
                        );
                    }
                    None => {
                        self.net.tcp_close_listener(lid);
                        sys::handle_close(ch);
                        self.reply(key, txid, pn::status::QUOTA, &[]);
                    }
                }
            }
        }
    }

    /// A message on a stream's endpoint channel.
    fn stream_request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        let close_all = |hs: &[u32]| {
            for &h in hs {
                sys::handle_close(h);
            }
        };
        let Some(req) = pn::decode_ep_request(msg) else {
            close_all(hs);
            return self.core.violation(key);
        };
        match req {
            EpRequest::Connect {
                cookie,
                dst,
                port,
                snd_buf,
                rcv_buf,
            } => {
                if hs.len() != 1 {
                    close_all(hs);
                    return self.core.violation(key);
                }
                let sock = hs[0];
                let (phase, nodelay, account) = match self.core.conn(key) {
                    Some(Conn::Stream(st)) => (st.phase, st.nodelay, st.account),
                    _ => return close_all(hs),
                };
                let fail = |d: &mut Netd, status: i32| {
                    sys::handle_close(sock);
                    d.notice(key, pn::notice::CONNECT_FAILED, cookie, status as u32);
                };
                if phase != Phase::Fresh {
                    return fail(self, pn::status::ALREADY_CONNECTED);
                }
                if !self.admit_socket(sock) {
                    return fail(self, pn::status::BAD_HANDLE);
                }
                let (snd, rcv) = clamp_bufs(snd_buf, rcv_buf);
                let bytes = (snd + rcv) as u32;
                if self.core.accounts().charge_stream(account, bytes).is_err() {
                    self.forget_socket(sock);
                    return fail(self, pn::status::QUOTA);
                }
                let cfg = TcpConfig {
                    snd_buf: snd,
                    rcv_buf: rcv,
                    mss: TCP_MSS,
                    nodelay,
                };
                match self.net.tcp_connect(dst, port, None, cfg, now_ns()) {
                    Ok(id) => {
                        self.streams.insert(id, key);
                        self.charged.insert(id, (account, bytes));
                        if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                            st.phase = Phase::Connecting;
                            st.core = Some(id);
                            st.sock = Some(sock);
                            st.cookie = cookie;
                        }
                    }
                    Err(e) => {
                        self.core.accounts().release_stream(account, bytes);
                        self.forget_socket(sock);
                        fail(self, status_of_connect_error(e));
                    }
                }
                self.more = true;
            }
            EpRequest::Abort => {
                close_all(hs);
                if let Some(id) = self.stream_core(key) {
                    self.net.tcp_abort(id);
                }
                self.more = true;
            }
            EpRequest::SetOption { option, value } => {
                close_all(hs);
                if option == pn::option::NODELAY {
                    if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                        st.nodelay = value != 0;
                    }
                    if let Some(id) = self.stream_core(key) {
                        self.net.tcp_set_nodelay(id, value != 0);
                    }
                }
            }
            _ => {
                close_all(hs);
                self.core.violation(key);
            }
        }
    }

    fn stream_core(&self, key: u64) -> Option<ConnId> {
        match self.core.conn(key) {
            Some(Conn::Stream(st)) => st.core,
            _ => None,
        }
    }

    /// A message on a listener's endpoint channel: `Accept` posts one accept (two handles: a channel end and a socket end).
    fn listener_request(&mut self, key: u64, lid: ListenerId, msg: &[u8], hs: &[u32]) {
        let close_all = |hs: &[u32]| {
            for &h in hs {
                sys::handle_close(h);
            }
        };
        let Some(EpRequest::Accept {
            cookie,
            snd_buf,
            rcv_buf,
        }) = pn::decode_ep_request(msg)
        else {
            close_all(hs);
            return self.core.violation(key);
        };
        if hs.len() != 2 {
            close_all(hs);
            return self.core.violation(key);
        }
        let (ch, sock) = (hs[0], hs[1]);
        let refuse = |d: &mut Netd, status: i32| {
            close_all(&[ch, sock]);
            d.notice(key, pn::notice::CONNECT_FAILED, cookie, status as u32);
        };
        if self.net.tcp_posted_accepts(lid) >= pn::limits::POSTED_ACCEPTS_MAX {
            return refuse(self, pn::status::QUOTA);
        }
        if !self.admit_socket(sock) {
            return refuse(self, pn::status::BAD_HANDLE);
        }
        let account = self.account_of_key(key);
        let (snd, rcv) = clamp_bufs(snd_buf, rcv_buf);
        let bytes = (snd + rcv) as u32;
        // (S9-SPEC 6.2 reserves when a SYN takes the accept; this reserves when it is posted: TIDY-UP T97)
        if self.core.accounts().charge_stream(account, bytes).is_err() {
            self.forget_socket(sock);
            return refuse(self, pn::status::QUOTA);
        }
        let acc = self.next_accept;
        self.next_accept = self.next_accept.wrapping_add(1).max(1);
        let mut st = StreamState::new(account);
        st.phase = Phase::Posted;
        st.sock = Some(sock);
        st.reserved = bytes;
        st.cookie = cookie;
        st.accept_cookie = acc;
        st.listener = Some(key);
        st.lid = Some(lid);
        let Some(skey) = self.core.add_conn(ch, account, Conn::Stream(Box::new(st))) else {
            self.core.accounts().release_stream(account, bytes);
            self.forget_socket(sock);
            return refuse(self, pn::status::QUOTA);
        };
        let cfg = TcpConfig {
            snd_buf: snd,
            rcv_buf: rcv,
            mss: TCP_MSS,
            nodelay: false,
        };
        if self.net.tcp_post_accept(lid, acc, cfg).is_err() {
            // (closing the stream's connection runs `stream_closed`, which gives the reservation and the socket back)
            self.core.close_conn(skey);
            self.notice(
                key,
                pn::notice::CONNECT_FAILED,
                cookie,
                pn::status::QUOTA as u32,
            );
            return;
        }
        self.accepts.insert(acc, skey);
    }

    /// A stream's endpoint channel closed (the client let go, or died).
    fn stream_closed(&mut self, st: StreamState) {
        match st.phase {
            Phase::Fresh => {}
            Phase::Posted => {
                if let Some(lid) = st.lid {
                    self.net.tcp_cancel_accept(lid, st.accept_cookie);
                }
                self.accepts.remove(&st.accept_cookie);
                self.core.accounts().release_stream(st.account, st.reserved);
                let _ = self.core.accounts().close(st.account);
                if let Some(sock) = st.sock {
                    self.forget_socket(sock);
                    sys::handle_close(sock);
                }
            }
            Phase::Connecting | Phase::Established | Phase::Over => {
                if let (Some(id), Some(sock)) = (st.core, st.sock) {
                    // take what the client had already written, then let the connection finish what it can as an orphan
                    for _ in 0..4 {
                        let space = self.net.tcp_send_space(id);
                        if space == 0 {
                            break;
                        }
                        let mut buf = [0u8; PIECE];
                        let r = sys::socket_read(sock, buf.as_mut_ptr(), space.min(PIECE));
                        if r.status != 0 || r.a1 == 0 {
                            break;
                        }
                        self.net.tcp_write(id, &buf[..r.a1]);
                    }
                    self.net.tcp_release(id, now_ns());
                    self.more = true;
                }
                if let Some(sock) = st.sock {
                    self.forget_socket(sock);
                    sys::handle_close(sock);
                }
            }
        }
    }

    /// A listener's endpoint channel closed: its posted accepts go with it.
    fn listener_closed(&mut self, lid: ListenerId) {
        self.net.tcp_close_listener(lid);
        self.listeners.remove(&lid);
    }

    // ---- what the core reports

    fn drain_tcp(&mut self) {
        for ev in self.net.take_tcp_events() {
            self.tcp_event(ev);
        }
    }

    fn stream_over(&mut self, key: u64) {
        let sock = match self.core.conn_mut(key) {
            Some(Conn::Stream(st)) => {
                st.phase = Phase::Over;
                st.sock.take()
            }
            _ => None,
        };
        if let Some(sock) = sock {
            self.forget_socket(sock);
            sys::handle_close(sock);
        }
    }

    fn tcp_event(&mut self, ev: TcpEvent) {
        match ev {
            TcpEvent::Connected {
                conn,
                local,
                remote,
            } => {
                let Some(&key) = self.streams.get(&conn) else {
                    return;
                };
                let cookie = self.establish(key);
                let _ = self.core.send(
                    key,
                    pn::encode_conn_info(pn::notice::CONNECTED, cookie, local, remote),
                    None,
                );
            }
            TcpEvent::Accepted {
                cookie: acc,
                conn,
                local,
                remote,
                ..
            } => {
                let Some(skey) = self.accepts.remove(&acc) else {
                    self.net.tcp_abort(conn);
                    return;
                };
                self.streams.insert(conn, skey);
                let (account, reserved, lkey) = match self.core.conn_mut(skey) {
                    Some(Conn::Stream(st)) => {
                        st.core = Some(conn);
                        let r = (st.account, core::mem::take(&mut st.reserved), st.listener);
                        st.listener = None;
                        r
                    }
                    _ => {
                        self.net.tcp_abort(conn);
                        return;
                    }
                };
                self.charged.insert(conn, (account, reserved));
                let cookie = self.establish(skey);
                if let Some(lkey) = lkey {
                    let _ = self.core.send(
                        lkey,
                        pn::encode_conn_info(pn::notice::ACCEPTED, cookie, local, remote),
                        None,
                    );
                }
                let _ = self.core.send(
                    skey,
                    pn::encode_conn_info(pn::notice::CONNECTED, cookie, local, remote),
                    None,
                );
            }
            TcpEvent::ConnectFailed { conn, reason } => {
                let Some(&key) = self.streams.get(&conn) else {
                    return;
                };
                let cookie = match self.core.conn(key) {
                    Some(Conn::Stream(st)) => st.cookie,
                    _ => 0,
                };
                let status = match reason {
                    FailReason::Refused => pn::status::CONN_REFUSED,
                    FailReason::TimedOut => pn::status::TIMED_OUT,
                    FailReason::Unreachable => pn::status::HOST_UNREACHABLE,
                };
                self.notice(key, pn::notice::CONNECT_FAILED, cookie, status as u32);
                self.stream_over(key);
            }
            TcpEvent::PeerFin { conn } => {
                if let Some(&key) = self.streams.get(&conn) {
                    if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                        st.fin_seen = true;
                    }
                    self.notice(key, pn::notice::PEER_FIN, 0, 0);
                }
            }
            TcpEvent::Reset { conn, reason } => {
                let Some(&key) = self.streams.get(&conn) else {
                    return;
                };
                let lost = matches!(self.core.conn(key), Some(Conn::Stream(st)) if st.lost);
                let detail = if lost {
                    pn::reset_reason::LINK_LOST
                } else {
                    match reason {
                        ResetReason::PeerReset => pn::reset_reason::PEER_RESET,
                        ResetReason::TimedOut => pn::reset_reason::TIMED_OUT,
                        ResetReason::Aborted => pn::reset_reason::ABORTED,
                    }
                };
                self.notice(key, pn::notice::RESET, 0, detail);
                self.stream_over(key);
            }
            TcpEvent::Closed { conn } => {
                // a clean end: the socket was shut down when the peer's FIN had been delivered; nothing more to say
                let _ = conn;
            }
            TcpEvent::AcceptStarved { listener } => {
                if let Some(&lkey) = self.listeners.get(&listener) {
                    self.notice(lkey, pn::notice::ACCEPT_STARVED, 0, 0);
                }
            }
            TcpEvent::Released { conn } => {
                // the connection left the table: if its peer's FIN was delivered but the socket not yet shut down, do it now
                // (a FIN that arrives with nothing before it ends the connection in one step: the pump never sees it)
                if let Some(&key) = self.streams.get(&conn) {
                    if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                        if st.phase == Phase::Established && st.fin_seen && !st.sock_shut {
                            if let Some(sock) = st.sock {
                                sys::socket_shutdown(sock);
                                st.sock_shut = true;
                            }
                        }
                    }
                }
                self.streams.remove(&conn);
                if let Some((account, bytes)) = self.charged.remove(&conn) {
                    self.core.accounts().release_stream(account, bytes);
                    // (the account may have been waiting only for this: close it if nothing else holds it)
                    let _ = self.core.accounts().close(account);
                }
            }
        }
    }

    /// A stream became established: start watching its socket. Returns the client's cookie.
    fn establish(&mut self, key: u64) -> u32 {
        let port = self.core.port();
        let Some(Conn::Stream(st)) = self.core.conn_mut(key) else {
            return 0;
        };
        st.phase = Phase::Established;
        if let Some(sock) = st.sock {
            // READABLE (data, or the client's end of stream) one-shot, armed now; the WRITABLE one is bound when needed
            let k = KEY_SOCK | (key << 1) | SOCK_RD;
            let r = sys::port_bind_flags(
                port,
                sock,
                signals::READABLE | signals::PEER_CLOSED | signals::WRITE_CLOSED,
                k,
                rt::abi::port_flags::ONESHOT,
            );
            st.rd_bound = r.status == 0;
            st.rd_armed = r.status == 0;
        }
        self.more = true;
        st.cookie
    }

    fn arm_wr(&mut self, key: u64) {
        let port = self.core.port();
        let Some(Conn::Stream(st)) = self.core.conn_mut(key) else {
            return;
        };
        let Some(sock) = st.sock else { return };
        if st.wr_armed {
            return;
        }
        let k = KEY_SOCK | (key << 1) | SOCK_WR;
        let ok = if st.wr_bound {
            sys::port_rearm(port, k).status == 0
        } else {
            let r = sys::port_bind_flags(
                port,
                sock,
                signals::WRITABLE | signals::PEER_CLOSED,
                k,
                rt::abi::port_flags::ONESHOT,
            );
            st.wr_bound = r.status == 0;
            r.status == 0
        };
        st.wr_armed = ok;
    }

    fn arm_rd(&mut self, key: u64) {
        let port = self.core.port();
        let Some(Conn::Stream(st)) = self.core.conn_mut(key) else {
            return;
        };
        if st.rd_armed || !st.rd_bound {
            return;
        }
        let k = KEY_SOCK | (key << 1) | SOCK_RD;
        st.rd_armed = sys::port_rearm(port, k).status == 0;
    }

    /// One pass over the streams: at most P of them (round robin), moving bytes between the core and the client sockets.
    /// Returns whether anything moved.
    fn pump_streams(&mut self) -> bool {
        let ids: Vec<u64> = self.streams.values().copied().collect();
        if ids.is_empty() {
            return false;
        }
        let n = ids.len().min(P_STREAMS);
        let start = self.stream_rr % ids.len();
        self.stream_rr = self.stream_rr.wrapping_add(n);
        let mut moved = false;
        let now = sys::clock_get();
        for i in 0..n {
            moved |= self.pump_stream(ids[(start + i) % ids.len()], now);
        }
        moved
    }

    fn pump_stream(&mut self, key: u64, now: u64) -> bool {
        let (id, sock, retry_at, client_eof, sock_shut) = match self.core.conn(key) {
            Some(Conn::Stream(st)) if st.phase == Phase::Established => match (st.core, st.sock) {
                (Some(id), Some(sock)) => (id, sock, st.retry_at, st.client_eof, st.sock_shut),
                _ => return false,
            },
            _ => return false,
        };
        let mut moved = false;
        // the receive path: bytes the peer sent, from R into the client's socket
        if retry_at.is_none_or(|t| now >= t) {
            for _ in 0..2 {
                let mut buf = [0u8; PIECE];
                let n = self.net.tcp_peek(id, &mut buf);
                if n == 0 {
                    break;
                }
                let w = sys::socket_write(sock, buf.as_ptr(), n);
                match w.status() {
                    Some(Sys::Ok) => {
                        self.net.tcp_consume(id, w.a1);
                        moved = true;
                        if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                            st.retry_at = None;
                            st.retry_delay = 20;
                        }
                        if w.a1 < n {
                            self.arm_wr(key);
                            break;
                        }
                    }
                    Some(Sys::ShouldWait) => {
                        self.arm_wr(key);
                        break;
                    }
                    Some(Sys::NoMemory) => {
                        // the client's own quota is spent: WRITABLE stays set, so wait on a timer (20 ms doubling to 500 ms)
                        if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                            st.retry_at = Some(now + st.retry_delay);
                            st.retry_delay = (st.retry_delay * 2).min(500);
                        }
                        break;
                    }
                    _ => {
                        // the client closed its socket end: nobody will read: reset
                        self.net.tcp_abort(id);
                        moved = true;
                        break;
                    }
                }
            }
        }
        // the peer's FIN and every byte before it delivered: now the client may read end-of-file (PeerFin went first)
        if !sock_shut && self.net.tcp_eof(id) {
            sys::socket_shutdown(sock);
            if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                st.sock_shut = true;
            }
            moved = true;
        }
        // the send path: bytes the client wrote, from its socket into S
        if !client_eof {
            for _ in 0..2 {
                let space = self.net.tcp_send_space(id);
                if space == 0 {
                    break;
                }
                let want = space.min(PIECE);
                let mut buf = [0u8; PIECE];
                let r = sys::socket_read(sock, buf.as_mut_ptr(), want);
                match r.status() {
                    Some(Sys::Ok) if r.a1 > 0 => {
                        self.net.tcp_write(id, &buf[..r.a1]);
                        moved = true;
                        if r.a1 < want {
                            self.arm_rd(key);
                            break;
                        }
                    }
                    Some(Sys::ShouldWait) | Some(Sys::Ok) => {
                        self.arm_rd(key);
                        break;
                    }
                    Some(Sys::PeerClosed) => {
                        // the client shut its write side down (or closed): a FIN follows what was written
                        self.net.tcp_shutdown_write(id);
                        if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                            st.client_eof = true;
                        }
                        moved = true;
                        break;
                    }
                    _ => break,
                }
            }
        }
        moved
    }

    fn stream_retry_deadline(&self) -> Option<u64> {
        self.streams
            .values()
            .filter_map(|&k| match self.core.conn(k) {
                Some(Conn::Stream(st)) => st.retry_at,
                _ => None,
            })
            .min()
    }

    /// The configuration went away: every connection is reset and its client told why (reason 4).
    fn reset_all_streams(&mut self) {
        let ids: Vec<(ConnId, u64)> = self.streams.iter().map(|(&i, &k)| (i, k)).collect();
        for (id, key) in ids {
            if let Some(Conn::Stream(st)) = self.core.conn_mut(key) {
                st.lost = true;
            }
            self.net.tcp_abort(id);
        }
        self.more = true;
    }
}

impl Service for Netd {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        enum K {
            Listener(Class),
            Service(Class),
            Endpoint(EpId),
            Stream,
            TcpListener(ListenerId),
        }
        let kind = match self.core.conn(key) {
            Some(Conn::Listener(c)) => K::Listener(*c),
            Some(Conn::Service { class }) => K::Service(*class),
            Some(Conn::Endpoint { ep, .. }) => K::Endpoint(*ep),
            Some(Conn::Stream(_)) => K::Stream,
            Some(Conn::TcpListener { lid }) => K::TcpListener(*lid),
            None => return,
        };
        match kind {
            K::Listener(class) => {
                // a forwarded Open: the client's endpoint is the new service connection, in an account of its own
                let ok = proto::file::decode_request(msg)
                    .is_ok_and(|r| r.op == proto::file::op::OPEN && r.payload.is_empty())
                    && hs.len() == 1;
                if !ok {
                    for &h in hs {
                        sys::handle_close(h);
                    }
                    return;
                }
                let limits = Limits {
                    conns: pn::limits::CONNS_PER_ACCOUNT,
                    held_bytes: 64 * 1024,
                    inflight: 16,
                    stream_bytes: pn::limits::STREAM_BYTES_PER_ACCOUNT,
                };
                let Ok(account) = self.core.accounts().open(svc::ROOT, limits) else {
                    sys::handle_close(hs[0]);
                    return;
                };
                if self
                    .core
                    .add_conn(hs[0], account, Conn::Service { class })
                    .is_none()
                {
                    sys::handle_close(hs[0]);
                    let _ = self.core.accounts().close(account);
                }
            }
            K::Service(class) => self.open_conn(key, class, msg, hs),
            K::Endpoint(ep) => self.endpoint_request(key, ep, msg, hs),
            K::Stream => self.stream_request(key, msg, hs),
            K::TcpListener(lid) => self.listener_request(key, lid, msg, hs),
        }
    }

    fn closed(&mut self, conn: Conn) {
        match conn {
            Conn::Endpoint { ep, .. } => {
                self.net.close(ep);
                self.eps.remove(&ep);
            }
            Conn::Stream(st) => self.stream_closed(*st),
            Conn::TcpListener { lid } => self.listener_closed(lid),
            _ => {}
        }
    }

    fn other_event(&mut self, key: u64, _observed: u32) {
        match key {
            KEY_DOORBELL => {
                let _ = sys::event_signal(self.to_stack, 1 << 24 | 1 << 25, 0);
            }
            KEY_SUP => {
                // handled in `tick` (the channel handle is not kept here)
            }
            k if k & KEY_SOCK != 0 && k >> 63 == 0 => {
                // a one-shot binding on a stream's socket fired (and disarmed itself)
                let skey = (k & !KEY_SOCK) >> 1;
                if let Some(Conn::Stream(st)) = self.core.conn_mut(skey) {
                    if k & 1 == SOCK_RD {
                        st.rd_armed = false;
                    } else {
                        st.wr_armed = false;
                    }
                }
            }
            _ => {}
        }
        self.more = true;
    }

    fn next_deadline(&mut self) -> Option<u64> {
        let now = sys::clock_get();
        if self.more {
            return Some(now);
        }
        let timer = self.net.next_deadline().map(|ns| ns.div_ceil(1_000_000));
        let timer = match (timer, self.stream_retry_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Some(timer.map_or(now + POLL_TICKS, |t| t.min(now + POLL_TICKS)))
    }

    fn tick(&mut self) {
        self.pump();
    }
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (
        Some(ring),
        Some(rxp),
        Some(txp),
        Some(to_stack),
        Some(to_driver),
        Some(ctl_ch),
        Some(registry),
        Some(sup),
    ) = (
        env.handle(Role::NetRing),
        env.handle(Role::NetRxPool),
        env.handle(Role::NetTxPool),
        env.handle(Role::NetEventToStack),
        env.handle(Role::NetEventToDriver),
        env.handle(Role::NetCtl),
        env.handle(Role::ServiceRegistry),
        env.handle(Role::ServiceInit),
    )
    else {
        return 3;
    };
    let me = sys::process_self().a1 as u32;
    for (vmo, va, pages, perms) in [
        (ring, RING_VA, 2u64, PF_RW),
        (rxp, RX_VA, 64, PF_R),
        (txp, TX_VA, 64, PF_RW),
    ] {
        if sys::process_map(me, vmo, va, 0, pages * 4096, perms).status != 0 {
            return 4;
        }
    }
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let pool = Limits {
        conns: pn::limits::POOL_CONNS,
        held_bytes: 256 * 1024,
        inflight: 64,
        stream_bytes: pn::limits::POOL_STREAM_BYTES,
    };
    let mut d = Netd {
        core: Core::new(
            p.a1 as u32,
            pool,
            rt::abi::limits::MAX_MSG_BYTES,
            rt::abi::limits::MAX_MSG_HANDLES,
            30_000,
        ),
        net: Net::new([0; 6]),
        shm: Shm {
            base: RING_VA as usize,
        },
        to_driver,
        to_stack,
        ctl: Some(ctl_ch),
        link: Link::Detached,
        last_generation: 0,
        rx: Pool::rx_posted(),
        tx: Pool::tx_free(),
        tx_free: Vec::new(),
        eps: BTreeMap::new(),
        streams: BTreeMap::new(),
        listeners: BTreeMap::new(),
        accepts: BTreeMap::new(),
        next_accept: 1,
        held_socks: Vec::new(),
        charged: BTreeMap::new(),
        stream_rr: 0,
        more: true,
        blob: None,
        fault_no_doorbell: env.var(b"FAULT") == Some(&b"no-doorbell"[..]),
        fault_crash: env.var(b"FAULT") == Some(&b"crash-on-magic"[..]),
    };
    // a restart re-applies the interface configuration the previous incarnation left in the blob (no waiting for DHCP)
    if let Some(bv) = env.handle(Role::NetConfigBlob) {
        if sys::process_map(me, bv, BLOB_VA, 0, 4096, PF_RW).status == 0 {
            let b = netblob::Blob {
                base: BLOB_VA as usize,
            };
            if let Some(i) = b.read_iface() {
                d.net.configure(
                    Config {
                        addr: i.addr,
                        mask: i.mask,
                        gateway: i.gateway,
                        dns: i.dns,
                    },
                    now_ns(),
                );
            }
            d.blob = Some(b);
        }
    }
    let port = d.core.port();
    let _ = sys::port_bind(port, to_stack, 1 << 24, KEY_DOORBELL);
    let _ = sys::port_bind(
        port,
        ctl_ch,
        signals::READABLE | signals::PEER_CLOSED,
        KEY_DRIVER,
    );
    let _ = sys::port_bind(port, sup, signals::READABLE, KEY_SUP);
    if !d.publish_all(registry) {
        return 6;
    }
    rt::println!("netd: up");
    // the runtime loop, with the supervisor channel and the control channel polled each pass
    loop_with_sup(&mut d, sup)
}

/// `service::run` does not know the supervisor channel: wrap it by polling `sup` from `tick` via a thin adapter.
fn loop_with_sup(d: &mut Netd, sup: u32) -> i32 {
    struct Wrap<'a> {
        d: &'a mut Netd,
        sup: u32,
    }
    impl Service for Wrap<'_> {
        type Conn = Conn;
        fn core(&mut self) -> &mut Core<Conn> {
            self.d.core()
        }
        fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
            self.d.request(key, msg, hs)
        }
        fn closed(&mut self, conn: Conn) {
            self.d.closed(conn)
        }
        fn other_event(&mut self, key: u64, observed: u32) {
            self.d.other_event(key, observed)
        }
        fn next_deadline(&mut self) -> Option<u64> {
            self.d.next_deadline()
        }
        fn tick(&mut self) {
            self.d.poll_sup(self.sup);
            self.d.tick();
        }
    }
    service::run(&mut Wrap { d, sup })
}
