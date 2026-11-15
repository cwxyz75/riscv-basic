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
            if rng.next() % 3 != 0 {
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
