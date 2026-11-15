//! H13 (step 2): packet scripts. The handshake, the closes and the simultaneous cases follow RFC 9293's figures (3.5), so
//! the expected exchange was written by the RFC's authors; the numbers are theirs where a figure has them.
mod common;
use common::*;
use nettcp::conn::{Config, Conn, Event, Reason, State};
use netwire::tcp::{self, ACK, FIN, RST, SYN};

fn cfg() -> Config {
    Config::default()
}

/// RFC 9293 figure 6: the basic three-way handshake. (TCP A: SYN-SENT, TCP B: LISTEN -> SYN-RECEIVED -> ESTABLISHED.)
#[test]
fn figure_6_the_three_way_handshake() {
    let mut lim = limiter();
    let mut a = Conn::connect(tuple_a(), 100, cfg(), 0);
    assert_eq!(a.state(), State::SynSent);
    let syn = out_segs(&mut a, 0);
    assert_eq!(syn.len(), 1);
    assert_eq!(
        (syn[0].flags, syn[0].seq, syn[0].mss),
        (SYN, 100, Some(1460)),
        "1. SYN seq=100"
    );
    let parsed_bytes = encode(&syn[0], A, B, 40000, 80);
    let parsed = tcp::parse(&parsed_bytes, A, B).unwrap();
    let mut b = Conn::accept(tuple_b(), &parsed, 300, cfg(), 0);
    assert_eq!(b.state(), State::SynReceived);
    let sa = out_segs(&mut b, 0);
    assert_eq!(
        (sa[0].flags, sa[0].seq, sa[0].ack),
        (SYN | ACK, 300, 101),
        "2. SYN,ACK seq=300 ack=101"
    );
    feed(&mut a, &sa[0], 0, &mut lim);
    assert_eq!(a.state(), State::Established);
    let ack = out_segs(&mut a, 0);
    assert_eq!(
        (ack[0].flags, ack[0].seq, ack[0].ack),
        (ACK, 101, 301),
        "3. ACK seq=101 ack=301"
    );
    feed(&mut b, &ack[0], 0, &mut lim);
    assert_eq!(b.state(), State::Established);
    assert!(
        out_segs(&mut a, 0).is_empty() && out_segs(&mut b, 0).is_empty(),
        "nothing more"
    );
}

/// RFC 9293 figure 7: simultaneous initiation.
#[test]
fn figure_7_simultaneous_open() {
    let mut lim = limiter();
    let mut a = Conn::connect(tuple_a(), 100, cfg(), 0);
    let mut b = Conn::connect(tuple_b(), 300, cfg(), 0);
    let sa = out_segs(&mut a, 0).remove(0);
    let sb = out_segs(&mut b, 0).remove(0);
    assert_eq!((sa.flags, sa.seq, sb.flags, sb.seq), (SYN, 100, SYN, 300));
    feed(&mut a, &sb, 0, &mut lim);
    feed(&mut b, &sa, 0, &mut lim);
    assert_eq!(
        (a.state(), b.state()),
        (State::SynReceived, State::SynReceived)
    );
    let ra = out_segs(&mut a, 0).remove(0);
    let rb = out_segs(&mut b, 0).remove(0);
    assert_eq!(
        (ra.flags, ra.seq, ra.ack),
        (SYN | ACK, 100, 301),
        "A: SYN,ACK seq=100 ack=301"
    );
    assert_eq!(
        (rb.flags, rb.seq, rb.ack),
        (SYN | ACK, 300, 101),
        "B: SYN,ACK seq=300 ack=101"
    );
    feed(&mut a, &rb, 0, &mut lim);
    feed(&mut b, &ra, 0, &mut lim);
    assert_eq!(
        (a.state(), b.state()),
        (State::Established, State::Established)
    );
}

/// A SYN answered with a RST: connection refused (3.5, reset generation).
#[test]
fn a_rst_to_our_syn_refuses() {
    let mut lim = limiter();
    let mut a = Conn::connect(tuple_a(), 100, cfg(), 0);
    out_segs(&mut a, 0);
    // a RST that does not acknowledge our SYN is ignored
    feed(&mut a, &seg(RST, 0, 0, 0, b""), 0, &mut lim);
    assert_eq!(a.state(), State::SynSent);
    feed(&mut a, &seg(RST | ACK, 0, 101, 0, b""), 0, &mut lim);
    assert_eq!(a.state(), State::Closed);
    assert_eq!(a.take_events(), vec![Event::Reset(Reason::Refused)]);
}

/// RFC 9293 figure 12: the normal close sequence.
#[test]
fn figure_12_normal_close() {
    let mut lim = limiter();
    let (mut a, mut b) = established(cfg(), 100, 300);
    a.close();
    assert_eq!(a.state(), State::FinWait1);
    let fin = out_segs(&mut a, 0);
    assert_eq!(
        (fin[0].flags, fin[0].seq, fin[0].ack),
        (FIN | ACK, 101, 301),
        "A: FIN,ACK seq=101 ack=301"
    );
    feed(&mut b, &fin[0], 0, &mut lim);
    assert_eq!(b.state(), State::CloseWait);
    assert!(b.take_events().contains(&Event::PeerFin));
    let ack = out_segs(&mut b, 0);
    assert_eq!(
        (ack[0].flags, ack[0].seq, ack[0].ack),
        (ACK, 301, 102),
        "B: ACK seq=301 ack=102"
    );
    feed(&mut a, &ack[0], 0, &mut lim);
    assert_eq!(a.state(), State::FinWait2);
    b.close();
    assert_eq!(b.state(), State::LastAck);
    let fin2 = out_segs(&mut b, 0);
    assert_eq!(
        (fin2[0].flags, fin2[0].seq, fin2[0].ack),
        (FIN | ACK, 301, 102),
        "B: FIN,ACK seq=301 ack=102"
    );
    feed(&mut a, &fin2[0], 0, &mut lim);
    assert_eq!(a.state(), State::TimeWait);
    let last = out_segs(&mut a, 0);
    assert_eq!(
        (last[0].flags, last[0].seq, last[0].ack),
        (ACK, 102, 302),
        "A: ACK seq=102 ack=302"
    );
    feed(&mut b, &last[0], 0, &mut lim);
    assert_eq!(b.state(), State::Closed);
    assert!(b.take_events().contains(&Event::Closed));
    // TIME-WAIT lasts 10 s, and a retransmitted FIN restarts it
    assert_eq!(a.next_deadline(), Some(10_000 * MS));
    feed(&mut a, &fin2[0], 5_000 * MS, &mut lim);
    assert_eq!(
        out_segs(&mut a, 5_000 * MS).len(),
        1,
        "the retransmitted FIN is acknowledged again"
    );
    assert_eq!(a.next_deadline(), Some(15_000 * MS));
    a.on_timer(14_999 * MS);
    assert_eq!(a.state(), State::TimeWait);
    a.on_timer(15_000 * MS);
    assert_eq!(a.state(), State::Closed);
}

/// RFC 9293 figure 13: simultaneous close.
#[test]
fn figure_13_simultaneous_close() {
    let mut lim = limiter();
    let (mut a, mut b) = established(cfg(), 100, 300);
    a.close();
    b.close();
    let fa = out_segs(&mut a, 0).remove(0);
    let fb = out_segs(&mut b, 0).remove(0);
    assert_eq!((fa.flags, fa.seq, fa.ack), (FIN | ACK, 101, 301));
    assert_eq!((fb.flags, fb.seq, fb.ack), (FIN | ACK, 301, 101));
    feed(&mut a, &fb, 0, &mut lim);
    feed(&mut b, &fa, 0, &mut lim);
    assert_eq!((a.state(), b.state()), (State::Closing, State::Closing));
    let aa = out_segs(&mut a, 0).remove(0);
    let ab = out_segs(&mut b, 0).remove(0);
    assert_eq!((aa.flags, aa.seq, aa.ack), (ACK, 102, 302));
    assert_eq!((ab.flags, ab.seq, ab.ack), (ACK, 302, 102));
    feed(&mut a, &ab, 0, &mut lim);
    feed(&mut b, &aa, 0, &mut lim);
    assert_eq!((a.state(), b.state()), (State::TimeWait, State::TimeWait));
}

/// A FIN that arrives together with the ACK of ours goes straight to TIME-WAIT.
#[test]
fn a_fin_with_the_ack_of_ours_goes_straight_to_time_wait() {
    let mut lim = limiter();
    let (mut a, _b) = established(cfg(), 100, 300);
    a.close();
    out_segs(&mut a, 0);
    feed(&mut a, &seg(FIN | ACK, 301, 102, 8000, b""), 0, &mut lim);
    assert_eq!(a.state(), State::TimeWait);
}

/// A close with unread data is a reset, not a close (RFC 1122 4.2.2.13).
#[test]
fn close_with_unread_data_resets() {
    let mut lim = limiter();
    let (mut a, mut b) = established(cfg(), 100, 300);
    feed(&mut a, &seg(ACK, 301, 101, 8000, b"unread"), 0, &mut lim);
    assert_eq!(a.readable(), 6);
    a.close();
    let r = out_segs(&mut a, 0);
    assert_eq!(r.iter().filter(|s| s.flags & RST != 0).count(), 1);
    assert_eq!(a.state(), State::Closed);
    let _ = &mut b;
}

/// Abort sends a RST with the right sequence number and ends at once.
#[test]
fn abort_sends_a_reset() {
    let (mut a, _b) = established(cfg(), 100, 300);
    a.abort();
    let r = out_segs(&mut a, 0);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].flags & RST != 0, r[0].seq), (true, 101));
    assert_eq!(a.state(), State::Closed);
    assert!(a.take_events().contains(&Event::Reset(Reason::Aborted)));
}

#[test]
fn a_segment_for_no_connection_draws_the_two_forms_of_reset() {
    let mk = |flags, seq, ack, data: &[u8]| encode(&seg(flags, seq, ack, 100, data), A, B, 1, 2);
    let b1 = mk(ACK, 50, 777, b"");
    let t1 = tcp::parse(&b1, A, B).unwrap();
    assert_eq!(
        nettcp::conn::reset_for(&t1),
        Some((777, 0, false)),
        "with ACK: seq = SEG.ACK, RST"
    );
    let b2 = mk(SYN, 50, 0, b"");
    let t2 = tcp::parse(&b2, A, B).unwrap();
    assert_eq!(
        nettcp::conn::reset_for(&t2),
        Some((0, 51, true)),
        "without ACK: seq 0, ack = SEG.SEQ + SEG.LEN, RST+ACK"
    );
    let b3 = mk(RST, 50, 0, b"");
    assert_eq!(
        nettcp::conn::reset_for(&tcp::parse(&b3, A, B).unwrap()),
        None,
        "never in answer to a RST"
    );
}
