//! TCP inside the core (S9-SPEC 5.9, 5.11, 5.13, 7.1): the connection table, listeners with posted accepts, TIME-WAIT
//! records, orphans, the loopback queue, initial sequence numbers and ephemeral ports. Each connection is a `nettcp::Conn`;
//! this module is what dispatches segments to it, wraps its segments in IP and gives the application side (`netd`) a small
//! API: bytes in and out, time as an argument. It makes no kernel call and knows nothing about sockets.
use super::*;
use alloc::collections::BTreeMap;
use nettcp::conn::{Conn, Event as CEvent, RateLimit, Reason, State, Tuple};
use nettcp::siphash;

pub use nettcp::conn::Config as TcpConfig;

pub type ConnId = u32;
pub type ListenerId = u32;

/// Posted accepts one listener may hold (the largest backlog).
pub const MAX_POSTED_ACCEPTS: usize = 16;
/// TIME-WAIT records kept; the oldest is evicted when full.
pub const TIME_WAIT_RECORDS: usize = 64;
/// Connections whose owner is gone but that are still draining.
pub const ORPHAN_MAX: usize = 32;
pub const ORPHAN_LIFE: Instant = 20 * SEC;
pub const LOOPBACK_QUEUE: usize = 32;
/// Segments one connection may emit per `poll`, so a bulk sender cannot take every transmit slot.
pub const SEGMENTS_PER_CONN: usize = 8;
/// Connections of all kinds in the table (accounts bound it per client; this is the core's own ceiling).
pub const MAX_CONNS: usize = 512;
const AMOUNT_RST_PER_SEC: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectError {
    NotConfigured,
    NoRoute,
    NoPorts,
    AddrInUse,
    TooMany,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListenError {
    AddrInUse,
    NotConfigured,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcceptError {
    NoSuchListener,
    TooMany,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetReason {
    PeerReset,
    TimedOut,
    Aborted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailReason {
    Refused,
    TimedOut,
    Unreachable,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TcpEvent {
    /// An active open completed.
    Connected {
        conn: ConnId,
        local: (Ip, u16),
        remote: (Ip, u16),
    },
    /// A posted accept became an established connection.
    Accepted {
        listener: ListenerId,
        cookie: u32,
        conn: ConnId,
        local: (Ip, u16),
        remote: (Ip, u16),
    },
    /// An active open failed (nothing was ever established).
    ConnectFailed { conn: ConnId, reason: FailReason },
    /// The peer's FIN arrived in order: every byte before it is in the receive buffer.
    PeerFin { conn: ConnId },
    /// An established connection ended abnormally.
    Reset { conn: ConnId, reason: ResetReason },
    /// A clean end (or TIME-WAIT began): the connection is gone from the table.
    Closed { conn: ConnId },
    /// SYNs are being dropped for want of posted accepts (at most one a second per listener).
    AcceptStarved { listener: ListenerId },
    /// A SYN took a posted accept: the connection exists (half-open) and its buffers are committed now.
    AcceptTaken {
        listener: ListenerId,
        cookie: u32,
        conn: ConnId,
        bytes: usize,
    },
    /// The connection left the table, whatever the reason and whether or not anybody was told (an orphan's end, a failed
    /// open, a TIME-WAIT conversion): exactly once per connection, last. The adapter releases what it charged for it.
    Released { conn: ConnId },
}

struct Entry {
    conn: Conn,
    /// The listener and cookie of the posted accept this connection took (a passive open).
    from: Option<(ListenerId, u32)>,
    established: bool,
    /// The owner's channel closed; the connection drains until this long after `Some(t)`.
    orphan_since: Option<Instant>,
    soft_unreach: bool,
    /// Both FINs are done but the owner has not read everything yet: the entry stays until it has.
    closed_pending: bool,
    /// The buffers of the posted accept this passive connection took, to put the accept back if it dies half-open.
    accept_cfg: Option<TcpConfig>,
}

struct Listener {
    addr: Option<Ip>,
    port: u16,
    /// `(cookie, buffers, admitted)`: an accept the adapter has not admitted (its owner has no room for the buffers) is not
    /// taken by a SYN. `tcp_post_accept` admits by default.
    posted: VecDeque<(u32, TcpConfig, bool)>,
    last_starved: Option<Instant>,
}

struct TimeWaitRec {
    tuple: Tuple,
    snd_nxt: u32,
    rcv_nxt: u32,
    until: Instant,
}

pub struct Tcp {
    conns: BTreeMap<ConnId, Entry>,
    by_tuple: BTreeMap<(Ip, u16, Ip, u16), ConnId>,
    listeners: BTreeMap<ListenerId, Listener>,
    time_wait: VecDeque<TimeWaitRec>,
    secret: [u8; 16],
    next_id: u32,
    rst_limiter: RateLimit,
    challenge_limiter: RateLimit,
    events: Vec<TcpEvent>,
    /// Packets to ourselves: `(src, dst, tcp bytes)`.
    loopback: VecDeque<(Ip, Ip, Vec<u8>)>,
    /// UDP datagrams to ourselves: `(src, dst, udp bytes)`, delivered on the next poll, never through the device.
    pub(crate) udp_loop: VecDeque<(Ip, Ip, Vec<u8>)>,
    port_counter: u32,
    rr: usize,
}

impl Tcp {
    pub fn new() -> Tcp {
        Tcp {
            conns: BTreeMap::new(),
            by_tuple: BTreeMap::new(),
            listeners: BTreeMap::new(),
            time_wait: VecDeque::new(),
            secret: [0; 16],
            next_id: 1,
            rst_limiter: RateLimit::new(AMOUNT_RST_PER_SEC),
            challenge_limiter: RateLimit::new(AMOUNT_RST_PER_SEC),
            events: Vec::new(),
            loopback: VecDeque::new(),
            udp_loop: VecDeque::new(),
            port_counter: 0,
            rr: 0,
        }
    }
}

impl Default for Tcp {
    fn default() -> Tcp {
        Tcp::new()
    }
}

fn is_loopback(a: Ip) -> bool {
    a[0] == 127
}

/// What the application learns about a connection.
#[derive(Clone, Copy, Debug)]
pub struct ConnInfo {
    pub state: State,
    pub tuple: Tuple,
    pub cwnd: u32,
    pub ssthresh: u32,
    pub rto_ns: u64,
    pub unacked: usize,
    pub readable: usize,
}

impl Core {
    // ------------------------------------------------------------------ setup and queries

    /// The 16-byte boot secret that keys initial sequence numbers and ephemeral ports (S9-SPEC 5.10).
    pub fn set_tcp_secret(&mut self, secret: [u8; 16]) {
        self.tcp.secret = secret;
    }

    pub fn tcp_conn_count(&self) -> usize {
        self.tcp.conns.len()
    }
    pub fn tcp_listener_count(&self) -> usize {
        self.tcp.listeners.len()
    }
    pub fn tcp_time_wait_count(&self) -> usize {
        self.tcp.time_wait.len()
    }
    pub fn tcp_orphan_count(&self) -> usize {
        self.tcp
            .conns
            .values()
            .filter(|e| e.orphan_since.is_some())
            .count()
    }
    pub fn tcp_posted_accepts(&self, l: ListenerId) -> usize {
        self.tcp.listeners.get(&l).map_or(0, |l| l.posted.len())
    }

    /// Everything that happened to connections since the last call.
    pub fn take_tcp_events(&mut self) -> Vec<TcpEvent> {
        core::mem::take(&mut self.tcp.events)
    }

    pub fn tcp_info(&self, id: ConnId) -> Option<ConnInfo> {
        let e = self.tcp.conns.get(&id)?;
        Some(ConnInfo {
            state: e.conn.state(),
            tuple: e.conn.tuple,
            cwnd: e.conn.cwnd(),
            ssthresh: e.conn.ssthresh(),
            rto_ns: e.conn.rto().rto(),
            unacked: e.conn.unacked(),
            readable: e.conn.readable(),
        })
    }

    fn tcp_port_in_use(&self, local: Ip, port: u16, remote: Ip, rport: u16) -> bool {
        self.tcp
            .by_tuple
            .contains_key(&(local, port, remote, rport))
            || self.tcp.listeners.values().any(|l| l.port == port)
            || self.tcp.time_wait.iter().any(|t| {
                t.tuple.lport == port && t.tuple.remote == remote && t.tuple.rport == rport
            })
    }

    // ------------------------------------------------------------------ the application side

    /// Open a listener on `port` (and `addr`, or any address). A port already listened on is `AddrInUse`.
    pub fn tcp_listen(&mut self, addr: Option<Ip>, port: u16) -> Result<ListenerId, ListenError> {
        if self.tcp.listeners.values().any(|l| l.port == port) {
            return Err(ListenError::AddrInUse);
        }
        let id = self.tcp.next_id;
        self.tcp.next_id += 1;
        self.tcp.listeners.insert(
            id,
            Listener {
                addr,
                port,
                posted: VecDeque::new(),
                last_starved: None,
            },
        );
        Ok(id)
    }

    /// Post one accept: the next SYN for this listener takes it. A listener holds at most 16.
    pub fn tcp_post_accept(
        &mut self,
        l: ListenerId,
        cookie: u32,
        cfg: TcpConfig,
    ) -> Result<(), AcceptError> {
        let l = self
            .tcp
            .listeners
            .get_mut(&l)
            .ok_or(AcceptError::NoSuchListener)?;
        if l.posted.len() >= MAX_POSTED_ACCEPTS {
            return Err(AcceptError::TooMany);
        }
        l.posted.push_back((cookie, cfg, true));
        Ok(())
    }

    /// Close a listener: posted accepts vanish, half-open connections it took are dropped silently, established ones stay.
    pub fn tcp_close_listener(&mut self, l: ListenerId) {
        if self.tcp.listeners.remove(&l).is_none() {
            return;
        }
        let drop: Vec<ConnId> = self
            .tcp
            .conns
            .iter()
            .filter(|(_, e)| e.from.is_some_and(|(x, _)| x == l) && !e.established)
            .map(|(&id, _)| id)
            .collect();
        for id in drop {
            self.tcp_remove(id);
        }
    }

    /// An active open to `remote:rport`. The local port comes from the keyed hash of RFC 6056 unless `local_port` is given.
    pub fn tcp_connect(
        &mut self,
        remote: Ip,
        rport: u16,
        local_port: Option<u16>,
        cfg: TcpConfig,
        now: Instant,
    ) -> Result<ConnId, ConnectError> {
        let net = self.cfg.ok_or(ConnectError::NotConfigured)?;
        if self.tcp.conns.len() >= MAX_CONNS {
            return Err(ConnectError::TooMany);
        }
        let local = if is_loopback(remote) || remote == net.addr {
            remote
        } else if net.on_link(remote) || net.gateway != [0; 4] {
            net.addr
        } else {
            return Err(ConnectError::NoRoute);
        };
        let lport = match local_port {
            Some(p) => {
                if self.tcp_port_in_use(local, p, remote, rport) {
                    return Err(ConnectError::AddrInUse);
                }
                p
            }
            None => {
                self.tcp.port_counter = self.tcp.port_counter.wrapping_add(1);
                let counter = self.tcp.port_counter;
                let (lo, hi) = EPHEMERAL;
                let key = self.tcp.secret;
                siphash::ephemeral_port(&key, local, remote, rport, lo, hi, counter, |p| {
                    self.tcp_port_in_use(local, p, remote, rport)
                })
                .ok_or(ConnectError::NoPorts)?
            }
        };
        let tuple = Tuple {
            local,
            lport,
            remote,
            rport,
        };
        let iss = siphash::isn(&self.tcp.secret, local, lport, remote, rport, now);
        let id = self.tcp.next_id;
        self.tcp.next_id += 1;
        self.tcp.by_tuple.insert((local, lport, remote, rport), id);
        self.tcp.conns.insert(
            id,
            Entry {
                conn: Conn::connect(tuple, iss, cfg, now),
                from: None,
                established: false,
                orphan_since: None,
                soft_unreach: false,
                closed_pending: false,
                accept_cfg: None,
            },
        );
        Ok(id)
    }

    /// Bytes the connection can take now.
    pub fn tcp_send_space(&self, id: ConnId) -> usize {
        self.tcp.conns.get(&id).map_or(0, |e| e.conn.send_space())
    }
    pub fn tcp_write(&mut self, id: ConnId, data: &[u8]) -> usize {
        self.tcp
            .conns
            .get_mut(&id)
            .map_or(0, |e| e.conn.write(data))
    }
    pub fn tcp_readable(&self, id: ConnId) -> usize {
        self.tcp.conns.get(&id).map_or(0, |e| e.conn.readable())
    }
    /// The bytes `tcp_read` would return, without taking them (the adapter writes them into a socket that may take less).
    pub fn tcp_peek(&self, id: ConnId, out: &mut [u8]) -> usize {
        self.tcp.conns.get(&id).map_or(0, |e| e.conn.peek(out))
    }
    pub fn tcp_consume(&mut self, id: ConnId, n: usize) {
        if let Some(e) = self.tcp.conns.get_mut(&id) {
            e.conn.consume(n);
        }
    }
    pub fn tcp_read(&mut self, id: ConnId, out: &mut [u8]) -> usize {
        self.tcp.conns.get_mut(&id).map_or(0, |e| e.conn.read(out))
    }
    /// The peer's FIN has arrived and everything before it has been read.
    pub fn tcp_eof(&self, id: ConnId) -> bool {
        self.tcp.conns.get(&id).is_some_and(|e| e.conn.eof())
    }
    pub fn tcp_shutdown_write(&mut self, id: ConnId) {
        if let Some(e) = self.tcp.conns.get_mut(&id) {
            e.conn.shutdown_write();
        }
    }
    pub fn tcp_set_nodelay(&mut self, id: ConnId, on: bool) {
        if let Some(e) = self.tcp.conns.get_mut(&id) {
            e.conn.set_nodelay(on);
        }
    }

    /// Admit or hold back a posted accept: a SYN takes only admitted ones (memory is committed when a SYN takes the accept, as
    /// in every stack, so the adapter says whether the owner could pay for it now).
    pub fn tcp_set_accept_ok(&mut self, l: ListenerId, cookie: u32, ok: bool) {
        if let Some(l) = self.tcp.listeners.get_mut(&l) {
            if let Some(p) = l.posted.iter_mut().find(|p| p.0 == cookie) {
                p.2 = ok;
            }
        }
    }

    /// Withdraw a posted accept that no SYN has taken yet (its owner closed it). Returns whether it was still posted.
    pub fn tcp_cancel_accept(&mut self, lid: ListenerId, cookie: u32) -> bool {
        let Some(l) = self.tcp.listeners.get_mut(&lid) else {
            return false;
        };
        let before = l.posted.len();
        l.posted.retain(|(c, _, _)| *c != cookie);
        let was_posted = l.posted.len() != before;
        // a SYN may already have taken it: drop that half-open connection too, without putting the accept back
        let taken: Vec<ConnId> = self
            .tcp
            .conns
            .iter()
            .filter(|(_, e)| !e.established && e.from == Some((lid, cookie)))
            .map(|(&i, _)| i)
            .collect();
        for id in &taken {
            if let Some(e) = self.tcp.conns.get_mut(id) {
                e.from = None;
            }
            self.tcp_remove(*id);
        }
        was_posted || !taken.is_empty()
    }

    /// Reset the connection now (the client's abort, or `SO_LINGER` zero).
    pub fn tcp_abort(&mut self, id: ConnId) {
        if let Some(e) = self.tcp.conns.get_mut(&id) {
            e.conn.abort();
        }
    }

    /// The client's channel is gone. Unread data resets the connection; otherwise the connection finishes what it was sending
    /// as an orphan (at most 20 s, at most 32 of them: the oldest is reset when a new one would be the 33rd).
    pub fn tcp_release(&mut self, id: ConnId, now: Instant) {
        let Some(e) = self.tcp.conns.get_mut(&id) else {
            return;
        };
        let draining = e.established
            && e.conn.readable() == 0
            && (e.conn.unacked() > 0
                || matches!(e.conn.state(), State::Established | State::CloseWait));
        if !draining {
            e.conn.abort();
            return;
        }
        e.conn.shutdown_write();
        e.orphan_since = Some(now);
        // too many orphans: reset the oldest
        let mut orphans: Vec<(Instant, ConnId)> = self
            .tcp
            .conns
            .iter()
            .filter_map(|(&i, e)| e.orphan_since.map(|t| (t, i)))
            .collect();
        if orphans.len() > ORPHAN_MAX {
            orphans.sort_unstable();
            if let Some(e) = self.tcp.conns.get_mut(&orphans[0].1) {
                e.conn.abort();
                self.counters.tcp_orphan_reset += 1;
            }
        }
    }

    fn tcp_remove(&mut self, id: ConnId) {
        if let Some(e) = self.tcp.conns.remove(&id) {
            // a passive connection that never became established (a RST, or the SYN-ACK timing out) frees its accept: put
            // it back, so a remote host cannot use up a listener's posted accepts with half-open connections (S9-SPEC 5.9)
            if !e.established {
                if let (Some((lid, cookie)), Some(cfg)) = (e.from, e.accept_cfg) {
                    if let Some(l) = self.tcp.listeners.get_mut(&lid) {
                        l.posted.push_front((cookie, cfg, true));
                    }
                }
            }
            let t = e.conn.tuple;
            self.tcp
                .by_tuple
                .remove(&(t.local, t.lport, t.remote, t.rport));
            self.tcp.events.push(TcpEvent::Released { conn: id });
        }
    }

    // ------------------------------------------------------------------ segments arriving

    /// A TCP segment addressed to us (the IP checks have passed).
    pub(crate) fn rx_tcp_segment(&mut self, h: &ipv4::Header, payload: &[u8], now: Instant) {
        let cfg = self.cfg;
        let for_us = cfg.is_some_and(|c| h.dst == c.addr) || is_loopback(h.dst);
        if !for_us {
            // broadcast and subnet-broadcast TCP is never answered (RFC 1122 4.2.3.10)
            self.counters.tcp_bad_header += 1;
            return;
        }
        match netwire::tcp::parse(payload, h.src, h.dst) {
            Ok(seg) => self.tcp_input(h.src, h.dst, &seg, now),
            Err(netwire::WireError::BadChecksum) => self.counters.tcp_checksum += 1,
            Err(_) => self.counters.tcp_bad_header += 1,
        }
    }

    fn tcp_input(&mut self, src: Ip, dst: Ip, seg: &netwire::tcp::Tcp<'_>, now: Instant) {
        use netwire::tcp::{ACK, RST, SYN};
        let key = (dst, seg.dst_port, src, seg.src_port);
        if let Some(&id) = self.tcp.by_tuple.get(&key) {
            if let Some(e) = self.tcp.conns.get_mut(&id) {
                e.conn.on_segment(seg, now, &mut self.tcp.challenge_limiter);
            }
            self.tcp_collect(id, now);
            return;
        }
        // a connection in TIME-WAIT
        if let Some(pos) = self.tcp.time_wait.iter().position(|t| {
            t.tuple.local == dst
                && t.tuple.lport == seg.dst_port
                && t.tuple.remote == src
                && t.tuple.rport == seg.src_port
        }) {
            let rec_rcv = self.tcp.time_wait[pos].rcv_nxt;
            let reuse = seg.has(SYN) && !seg.has(ACK) && nettcp::seq::gt(seg.seq, rec_rcv);
            if reuse {
                self.tcp.time_wait.remove(pos);
            } else {
                if seg.has(RST) {
                    self.tcp.time_wait.remove(pos);
                } else {
                    let (t, snd, rcv) = {
                        let r = &mut self.tcp.time_wait[pos];
                        r.until = now + nettcp::conn::TIME_WAIT;
                        (r.tuple, r.snd_nxt, r.rcv_nxt)
                    };
                    self.tcp_send_raw(t.local, t.remote, t.lport, t.rport, snd, rcv, ACK, now);
                }
                return;
            }
        }
        // a SYN for a listener
        if seg.has(SYN) && !seg.has(ACK) && !seg.has(RST) {
            let lid = self
                .tcp
                .listeners
                .iter()
                .find(|(_, l)| l.port == seg.dst_port && l.addr.is_none_or(|a| a == dst))
                .map(|(&id, _)| id);
            if let Some(lid) = lid {
                let taken = self.tcp.listeners.get_mut(&lid).and_then(|l| {
                    let i = l.posted.iter().position(|p| p.2)?;
                    l.posted.remove(i).map(|(c, cfg, _)| (c, cfg))
                });
                match taken {
                    None => {
                        self.counters.tcp_listen_overflow += 1;
                        let l = self.tcp.listeners.get_mut(&lid).unwrap();
                        if l.last_starved.is_none_or(|t| now.saturating_sub(t) >= SEC) {
                            l.last_starved = Some(now);
                            self.tcp
                                .events
                                .push(TcpEvent::AcceptStarved { listener: lid });
                        }
                    }
                    Some((cookie, cfg)) => {
                        if self.tcp.conns.len() >= MAX_CONNS {
                            if let Some(l) = self.tcp.listeners.get_mut(&lid) {
                                l.posted.push_front((cookie, cfg, true));
                            }
                            self.counters.tcp_listen_overflow += 1;
                            return;
                        }
                        let tuple = Tuple {
                            local: dst,
                            lport: seg.dst_port,
                            remote: src,
                            rport: seg.src_port,
                        };
                        let iss = siphash::isn(
                            &self.tcp.secret,
                            dst,
                            seg.dst_port,
                            src,
                            seg.src_port,
                            now,
                        );
                        let id = self.tcp.next_id;
                        self.tcp.next_id += 1;
                        self.tcp.by_tuple.insert(key, id);
                        self.tcp.conns.insert(
                            id,
                            Entry {
                                conn: Conn::accept(tuple, seg, iss, cfg, now),
                                from: Some((lid, cookie)),
                                established: false,
                                orphan_since: None,
                                soft_unreach: false,
                                closed_pending: false,
                                accept_cfg: Some(cfg),
                            },
                        );
                        self.tcp.events.push(TcpEvent::AcceptTaken {
                            listener: lid,
                            cookie,
                            conn: id,
                            bytes: cfg.snd_buf + cfg.rcv_buf,
                        });
                    }
                }
                return;
            }
        }
        // no connection: the reset of RFC 9293 3.10.7.1, limited
        self.counters.tcp_no_connection += 1;
        if let Some((seq, ack, has_ack)) = nettcp::conn::reset_for(seg) {
            if self.tcp.rst_limiter.allow(now) {
                let flags = RST | if has_ack { ACK } else { 0 };
                self.counters.tcp_rst_sent += 1;
                self.tcp_send_raw(dst, src, seg.dst_port, seg.src_port, seq, ack, flags, now);
            } else {
                self.counters.tcp_rst_rate_limited += 1;
            }
        }
    }

    /// The soft error of RFC 5927's advice: an unreachable for a connection still in SYN-SENT is remembered and reported if
    /// the connect then times out. Never a reset.
    pub(crate) fn tcp_soft_error(&mut self, src_port: u16, dst: Ip, dst_port: u16) {
        let Some(c) = self.cfg else { return };
        for local in [c.addr, dst] {
            if let Some(&id) = self.tcp.by_tuple.get(&(local, src_port, dst, dst_port)) {
                if let Some(e) = self.tcp.conns.get_mut(&id) {
                    if e.conn.state() == State::SynSent {
                        e.soft_unreach = true;
                    }
                }
                return;
            }
        }
    }

    // ------------------------------------------------------------------ segments leaving

    #[allow(clippy::too_many_arguments)]
    fn tcp_send_raw(
        &mut self,
        src: Ip,
        dst: Ip,
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        now: Instant,
    ) {
        let h = netwire::tcp::Header {
            src_port: sport,
            dst_port: dport,
            seq,
            ack,
            flags,
            window: 0,
        };
        let mut b = [0u8; netwire::tcp::HEADER];
        if let Ok(n) = netwire::tcp::write(&mut b, src, dst, &h, &[], &[]) {
            self.tcp_emit(src, dst, b[..n].to_vec(), now);
        }
    }

    /// Wrap TCP bytes in IP and route them: to the loopback queue, or toward the next hop.
    fn tcp_emit(&mut self, src: Ip, dst: Ip, bytes: Vec<u8>, now: Instant) {
        let Some(c) = self.cfg else { return };
        if is_loopback(dst) || dst == c.addr {
            if self.tcp.loopback.len() >= LOOPBACK_QUEUE {
                self.counters.tcp_loopback_dropped += 1;
            } else {
                self.tcp.loopback.push_back((src, dst, bytes));
            }
            return;
        }
        let next_hop = if c.on_link(dst) {
            dst
        } else if c.gateway != [0; 4] {
            c.gateway
        } else {
            self.counters.tcp_tx_no_route += 1;
            return;
        };
        let packet = self.ip_packet(src, dst, ipv4::PROTO_TCP, 64, &bytes);
        self.route(next_hop, packet, None, now);
    }

    /// Translate what a connection reported into events for the application, and free it if it is over.
    fn tcp_collect(&mut self, id: ConnId, now: Instant) {
        if let Some(e) = self.tcp.conns.get(&id) {
            if e.closed_pending && e.conn.readable() == 0 {
                self.tcp.events.push(TcpEvent::Closed { conn: id });
                self.tcp_remove(id);
                return;
            }
        }
        let Some(e) = self.tcp.conns.get_mut(&id) else {
            return;
        };
        let evs = e.conn.take_events();
        let tuple = e.conn.tuple;
        let mut gone = false;
        for ev in evs {
            let Some(e) = self.tcp.conns.get_mut(&id) else {
                return;
            };
            match ev {
                CEvent::Connected => {
                    e.established = true;
                    let (local, remote) = ((tuple.local, tuple.lport), (tuple.remote, tuple.rport));
                    match e.from {
                        Some((listener, cookie)) => self.tcp.events.push(TcpEvent::Accepted {
                            listener,
                            cookie,
                            conn: id,
                            local,
                            remote,
                        }),
                        None => self.tcp.events.push(TcpEvent::Connected {
                            conn: id,
                            local,
                            remote,
                        }),
                    }
                }
                CEvent::PeerFin => {
                    if e.orphan_since.is_none() {
                        self.tcp.events.push(TcpEvent::PeerFin { conn: id });
                    }
                }
                CEvent::Reset(why) => {
                    let ev = if !e.established {
                        if e.from.is_some() {
                            None // a half-open passive connection timing out: the listener never knew it
                        } else {
                            Some(TcpEvent::ConnectFailed {
                                conn: id,
                                reason: match why {
                                    Reason::Refused | Reason::PeerReset => FailReason::Refused,
                                    Reason::TimedOut if e.soft_unreach => FailReason::Unreachable,
                                    Reason::TimedOut => FailReason::TimedOut,
                                    Reason::Aborted => FailReason::Refused,
                                },
                            })
                        }
                    } else if e.orphan_since.is_some() {
                        None
                    } else {
                        Some(TcpEvent::Reset {
                            conn: id,
                            reason: match why {
                                Reason::PeerReset | Reason::Refused => ResetReason::PeerReset,
                                Reason::TimedOut => ResetReason::TimedOut,
                                Reason::Aborted => ResetReason::Aborted,
                            },
                        })
                    };
                    self.tcp.events.extend(ev);
                    gone = true;
                }
                CEvent::Closed => {
                    if e.orphan_since.is_none() && e.conn.readable() > 0 {
                        // the owner has not read everything yet: keep the entry until it has
                        e.closed_pending = true;
                    } else {
                        if e.orphan_since.is_none() {
                            self.tcp.events.push(TcpEvent::Closed { conn: id });
                        }
                        gone = true;
                    }
                }
            }
        }
        if gone {
            self.tcp_remove(id);
            return;
        }
        // TIME-WAIT keeps no buffers and no owner: a small record replaces the connection
        if let Some(e) = self.tcp.conns.get(&id) {
            if e.conn.state() == State::TimeWait
                && (e.orphan_since.is_some() || e.conn.readable() == 0)
            {
                let rec = TimeWaitRec {
                    tuple: e.conn.tuple,
                    snd_nxt: e.conn.snd_nxt(),
                    rcv_nxt: e.conn.rcv_nxt(),
                    until: now + nettcp::conn::TIME_WAIT,
                };
                let announce = e.orphan_since.is_none();
                // the ACK of the peer's FIN is still owed: send it now, the record answers any later retransmission
                let t = rec.tuple;
                self.tcp_send_raw(
                    t.local,
                    t.remote,
                    t.lport,
                    t.rport,
                    rec.snd_nxt,
                    rec.rcv_nxt,
                    netwire::tcp::ACK,
                    now,
                );
                if announce {
                    self.tcp.events.push(TcpEvent::Closed { conn: id });
                }
                // the port stays reserved through the record; the connection's own entry goes
                self.tcp_remove(id);
                if self.tcp.time_wait.len() >= TIME_WAIT_RECORDS {
                    self.tcp.time_wait.pop_front();
                }
                self.tcp.time_wait.push_back(rec);
            } else if e.conn.state() == State::Closed && !e.closed_pending {
                self.tcp_remove(id);
            }
        }
    }

    /// Timers, loopback delivery and segment output. Called from the core's `poll`.
    pub(crate) fn tcp_poll(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        // datagrams we sent to ourselves
        let n = self.tcp.udp_loop.len();
        for _ in 0..n {
            let Some((src, dst, bytes)) = self.tcp.udp_loop.pop_front() else {
                break;
            };
            let h = ipv4::Header {
                tos: 0,
                id: 0,
                flags_frag: 0,
                ttl: 64,
                proto: ipv4::PROTO_UDP,
                src,
                dst,
            };
            let mut whole = vec![0u8; ipv4::HEADER + bytes.len()];
            if ipv4::write_header(&mut whole, &h, bytes.len()).is_ok() {
                whole[ipv4::HEADER..].copy_from_slice(&bytes);
                events.extend(self.rx_udp(self.mac, &h, &whole, ipv4::HEADER, now));
            }
        }
        // loopback packets queued before this call: deliver (anything they cause is queued for the next call)
        let n = self.tcp.loopback.len();
        for _ in 0..n {
            let Some((src, dst, bytes)) = self.tcp.loopback.pop_front() else {
                break;
            };
            match netwire::tcp::parse(&bytes, src, dst) {
                Ok(seg) => self.tcp_input(src, dst, &seg, now),
                Err(netwire::WireError::BadChecksum) => self.counters.tcp_checksum += 1,
                Err(_) => self.counters.tcp_bad_header += 1,
            }
        }
        // TIME-WAIT expiry
        self.tcp.time_wait.retain(|t| t.until > now);
        // orphans out of time or holding data nobody will read
        let stale: Vec<ConnId> = self
            .tcp
            .conns
            .iter()
            .filter(|(_, e)| {
                e.orphan_since
                    .is_some_and(|t| now.saturating_sub(t) >= ORPHAN_LIFE)
                    || (e.orphan_since.is_some() && e.conn.readable() > 0)
            })
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            if let Some(e) = self.tcp.conns.get_mut(&id) {
                e.conn.abort();
                self.counters.tcp_orphan_reset += 1;
            }
        }
        // each connection: its timers, then up to 8 segments while the transmit queue has room
        let ids: Vec<ConnId> = self.tcp.conns.keys().copied().collect();
        let len = ids.len();
        if len == 0 {
            return events;
        }
        let start = self.tcp.rr % len;
        self.tcp.rr = self.tcp.rr.wrapping_add(1);
        for k in 0..len {
            let id = ids[(start + k) % len];
            if let Some(e) = self.tcp.conns.get_mut(&id) {
                e.conn.on_timer(now);
            }
            for _ in 0..SEGMENTS_PER_CONN {
                if self.outq.len() >= TX_QUEUE_MAX || self.tcp.loopback.len() >= LOOPBACK_QUEUE {
                    self.counters.tx_queue_full += 1;
                    break;
                }
                let Some(e) = self.tcp.conns.get_mut(&id) else {
                    break;
                };
                let mut buf = [0u8; 1500];
                let Some(n) = e.conn.poll(now, &mut buf) else {
                    break;
                };
                let t = e.conn.tuple;
                self.tcp_emit(t.local, t.remote, buf[..n].to_vec(), now);
            }
            self.tcp_collect(id, now);
        }
        events
    }

    /// The earliest TCP deadline (connection timers, TIME-WAIT, orphans), or `now` when loopback packets wait.
    pub(crate) fn tcp_next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut take = |t: Instant| d = Some(d.map_or(t, |x| x.min(t)));
        if !self.tcp.loopback.is_empty() || !self.tcp.udp_loop.is_empty() {
            take(0);
        }
        for e in self.tcp.conns.values() {
            if let Some(t) = e.conn.next_deadline() {
                take(t);
            }
            if let Some(t) = e.orphan_since {
                take(t + ORPHAN_LIFE);
            }
        }
        for t in &self.tcp.time_wait {
            take(t.until);
        }
        d
    }
}
