//! The service core against an independent reference: usage is recomputed from the list of live
//! connections and held replies after every step (never kept incrementally), and every refusal must be
//! exactly the one the limits along the chain demand.
use svc::{AccountId, Accounts, ConnTable, Limits, Refuse, ROOT};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The reference: accounts as (parent, limits, alive), connections as (key, account, held bytes).
#[derive(Default)]
struct Model {
    accts: Vec<(Option<u32>, Limits, bool)>,
    conns: Vec<(u64, u32, Option<usize>)>,
}

impl Model {
    fn chain(&self, a: u32) -> Vec<u32> {
        let mut out = vec![a];
        let mut cur = a;
        while let Some(p) = self.accts[cur as usize].0 {
            out.push(p);
            cur = p;
        }
        out
    }
    fn usage(&self, a: u32) -> (u32, u32, u32) {
        let mut conns = 0;
        let mut held = 0;
        for &(_, acct, h) in &self.conns {
            if self.chain(acct).contains(&a) {
                conns += 1;
                held += h.unwrap_or(0) as u32;
            }
        }
        (conns, held, 0)
    }
    fn can(&self, a: u32, conns: u32, held: u32) -> bool {
        self.chain(a).into_iter().all(|c| {
            let (uc, uh, _) = self.usage(c);
            let l = self.accts[c as usize].1;
            uc + conns <= l.conns && uh + held <= l.held_bytes
        })
    }
}

#[test]
fn random_walk_against_the_reference() {
    for seed in 1..300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let pool = Limits {
            conns: 40,
            held_bytes: 4096,
            inflight: 8,
            stream_bytes: 0,
        };
        let mut acc = Accounts::new(pool);
        let mut table: ConnTable<u32> = ConnTable::new();
        let mut m = Model::default();
        m.accts.push((None, pool, true));
        for step in 0..300 {
            let live: Vec<u32> = (0..m.accts.len() as u32)
                .filter(|&i| m.accts[i as usize].2)
                .collect();
            match rng.below(9) {
                0 => {
                    // open a child account with limits <= the parent's (sometimes deliberately above)
                    let parent = live[rng.below(live.len() as u64) as usize];
                    let pl = m.accts[parent as usize].1;
                    let over = rng.below(6) == 0;
                    let lim = Limits {
                        conns: rng.below(u64::from(pl.conns) + 1) as u32 + u32::from(over),
                        held_bytes: rng.below(u64::from(pl.held_bytes) + 1) as u32,
                        inflight: rng.below(u64::from(pl.inflight) + 1) as u32,
                        stream_bytes: 0,
                    };
                    let r = acc.open(AccountId(parent), lim);
                    if lim.conns > pl.conns {
                        assert_eq!(
                            r,
                            Err(Refuse::Invalid),
                            "seed {seed} step {step}: a looser child"
                        );
                    } else {
                        let id = r.unwrap_or_else(|e| panic!("seed {seed} step {step}: {e:?}"));
                        if id.0 as usize == m.accts.len() {
                            m.accts.push((Some(parent), lim, true));
                        } else {
                            m.accts[id.0 as usize] = (Some(parent), lim, true);
                        }
                    }
                }
                1..=3 => {
                    let a = live[rng.below(live.len() as u64) as usize];
                    let want = m.can(a, 1, 0);
                    let r = table.insert(&mut acc, AccountId(a), a);
                    assert_eq!(
                        r.is_ok(),
                        want,
                        "seed {seed} step {step}: connection charge"
                    );
                    if let Ok(k) = r {
                        m.conns.push((k, a, None));
                    } else {
                        assert_eq!(r, Err(Refuse::Quota));
                    }
                }
                4 if !m.conns.is_empty() => {
                    let i = rng.below(m.conns.len() as u64) as usize;
                    let (k, ..) = m.conns.swap_remove(i);
                    assert!(table.remove(&mut acc, k).is_some());
                }
                5 if !m.conns.is_empty() => {
                    let i = rng.below(m.conns.len() as u64) as usize;
                    let (k, a, held) = m.conns[i];
                    let n = 1 + rng.below(1500) as usize;
                    let r = table.hold(&mut acc, k, vec![7; n], None, step);
                    if held.is_some() {
                        assert_eq!(r, Err(Refuse::Invalid));
                    } else {
                        let want = m.can(a, 0, n as u32);
                        assert_eq!(r.is_ok(), want, "seed {seed} step {step}: held charge");
                        if r.is_ok() {
                            m.conns[i].2 = Some(n);
                        }
                    }
                }
                6 if !m.conns.is_empty() => {
                    let i = rng.below(m.conns.len() as u64) as usize;
                    let (k, ..) = m.conns[i];
                    let was = m.conns[i].2;
                    let h = table.take_held(&mut acc, k);
                    assert_eq!(h.as_ref().map(|h| h.bytes.len()), was);
                    m.conns[i].2 = None;
                    // sometimes put it back (the queue was still full): it must fit again, it just left
                    if let Some(h) = h {
                        if rng.below(2) == 0 {
                            table
                                .rehold(&mut acc, k, h)
                                .expect("what was just released fits again");
                            m.conns[i].2 = was;
                        }
                    }
                }
                7 => {
                    // expiry: exactly the holds older than the timeout
                    let now = step + 50;
                    let mut got = table.expired(now, 30);
                    got.sort_unstable();
                    assert!(got.iter().all(|k| table.is_held(*k)));
                    let held_keys = table.held_keys();
                    assert!(got.iter().all(|k| held_keys.contains(k)));
                }
                8 if live.len() > 1 => {
                    let a = live[1 + rng.below(live.len() as u64 - 1) as usize];
                    let busy = m.conns.iter().any(|c| c.1 == a)
                        || m.accts
                            .iter()
                            .enumerate()
                            .any(|(i, x)| x.2 && x.0 == Some(a) && i as u32 != a);
                    let r = acc.close(AccountId(a));
                    assert_eq!(
                        r.is_ok(),
                        !busy,
                        "seed {seed} step {step}: closing account {a}"
                    );
                    if r.is_ok() {
                        m.accts[a as usize].2 = false;
                    }
                }
                _ => {}
            }
            // the incremental books equal the recomputation, and no limit is exceeded anywhere
            for a in 0..m.accts.len() as u32 {
                if !m.accts[a as usize].2 {
                    assert_eq!(acc.usage(AccountId(a)), Err(Refuse::NoSuchAccount));
                    continue;
                }
                let (c, h, _) = m.usage(a);
                let (uc, uh, _) = acc.usage(AccountId(a)).unwrap();
                assert_eq!(
                    (uc, uh),
                    (c, h),
                    "seed {seed} step {step}: usage of account {a}"
                );
                let l = m.accts[a as usize].1;
                assert!(
                    c <= l.conns && h <= l.held_bytes,
                    "seed {seed} step {step}: account {a} over its limits"
                );
            }
            assert_eq!(table.len(), m.conns.len());
        }
        // everything released: nothing is charged anywhere
        for k in table.keys() {
            table.remove(&mut acc, k);
        }
        assert_eq!(
            acc.usage(ROOT).unwrap(),
            (0, 0, 0),
            "seed {seed}: books not back to zero"
        );
    }
}

#[test]
fn inflight_and_violations_and_keys() {
    let mut acc = Accounts::new(Limits {
        conns: 4,
        held_bytes: 100,
        inflight: 2,
        stream_bytes: 0,
    });
    let child = acc
        .open(
            ROOT,
            Limits {
                conns: 2,
                held_bytes: 50,
                inflight: 1,
                stream_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(acc.charge_inflight(child), Ok(()));
    assert_eq!(
        acc.charge_inflight(child),
        Err(Refuse::Quota),
        "the child's own limit"
    );
    assert_eq!(acc.charge_inflight(ROOT), Ok(()));
    assert_eq!(
        acc.charge_inflight(ROOT),
        Err(Refuse::Quota),
        "the pool: the child's one counts there too"
    );
    acc.release_inflight(child);
    acc.release_inflight(ROOT);
    assert_eq!(acc.usage(ROOT).unwrap().2, 0);
    acc.note_violation(child);
    acc.note_violation(child);
    assert_eq!(acc.violations(child), Ok(2));
    assert_eq!(
        acc.violations(ROOT),
        Ok(0),
        "counted against the account, not the pool"
    );
    // keys are never reused while the server runs
    let mut t: ConnTable<()> = ConnTable::new();
    let a = t.insert(&mut acc, ROOT, ()).unwrap();
    t.remove(&mut acc, a);
    let b = t.insert(&mut acc, ROOT, ()).unwrap();
    assert_ne!(a, b);
    // a looser child is refused; an unknown account is reported
    assert_eq!(
        acc.open(
            child,
            Limits {
                conns: 3,
                held_bytes: 1,
                inflight: 1,
                stream_bytes: 0,
            }
        ),
        Err(Refuse::Invalid)
    );
    assert_eq!(
        acc.open(
            AccountId(99),
            Limits {
                conns: 1,
                held_bytes: 1,
                inflight: 1,
                stream_bytes: 0,
            }
        ),
        Err(Refuse::NoSuchAccount)
    );
    assert_eq!(
        acc.close(ROOT),
        Err(Refuse::Invalid),
        "the pool is never closed"
    );
}
