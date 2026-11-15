//! H18: the reassembly and send buffers against a naive byte map, H14's RTO estimator against a floating-point reference,
//! and the sequence arithmetic at the wrap.
use nettcp::reasm::{Reasm, MAX_RANGES};
use nettcp::rto::{Rto, GRANULARITY, MAX_RTO, MIN_RTO, MS};
use nettcp::sendbuf::SendBuf;
use nettcp::seq;
use std::collections::BTreeMap;

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

/// The reference: every byte ever accepted by absolute stream position, first copy wins.
struct Model {
    cap: usize,
    nxt: u64,
    read: u64,
    bytes: BTreeMap<u64, u8>,
}

impl Model {
    fn ready(&self) -> usize {
        (self.nxt - self.read) as usize
    }
    fn free(&self) -> usize {
        self.cap - self.ready()
    }
    /// The maximal runs of stored bytes beyond `nxt`.
    fn ranges(bytes: &BTreeMap<u64, u8>, nxt: u64) -> usize {
        let mut n = 0;
        let mut prev: Option<u64> = None;
        for (&p, _) in bytes.range(nxt..) {
            if prev.is_none_or(|q| p != q + 1) {
                n += 1;
            }
            prev = Some(p);
        }
        n
    }
    /// Insert as the buffer should: cut to the window, keep first copies, refuse if more than eight ranges would result.
    fn insert(&mut self, off: usize, data: &[u8]) -> (usize, bool) {
        let mut b = self.bytes.clone();
        if off < self.free() {
            for (i, &x) in data.iter().enumerate() {
                let rel = off + i;
                if rel >= self.free() {
                    break;
                }
                b.entry(self.nxt + rel as u64).or_insert(x);
            }
        }
        if Model::ranges(&b, self.nxt) > MAX_RANGES {
            return (0, true);
        }
        self.bytes = b;
        let before = self.nxt;
        while self.bytes.contains_key(&self.nxt) {
            self.nxt += 1;
        }
        ((self.nxt - before) as usize, false)
    }
    fn read(&mut self, n: usize) -> Vec<u8> {
        let n = n.min(self.ready());
        let v: Vec<u8> = (0..n as u64)
            .map(|i| self.bytes[&(self.read + i)])
            .collect();
        for i in 0..n as u64 {
            self.bytes.remove(&(self.read + i));
        }
        self.read += n as u64;
        v
    }
}

#[test]
fn the_reassembly_buffer_matches_a_byte_map_over_random_segments() {
    let mut dropped_seen = 0;
    for seed in 1..=300u64 {
        let mut r = Rng(seed * 7919 + 1);
        let cap = 64 + r.below(300) as usize;
        let mut real = Reasm::new(cap);
        let mut m = Model {
            cap,
            nxt: 0,
            read: 0,
            bytes: BTreeMap::new(),
        };
        for step in 0..400 {
            if r.below(4) == 0 {
                let n = r.below(80) as usize;
                let mut out = vec![0u8; n];
                let got = real.read(&mut out);
                let want = m.read(n);
                assert_eq!(
                    &out[..got],
                    &want[..],
                    "seed {seed} step {step}: bytes read"
                );
            } else {
                // overlaps, duplicates, holes and data past the window
                let off = r.below((cap + 40) as u64) as usize;
                let len = 1 + r.below(60) as usize;
                let data: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
                let got = real.insert(off, &data);
                let (adv, dropped) = m.insert(off, &data);
                assert_eq!(
                    (got.advanced, got.dropped_ooo),
                    (adv, dropped),
                    "seed {seed} step {step}: insert at {off} len {len}"
                );
                dropped_seen += usize::from(dropped);
            }
            assert_eq!(
                real.readable(),
                m.ready(),
                "seed {seed} step {step}: readable"
            );
            assert_eq!(real.free(), m.free(), "seed {seed} step {step}: free");
            assert!(real.ooo_ranges().len() <= MAX_RANGES);
            assert_eq!(
                real.ooo_ranges().len(),
                Model::ranges(&m.bytes, m.nxt),
                "seed {seed} step {step}: ranges"
            );
            let held: usize = real.ooo_ranges().iter().map(|&(a, b)| b - a).sum();
            assert!(
                real.readable() + held <= cap,
                "never beyond the reservation"
            );
        }
    }
    assert!(dropped_seen > 0, "the nine-range rule was exercised");
}

#[test]
fn first_copy_wins() {
    let mut r = Reasm::new(100);
    r.insert(10, b"AAAAA");
    r.insert(12, b"BBBBBBBBBB"); // overlaps: only the new tail may land
    r.insert(0, b"0123456789");
    let mut out = [0u8; 32];
    let n = r.read(&mut out);
    assert_eq!(&out[..n], b"0123456789AAAAABBBBBBB");
    // a re-sent copy with other contents changes nothing
    let adv = r.insert(0, b"zzz");
    assert_eq!(
        adv.advanced, 3,
        "new in-order bytes are new: nothing was there at offset 0 after the read"
    );
}

#[test]
fn nothing_is_stored_beyond_the_window() {
    let mut r = Reasm::new(16);
    assert_eq!(r.insert(0, &[1u8; 40]).advanced, 16);
    assert_eq!(r.free(), 0);
    assert_eq!(r.insert(0, &[2u8; 4]).advanced, 0);
    assert_eq!(r.insert(3, &[2u8; 4]).advanced, 0);
    let mut out = [0u8; 4];
    assert_eq!(r.read(&mut out), 4);
    assert_eq!(r.free(), 4);
    assert_eq!(
        r.insert(0, &[9u8; 20]).advanced,
        4,
        "space freed by the reader is usable, up to it only"
    );
}

#[test]
fn a_ninth_out_of_order_range_is_dropped() {
    let mut r = Reasm::new(1000);
    for i in 0..MAX_RANGES {
        let x = r.insert(10 + i * 10, b"xx");
        assert!(!x.dropped_ooo, "range {i}");
    }
    assert!(r.insert(10 + MAX_RANGES * 10, b"xx").dropped_ooo);
    // a segment that merges into an existing range is not a new one
    assert!(!r.insert(11, b"xxx").dropped_ooo);
    assert_eq!(r.ooo_ranges().len(), MAX_RANGES);
    // filling the first hole releases everything contiguous with it
    let x = r.insert(0, &[7u8; 10]);
    assert!(x.filled_hole);
    assert_eq!(x.advanced, 10 + 4);
}

#[test]
fn the_send_buffer_is_a_ring() {
    let mut s = SendBuf::new(10);
    assert_eq!(s.push(b"abcdefgh"), 8);
    assert_eq!(s.push(b"ijklm"), 2);
    let mut o = [0u8; 4];
    assert_eq!(s.peek(3, &mut o), 4);
    assert_eq!(&o, b"defg");
    s.ack(6);
    assert_eq!((s.len(), s.free()), (4, 6));
    assert_eq!(s.push(b"123456789"), 6);
    let mut o = [0u8; 20];
    let n = s.peek(0, &mut o);
    assert_eq!(&o[..n], b"ghij123456");
    assert_eq!(s.peek(10, &mut o), 0);
    s.ack(99);
    assert!(s.is_empty());
}

#[test]
fn sequence_numbers_compare_across_the_wrap() {
    let a = u32::MAX - 5;
    let b = 10u32;
    assert!(seq::lt(a, b) && seq::gt(b, a) && seq::le(a, a) && seq::ge(b, b));
    assert!(seq::in_range(2, a, 20) && !seq::in_range(30, a, 20) && !seq::in_range(a - 1, a, 20));
    assert_eq!(seq::dist(a, b), 16);
    assert!(!seq::lt(5, 5));
    // half the space away is "before" in one direction only
    assert!(seq::lt(0, 0x7fff_ffff) && seq::gt(0, 0x8000_0001));
}

// ---- H14: the RTO estimator against a floating-point reference ----

fn reference(samples: &[f64]) -> f64 {
    let (mut srtt, mut rttvar) = (0.0f64, 0.0f64);
    let mut rto = 1.0;
    for (i, &r) in samples.iter().enumerate() {
        if i == 0 {
            srtt = r;
            rttvar = r / 2.0;
        } else {
            rttvar = 0.75 * rttvar + 0.25 * (srtt - r).abs();
            srtt = 0.875 * srtt + 0.125 * r;
        }
        rto = (srtt + (4.0 * rttvar).max(0.001)).clamp(0.2, 60.0);
    }
    rto
}

#[test]
fn the_estimator_matches_a_floating_point_reference_within_a_granule() {
    for seed in 1..=500u64 {
        let mut r = Rng(seed);
        let n = 1 + r.below(40) as usize;
        let ms: Vec<u64> = (0..n).map(|_| 1 + r.below(900)).collect();
        let mut e = Rto::new();
        for &x in &ms {
            e.sample(x * MS);
        }
        let want = reference(&ms.iter().map(|&x| x as f64 / 1000.0).collect::<Vec<_>>());
        let got = e.rto() as f64 / 1e9;
        // integer division loses under a nanosecond per step, so within a granule is generous but meaningful
        assert!(
            (got - want).abs() <= GRANULARITY as f64 / 1e9,
            "seed {seed}: {got} against {want}"
        );
    }
}

#[test]
fn rfc_6298_worked_examples() {
    let mut e = Rto::new();
    assert_eq!(e.rto(), 1000 * MS, "initial RTO 1 s");
    e.sample(100 * MS);
    // first sample: SRTT = R, RTTVAR = R/2, RTO = SRTT + 4 RTTVAR = 100 + 200 = 300 ms
    assert_eq!(
        (e.srtt(), e.rttvar(), e.rto()),
        (Some(100 * MS), 50 * MS, 300 * MS)
    );
    e.sample(180 * MS);
    // RTTVAR = 0.75*50 + 0.25*80 = 57.5; SRTT = 0.875*100 + 0.125*180 = 110; RTO = 110 + 230 = 340
    assert_eq!(
        (e.srtt(), e.rttvar(), e.rto()),
        (Some(110 * MS), 57_500_000, 340 * MS)
    );
    // a tiny round trip: the 200 ms floor
    let mut f = Rto::new();
    for _ in 0..30 {
        f.sample(MS / 10);
    }
    assert_eq!(f.rto(), MIN_RTO);
    // backoff doubles and stops at 60 s
    let g = Rto::new();
    assert_eq!(
        [g.backed_off(0), g.backed_off(1), g.backed_off(3)],
        [1000 * MS, 2000 * MS, 8000 * MS]
    );
    assert_eq!(g.backed_off(20), MAX_RTO);
    // a retransmitted SYN makes the first data timeout 3 s (5.7)
    let mut h = Rto::new();
    h.after_syn_retransmit();
    assert_eq!(h.rto(), 3000 * MS);
}
