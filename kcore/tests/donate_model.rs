//! The donation graph against a reference that keeps only bases and edges and recomputes every effective priority
//! from scratch (SCHED-SMP-SPEC 6.2): random add, set-base, donate, revoke, retarget and remove; after every step each
//! thread's `eff` equals the recomputation, the change list names exactly the threads whose effective priority
//! changed (with their new values), refusals change nothing, and `audit` agrees.
mod common;
use common::Rng;
use kcore::donate::{DonateError, Donations, MAX_DONATION_DEPTH};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Clone)]
struct Reference {
    base: BTreeMap<u64, u8>,
    edge: BTreeMap<u64, u64>, // from -> to
}

impl Reference {
    fn eff(&self, t: u64) -> u8 {
        let mut e = self.base[&t];
        for (&d, &to) in &self.edge {
            if to == t {
                e = e.max(self.eff(d));
            }
        }
        e
    }
    fn effs(&self) -> BTreeMap<u64, u8> {
        self.base.keys().map(|&t| (t, self.eff(t))).collect()
    }
    fn chain_ok(&self, from: u64, to: u64) -> Result<(), DonateError> {
        // cycle: to's chain reaches from
        let mut t = to;
        loop {
            if t == from {
                return Err(DonateError::Cycle);
            }
            match self.edge.get(&t) {
                Some(&n) => t = n,
                None => break,
            }
        }
        // depth: longest chain of donors under `from`, plus the hops above `to`
        fn height(r: &Reference, t: u64) -> usize {
            r.edge
                .iter()
                .filter(|(_, &to)| to == t)
                .map(|(&d, _)| 1 + height(r, d))
                .max()
                .unwrap_or(0)
        }
        let mut up = 0;
        let mut t = to;
        while let Some(&n) = self.edge.get(&t) {
            up += 1;
            t = n;
        }
        if height(self, from) + 1 + up > MAX_DONATION_DEPTH {
            return Err(DonateError::TooDeep);
        }
        Ok(())
    }
}

fn check(
    d: &Donations,
    r: &Reference,
    before: &BTreeMap<u64, u8>,
    changes: &[kcore::donate::Change],
    what: &str,
) {
    let after = r.effs();
    for (&t, &e) in &after {
        assert_eq!(d.eff(t), Some(e), "{what}: eff of {t}");
    }
    // the change list is exactly the threads whose effective priority changed (a removed thread is not in it)
    let want: BTreeSet<(u64, u8)> = after
        .iter()
        .filter(|(t, e)| before.get(t) != Some(e))
        .map(|(&t, &e)| (t, e))
        .filter(|(t, _)| before.contains_key(t))
        .collect();
    let got: BTreeSet<(u64, u8)> = changes.iter().map(|c| (c.tid, c.eff)).collect();
    assert_eq!(got, want, "{what}: the change list");
    assert_eq!(changes.len(), got.len(), "{what}: no thread listed twice");
    d.audit().unwrap_or_else(|e| panic!("{what}: audit: {e}"));
}

#[test]
fn random_walk_against_the_recomputation() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut d = Donations::new();
        let mut r = Reference::default();
        let mut next = 1u64;
        for step in 0..300 {
            let tids: Vec<u64> = r.base.keys().copied().collect();
            let pick = |rng: &mut Rng| tids[rng.next() as usize % tids.len()];
            let before = r.effs();
            let what = format!("seed {seed} step {step}");
            match rng.next() % 8 {
                0 | 1 => {
                    let b = (rng.next() % 64) as u8;
                    d.add_thread(next, b);
                    r.base.insert(next, b);
                    next += 1;
                    check(&d, &r, &before, &[], &format!("{what}: add"));
                }
                2 if !tids.is_empty() => {
                    let t = pick(&mut rng);
                    let b = (rng.next() % 64) as u8;
                    let ch = d.set_base(t, b);
                    r.base.insert(t, b);
                    check(&d, &r, &before, ch.as_slice(), &format!("{what}: set_base"));
                }
                3 | 4 if tids.len() >= 2 => {
                    let (from, to) = (pick(&mut rng), pick(&mut rng));
                    let res = d.donate(from, to);
                    if r.edge.contains_key(&from) {
                        assert_eq!(res.err(), Some(DonateError::AlreadyDonating), "{what}");
                    } else {
                        match r.chain_ok(from, to) {
                            Ok(()) => {
                                let ch = res.unwrap_or_else(|e| panic!("{what}: refused {e:?}"));
                                r.edge.insert(from, to);
                                check(&d, &r, &before, ch.as_slice(), &format!("{what}: donate"));
                            }
                            Err(e) => {
                                assert_eq!(res.err(), Some(e), "{what}: refusal");
                                check(
                                    &d,
                                    &r,
                                    &before,
                                    &[],
                                    &format!("{what}: a refusal changes nothing"),
                                );
                            }
                        }
                    }
                }
                5 if !tids.is_empty() => {
                    let t = pick(&mut rng);
                    let ch = d.revoke(t);
                    r.edge.remove(&t);
                    check(&d, &r, &before, ch.as_slice(), &format!("{what}: revoke"));
                }
                6 if tids.len() >= 2 => {
                    let (from, to) = (pick(&mut rng), pick(&mut rng));
                    // reference: revoke then donate, restoring on refusal
                    let mut r2 = r.clone();
                    r2.edge.remove(&from);
                    let verdict = r2.chain_ok(from, to);
                    let res = d.retarget(from, to);
                    match verdict {
                        Ok(()) => {
                            r.edge.insert(from, to);
                            let ch =
                                res.unwrap_or_else(|e| panic!("{what}: retarget refused {e:?}"));
                            check(&d, &r, &before, ch.as_slice(), &format!("{what}: retarget"));
                        }
                        Err(e) => {
                            assert_eq!(res.err(), Some(e), "{what}: retarget refusal");
                            check(
                                &d,
                                &r,
                                &before,
                                &[],
                                &format!("{what}: a refused retarget restores the edge"),
                            );
                        }
                    }
                }
                7 if tids.len() > 1 => {
                    let t = pick(&mut rng);
                    let ch = d.remove_thread(t);
                    r.base.remove(&t);
                    r.edge.remove(&t);
                    r.edge.retain(|_, to| *to != t);
                    let mut b2 = before.clone();
                    b2.remove(&t);
                    // the reference "before" for the list excludes the removed thread, whose in-edge donors
                    // keep their own priorities
                    check(&d, &r, &b2, ch.as_slice(), &format!("{what}: remove"));
                }
                _ => {}
            }
        }
    }
}

#[test]
fn a_boost_propagates_the_whole_chain_and_drops_back() {
    let mut d = Donations::new();
    for (t, b) in [(1, 50), (2, 10), (3, 10), (4, 10)] {
        d.add_thread(t, b);
    }
    d.donate(3, 4).unwrap();
    d.donate(2, 3).unwrap();
    assert_eq!(
        (d.eff(2), d.eff(3), d.eff(4)),
        (Some(10), Some(10), Some(10))
    );
    let ch = d.donate(1, 2).unwrap();
    assert_eq!(
        ch.as_slice()
            .iter()
            .map(|c| (c.tid, c.eff))
            .collect::<Vec<_>>(),
        vec![(2, 50), (3, 50), (4, 50)],
        "the boost runs up the chain, in order"
    );
    let ch = d.revoke(1);
    assert_eq!(ch.as_slice().len(), 3);
    assert_eq!(
        (d.eff(2), d.eff(3), d.eff(4)),
        (Some(10), Some(10), Some(10)),
        "revoke leaves no boost behind"
    );
    d.audit().unwrap();
}

#[test]
fn cycles_and_over_long_chains_are_refused() {
    let mut d = Donations::new();
    for t in 1..=12 {
        d.add_thread(t, t as u8);
    }
    assert_eq!(
        d.donate(1, 1).err(),
        Some(DonateError::Cycle),
        "a thread cannot lend to itself"
    );
    d.donate(1, 2).unwrap();
    assert_eq!(d.donate(2, 1).err(), Some(DonateError::Cycle));
    // a chain of exactly MAX_DONATION_DEPTH hops is allowed, one more is refused, and nothing changes
    for t in 2..=MAX_DONATION_DEPTH as u64 {
        d.donate(t, t + 1).unwrap();
    }
    assert_eq!(
        d.donate(MAX_DONATION_DEPTH as u64 + 1, MAX_DONATION_DEPTH as u64 + 2)
            .err(),
        Some(DonateError::TooDeep)
    );
    assert_eq!(d.target(MAX_DONATION_DEPTH as u64 + 1), None);
    d.audit().unwrap();
    assert_eq!(d.donate(1, 3).err(), Some(DonateError::AlreadyDonating));
    assert_eq!(d.donate(99, 1).err(), Some(DonateError::NoThread));
}

#[test]
fn a_change_list_says_whether_it_is_empty() {
    // (KT10: the mutant "Changes::is_empty gives true" survived the first mutation run)
    let mut d = Donations::new();
    d.add_thread(1, 10);
    let none = d.set_base(1, 10);
    assert!(none.is_empty(), "no thread's effective priority changed");
    assert!(none.as_slice().is_empty());
    let some = d.set_base(1, 20);
    assert!(!some.is_empty(), "thread 1's effective priority changed");
    assert_eq!(some.as_slice().len(), 1);
}
