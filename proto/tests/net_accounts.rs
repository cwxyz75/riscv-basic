//! H9 of NET-SPEC 12.1: the network service's accounts (7.4) against an independent recomputation: 32 connections per
//! client account, 256 in the pool, charges and refunds exact.
use proto::net::limits;
use svc::{Accounts, Limits, Refuse};

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
}

fn limits_for(conns: u32) -> Limits {
    Limits {
        conns,
        held_bytes: 64 * 1024,
        inflight: 16,
        stream_bytes: limits::STREAM_BYTES_PER_ACCOUNT,
    }
}

#[test]
fn a_client_gets_thirty_two_connections_and_the_pool_holds_two_hundred_and_fifty_six() {
    let mut a = Accounts::new(limits_for(limits::POOL_CONNS));
    let mut clients = vec![];
    for _ in 0..8 {
        clients.push(
            a.open(svc::ROOT, limits_for(limits::CONNS_PER_ACCOUNT))
                .unwrap(),
        );
    }
    // one client alone: exactly 32, then Quota
    let c0 = clients[0];
    for i in 0..limits::CONNS_PER_ACCOUNT {
        assert_eq!(a.charge_conn(c0), Ok(()), "connection {i}");
    }
    assert_eq!(a.charge_conn(c0), Err(Refuse::Quota), "the 33rd");
    assert!(!a.would_admit_conn(c0));
    // the others are unaffected until the pool itself is full: 8 clients x 32 = 256
    for &c in &clients[1..] {
        for _ in 0..limits::CONNS_PER_ACCOUNT {
            a.charge_conn(c).unwrap();
        }
    }
    assert_eq!(a.usage(svc::ROOT).unwrap().0, 256);
    let ninth = a
        .open(svc::ROOT, limits_for(limits::CONNS_PER_ACCOUNT))
        .unwrap();
    assert_eq!(
        a.charge_conn(ninth),
        Err(Refuse::Quota),
        "the pool is full: a ninth client gets nothing"
    );
    // closing one connection frees exactly one
    a.release_conn(c0);
    assert_eq!(a.charge_conn(c0), Ok(()));
    assert_eq!(a.charge_conn(c0), Err(Refuse::Quota));
}

#[test]
fn random_charges_and_refunds_match_a_recount() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut a = Accounts::new(limits_for(limits::POOL_CONNS));
        let ids: Vec<_> = (0..12)
            .map(|_| {
                a.open(svc::ROOT, limits_for(limits::CONNS_PER_ACCOUNT))
                    .unwrap()
            })
            .collect();
        let mut held = vec![0u32; ids.len()];
        for step in 0..3000 {
            let i = (rng.next() % ids.len() as u64) as usize;
            if !rng.next().is_multiple_of(3) {
                let total: u32 = held.iter().sum();
                let want_ok = held[i] < limits::CONNS_PER_ACCOUNT && total < limits::POOL_CONNS;
                let got = a.charge_conn(ids[i]);
                assert_eq!(got.is_ok(), want_ok, "seed {seed} step {step}");
                if want_ok {
                    held[i] += 1;
                }
            } else if held[i] > 0 {
                a.release_conn(ids[i]);
                held[i] -= 1;
            }
            for (k, h) in held.iter().enumerate() {
                assert_eq!(
                    a.usage(ids[k]).unwrap().0,
                    *h,
                    "seed {seed} step {step}: account {k}"
                );
            }
            assert_eq!(a.usage(svc::ROOT).unwrap().0, held.iter().sum::<u32>());
        }
    }
}

/// S8 follow-up item 2 (S9-SPEC 18.2): the account carries a byte reservation, `stream_bytes`, beside its connections. S8
/// charges none, but the model is in place and exact before S9's tests are built on it.
#[test]
fn stream_bytes_are_charged_and_refunded_exactly_down_the_chain() {
    assert_eq!(limits::STREAM_BYTES_PER_ACCOUNT, 512 * 1024);
    assert_eq!(limits::POOL_STREAM_BYTES, 4 * 1024 * 1024);
    let mut pool = limits_for(limits::POOL_CONNS);
    pool.stream_bytes = limits::POOL_STREAM_BYTES;
    let mut a = Accounts::new(pool);
    let c1 = a.open(svc::ROOT, limits_for(32)).unwrap();
    let c2 = a.open(svc::ROOT, limits_for(32)).unwrap();
    let kib = 1024;
    // 16 default streams (32 KiB each) fill one account exactly
    for _ in 0..16 {
        a.charge_stream(c1, 32 * kib).unwrap();
    }
    assert_eq!(a.charge_stream(c1, 1), Err(Refuse::Quota));
    assert_eq!(a.stream_usage(c1), Ok(512 * kib));
    assert_eq!(
        a.stream_usage(svc::ROOT),
        Ok(512 * kib),
        "the pool sees it too"
    );
    // a second client is unaffected
    a.charge_stream(c2, 32 * kib).unwrap();
    // the pool limit binds even when the client's own does not: 4 MiB in all
    let mut c = vec![];
    for _ in 0..8 {
        c.push(a.open(svc::ROOT, limits_for(32)).unwrap());
    }
    let mut charged = 512 * kib + 32 * kib;
    'fill: for &id in &c {
        for _ in 0..16 {
            if a.charge_stream(id, 32 * kib).is_err() {
                break 'fill;
            }
            charged += 32 * kib;
        }
    }
    assert_eq!(charged, limits::POOL_STREAM_BYTES);
    assert_eq!(a.stream_usage(svc::ROOT), Ok(charged));
    // a refusal charged nothing; a refund is exact
    a.release_stream(c1, 512 * kib);
    assert_eq!(a.stream_usage(c1), Ok(0));
    assert_eq!(a.stream_usage(svc::ROOT), Ok(charged - 512 * kib));
    // a child may not be given more than its parent
    let mut big = limits_for(32);
    big.stream_bytes = limits::POOL_STREAM_BYTES + 1;
    assert_eq!(a.open(svc::ROOT, big), Err(Refuse::Invalid));
}
