//! H13/H14 (step 3): congestion control by packet script and against an independent model of RFC 5681 and RFC 6582, and
//! the lossy-link suite over a bottleneck (H16).
mod common;
use common::*;
use netsim::{Faults, Link};
use nettcp::conn::{Config, Conn, State};
use netwire::tcp::{self, ACK, SYN};

const MSS: u32 = 1460;

fn big() -> Config {
    Config {
        snd_buf: 64 * 1024,
        rcv_buf: 65535,
        ..Config::default()
    }
}

/// Feed the peer's pure ACK (window 65535, which is what its SYN-ACK offered: no window change).
fn ack(c: &mut Conn, n: u32, now: u64) {
    let mut lim = limiter();
    feed(c, &seg(ACK, 301, n, 65535, b""), now, &mut lim);
}

#[test]
fn the_initial_window_is_three_segments_four_for_a_small_mss_and_one_after_a_retransmitted_syn() {
    let (mut a, _b) = established(big(), 100, 300);
    assert_eq!((a.cwnd(), a.ssthresh()), (3 * MSS, u32::MAX));
    a.write(&vec![1u8; 20_000]);
    let o = out_segs(&mut a, 0);
    assert_eq!(
        o.iter().map(|s| s.data.len()).sum::<usize>(),
        3 * 1460,
        "three segments, then the window is full"
    );
    let small = Config { mss: 536, ..big() };
    let (a, _b) = established(small, 100, 300);
    assert_eq!(a.cwnd(), 4 * 536, "an MSS below 1096: four segments");
    // a retransmitted SYN: one segment
    let mut a = Conn::connect(tuple_a(), 100, big(), 0);
    out_segs(&mut a, 0);
    a.on_timer(1000 * MS);
    out_segs(&mut a, 1000 * MS);
    let mut lim = limiter();
    let mut sa = seg(SYN | ACK, 300, 101, 60000, b"");
    sa.mss = Some(1460);
    feed(&mut a, &sa, 1001 * MS, &mut lim);
    assert_eq!(a.state(), State::Established);
    assert_eq!(
        a.cwnd(),
        MSS,
        "after a retransmitted SYN the initial window is one segment"
    );
}

#[test]
fn slow_start_adds_one_segment_per_acknowledged_segment() {
    let (mut a, _b) = established(big(), 100, 300);
    a.write(&vec![1u8; 60_000]);
    let first = out_segs(&mut a, 0);
    assert_eq!(first.len(), 3);
    // ACK the first segment: cwnd 3 -> 4 segments, two still in flight, so two more may go
    ack(&mut a, 101 + 1460, MS);
    assert_eq!(a.cwnd(), 4 * MSS);
    let o = out_segs(&mut a, MS);
    assert_eq!(
        o.len(),
        2,
        "two in flight, a window of four: room for two more"
    );
}

#[test]
fn congestion_avoidance_grows_by_about_a_segment_per_window() {
    let (mut a, _b) = established(big(), 100, 300);
    a.write(&vec![1u8; 60_000]);
    out_segs(&mut a, 0);
    // a timeout sets ssthresh = max(flight/2, 2 MSS) = 2 MSS(2190), cwnd = 1 MSS
    a.on_timer(300 * MS);
    out_segs(&mut a, 300 * MS);
    assert_eq!(
        (a.cwnd(), a.ssthresh()),
        (MSS, 2 * MSS),
        "timeout: ssthresh = flight/2 floored at two segments, cwnd = one segment"
    );
    // slow start up to ssthresh, then additive increase
    let mut una = 101 + 1460;
    ack(&mut a, una, 310 * MS);
    assert_eq!(a.cwnd(), 2 * MSS, "slow start");
    out_segs(&mut a, 310 * MS);
    una += 1460;
    ack(&mut a, una, 320 * MS);
    // cwnd == ssthresh now: congestion avoidance, cwnd += MSS*MSS/cwnd = MSS/2
    assert_eq!(a.cwnd(), 2 * MSS + MSS / 2);
}

#[test]
fn three_duplicate_acks_retransmit_the_first_segment_and_newreno_walks_out_of_recovery() {
    let (mut a, _b) = established(big(), 100, 300);
    // grow the window first so there are eight segments in flight
    a.write(&vec![7u8; 60_000]);
    let mut now = 0;
    let mut sent = out_segs(&mut a, now);
    while a.cwnd() < 8 * MSS {
        now += MS;
        for s in &sent {
            ack(&mut a, s.seq + s.data.len() as u32, now);
        }
        sent = out_segs(&mut a, now);
    }
    // whatever is in flight now: lose the first segment, deliver the rest as duplicate ACKs
    let flight_before = a.snd_nxt().wrapping_sub(a.snd_una());
    assert!(
        flight_before >= 4 * MSS,
        "enough in flight: {flight_before}"
    );
    let una = a.snd_una();
    now += MS;
    for i in 0..3 {
        ack(&mut a, una, now);
        if i < 2 {
            assert!(
                out_segs(&mut a, now).iter().all(|s| s.seq != una),
                "not yet at {} duplicates",
                i + 1
            );
        }
    }
    assert!(a.in_recovery());
    let flight = a.snd_nxt().wrapping_sub(una);
    assert_eq!(a.ssthresh(), (flight / 2).max(2 * MSS));
    assert_eq!(a.cwnd(), a.ssthresh() + 3 * MSS);
    let o = out_segs(&mut a, now);
    assert_eq!(
        o[0].seq, una,
        "the first unacknowledged segment is retransmitted"
    );
    assert_eq!(a.stats.fast_retransmits, 1);
    // a further duplicate inflates the window by one segment
    let before = a.cwnd();
    ack(&mut a, una, now);
    assert_eq!(a.cwnd(), before + MSS);
    // a partial ACK (one segment's worth): the next hole is retransmitted, the window deflates
    let partial = una + 1460;
    let recover = a.snd_nxt();
    assert!(partial < recover);
    let before = a.cwnd();
    ack(&mut a, partial, now + MS);
    assert!(a.in_recovery(), "still in recovery after a partial ACK");
    assert_eq!(a.cwnd(), before - 1460 + MSS);
    let o = out_segs(&mut a, now + MS);
    assert_eq!(o[0].seq, partial, "the next hole");
    assert_eq!(a.stats.partial_acks, 1);
    // the full ACK ends it: cwnd = ssthresh
    ack(&mut a, recover, now + 2 * MS);
    assert!(!a.in_recovery());
    assert_eq!(a.cwnd(), a.ssthresh());
}

#[test]
fn after_an_idle_period_the_window_restarts_from_the_initial_window() {
    let (mut a, _b) = established(big(), 100, 300);
    a.write(&[1u8; 100]);
    out_segs(&mut a, 0);
    ack(&mut a, 201, MS);
    for i in 0..10 {
        a.write(&vec![2u8; 1460]);
        let t = (2 + i) * MS;
        for s in out_segs(&mut a, t) {
            ack(&mut a, s.seq + s.data.len() as u32, t);
        }
    }
    let grown = a.cwnd();
    assert!(grown > 3 * MSS);
    a.write(&vec![3u8; 10_000]);
    let o = out_segs(&mut a, 60_000 * MS);
    assert_eq!(
        a.cwnd(),
        3 * MSS,
        "idle for a minute: back to the initial window"
    );
    assert!(o.iter().map(|s| s.data.len()).sum::<usize>() <= 3 * 1460);
}

// ---------------------------------------------------------------- H14: cwnd and ssthresh against an independent model

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// RFC 5681 and RFC 6582 written out separately, in the simplest form that keeps its own state.
struct Model {
    mss: u64,
    cwnd: u64,
    ssthresh: u64,
    in_rec: bool,
    recover: u64,
    dups: u32,
}

impl Model {
    fn new(iw: u64, mss: u64) -> Model {
        Model {
            mss,
            cwnd: iw,
            ssthresh: u32::MAX as u64,
            in_rec: false,
            recover: 0,
            dups: 0,
        }
    }
    /// `una` is the sequence (as an offset) before the ACK, `ack` the ACK, `nxt` SND.NXT, all as offsets from the ISS.
    fn new_ack(&mut self, una: u64, ack: u64) {
        let acked = ack - una;
        self.dups = 0;
        if self.in_rec {
            if ack >= self.recover {
                self.cwnd = self.ssthresh;
                self.in_rec = false;
            } else {
                self.cwnd =
                    self.cwnd.saturating_sub(acked) + if acked >= self.mss { self.mss } else { 0 };
            }
        } else if self.cwnd < self.ssthresh {
            self.cwnd += acked.min(self.mss);
        } else {
            self.cwnd += (self.mss * self.mss / self.cwnd).max(1);
        }
    }
    fn dup(&mut self, una: u64, nxt: u64) {
        self.dups += 1;
        if self.in_rec {
            self.cwnd += self.mss;
        } else if self.dups == 3 && una > self.recover {
            let flight = nxt - una;
            self.ssthresh = (flight / 2).max(2 * self.mss);
            self.cwnd = self.ssthresh + 3 * self.mss;
            self.recover = nxt;
            self.in_rec = true;
        }
    }
    fn timeout(&mut self, una: u64, nxt: u64) {
        let flight = nxt - una;
        self.ssthresh = (flight / 2).max(2 * self.mss);
        self.cwnd = self.mss;
        self.in_rec = false;
        self.dups = 0;
        self.recover = nxt;
    }
}

#[test]
fn cwnd_and_ssthresh_match_a_separate_model_over_random_acks_duplicates_and_timeouts() {
    for seed in 1..=300u64 {
        let mut r = Rng(seed * 104729 + 7);
        let (mut a, _b) = established(big(), 1000, 300);
        let mut m = Model::new(3 * 1460, 1460);
        m.recover = 0;
        let mut now = 0u64;
        for step in 0..300 {
            now += 10 * MS;
            // keep data flowing
            a.write(&vec![1u8; 20_000]);
            out_segs(&mut a, now);
            let una_off = u64::from(a.snd_una().wrapping_sub(1000));
            let nxt = u64::from(a.snd_nxt().wrapping_sub(1000));
            let flight = nxt - una_off;
            match r.below(10) {
                0 => {
                    // a timeout (only meaningful with something outstanding)
                    if flight > 0 {
                        // the timer must be due
                        if let Some(d) = a.next_deadline() {
                            now = now.max(d);
                        }
                        let before = (a.cwnd(), a.ssthresh());
                        a.on_timer(now);
                        if a.stats.retransmit_timeouts > 0 && (a.cwnd(), a.ssthresh()) != before {
                            m.timeout(una_off, nxt);
                        }
                        out_segs(&mut a, now);
                    }
                }
                1..=3 => {
                    if flight > 0 {
                        let u = a.snd_una();
                        ack(&mut a, u, now);
                        m.dup(una_off, nxt);
                        out_segs(&mut a, now);
                    }
                }
                _ => {
                    if flight > 0 {
                        let k = 1 + r.below(flight.min(4 * 1460));
                        let target = una_off + k;
                        ack(&mut a, (1000 + target) as u32, now);
                        m.new_ack(una_off, target);
                    }
                }
            }
            assert_eq!(
                (u64::from(a.cwnd()), u64::from(a.ssthresh()), a.in_recovery()),
                (m.cwnd, m.ssthresh, m.in_rec),
                "seed {seed} step {step} flight {flight} una {una_off} nxt {nxt} state {:?} cwnd0 {}",
                a.state(),
                a.cwnd()
            );
        }
    }
}

// ---------------------------------------------------------------- H16: the lossy link with a bottleneck

#[test]
fn a_transfer_over_a_bottleneck_with_loss_completes_and_keeps_the_pipe_busy() {
    for seed in 1..=20u64 {
        let mut r = netsim::Rng::new(seed);
        // 1.25 MB/s (about 10 Mbit/s: 1,250 bytes per ms) with a queue of 20 frames, 2% random loss, 5-15 ms of delay
        let faults = Faults {
            loss_pct: r.below(4),
            reorder_pct: r.below(3),
            min_delay: 5 * MS,
            max_delay: 15 * MS,
            rate: 1_250_000 / 1000, // bytes per ms: the unit of netsim time here is one nanosecond, so scale below
            queue_limit: 20,
            ..Faults::NONE
        };
        // netsim's rate is bytes per time unit; the time unit here is a nanosecond, so 1.25 bytes per microsecond is
        // 1,250 bytes per millisecond: express the rate per millisecond by running netsim on a millisecond clock
        let to_ms = |ns: u64| ns / MS;
        let mut link = Link::new(
            seed,
            Faults {
                min_delay: 5,
                max_delay: 15,
                rate: 1250,
                ..faults
            },
        );
        let cfg = Config {
            snd_buf: 64 * 1024,
            rcv_buf: 65535,
            ..Config::default()
        };
        let mut lim = limiter();
        let mut a = Conn::connect(tuple_a(), 1000, cfg, 0);
        let mut b: Option<Conn> = None;
        let want: Vec<u8> = (0..200_000u32)
            .map(|i| (i * 31 + seed as u32) as u8)
            .collect();
        let (mut sent, mut got) = (0usize, Vec::new());
        let mut now = 0u64; // nanoseconds
        let mut closed = false;
        for _ in 0..4_000_000u64 {
            let ms = to_ms(now);
            for (to, bytes) in link.due(ms) {
                if to == 1 {
                    if b.is_none() {
                        if let Ok(p) = tcp::parse(&bytes, A, B) {
                            if p.has(SYN) && !p.has(ACK) {
                                b = Some(Conn::accept(tuple_b(), &p, 9000, cfg, now));
                            }
                        }
                        continue;
                    }
                    if let Ok(p) = tcp::parse(&bytes, A, B) {
                        b.as_mut().unwrap().on_segment(&p, now, &mut lim);
                    }
                } else if let Ok(p) = tcp::parse(&bytes, B, A) {
                    a.on_segment(&p, now, &mut lim);
                }
            }
            a.on_timer(now);
            if let Some(b) = b.as_mut() {
                b.on_timer(now);
            }
            sent += a.write(&want[sent..]);
            if let Some(b) = b.as_mut() {
                let mut buf = [0u8; 4096];
                loop {
                    let n = b.read(&mut buf);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                if got.len() == want.len() && !closed {
                    closed = true;
                }
            }
            for bytes in drain(&mut a, now) {
                let s = decode(&bytes, A, B);
                assert!(s.data.len() <= 1460);
                link.send(0, ms, &bytes);
            }
            if let Some(b) = b.as_mut() {
                for bytes in drain(b, now) {
                    link.send(1, ms, &bytes);
                }
            }
            if got.len() == want.len() {
                break;
            }
            let next = [
                a.next_deadline(),
                b.as_ref().and_then(|b| b.next_deadline()),
                link.next_arrival().map(|m| m * MS),
            ]
            .into_iter()
            .flatten()
            .filter(|&t| t > now)
            .min();
            match next {
                Some(t) => now = t,
                None => now += MS,
            }
        }
        assert_eq!(got.len(), want.len(), "seed {seed}: the transfer completed");
        assert_eq!(got, want, "seed {seed}: the bytes are right");
        // the pipe: 200,000 bytes at 1,250 bytes/ms is 160 ms at full rate; with loss and slow start allow a generous multiple
        assert!(to_ms(now) < 160 * 40, "seed {seed}: took {} ms", to_ms(now));
    }
}
