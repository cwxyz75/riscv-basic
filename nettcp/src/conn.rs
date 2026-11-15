//! One TCP connection (S9-SPEC 5.2 to 5.8): RFC 9293's eleven states, driven by segments (`on_segment`), the application
//! (`write`, `read`, `close`, `abort`) and time (`on_timer`), producing segments on request (`poll`). Sans-IO: it reads no
//! clock and sends nothing by itself.
//!
//! Congestion control is RFC 5681 with NewReno recovery (RFC 6582), counted in bytes (5.7). Window scaling, SACK and
//! timestamps are not negotiated (5.3).
use crate::reasm::Reasm;
use crate::rto::{Rto, MS};
use crate::sendbuf::SendBuf;
use crate::seq;
use alloc::vec::Vec;
use netwire::tcp::{self, Header, Tcp};
use netwire::Ip;

/// Nanoseconds on the caller's monotonic clock.
pub type Instant = u64;

pub const MSS: u16 = 1460;
pub const DEFAULT_PEER_MSS: u16 = 536;
pub const DELAYED_ACK: u64 = 40 * MS;
pub const QUICK_ACKS: u32 = 16;
pub const TIME_WAIT: u64 = 10_000 * MS;
pub const GIVE_UP: u64 = 100_000 * MS;
pub const MAX_PERSIST: u64 = 60_000 * MS;
/// SYN retransmissions before the connect fails: waits of 1, 2, 4 and 8 s, so retransmissions at 1, 3 and 7 s and failure at 15 s.
pub const SYN_RETRIES: u32 = 3;
/// SYN-ACK retransmissions before a half-open connection is dropped: waits of 1, 2 and 4 s, retransmissions at 1 and 3 s, dropped at 7 s.
pub const SYNACK_RETRIES: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// The peer sent a valid RST.
    PeerReset,
    /// The peer answered our SYN with a RST: nothing listens.
    Refused,
    /// Retransmission gave up (data after 100 s, a SYN after 15 s, a SYN-ACK after 7 s).
    TimedOut,
    /// Our own abort, or a close with unread data.
    Aborted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The handshake finished.
    Connected,
    /// The peer's FIN arrived in order: every byte before it is in the receive buffer.
    PeerFin,
    /// The connection is gone.
    Reset(Reason),
    /// A clean end (both FINs done, or TIME-WAIT over).
    Closed,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub snd_buf: usize,
    /// At most 65,535 (no window scaling).
    pub rcv_buf: usize,
    /// The MSS we announce.
    pub mss: u16,
    pub nodelay: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            snd_buf: 16 * 1024,
            rcv_buf: 16 * 1024,
            mss: MSS,
            nodelay: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tuple {
    pub local: Ip,
    pub lport: u16,
    pub remote: Ip,
    pub rport: u16,
}

/// A limiter shared by every connection: at most `max` things per second (challenge ACKs, resets).
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    max: u32,
    second_start: Instant,
    used: u32,
    pub limited: u64,
}

impl RateLimit {
    pub fn new(max: u32) -> RateLimit {
        RateLimit {
            max,
            second_start: 0,
            used: 0,
            limited: 0,
        }
    }
    pub fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_sub(self.second_start) >= 1000 * MS {
            self.second_start = now;
            self.used = 0;
        }
        if self.used < self.max {
            self.used += 1;
            true
        } else {
            self.limited += 1;
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub segs_in: u64,
    pub segs_out: u64,
    pub challenge_acks: u64,
    pub rst_received: u64,
    pub retransmit_timeouts: u64,
    pub persist_probes: u64,
    pub delayed_acks: u64,
    pub ooo_dropped: u64,
    pub dup_acks_received: u64,
    pub fast_retransmits: u64,
    pub partial_acks: u64,
}

pub struct Conn {
    pub tuple: Tuple,
    cfg: Config,
    state: State,
    // ---- send side ----
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_wnd: u32,
    snd_wl1: u32,
    snd_wl2: u32,
    max_wnd: u32,
    peer_mss: u16,
    sbuf: SendBuf,
    /// Sequence number of the first byte in `sbuf`.
    data_base: u32,
    fin_seq: Option<u32>,
    pending_close: bool,
    // ---- receive side ----
    irs: u32,
    rcv_nxt: u32,
    rcv_adv: u32,
    rbuf: Reasm,
    fin_rcvd: bool,
    // ---- output requests ----
    syn_pending: bool,
    synack_pending: bool,
    rtx_pending: bool,
    probe_pending: bool,
    rst_pending: Option<(u32, u32, bool)>,
    ack_now: bool,
    // ---- timers ----
    rto: Rto,
    rtx_deadline: Option<Instant>,
    rtx_count: u32,
    oldest_outstanding_since: Instant,
    /// The segment being timed for an RTT sample: (last sequence number it covers, when it was sent).
    timed: Option<(u32, Instant)>,
    delack_deadline: Option<Instant>,
    unacked_in_order: u32,
    data_segs_rcvd: u32,
    last_activity: Instant,
    persist_deadline: Option<Instant>,
    persist_count: u32,
    timewait_deadline: Option<Instant>,
    events: Vec<Event>,
    pub stats: Stats,
    dup_acks: u32,
    // ---- congestion control (5.7) ----
    cwnd: u32,
    ssthresh: u32,
    /// NewReno's `recover`: the highest sequence number sent when loss was detected.
    recover: u32,
    in_recovery: bool,
    /// A SYN or SYN-ACK was retransmitted: the initial window is one segment (RFC 5681).
    syn_retransmitted: bool,
    last_send: Instant,
}

impl Conn {
    fn base(tuple: Tuple, cfg: Config, iss: u32, now: Instant) -> Conn {
        Conn {
            tuple,
            cfg,
            state: State::Closed,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_wnd: 0,
            snd_wl1: 0,
            snd_wl2: 0,
            max_wnd: 0,
            peer_mss: DEFAULT_PEER_MSS,
            sbuf: SendBuf::new(cfg.snd_buf),
            data_base: iss.wrapping_add(1),
            fin_seq: None,
            pending_close: false,
            irs: 0,
            rcv_nxt: 0,
            rcv_adv: 0,
            rbuf: Reasm::new(cfg.rcv_buf.min(65535)),
            fin_rcvd: false,
            syn_pending: false,
            synack_pending: false,
            rtx_pending: false,
            probe_pending: false,
            rst_pending: None,
            ack_now: false,
            rto: Rto::new(),
            rtx_deadline: None,
            rtx_count: 0,
            oldest_outstanding_since: now,
            timed: None,
            delack_deadline: None,
            unacked_in_order: 0,
            data_segs_rcvd: 0,
            last_activity: now,
            persist_deadline: None,
            persist_count: 0,
            timewait_deadline: None,
            events: Vec::new(),
            stats: Stats::default(),
            dup_acks: 0,
            cwnd: u32::MAX,
            ssthresh: u32::MAX,
            recover: iss,
            in_recovery: false,
            syn_retransmitted: false,
            last_send: now,
        }
    }

    /// RFC 5681's initial window for the segment size in use.
    fn initial_window(&self) -> u32 {
        let mss = self.send_mss() as u32;
        if self.syn_retransmitted {
            mss
        } else if mss > 2190 {
            2 * mss
        } else if mss > 1095 {
            3 * mss
        } else {
            4 * mss
        }
    }

    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }
    pub fn ssthresh(&self) -> u32 {
        self.ssthresh
    }
    pub fn in_recovery(&self) -> bool {
        self.in_recovery
    }

    /// An active open: the first `poll` sends the SYN.
    pub fn connect(tuple: Tuple, iss: u32, cfg: Config, now: Instant) -> Conn {
        let mut c = Conn::base(tuple, cfg, iss, now);
        c.state = State::SynSent;
        c.syn_pending = true;
        c
    }

    /// A passive open from a SYN that arrived for a listener: the first `poll` sends the SYN-ACK.
    pub fn accept(tuple: Tuple, syn: &Tcp<'_>, iss: u32, cfg: Config, now: Instant) -> Conn {
        let mut c = Conn::base(tuple, cfg, iss, now);
        c.state = State::SynReceived;
        c.irs = syn.seq;
        c.rcv_nxt = syn.seq.wrapping_add(1);
        c.rcv_adv = c.rcv_nxt.wrapping_add(c.rbuf.free().min(65535) as u32);
        c.snd_wnd = u32::from(syn.window);
        c.snd_wl1 = syn.seq;
        c.max_wnd = c.snd_wnd;
        c.peer_mss = syn
            .options
            .mss
            .filter(|&m| m > 0)
            .unwrap_or(DEFAULT_PEER_MSS);
        c.synack_pending = true;
        c
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }
    pub fn snd_una(&self) -> u32 {
        self.snd_una
    }
    pub fn snd_nxt(&self) -> u32 {
        self.snd_nxt
    }
    pub fn rcv_nxt(&self) -> u32 {
        self.rcv_nxt
    }
    pub fn snd_wnd(&self) -> u32 {
        self.snd_wnd
    }
    pub fn rto(&self) -> &Rto {
        &self.rto
    }
    pub fn peer_mss(&self) -> u16 {
        self.peer_mss
    }
    /// The size of the segments we send: the smaller of the peer's MSS and ours.
    pub fn send_mss(&self) -> usize {
        usize::from(self.peer_mss.min(self.cfg.mss).max(1))
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        core::mem::take(&mut self.events)
    }
    /// Bytes that can still be written.
    pub fn send_space(&self) -> usize {
        self.sbuf.free()
    }
    /// Bytes written and not yet acknowledged.
    pub fn unacked(&self) -> usize {
        self.sbuf.len()
    }
    /// In-order bytes waiting to be read.
    pub fn readable(&self) -> usize {
        self.rbuf.readable()
    }
    /// The peer's FIN has arrived and every byte before it has been read.
    pub fn eof(&self) -> bool {
        self.fin_rcvd && self.rbuf.readable() == 0
    }
    pub fn peer_closed(&self) -> bool {
        self.fin_rcvd
    }

    fn fail(&mut self, why: Reason) {
        self.state = State::Closed;
        self.rtx_deadline = None;
        self.delack_deadline = None;
        self.persist_deadline = None;
        self.timewait_deadline = None;
        self.events.push(Event::Reset(why));
    }

    fn established(&mut self, now: Instant) {
        self.state = if self.pending_close {
            State::FinWait1
        } else {
            State::Established
        };
        self.pending_close = false;
        self.events.push(Event::Connected);
        self.last_activity = now;
        self.last_send = now;
        self.cwnd = self.initial_window();
        self.ssthresh = u32::MAX;
    }

    // ---------------------------------------------------------------- application

    /// Queue bytes to send; returns how many were taken (0 when the buffer is full or the write side is closed).
    pub fn write(&mut self, data: &[u8]) -> usize {
        match self.state {
            State::SynSent | State::SynReceived | State::Established | State::CloseWait
                if self.fin_seq.is_none() && !self.pending_close =>
            {
                self.sbuf.push(data)
            }
            _ => 0,
        }
    }

    /// Take in-order bytes the peer sent. Freeing buffer space may call for a window update.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = self.rbuf.read(out);
        if n > 0 && self.window_update_due() {
            self.ack_now = true;
        }
        n
    }

    /// Set or clear NODELAY (Nagle's algorithm off).
    pub fn set_nodelay(&mut self, on: bool) {
        self.cfg.nodelay = on;
    }

    /// The in-order bytes waiting, without taking them.
    pub fn peek(&self, out: &mut [u8]) -> usize {
        self.rbuf.peek(out)
    }

    /// Take `n` bytes that `peek` showed (the application has them now). Freeing space may call for a window update.
    pub fn consume(&mut self, n: usize) {
        self.rbuf.consume(n);
        if n > 0 && self.window_update_due() {
            self.ack_now = true;
        }
    }

    /// The application is done writing: a FIN follows the data. With unread data the connection is reset instead (RFC 1122
    /// 4.2.2.13).
    pub fn close(&mut self) {
        if self.rbuf.readable() > 0 && !matches!(self.state, State::Closed | State::TimeWait) {
            self.abort();
            return;
        }
        self.shutdown_write();
    }

    /// Half-close: a FIN follows the data, and what the peer sends is still received (the unread-data rule of `close`
    /// does not apply).
    pub fn shutdown_write(&mut self) {
        match self.state {
            State::SynSent => {
                self.state = State::Closed;
                self.syn_pending = false;
                self.rtx_deadline = None;
                self.events.push(Event::Closed);
            }
            State::SynReceived => {
                self.fin_seq = Some(self.data_base.wrapping_add(self.sbuf.len() as u32));
                self.pending_close = true;
            }
            State::Established => {
                self.fin_seq = Some(self.data_base.wrapping_add(self.sbuf.len() as u32));
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                self.fin_seq = Some(self.data_base.wrapping_add(self.sbuf.len() as u32));
                self.state = State::LastAck;
            }
            _ => {}
        }
    }

    /// Reset the connection now.
    pub fn abort(&mut self) {
        match self.state {
            State::Closed | State::SynSent | State::TimeWait => {
                self.state = State::Closed;
            }
            _ => {
                self.rst_pending = Some((self.snd_nxt, 0, false));
            }
        }
        self.rtx_deadline = None;
        self.delack_deadline = None;
        self.persist_deadline = None;
        self.syn_pending = false;
        self.synack_pending = false;
        self.rtx_pending = false;
        self.events.push(Event::Reset(Reason::Aborted));
        if self.rst_pending.is_none() {
            self.state = State::Closed;
        }
    }

    // ---------------------------------------------------------------- the window we advertise

    fn window_update_due(&self) -> bool {
        let edge = self.rcv_nxt.wrapping_add(self.rbuf.free() as u32);
        if !seq::gt(edge, self.rcv_adv) {
            return false;
        }
        let grow = seq::dist(self.rcv_adv, edge) as usize;
        grow >= (2 * usize::from(self.cfg.mss)).min(self.rbuf.capacity() / 2)
    }

    /// The window to put in the next segment; the right edge never moves left, and grows only in steps of at least
    /// `min(MSS, rcv_buf / 2)` (receiver silly-window avoidance).
    fn advertise(&mut self) -> u16 {
        let edge = self
            .rcv_nxt
            .wrapping_add(self.rbuf.free().min(65535) as u32);
        if seq::gt(edge, self.rcv_adv) {
            let step = seq::dist(self.rcv_adv, edge) as usize;
            let min_step = usize::from(self.cfg.mss)
                .min(self.rbuf.capacity() / 2)
                .max(1);
            if step >= min_step {
                self.rcv_adv = edge;
            }
        }
        let w = seq::dist(self.rcv_nxt, self.rcv_adv);
        if seq::lt(self.rcv_adv, self.rcv_nxt) {
            0
        } else {
            w.min(65535) as u16
        }
    }

    fn rcv_window(&self) -> u32 {
        if seq::lt(self.rcv_adv, self.rcv_nxt) {
            0
        } else {
            seq::dist(self.rcv_nxt, self.rcv_adv)
        }
    }

    // ---------------------------------------------------------------- segments arriving

    fn challenge(&mut self, now: Instant, limiter: &mut RateLimit) {
        if limiter.allow(now) {
            self.stats.challenge_acks += 1;
            self.ack_now = true;
        }
    }

    pub fn on_segment(&mut self, s: &Tcp<'_>, now: Instant, challenge: &mut RateLimit) {
        self.stats.segs_in += 1;
        match self.state {
            State::Closed => {}
            State::SynSent => self.input_syn_sent(s, now),
            _ => self.input_synchronised(s, now, challenge),
        }
    }

    fn input_syn_sent(&mut self, s: &Tcp<'_>, now: Instant) {
        let mut ack_ok = false;
        if s.has(tcp::ACK) {
            if seq::le(s.ack, self.iss) || seq::gt(s.ack, self.snd_nxt) {
                if !s.has(tcp::RST) {
                    self.rst_pending = Some((s.ack, 0, false));
                }
                return;
            }
            ack_ok = true;
        }
        if s.has(tcp::RST) {
            if ack_ok {
                self.stats.rst_received += 1;
                self.fail(Reason::Refused);
            }
            return;
        }
        if !s.has(tcp::SYN) {
            return;
        }
        self.irs = s.seq;
        self.rcv_nxt = s.seq.wrapping_add(1);
        self.rcv_adv = self
            .rcv_nxt
            .wrapping_add(self.rbuf.free().min(65535) as u32);
        self.peer_mss = s.options.mss.filter(|&m| m > 0).unwrap_or(DEFAULT_PEER_MSS);
        self.snd_wnd = u32::from(s.window);
        self.snd_wl1 = s.seq;
        self.snd_wl2 = s.ack;
        self.max_wnd = self.snd_wnd;
        if ack_ok {
            self.snd_una = s.ack;
            self.sample_rtt(s.ack, now);
            self.rtx_deadline = None;
            self.rtx_count = 0;
            self.established(now);
            self.ack_now = true;
            // data riding on the SYN-ACK is unusual; take it through the ordinary path
            if !s.payload.is_empty() {
                self.accept_data(s, now);
            }
        } else {
            // simultaneous open: our SYN and theirs crossed
            self.state = State::SynReceived;
            self.synack_pending = true;
        }
    }

    /// Is the segment acceptable by RFC 9293's four-case test?
    fn acceptable(&self, s: &Tcp<'_>) -> bool {
        let len = s.seg_len();
        let wnd = self.rcv_window();
        let in_wnd = |x: u32| seq::in_range(x, self.rcv_nxt, self.rcv_nxt.wrapping_add(wnd));
        match (len, wnd) {
            (0, 0) => s.seq == self.rcv_nxt,
            (0, _) => in_wnd(s.seq),
            (_, 0) => false,
            (_, _) => in_wnd(s.seq) || in_wnd(s.seq.wrapping_add(len).wrapping_sub(1)),
        }
    }

    fn input_synchronised(&mut self, s: &Tcp<'_>, now: Instant, challenge: &mut RateLimit) {
        if self.state == State::TimeWait {
            // nothing but a retransmitted FIN matters: acknowledge it and wait again
            if !s.has(tcp::RST) {
                self.ack_now = true;
                if s.has(tcp::FIN) {
                    self.timewait_deadline = Some(now + TIME_WAIT);
                }
            }
            return;
        }
        // a SYN in SYN-RECEIVED that repeats the one that opened it: the peer retransmitting (answer with the SYN-ACK again),
        // or the SYN-ACK of a simultaneous open (take its ACK)
        let reopen = self.state == State::SynReceived && s.has(tcp::SYN) && s.seq == self.irs;
        if reopen && !s.has(tcp::ACK) {
            self.synack_pending = true;
            return;
        }
        if !reopen && !self.acceptable(s) {
            if !s.has(tcp::RST) {
                self.ack_now = true;
            }
            return;
        }
        if s.has(tcp::RST) {
            // RFC 5961: act only on an exact match; anywhere else in the window is answered with a challenge ACK
            if s.seq == self.rcv_nxt {
                self.stats.rst_received += 1;
                self.fail(Reason::PeerReset);
            } else {
                self.challenge(now, challenge);
            }
            return;
        }
        if s.has(tcp::SYN) && !reopen {
            // a SYN on a synchronised connection: challenge ACK, drop
            self.challenge(now, challenge);
            return;
        }
        if !s.has(tcp::ACK) {
            return;
        }
        // ---- ACK processing ----
        if self.state == State::SynReceived {
            if seq::lt(self.snd_una, s.ack) && seq::le(s.ack, self.snd_nxt) {
                self.snd_una = s.ack;
                self.snd_wnd = u32::from(s.window);
                self.snd_wl1 = s.seq;
                self.snd_wl2 = s.ack;
                self.max_wnd = self.max_wnd.max(self.snd_wnd);
                self.sample_rtt(s.ack, now);
                self.rtx_deadline = None;
                self.rtx_count = 0;
                self.established(now);
            } else {
                self.rst_pending = Some((s.ack, 0, false));
                return;
            }
        } else {
            if seq::gt(s.ack, self.snd_nxt) {
                // acknowledges data we never sent
                self.challenge(now, challenge);
                return;
            }
            if seq::lt(s.ack, self.snd_una.wrapping_sub(self.max_wnd)) {
                return; // older than anything plausible
            }
            self.process_ack(s, now);
            if self.state == State::Closed {
                return;
            }
        }
        // ---- data and FIN ----
        if matches!(
            self.state,
            State::Established | State::FinWait1 | State::FinWait2
        ) && !s.payload.is_empty()
        {
            self.accept_data(s, now);
        } else if !s.payload.is_empty() {
            // data after the peer's FIN, or in a state that takes none: acknowledge, do not keep
            self.ack_now = true;
        }
        if s.has(tcp::FIN) {
            self.process_fin(s, now);
        }
        if self.state == State::TimeWait && s.has(tcp::FIN) {
            self.timewait_deadline = Some(now + TIME_WAIT);
        }
    }

    fn sample_rtt(&mut self, ack: u32, now: Instant) {
        if let Some((last, at)) = self.timed {
            if seq::ge(ack, last) {
                self.rto.sample(now.saturating_sub(at));
                self.timed = None;
            }
        }
    }

    fn process_ack(&mut self, s: &Tcp<'_>, now: Instant) {
        let was_outstanding = self.snd_una != self.snd_nxt;
        if seq::gt(s.ack, self.snd_una) {
            let newly = seq::dist(self.snd_una, s.ack);
            self.ack_congestion(newly, s.ack);
            let fin_acked = self.fin_seq.is_some_and(|f| seq::gt(s.ack, f));
            let acked_data = {
                let upto = if fin_acked {
                    self.data_base.wrapping_add(self.sbuf.len() as u32)
                } else {
                    s.ack
                };
                if seq::gt(upto, self.data_base) {
                    seq::dist(self.data_base, upto).min(self.sbuf.len() as u32)
                } else {
                    0
                }
            };
            self.sbuf.ack(acked_data as usize);
            self.data_base = self.data_base.wrapping_add(acked_data);
            self.snd_una = s.ack;
            self.dup_acks = 0;
            self.sample_rtt(s.ack, now);
            self.rtx_count = 0;
            self.oldest_outstanding_since = now;
            self.rtx_deadline = if self.snd_una != self.snd_nxt {
                Some(now + self.rto.rto())
            } else {
                None
            };
            if self.rtx_pending && seq::ge(self.snd_una, self.snd_nxt) {
                self.rtx_pending = false;
            }
            self.last_activity = now;
            if fin_acked {
                match self.state {
                    State::FinWait1 => {
                        self.state = if self.fin_rcvd {
                            State::TimeWait
                        } else {
                            State::FinWait2
                        };
                        if self.state == State::TimeWait {
                            self.timewait_deadline = Some(now + TIME_WAIT);
                        }
                    }
                    State::Closing => {
                        self.state = State::TimeWait;
                        self.timewait_deadline = Some(now + TIME_WAIT);
                    }
                    State::LastAck => {
                        self.state = State::Closed;
                        self.events.push(Event::Closed);
                        self.rtx_deadline = None;
                        return;
                    }
                    _ => {}
                }
            }
        } else if s.ack == self.snd_una
            && was_outstanding
            && s.payload.is_empty()
            && !s.has(tcp::SYN)
            && !s.has(tcp::FIN)
            && u32::from(s.window) == self.snd_wnd
        {
            self.stats.dup_acks_received += 1;
            self.dup_acks += 1;
            self.dup_ack_congestion();
        }
        // the window: take it from the newest segment (RFC 9293 3.10.7.4)
        if seq::lt(self.snd_wl1, s.seq) || (self.snd_wl1 == s.seq && seq::le(self.snd_wl2, s.ack)) {
            self.snd_wnd = u32::from(s.window);
            self.snd_wl1 = s.seq;
            self.snd_wl2 = s.ack;
            self.max_wnd = self.max_wnd.max(self.snd_wnd);
            if self.snd_wnd > 0 {
                self.persist_deadline = None;
                self.persist_count = 0;
            }
        }
    }

    /// New data was acknowledged: grow the window, or take part in NewReno recovery.
    fn ack_congestion(&mut self, newly: u32, ack: u32) {
        let mss = self.send_mss() as u32;
        if self.in_recovery {
            if seq::ge(ack, self.recover) {
                // a full ACK: recovery is over
                self.cwnd = self.ssthresh;
                self.in_recovery = false;
            } else {
                // a partial ACK: retransmit the next hole, deflate by what was acknowledged, add one segment back
                self.stats.partial_acks += 1;
                self.cwnd = self
                    .cwnd
                    .saturating_sub(newly)
                    .saturating_add(if newly >= mss { mss } else { 0 });
                self.rtx_pending = true;
            }
            return;
        }
        if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(newly.min(mss));
        } else {
            let grow = (mss.saturating_mul(mss) / self.cwnd.max(1)).max(1);
            self.cwnd = self.cwnd.saturating_add(grow);
        }
    }

    /// A duplicate ACK: the third starts fast retransmit and recovery; later ones inflate the window.
    fn dup_ack_congestion(&mut self) {
        let mss = self.send_mss() as u32;
        if self.in_recovery {
            self.cwnd = self.cwnd.saturating_add(mss);
        } else if self.dup_acks == 3 && seq::gt(self.snd_una, self.recover) {
            let flight = seq::dist(self.snd_una, self.snd_nxt);
            self.ssthresh = (flight / 2).max(2 * mss);
            self.cwnd = self.ssthresh.saturating_add(3 * mss);
            self.recover = self.snd_nxt;
            self.in_recovery = true;
            self.rtx_pending = true;
            self.timed = None;
            self.stats.fast_retransmits += 1;
        }
    }

    fn accept_data(&mut self, s: &Tcp<'_>, now: Instant) {
        // trim to the window on both sides
        let wnd = self.rcv_window();
        let mut start = s.seq;
        let mut data = s.payload;
        if seq::lt(start, self.rcv_nxt) {
            let cut = seq::dist(start, self.rcv_nxt) as usize;
            if cut >= data.len() {
                // entirely old: a duplicate. Acknowledge at once so the sender learns where we are.
                self.ack_now = true;
                return;
            }
            data = &data[cut..];
            start = self.rcv_nxt;
        }
        let off = seq::dist(self.rcv_nxt, start) as usize;
        if off >= wnd as usize {
            self.ack_now = true;
            return;
        }
        let keep = data.len().min(wnd as usize - off);
        let r = self.rbuf.insert(off, &data[..keep]);
        if r.dropped_ooo {
            self.stats.ooo_dropped += 1;
            self.ack_now = true;
            return;
        }
        let was_in_order = off == 0;
        self.rcv_nxt = self.rcv_nxt.wrapping_add(r.advanced as u32);
        self.data_segs_rcvd = self.data_segs_rcvd.saturating_add(1);
        if !was_in_order || r.filled_hole || self.rbuf.has_ooo() {
            self.ack_now = true; // out of order, or it filled a hole
            return;
        }
        let idle = now.saturating_sub(self.last_activity) > self.rto.rto();
        self.unacked_in_order += r.advanced as u32;
        if self.data_segs_rcvd <= QUICK_ACKS
            || idle
            || self.unacked_in_order >= 2 * u32::from(self.peer_mss)
        {
            self.ack_now = true;
        } else if self.delack_deadline.is_none() {
            self.delack_deadline = Some(now + DELAYED_ACK);
        }
        self.last_activity = now;
    }

    fn process_fin(&mut self, s: &Tcp<'_>, now: Instant) {
        if self.fin_rcvd {
            self.ack_now = true;
            return;
        }
        // the FIN counts only when everything before it has arrived
        let fin_at = s.seq.wrapping_add(s.payload.len() as u32);
        if fin_at != self.rcv_nxt {
            return; // out of order: dropped; the peer retransmits it
        }
        self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
        self.fin_rcvd = true;
        self.ack_now = true;
        self.events.push(Event::PeerFin);
        match self.state {
            State::Established => self.state = State::CloseWait,
            State::FinWait1 => {
                // our FIN not yet acknowledged (else process_ack moved us on): simultaneous close
                self.state = State::Closing;
            }
            State::FinWait2 => {
                self.state = State::TimeWait;
                self.timewait_deadline = Some(now + TIME_WAIT);
            }
            _ => {}
        }
        // a FIN-WAIT-1 whose FIN was acknowledged by this very segment is TIME-WAIT already (handled in process_ack, which
        // saw FinWait1 -> FinWait2 or TimeWait before this ran)
    }

    // ---------------------------------------------------------------- time

    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.rtx_deadline,
            self.delack_deadline,
            self.persist_deadline,
            self.timewait_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn on_timer(&mut self, now: Instant) {
        if self.state == State::Closed {
            return;
        }
        if self.timewait_deadline.is_some_and(|d| now >= d) {
            self.timewait_deadline = None;
            self.state = State::Closed;
            self.events.push(Event::Closed);
            return;
        }
        if self.delack_deadline.is_some_and(|d| now >= d) {
            self.delack_deadline = None;
            self.stats.delayed_acks += 1;
            self.ack_now = true;
        }
        if self.rtx_deadline.is_some_and(|d| now >= d) {
            self.retransmission_timeout(now);
        }
        if self.persist_deadline.is_some_and(|d| now >= d) {
            self.persist_deadline = None;
            self.probe_pending = true;
            self.stats.persist_probes += 1;
            self.persist_count += 1;
        }
    }

    fn retransmission_timeout(&mut self, now: Instant) {
        self.stats.retransmit_timeouts += 1;
        match self.state {
            State::SynSent => {
                if self.rtx_count >= SYN_RETRIES {
                    return self.fail(Reason::TimedOut);
                }
                self.rtx_count += 1;
                self.syn_pending = true;
                self.syn_retransmitted = true;
                self.rto.after_syn_retransmit();
                self.rtx_deadline = Some(now + self.rto_backed_off_syn());
            }
            State::SynReceived => {
                if self.rtx_count >= SYNACK_RETRIES {
                    return self.fail(Reason::TimedOut);
                }
                self.rtx_count += 1;
                self.synack_pending = true;
                self.syn_retransmitted = true;
                self.rto.after_syn_retransmit();
                self.rtx_deadline = Some(now + self.rto_backed_off_syn());
            }
            _ => {
                if now.saturating_sub(self.oldest_outstanding_since) >= GIVE_UP {
                    return self.fail(Reason::TimedOut);
                }
                // RFC 5681 3.1: the window collapses to one segment and the threshold halves
                let mss = self.send_mss() as u32;
                let flight = seq::dist(self.snd_una, self.snd_nxt);
                self.ssthresh = (flight / 2).max(2 * mss);
                self.cwnd = mss;
                self.in_recovery = false;
                self.dup_acks = 0;
                self.recover = self.snd_nxt;
                self.rtx_count += 1;
                self.rtx_pending = true;
                self.timed = None; // Karn: a retransmitted segment is never timed
                self.rtx_deadline = Some(now + self.rto.backed_off(self.rtx_count));
            }
        }
    }

    /// SYN timers use the fixed 1, 2, 4, 8 s schedule, whatever the estimator says.
    fn rto_backed_off_syn(&self) -> u64 {
        1000 * MS * (1u64 << self.rtx_count.min(8))
    }

    // ---------------------------------------------------------------- segments leaving

    /// Build one segment into `out`: `data_len` bytes of the send buffer starting at sequence `seq`, with `flags`. Adds the
    /// ACK and the window unless this is a SYN to a peer we have not heard from, and clears the ACK obligations.
    fn build(
        &mut self,
        flags: u8,
        seq_no: u32,
        data_len: usize,
        opts: &[u8],
        out: &mut [u8],
    ) -> Option<usize> {
        let mut tmp = [0u8; 1500];
        let data_len = data_len.min(tmp.len());
        if data_len > 0 {
            let off = seq::dist(self.data_base, seq_no) as usize;
            let n = self.sbuf.peek(off, &mut tmp[..data_len]);
            if n != data_len {
                return None; // (cannot happen: callers stay inside the buffer)
            }
        }
        let with_ack = self.state != State::SynSent;
        let window = if with_ack {
            self.advertise()
        } else {
            self.cfg.rcv_buf.min(65535) as u16
        };
        let h = Header {
            src_port: self.tuple.lport,
            dst_port: self.tuple.rport,
            seq: seq_no,
            ack: if with_ack { self.rcv_nxt } else { 0 },
            flags: flags | if with_ack { tcp::ACK } else { 0 },
            window,
        };
        let n = tcp::write(
            out,
            self.tuple.local,
            self.tuple.remote,
            &h,
            opts,
            &tmp[..data_len],
        )
        .ok()?;
        self.stats.segs_out += 1;
        if with_ack {
            self.ack_now = false;
            self.delack_deadline = None;
            self.unacked_in_order = 0;
        }
        Some(n)
    }

    fn start_rtx_if_idle(&mut self, now: Instant) {
        if self.rtx_deadline.is_none() {
            self.rtx_deadline = Some(now + self.rto.backed_off(self.rtx_count));
        }
    }

    /// The next segment this connection wants to send, if any: written into `out` as TCP bytes (the caller adds the IP
    /// header). Call until it returns `None`.
    pub fn poll(&mut self, now: Instant, out: &mut [u8]) -> Option<usize> {
        if let Some((seq_no, ack, has_ack)) = self.rst_pending.take() {
            let h = Header {
                src_port: self.tuple.lport,
                dst_port: self.tuple.rport,
                seq: seq_no,
                ack,
                flags: tcp::RST | if has_ack { tcp::ACK } else { 0 },
                window: 0,
            };
            let n = tcp::write(out, self.tuple.local, self.tuple.remote, &h, &[], &[]).ok()?;
            self.state = State::Closed;
            self.stats.segs_out += 1;
            return Some(n);
        }
        if self.state == State::Closed {
            return None;
        }
        if self.syn_pending {
            self.syn_pending = false;
            let first = self.snd_nxt == self.iss;
            let opts = tcp::mss_option(self.cfg.mss);
            let n = self.build(tcp::SYN, self.iss, 0, &opts, out)?;
            if first {
                self.snd_nxt = self.iss.wrapping_add(1);
                self.oldest_outstanding_since = now;
                self.timed = Some((self.snd_nxt, now));
                self.rtx_deadline = Some(now + 1000 * MS);
            } else {
                self.timed = None;
            }
            return Some(n);
        }
        if self.synack_pending {
            self.synack_pending = false;
            let first = self.snd_nxt == self.iss;
            let opts = tcp::mss_option(self.cfg.mss);
            if self.rcv_adv == self.rcv_nxt {
                // the first window we ever offer (the SYN-ACK has no earlier edge to respect)
                self.rcv_adv = self
                    .rcv_nxt
                    .wrapping_add(self.rbuf.free().min(65535) as u32);
            }
            let n = self.build(tcp::SYN, self.iss, 0, &opts, out)?;
            if first {
                self.snd_nxt = self.iss.wrapping_add(1);
                self.oldest_outstanding_since = now;
                self.timed = Some((self.snd_nxt, now));
                self.rtx_deadline = Some(now + 1000 * MS);
            } else {
                self.timed = None;
            }
            return Some(n);
        }
        let can_send = matches!(
            self.state,
            State::Established
                | State::CloseWait
                | State::FinWait1
                | State::LastAck
                | State::Closing
        );
        let data_end = self.data_base.wrapping_add(self.sbuf.len() as u32);
        // a retransmission: the earliest unacknowledged segment, never advancing SND.NXT
        if self.rtx_pending && can_send {
            self.rtx_pending = false;
            let mss = self.send_mss();
            let sent_data = if seq::gt(self.snd_nxt, data_end) {
                data_end
            } else {
                self.snd_nxt
            };
            let outstanding_data = if seq::gt(sent_data, self.snd_una) {
                seq::dist(self.snd_una, sent_data) as usize
            } else {
                0
            };
            let len = outstanding_data.min(mss);
            let fin_here = self.fin_seq.is_some_and(|f| {
                self.snd_una.wrapping_add(len as u32) == f && seq::gt(self.snd_nxt, f)
            });
            if len > 0 || fin_here {
                let flags = if fin_here { tcp::FIN } else { 0 };
                return self.build(flags, self.snd_una, len, &[], out);
            }
        }
        if self.probe_pending && can_send {
            self.probe_pending = false;
            if seq::gt(data_end, self.snd_nxt) {
                let seq_no = self.snd_nxt;
                let n = self.build(0, seq_no, 1, &[], out)?;
                self.snd_nxt = self.snd_nxt.wrapping_add(1);
                self.start_rtx_if_idle(now);
                return Some(n);
            }
        }
        if can_send {
            let unsent = if seq::gt(data_end, self.snd_nxt) {
                seq::dist(self.snd_nxt, data_end) as usize
            } else {
                0
            };
            let fin_unsent = self.fin_seq.is_some_and(|f| {
                f == self.snd_nxt.wrapping_add(unsent as u32) && !seq::gt(self.snd_nxt, f)
            });
            let in_flight = seq::dist(self.snd_una, self.snd_nxt);
            // after an idle period longer than the RTO the window restarts from the initial window (RFC 5681 4.1)
            if in_flight == 0 && now.saturating_sub(self.last_send) > self.rto.rto() {
                self.cwnd = self.cwnd.min(self.initial_window());
            }
            let usable = self.snd_wnd.min(self.cwnd).saturating_sub(in_flight) as usize;
            if (unsent > 0 || fin_unsent)
                && self.snd_wnd == 0
                && in_flight == 0
                && self.persist_deadline.is_none()
            {
                self.persist_deadline =
                    Some(now + self.rto.backed_off(self.persist_count).min(MAX_PERSIST));
            }
            let mss = self.send_mss();
            let len = unsent.min(usable).min(mss);
            let carries_fin = fin_unsent && len == unsent && usable > len;
            let nagle_ok = self.cfg.nodelay || in_flight == 0 || carries_fin;
            let send_data = len > 0
                && (len >= mss
                    || (self.max_wnd > 0 && len as u32 >= self.max_wnd / 2)
                    || (len == unsent && nagle_ok));
            if send_data || (len == 0 && carries_fin) {
                let mut flags = 0;
                if send_data && len == unsent {
                    flags |= tcp::PSH;
                }
                if carries_fin {
                    flags |= tcp::FIN;
                }
                let seq_no = self.snd_nxt;
                self.last_send = now;
                let n = self.build(flags, seq_no, if send_data { len } else { 0 }, &[], out)?;
                let advance = (if send_data { len } else { 0 }) as u32 + u32::from(carries_fin);
                if self.snd_una == self.snd_nxt {
                    self.oldest_outstanding_since = now;
                }
                self.snd_nxt = self.snd_nxt.wrapping_add(advance);
                if self.timed.is_none() && send_data {
                    self.timed = Some((self.snd_nxt, now));
                }
                self.start_rtx_if_idle(now);
                return Some(n);
            }
        }
        if (self.ack_now || self.window_update_due()) && self.state != State::SynSent {
            let seq_no = self.snd_nxt;
            return self.build(0, seq_no, 0, &[], out);
        }
        None
    }
}

/// The reset a segment for no connection draws (RFC 9293 3.10.7.1), as `(seq, ack, has_ack)`; `None` for a RST (never answered).
/// The caller applies its own limits (no broadcast or multicast addresses, 100 a second).
pub fn reset_for(s: &Tcp<'_>) -> Option<(u32, u32, bool)> {
    if s.has(tcp::RST) {
        return None;
    }
    if s.has(tcp::ACK) {
        Some((s.ack, 0, false))
    } else {
        Some((0, s.seq.wrapping_add(s.seg_len()), true))
    }
}
