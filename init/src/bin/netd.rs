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

#[path = "../netshm.rs"]
mod netshm;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use netcore::{Config, Core as Net, Device, EpId, Event};
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
use svc::Limits;

const RING_VA: u64 = 0x510_0000;
const RX_VA: u64 = 0x520_0000;
const TX_VA: u64 = 0x530_0000;
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

/// What this stack's connections are.
enum Conn {
    /// A published name's listener: forwarded opens arrive here.
    Listener(Class),
    /// A client's service connection (its own account).
    Service { class: Class },
    /// An endpoint channel: a UDP or echo endpoint of the stack, owned by this connection.
    Endpoint { ep: EpId, dropped: u32 },
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
    more: bool,
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
                            // a generation that is not newer, or a second READY: a violation by the driver
                            self.shm.count(stat::RING_VIOLATIONS);
                            self.detach();
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
                    _ => {
                        self.shm.count(stat::RING_VIOLATIONS);
                        self.detach();
                    }
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
        if published {
            moved = true;
        }
        if (published && must_signal(&self.shm, ctl::DRV_WAITING, was_empty))
            || (returned && must_signal(&self.shm, ctl::DRV_WAITING, rx_free_was_empty))
        {
            let _ = sys::event_signal(self.to_driver, 0, 1 << 24);
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
                if hs.len() != 1
                    || oflags & !pn::oflags::ALL != 0
                    || (kind != pn::kind::UDP && kind != pn::kind::ICMP_ECHO)
                {
                    close_all(hs);
                    return self.reply(key, txid, pn::status::BAD_REQUEST, &[]);
                }
                let privileged = class == Class::Privileged;
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
                    self.net
                        .open_udp(local_port, addr, oflags != 0)
                        .map(|(e, p)| (e, p))
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
                self.reply(key, txid, pn::status::OK, &[]);
                self.more = true;
            }
            Request::ClearConfig => {
                if class != Class::Config {
                    return self.reply(key, txid, pn::status::REFUSED, &[]);
                }
                self.net.clear_config();
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
            } => (
                cookie,
                self.net.send_udp(ep, dst, port, payload, cookie, now),
            ),
            EpRequest::SendEcho {
                cookie,
                dst,
                seq,
                payload,
            } => (
                cookie,
                self.net.send_echo(ep, dst, seq, payload, cookie, now),
            ),
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
        core::mem::forget(fs);
        true
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
        }
        let kind = match self.core.conn(key) {
            Some(Conn::Listener(c)) => K::Listener(*c),
            Some(Conn::Service { class }) => K::Service(*class),
            Some(Conn::Endpoint { ep, .. }) => K::Endpoint(*ep),
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
        }
    }

    fn closed(&mut self, conn: Conn) {
        if let Conn::Endpoint { ep, .. } = conn {
            self.net.close(ep);
            self.eps.remove(&ep);
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
        more: true,
    };
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
    let code = loop_with_sup(&mut d, sup);
    code
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
