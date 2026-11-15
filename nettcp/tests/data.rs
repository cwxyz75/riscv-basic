//! H13 (step 2) continued: data transfer, acknowledgement rules, timers and the RFC 5961 checks, by packet script; and two
//! connections over a seeded lossy link (netsim) moving real data.
mod common;
use common::*;
use netsim::{Faults, Link};
use nettcp::conn::{Config, Conn, Event, Reason, State};
use netwire::tcp::{self, ACK, FIN, PSH, RST, SYN};

fn cfg() -> Config {
    Config::default()
}

fn data_segs(v: &[S]) -> Vec<&S> {
    v.iter().filter(|s| !s.data.is_empty()).collect()
}

// ---------------------------------------------------------------- acknowledgement rules

#[test]
fn the_first_sixteen_data_segments_are_acknowledged_at_once_then_acks_are_delayed() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut seq = 301u32;
    for i in 0..16 {
        feed(&mut a, &seg(ACK, seq, 101, 8000, &[1u8; 100]), 0, &mut lim);
        seq += 100;
        let out = out_segs(&mut a, 0);
        assert_eq!(
            out.len(),
            1,
            "segment {i}: acknowledged at once (quick ACK)"
        );
        assert_eq!(out[0].ack, seq);
        let _ = a.read(&mut [0u8; 1000]);
    }
    // the seventeenth: nothing now, an ACK when 40 ms have passed
    feed(
        &mut a,
        &seg(ACK, seq, 101, 8000, &[1u8; 100]),
        10 * MS,
        &mut lim,
    );
    assert!(out_segs(&mut a, 10 * MS).is_empty(), "delayed");
    assert_eq!(a.next_deadline(), Some(10 * MS + 40 * MS));
    a.on_timer(49 * MS);
    assert!(out_segs(&mut a, 49 * MS).is_empty());
    a.on_timer(50 * MS);
    let out = out_segs(&mut a, 50 * MS);
    assert_eq!(
        (out.len(), out[0].ack),
        (1, seq + 100),
        "acknowledged after 40 ms"
    );
    // a second full-sized segment forces the ACK at once
    let _ = a.read(&mut [0u8; 4000]);
    feed(
        &mut a,
        &seg(ACK, seq + 100, 101, 8000, &[1u8; 1460]),
        60 * MS,
        &mut lim,
    );
    assert!(out_segs(&mut a, 60 * MS).is_empty());
    feed(
        &mut a,
        &seg(ACK, seq + 1560, 101, 8000, &[1u8; 1460]),
        61 * MS,
        &mut lim,
    );
    assert_eq!(
        out_segs(&mut a, 61 * MS).len(),
        1,
        "two full segments: acknowledge now"
    );
}

#[test]
fn out_of_order_data_gets_an_immediate_duplicate_ack_and_a_filled_hole_an_immediate_ack() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    for _ in 0..20 {
        // burn the quick-ACK allowance
        let nx = a.rcv_nxt();
        feed(&mut a, &seg(ACK, nx, 101, 8000, &[1u8; 10]), 0, &mut lim);
        let _ = out_segs(&mut a, 0);
        let _ = a.read(&mut [0u8; 100]);
    }
    let n = a.rcv_nxt();
    feed(
        &mut a,
        &seg(ACK, n + 100, 101, 8000, &[2u8; 100]),
        0,
        &mut lim,
    );
    let o = out_segs(&mut a, 0);
    assert_eq!((o.len(), o[0].ack), (1, n), "a duplicate ACK for the hole");
    feed(
        &mut a,
        &seg(ACK, n + 200, 101, 8000, &[3u8; 100]),
        0,
        &mut lim,
    );
    assert_eq!(out_segs(&mut a, 0)[0].ack, n, "and again");
    feed(&mut a, &seg(ACK, n, 101, 8000, &[1u8; 100]), 0, &mut lim);
    assert_eq!(
        out_segs(&mut a, 0)[0].ack,
        n + 300,
        "the hole filled: ACK over everything now contiguous"
    );
    let mut got = vec![0u8; 400];
    let k = a.read(&mut got);
    assert_eq!(
        &got[..k],
        &[[1u8; 100], [2u8; 100], [3u8; 100]].concat()[..],
        "in the right order"
    );
}

#[test]
fn overlapping_and_old_data_is_trimmed_and_first_copies_win() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    feed(&mut a, &seg(ACK, 301, 101, 8000, b"hello"), 0, &mut lim);
    let _ = out_segs(&mut a, 0);
    // a retransmission that overlaps: bytes 303..310 where 303..306 are old and differ
    feed(&mut a, &seg(ACK, 303, 101, 8000, b"XXXworld"), 0, &mut lim);
    let o = out_segs(&mut a, 0);
    assert_eq!(o[0].ack, 311);
    let mut got = [0u8; 32];
    let n = a.read(&mut got);
    assert_eq!(
        &got[..n],
        b"helloworld"[..]
            .iter()
            .chain(b"")
            .copied()
            .collect::<Vec<u8>>()
            .as_slice()
            .get(..n)
            .unwrap()
    );
    // a segment entirely old draws an immediate ACK and changes nothing
    feed(&mut a, &seg(ACK, 301, 101, 8000, b"hel"), 0, &mut lim);
    assert_eq!(out_segs(&mut a, 0)[0].ack, 311);
    assert_eq!(a.readable(), 0);
}

#[test]
fn data_beyond_the_window_is_dropped_and_acknowledged() {
    let mut lim = limiter();
    let small = Config {
        rcv_buf: 1000,
        ..cfg()
    };
    let (mut a, _b) = established(small, 100, 300);
    feed(&mut a, &seg(ACK, 301, 101, 8000, &[7u8; 1500]), 0, &mut lim);
    let o = out_segs(&mut a, 0);
    assert_eq!(o[0].ack, 301 + 1000, "only what fits is taken");
    assert_eq!(a.readable(), 1000);
    assert_eq!(o[0].win, 0, "and the window is now closed");
    feed(&mut a, &seg(ACK, 1301, 101, 8000, &[7u8; 10]), 0, &mut lim);
    assert_eq!(
        out_segs(&mut a, 0)[0].ack,
        1301,
        "more is refused with an ACK"
    );
    assert_eq!(a.readable(), 1000);
}

#[test]
fn the_advertised_window_never_shrinks_and_reopens_in_whole_steps() {
    let mut lim = limiter();
    let cfg = Config {
        rcv_buf: 4000,
        mss: 1000,
        ..Config::default()
    };
    let (mut a, _b) = established(cfg, 100, 300);
    // the window right edge after the handshake: 301 + 4000
    feed(&mut a, &seg(ACK, 301, 101, 8000, &[1u8; 1000]), 0, &mut lim);
    let o = out_segs(&mut a, 0);
    assert_eq!((o[0].ack, o[0].win), (1301, 3000), "the edge stays at 4301");
    feed(
        &mut a,
        &seg(ACK, 1301, 101, 8000, &[1u8; 3000]),
        0,
        &mut lim,
    );
    let o = out_segs(&mut a, 0);
    assert_eq!((o[0].ack, o[0].win), (4301, 0));
    // the application reads 100 bytes: too small a step to announce (silly-window avoidance)
    let mut buf = vec![0u8; 100];
    a.read(&mut buf);
    assert!(
        out_segs(&mut a, 0).is_empty(),
        "no window update for 100 bytes"
    );
    // an ACK that goes out for another reason still carries the old right edge: the window does not creep open in small steps
    a.write(b"x");
    let o = out_segs(&mut a, 0);
    assert_eq!((o[0].ack, o[0].win), (4301, 0), "the edge has not moved");
    // 1,000 bytes is a step of min(MSS, rcv_buf/2): the edge may move, but with the next ACK, not on its own
    a.read(&mut vec![0u8; 900]);
    assert!(
        out_segs(&mut a, 0).is_empty(),
        "below two segments or half the buffer: no ACK just for this"
    );
    // two segments' worth (here also half the buffer): an update is sent at once
    a.read(&mut vec![0u8; 1000]);
    let o = out_segs(&mut a, 0);
    assert_eq!(o.len(), 1, "a window update");
    assert_eq!((o[0].ack, o[0].win), (4301, 2000));
}

// ---------------------------------------------------------------- sending

#[test]
fn data_is_segmented_at_the_mss_with_psh_on_the_last_and_the_peer_window_respected() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    // the handshake gave the peer's window 16384 (our SYN-ACK from b said so); send 5000 bytes
    let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
    assert_eq!(a.write(&data), 5000);
    let o = out_segs(&mut a, 0);
    let sizes: Vec<usize> = o.iter().map(|s| s.data.len()).collect();
    // the three full segments go; the last, partial one waits for the ACK (Nagle)
    assert_eq!(sizes, vec![1460, 1460, 1460]);
    assert_eq!(
        o.iter().map(|s| s.flags & PSH != 0).collect::<Vec<_>>(),
        vec![false, false, false]
    );
    assert_eq!(o[1].seq, 101 + 1460);
    feed(&mut a, &seg(ACK, 301, 101 + 4380, 16384, b""), 0, &mut lim);
    let o = out_segs(&mut a, 0);
    assert_eq!(
        (o.len(), o[0].data.len(), o[0].flags & PSH != 0),
        (1, 620, true),
        "the rest, with PSH"
    );
    // the ACK frees the buffer and the sender takes more
    feed(&mut a, &seg(ACK, 301, 101 + 5000, 16384, b""), 0, &mut lim);
    assert_eq!(a.unacked(), 0);
    assert_eq!(
        a.write(&[0u8; 20000]),
        16 * 1024,
        "the buffer takes what fits"
    );
}

#[test]
fn a_small_write_waits_while_data_is_unacknowledged_unless_nodelay_is_set() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    a.write(b"first");
    assert_eq!(out_segs(&mut a, 0)[0].data, b"first");
    a.write(b"second");
    assert!(
        out_segs(&mut a, 0).is_empty(),
        "Nagle: a small segment waits for the ACK"
    );
    feed(&mut a, &seg(ACK, 301, 106, 16384, b""), 0, &mut lim);
    assert_eq!(out_segs(&mut a, 0)[0].data, b"second");
    // with NODELAY
    let (mut a, _b) = established(
        Config {
            nodelay: true,
            ..cfg()
        },
        100,
        300,
    );
    a.write(b"first");
    let one = out_segs(&mut a, 0);
    a.write(b"second");
    let two = out_segs(&mut a, 0);
    assert_eq!(
        (data_segs(&one).len(), data_segs(&two).len()),
        (1, 1),
        "NODELAY sends at once"
    );
}

#[test]
fn a_fin_follows_the_data_and_rides_on_the_last_segment() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    a.write(b"bye");
    a.close();
    let o = out_segs(&mut a, 0);
    assert_eq!(o.len(), 1);
    assert_eq!(
        (o[0].data.clone(), o[0].flags & FIN != 0, o[0].seq),
        (b"bye".to_vec(), true, 101)
    );
    // not acknowledged: retransmitted whole, FIN included
    a.on_timer(200 * MS);
    let o = out_segs(&mut a, 200 * MS);
    assert_eq!(
        (o[0].data.clone(), o[0].flags & FIN != 0, o[0].seq),
        (b"bye".to_vec(), true, 101)
    );
    feed(&mut a, &seg(ACK, 301, 105, 8000, b""), 300 * MS, &mut lim);
    assert_eq!(a.state(), State::FinWait2);
    assert!(a.write(b"more") == 0, "no writes after close");
}

// ---------------------------------------------------------------- timers

#[test]
fn retransmission_backs_off_doubling_until_the_hundred_second_limit() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    // the handshake sampled a zero round trip: RTO is the 200 ms floor
    assert_eq!(a.rto().rto(), 200 * MS);
    a.write(b"x");
    let first = out_segs(&mut a, 0);
    assert_eq!(first.len(), 1);
    let mut t = 0u64;
    let mut gaps = vec![];
    let mut last = 0;
    while let Some(d) = a.next_deadline() {
        t = d;
        a.on_timer(t);
        if a.state() == State::Closed {
            break;
        }
        let o = out_segs(&mut a, t);
        assert_eq!(o.len(), 1, "one retransmission per expiry");
        assert_eq!((o[0].seq, o[0].data.as_slice()), (101, &b"x"[..]));
        gaps.push(t - last);
        last = t;
        if gaps.len() > 40 {
            panic!("never gave up");
        }
    }
    assert_eq!(
        &gaps[..5],
        &[200 * MS, 400 * MS, 800 * MS, 1600 * MS, 3200 * MS]
    );
    assert!(gaps.iter().all(|&g| g <= 60_000 * MS), "capped at 60 s");
    assert!(t >= 100_000 * MS, "gave up after 100 s: {t}");
    assert_eq!(
        a.take_events().last(),
        Some(&Event::Reset(Reason::TimedOut))
    );
    let _ = &mut lim;
}

#[test]
fn a_retransmitted_segment_is_not_timed_karns_rule() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    a.write(b"hello");
    out_segs(&mut a, 0);
    a.on_timer(200 * MS);
    out_segs(&mut a, 200 * MS);
    let before = a.rto().rto();
    // the ACK arrives 5 ms after the retransmission: no sample may be taken from it
    feed(&mut a, &seg(ACK, 301, 106, 8000, b""), 205 * MS, &mut lim);
    assert_eq!(a.rto().rto(), before, "no RTT sample from an ambiguous ACK");
    // a fresh segment, ACKed promptly, is sampled
    a.write(b"again");
    out_segs(&mut a, 300 * MS);
    feed(&mut a, &seg(ACK, 301, 111, 8000, b""), 380 * MS, &mut lim);
    assert!(a.rto().srtt().is_some());
}

#[test]
fn a_syn_is_retried_at_1_3_and_7_seconds_and_fails_at_15() {
    let mut a = Conn::connect(tuple_a(), 100, cfg(), 0);
    let mut times = vec![];
    let mut t = 0;
    let o = out_segs(&mut a, 0);
    assert_eq!(o[0].flags, SYN);
    while let Some(d) = a.next_deadline() {
        t = d;
        a.on_timer(t);
        if a.state() == State::Closed {
            break;
        }
        let o = out_segs(&mut a, t);
        assert_eq!((o.len(), o[0].flags, o[0].seq), (1, SYN, 100));
        times.push(t / (1000 * MS));
    }
    assert_eq!(times, vec![1, 3, 7]);
    assert_eq!(t, 15_000 * MS, "failure at 15 s");
    assert_eq!(a.take_events(), vec![Event::Reset(Reason::TimedOut)]);
}

#[test]
fn a_syn_ack_is_retried_at_1_and_3_seconds_and_the_half_open_connection_dropped_at_7() {
    let mut a = Conn::connect(tuple_a(), 100, cfg(), 0);
    let syn = out_segs(&mut a, 0).remove(0);
    let bytes = encode(&syn, A, B, 40000, 80);
    let parsed = tcp::parse(&bytes, A, B).unwrap();
    let mut b = Conn::accept(tuple_b(), &parsed, 300, cfg(), 0);
    out_segs(&mut b, 0);
    let mut resent = vec![];
    let mut t = 0;
    while let Some(d) = b.next_deadline() {
        t = d;
        b.on_timer(t);
        if b.state() == State::Closed {
            break;
        }
        let o = out_segs(&mut b, t);
        assert_eq!((o[0].flags, o[0].seq), (SYN | ACK, 300));
        resent.push(t / (1000 * MS));
    }
    assert_eq!(resent, vec![1, 3]);
    assert_eq!(t, 7_000 * MS, "dropped at 7 s");
}

#[test]
fn a_zero_window_is_probed_with_one_byte_at_a_growing_interval_and_the_data_follows_when_it_opens()
{
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    // the peer closes its window completely
    feed(&mut a, &seg(ACK, 301, 101, 0, b""), 0, &mut lim);
    a.write(b"payload");
    assert!(
        out_segs(&mut a, 0).is_empty(),
        "nothing is sent into a zero window"
    );
    let d1 = a.next_deadline().expect("the persist timer");
    assert_eq!(d1, 200 * MS);
    a.on_timer(d1);
    let p = out_segs(&mut a, d1);
    assert_eq!(
        (p.len(), p[0].data.clone()),
        (1, b"p".to_vec()),
        "a one-byte probe"
    );
    assert_eq!(a.stats.persist_probes, 1);
    // the probe is refused (ACK for nothing new, window still 0): the retransmission timer keeps probing, doubling
    feed(&mut a, &seg(ACK, 301, 101, 0, b""), d1 + MS, &mut lim);
    // the window opens and the probe byte is acknowledged: the rest goes
    feed(
        &mut a,
        &seg(ACK, 301, 102, 8000, b""),
        d1 + 2 * MS,
        &mut lim,
    );
    let mut sent = vec![];
    for s in out_segs(&mut a, d1 + 2 * MS) {
        sent.extend(s.data);
    }
    assert_eq!(sent, b"ayload".to_vec());
}

// ---------------------------------------------------------------- RFC 5961

#[test]
fn a_rst_is_acted_on_only_at_exactly_rcv_nxt() {
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut lim = limiter();
    // outside the window: dropped, nothing sent
    feed(&mut a, &seg(RST, 301 + 100_000, 0, 0, b""), 0, &mut lim);
    assert_eq!(a.state(), State::Established);
    assert!(out_segs(&mut a, 0).is_empty());
    // inside the window but not exact: a challenge ACK, still established
    feed(&mut a, &seg(RST, 301 + 50, 0, 0, b""), 0, &mut lim);
    assert_eq!(a.state(), State::Established);
    let o = out_segs(&mut a, 0);
    assert_eq!(
        (o.len(), o[0].flags, o[0].ack),
        (1, ACK, 301),
        "challenge ACK"
    );
    assert_eq!(a.stats.challenge_acks, 1);
    // exact: the connection ends
    feed(&mut a, &seg(RST, 301, 0, 0, b""), 0, &mut lim);
    assert_eq!(a.state(), State::Closed);
    assert_eq!(a.take_events(), vec![Event::Reset(Reason::PeerReset)]);
}

#[test]
fn a_syn_on_an_established_connection_is_challenged_and_ignored() {
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut lim = limiter();
    feed(&mut a, &seg(SYN, 301, 0, 1000, b""), 0, &mut lim);
    assert_eq!(a.state(), State::Established);
    let o = out_segs(&mut a, 0);
    assert_eq!((o.len(), o[0].flags), (1, ACK));
}

#[test]
fn an_ack_for_data_never_sent_gets_an_ack_and_is_dropped() {
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut lim = limiter();
    a.write(b"abc");
    out_segs(&mut a, 0);
    feed(&mut a, &seg(ACK, 301, 5000, 8000, b""), 0, &mut lim);
    assert_eq!(a.unacked(), 3, "nothing was acknowledged");
    assert_eq!(out_segs(&mut a, 0).len(), 1);
}

#[test]
fn challenge_acks_are_limited_to_a_hundred_a_second_in_all() {
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut lim = limiter();
    for _ in 0..150 {
        feed(&mut a, &seg(SYN, 301, 0, 1000, b""), 5 * MS, &mut lim);
    }
    assert_eq!(a.stats.challenge_acks, 100);
    assert_eq!(lim.limited, 50);
    feed(&mut a, &seg(SYN, 301, 0, 1000, b""), 1100 * MS, &mut lim);
    assert_eq!(
        a.stats.challenge_acks, 101,
        "the next second has a fresh allowance"
    );
}

#[test]
fn a_segment_outside_the_window_draws_an_ack_and_is_not_kept() {
    let (mut a, _b) = established(cfg(), 100, 300);
    let mut lim = limiter();
    feed(
        &mut a,
        &seg(ACK, 301 + 70_000, 101, 8000, b"far"),
        0,
        &mut lim,
    );
    assert_eq!(a.readable(), 0);
    let o = out_segs(&mut a, 0);
    assert_eq!((o.len(), o[0].ack), (1, 301));
}

// ---------------------------------------------------------------- two connections over a lossy link

struct Sim {
    a: Conn,
    b: Conn,
    link: Link,
    now: u64,
    lim: nettcp::conn::RateLimit,
    segs: u64,
}

impl Sim {
    fn pump_out(&mut self) {
        for (from_a, conn) in [(true, 0), (false, 1)] {
            let _ = conn;
            let c = if from_a { &mut self.a } else { &mut self.b };
            let (f, t) = (c.tuple.local, c.tuple.remote);
            let (sp, dp) = (c.tuple.lport, c.tuple.rport);
            for bytes in drain(c, self.now) {
                // wire checks on everything sent
                let s = decode(&bytes, f, t);
                assert!(s.data.len() <= 1460, "never above the MSS");
                let _ = (sp, dp);
                self.segs += 1;
                self.link.send(usize::from(!from_a), self.now, &bytes);
            }
        }
    }
    fn timers(&mut self) {
        self.a.on_timer(self.now);
        self.b.on_timer(self.now);
    }
    fn next_time(&self) -> Option<u64> {
        [
            self.link.next_arrival(),
            self.a.next_deadline(),
            self.b.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .filter(|&t| t > self.now)
        .min()
    }
}

#[test]
fn two_connections_move_data_over_a_hostile_link_and_close_cleanly() {
    for seed in 1..=60u64 {
        let mut r = netsim::Rng::new(seed);
        let faults = Faults {
            loss_pct: r.below(15),
            dup_pct: r.below(8),
            reorder_pct: r.below(15),
            min_delay: MS / 10,
            max_delay: 3 * MS,
            corrupt_pct: r.below(5),
            ..Faults::NONE
        };
        let cfg = Config {
            snd_buf: 4096,
            rcv_buf: 8192,
            ..Config::default()
        };
        let mut sim = Sim {
            a: Conn::connect(tuple_a(), 1000 + seed as u32, cfg, 0),
            b: Conn::connect(tuple_b(), 5000 + seed as u32, cfg, 0),
            link: Link::new(seed, faults),
            now: 0,
            lim: limiter(),
            segs: 0,
        };
        // a connects; b is made by the first SYN that reaches it (a listener's job, played here)
        let syn = drain(&mut sim.a, 0).remove(0);
        sim.link.send(0, 0, &syn);
        sim.b = Conn::connect(tuple_b(), 0, cfg, 0); // placeholder replaced when the SYN arrives
        let mut b_made = false;
        let want_a_to_b: Vec<u8> = (0..30_000u32)
            .map(|i| (i * 7 + seed as u32) as u8)
            .collect();
        let want_b_to_a: Vec<u8> = (0..20_000u32).map(|i| (i * 13 + 5) as u8).collect();
        let (mut sent_a, mut sent_b) = (0usize, 0usize);
        let (mut got_a, mut got_b) = (vec![], vec![]);
        let (mut a_closed, mut b_closed) = (false, false);
        let mut steps = 0u64;
        'run: loop {
            steps += 1;
            assert!(steps < 100_000, "seed {seed}: no progress (now {} ms, a {:?} b {:?} segs {} sent {sent_a}/{sent_b} got {}/{} unacked {}/{} lost {} in flight {})", sim.now / 1_000_000, sim.a.state(), sim.b.state(), sim.segs, got_a.len(), got_b.len(), sim.a.unacked(), sim.b.unacked(), sim.link.lost, sim.link.in_flight());
            // deliver; the first SYN to b makes b
            for (to, bytes) in sim.link.due(sim.now) {
                if to == 1 && !b_made {
                    if let Ok(p) = tcp::parse(&bytes, A, B) {
                        if p.has(SYN) && !p.has(ACK) {
                            sim.b = Conn::accept(tuple_b(), &p, 7000 + seed as u32, cfg, sim.now);
                            b_made = true;
                        }
                    }
                    continue;
                }
                let (c, f, t) = if to == 1 {
                    (&mut sim.b, A, B)
                } else {
                    (&mut sim.a, B, A)
                };
                if let Ok(p) = tcp::parse(&bytes, f, t) {
                    c.on_segment(&p, sim.now, &mut sim.lim);
                }
            }
            sim.timers();
            // the applications
            sent_a += sim.a.write(&want_a_to_b[sent_a..]);
            if b_made {
                sent_b += sim.b.write(&want_b_to_a[sent_b..]);
            }
            let mut buf = [0u8; 700];
            loop {
                let n = sim.a.read(&mut buf);
                if n == 0 {
                    break;
                }
                got_a.extend_from_slice(&buf[..n]);
            }
            loop {
                let n = sim.b.read(&mut buf);
                if n == 0 {
                    break;
                }
                got_b.extend_from_slice(&buf[..n]);
            }
            if sent_a == want_a_to_b.len() && !a_closed && sim.a.state() == State::Established {
                sim.a.close();
                a_closed = true;
            }
            if b_made
                && sent_b == want_b_to_a.len()
                && !b_closed
                && matches!(sim.b.state(), State::Established | State::CloseWait)
            {
                sim.b.close();
                b_closed = true;
            }
            if b_made && sim.a.is_closed() && sim.b.is_closed() {
                break 'run;
            }
            if b_made
                && sim.a.state() == State::TimeWait
                && sim.b.is_closed()
                && got_a == want_b_to_a
                && got_b == want_a_to_b
            {
                break 'run;
            }
            // segments out (b before it exists sends nothing)
            if b_made {
                sim.pump_out();
            } else {
                for bytes in drain(&mut sim.a, sim.now) {
                    sim.link.send(0, sim.now, &bytes);
                }
            }
            match sim.next_time() {
                Some(t) => sim.now = t,
                None => {
                    // nothing scheduled: polls may still produce output at the same instant
                    if b_made {
                        sim.pump_out();
                    }
                    if sim.next_time().is_none() {
                        break 'run;
                    }
                }
            }
            assert!(sim.now < 3_000_000_000_000, "seed {seed}: too long");
        }
        assert_eq!(got_b, want_a_to_b, "seed {seed}: a to b");
        assert_eq!(got_a, want_b_to_a, "seed {seed}: b to a");
    }
}
