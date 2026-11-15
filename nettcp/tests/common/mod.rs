#![allow(dead_code)]
use nettcp::conn::{Config, Conn, Event, RateLimit, Tuple};
use netwire::tcp::{self, Header, Tcp};

pub const MS: u64 = 1_000_000;
pub const A: [u8; 4] = [10, 0, 2, 15];
pub const B: [u8; 4] = [10, 0, 2, 2];

pub fn tuple_a() -> Tuple {
    Tuple {
        local: A,
        lport: 40000,
        remote: B,
        rport: 80,
    }
}
pub fn tuple_b() -> Tuple {
    Tuple {
        local: B,
        lport: 80,
        remote: A,
        rport: 40000,
    }
}

/// A segment as a test writes and reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S {
    pub flags: u8,
    pub seq: u32,
    pub ack: u32,
    pub win: u16,
    pub data: Vec<u8>,
    pub mss: Option<u16>,
}

pub fn seg(flags: u8, seq: u32, ack: u32, win: u16, data: &[u8]) -> S {
    S {
        flags,
        seq,
        ack,
        win,
        data: data.to_vec(),
        mss: None,
    }
}

pub fn encode(s: &S, from: [u8; 4], to: [u8; 4], sp: u16, dp: u16) -> Vec<u8> {
    let mut b = vec![0u8; 60 + s.data.len()];
    let opts = s.mss.map(tcp::mss_option);
    let h = Header {
        src_port: sp,
        dst_port: dp,
        seq: s.seq,
        ack: s.ack,
        flags: s.flags,
        window: s.win,
    };
    let n = tcp::write(
        &mut b,
        from,
        to,
        &h,
        opts.as_ref().map_or(&[], |o| &o[..]),
        &s.data,
    )
    .unwrap();
    b.truncate(n);
    b
}

pub fn decode(bytes: &[u8], from: [u8; 4], to: [u8; 4]) -> S {
    let t = tcp::parse(bytes, from, to).expect("a valid segment");
    S {
        flags: t.flags,
        seq: t.seq,
        ack: t.ack,
        win: t.window,
        data: t.payload.to_vec(),
        mss: t.options.mss,
    }
}

/// Everything `c` wants to send now.
pub fn drain(c: &mut Conn, now: u64) -> Vec<Vec<u8>> {
    let mut out = vec![];
    loop {
        let mut buf = [0u8; 1600];
        match c.poll(now, &mut buf) {
            Some(n) => out.push(buf[..n].to_vec()),
            None => break,
        }
        assert!(out.len() < 1000, "a connection that never stops sending");
    }
    out
}

/// Parsed output of `c` (from `from` to `to`).
pub fn out_segs(c: &mut Conn, now: u64) -> Vec<S> {
    let (f, t) = (c.tuple.local, c.tuple.remote);
    drain(c, now).iter().map(|b| decode(b, f, t)).collect()
}

pub fn feed(c: &mut Conn, s: &S, now: u64, limiter: &mut RateLimit) {
    let (f, t) = (c.tuple.remote, c.tuple.local);
    let (sp, dp) = (c.tuple.rport, c.tuple.lport);
    let bytes = encode(s, f, t, sp, dp);
    let parsed: Tcp = tcp::parse(&bytes, f, t).unwrap();
    c.on_segment(&parsed, now, limiter);
}

pub fn limiter() -> RateLimit {
    RateLimit::new(100)
}

/// Make a connected pair on a lossless, zero-delay wire: client `a` to listener-made `b`.
pub fn established(cfg: Config, iss_a: u32, iss_b: u32) -> (Conn, Conn) {
    let mut lim = limiter();
    let mut a = Conn::connect(tuple_a(), iss_a, cfg, 0);
    let syn = out_segs(&mut a, 0).remove(0);
    let bytes = encode(&syn, A, B, 40000, 80);
    let parsed = tcp::parse(&bytes, A, B).unwrap();
    let mut b = Conn::accept(tuple_b(), &parsed, iss_b, cfg, 0);
    let synack = out_segs(&mut b, 0).remove(0);
    feed(&mut a, &synack, 0, &mut lim);
    let ack = out_segs(&mut a, 0);
    assert_eq!(ack.len(), 1);
    feed(&mut b, &ack[0], 0, &mut lim);
    assert!(a.take_events().contains(&Event::Connected));
    assert!(b.take_events().contains(&Event::Connected));
    (a, b)
}
