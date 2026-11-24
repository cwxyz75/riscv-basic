//! MM9: the pager's request and wait bookkeeping against a naive model. Random waits, supplies, failures, cancels and pager deaths; after every
//! step: a request is made once per page however many threads wait for it, every waiter is woken exactly once and with the result of the range that
//! ended its wait, a destroyed pager's waiters are all woken, and when nothing is left registered nothing is left pending.
use abi::status::Status;
use kcore::pager::Pagers;
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::Rng;

#[test]
fn requests_are_made_once_and_every_waiter_is_woken_exactly_once() {
    for seed in 1..=300u64 {
        let mut rng = Rng::new(seed);
        let mut p = Pagers::new();
        p.reserve_room(64).unwrap();
        let mut alive: BTreeSet<u64> = BTreeSet::new();
        for id in 1..=2u64 {
            p.reserve_pager().unwrap();
            p.create(id);
            alive.insert(id);
        }
        // the model: who waits for what, what is requested, who got which result
        let mut waiting: BTreeMap<u64, (u64, u64, u64)> = BTreeMap::new();
        let mut requested: BTreeSet<(u64, u64, u64)> = BTreeSet::new();
        let mut woken_once: BTreeMap<u64, Result<(), Status>> = BTreeMap::new();
        for step in 0..200 {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(6) {
                0..=2 => {
                    // a thread (that is not waiting) needs a page
                    let tid = 1 + rng.below(8);
                    if waiting.contains_key(&tid) || woken_once.contains_key(&tid) {
                        continue;
                    }
                    let pager = 1 + rng.below(2);
                    let (vmo, page) = (pager * 10 + rng.below(2), rng.below(6));
                    let r = p.wait(tid, pager, vmo, page);
                    if !alive.contains(&pager) {
                        assert_eq!(r, Err(Status::PeerClosed), "{ctx}: a dead pager refuses");
                        continue;
                    }
                    waiting.insert(tid, (pager, vmo, page));
                    let first = requested.insert((pager, vmo, page));
                    assert_eq!(r.unwrap().is_some(), first, "{ctx}: one request per page");
                }
                3 | 4 => {
                    // the pager supplies or fails a range
                    let pager = 1 + rng.below(2);
                    if !alive.contains(&pager) {
                        continue;
                    }
                    let vmo = pager * 10 + rng.below(2);
                    let (first, count) = (rng.below(6), 1 + rng.below(4));
                    let failing = rng.below(3) == 0;
                    if failing {
                        p.fail(pager, vmo, first, count, Status::TimedOut);
                    } else {
                        p.supply(pager, vmo, first, count);
                    }
                    let woken = drained(&mut p);
                    let want: BTreeSet<u64> = waiting
                        .iter()
                        .filter(|(_, &(pg, v, page))| {
                            pg == pager && v == vmo && page >= first && page < first + count
                        })
                        .map(|(t, _)| *t)
                        .collect();
                    assert_eq!(
                        woken.iter().copied().collect::<BTreeSet<_>>(),
                        want,
                        "{ctx}: exactly the waiters of the range"
                    );
                    assert_eq!(woken.len(), want.len(), "{ctx}: each once");
                    for t in &want {
                        waiting.remove(t);
                        let res = if failing {
                            Err(Status::TimedOut)
                        } else {
                            Ok(())
                        };
                        woken_once.insert(*t, res);
                        assert_eq!(p.result(*t), Some(res), "{ctx}");
                    }
                    requested.retain(|&(pg, v, page)| {
                        !(pg == pager && v == vmo && page >= first && page < first + count)
                    });
                }
                _ => {
                    // a woken thread takes its result, or a waiting one is cancelled (killed), or a pager dies
                    match rng.below(3) {
                        0 => {
                            if let Some((&t, &res)) = woken_once.iter().next() {
                                assert_eq!(p.cancel(t), Some(res), "{ctx}");
                                woken_once.remove(&t);
                            }
                        }
                        1 => {
                            if let Some((&t, _)) = waiting.iter().next() {
                                assert_eq!(
                                    p.cancel(t),
                                    None,
                                    "{ctx}: a cancelled wait has no result"
                                );
                                let key = waiting.remove(&t).unwrap();
                                // the last waiter for a page takes its request with it
                                if !waiting.values().any(|k| *k == key) {
                                    requested.remove(&key);
                                }
                            }
                        }
                        _ => {
                            let pager = 1 + rng.below(2);
                            if alive.remove(&pager) {
                                p.destroy(pager);
                                let woken = drained(&mut p);
                                let want: BTreeSet<u64> = waiting
                                    .iter()
                                    .filter(|(_, &(pg, _, _))| pg == pager)
                                    .map(|(t, _)| *t)
                                    .collect();
                                assert_eq!(
                                    woken.iter().copied().collect::<BTreeSet<_>>(),
                                    want,
                                    "{ctx}: a dead pager wakes all its waiters"
                                );
                                for t in want {
                                    waiting.remove(&t);
                                    woken_once.insert(t, Err(Status::PeerClosed));
                                }
                                requested.retain(|&(pg, _, _)| pg != pager);
                            }
                        }
                    }
                }
            }
            for &t in waiting.keys() {
                assert!(p.waiting(t), "{ctx}: a waiter is still registered");
            }
            assert_eq!(p.waits(), waiting.len() + woken_once.len(), "{ctx}");
            assert_eq!(
                p.pending_requests(),
                requested.len(),
                "{ctx}: no request outlives its waiters"
            );
        }
        // everything taken: nothing is left registered
        for t in woken_once
            .keys()
            .chain(waiting.keys())
            .copied()
            .collect::<Vec<_>>()
        {
            p.cancel(t);
        }
        assert_eq!(p.waits(), 0, "seed {seed}: no waiter left");
    }
}

#[test]
fn unsent_requests_wait_for_room_in_order_and_go_with_their_pages() {
    let mut p = Pagers::new();
    p.reserve_room(64).unwrap();
    p.reserve_pager().unwrap();
    p.create(1);
    assert!(p.wait(1, 1, 10, 0).unwrap().is_some());
    assert!(p.wait(2, 1, 10, 1).unwrap().is_some());
    assert!(
        p.wait(3, 1, 10, 0).unwrap().is_none(),
        "the same page again is not asked twice"
    );
    assert_eq!(p.with_unsent(), vec![1]);
    assert_eq!(p.unsent(1).map(|r| r.page), Some(0), "oldest first");
    p.sent(1);
    assert_eq!(p.unsent(1).map(|r| r.page), Some(1));
    // page 1 is supplied before it was sent: its request is dropped, and both waiters of page 0 are still waiting
    p.supply(1, 10, 1, 1);
    assert_eq!(drained(&mut p), vec![2]);
    assert!(!p.any_unsent());
    assert!(p.waiting(1) && p.waiting(3));
}

/// The threads whose wait the last call ended.
fn drained(p: &mut Pagers) -> Vec<u64> {
    let mut out = Vec::new();
    p.drain_woken(&mut |w| out.extend_from_slice(w));
    out
}
