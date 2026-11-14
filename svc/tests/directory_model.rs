//! The service directory against a reference: a plain map of names to (listener, stale), views as plain
//! sets, every answer recomputed. Covers: no hijacking, stale replacement, hidden names that look like
//! missing ones, views that only narrow, and the name rules.
use std::collections::{BTreeMap, BTreeSet};
use svc::directory::{valid_name, MAX_NAMES};
use svc::{Directory, Lookup, Refuse, View};

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

fn name(rng: &mut Rng) -> Vec<u8> {
    format!("svc{}", rng.below(12)).into_bytes()
}

#[test]
fn random_walk_against_the_reference() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut dir: Directory<u32> = Directory::new();
        let mut model: BTreeMap<Vec<u8>, (u32, bool)> = BTreeMap::new();
        // a few views, each with the set it should mean
        let mut views: Vec<(View, Option<BTreeSet<Vec<u8>>>)> = vec![(View::all(), None)];
        let mut next_listener = 1u32;
        for step in 0..300 {
            let n = name(&mut rng);
            match rng.below(7) {
                0 | 1 => {
                    let l = next_listener;
                    next_listener += 1;
                    let r = dir.publish(&n, l);
                    match model.get(&n) {
                        Some((_, false)) => {
                            assert_eq!(r, Err(Refuse::Invalid), "seed {seed} step {step}: hijack")
                        }
                        Some(&(old, true)) => {
                            assert_eq!(r, Ok(Some(old)), "seed {seed} step {step}: stale replaced");
                            model.insert(n, (l, false));
                        }
                        None => {
                            assert_eq!(r, Ok(None));
                            model.insert(n, (l, false));
                        }
                    }
                }
                2 => {
                    let r = dir.unpublish(&n);
                    assert_eq!(r, model.remove(&n).map(|e| e.0));
                }
                3 => {
                    dir.mark_stale(&n);
                    if let Some(e) = model.get_mut(&n) {
                        e.1 = true;
                    }
                }
                4 => {
                    // narrow a random view to a random subset of what it allows (and sometimes ask for more)
                    let (v, set) = views[rng.below(views.len() as u64) as usize].clone();
                    let want: Vec<Vec<u8>> = (0..rng.below(5)).map(|_| name(&mut rng)).collect();
                    let refs: Vec<&[u8]> = want.iter().map(|w| w.as_slice()).collect();
                    let allowed = |w: &Vec<u8>| set.as_ref().is_none_or(|s| s.contains(w));
                    let r = v.restrict(&refs);
                    if want.iter().all(allowed) {
                        let nv = r.expect("a subset is a valid view");
                        views.push((nv, Some(want.iter().cloned().collect())));
                    } else {
                        assert_eq!(
                            r.err(),
                            Some(Refuse::Invalid),
                            "seed {seed} step {step}: a view cannot grant more"
                        );
                    }
                }
                _ => {}
            }
            // every view answers every name exactly as the reference says
            for (v, set) in &views {
                for i in 0..12 {
                    let nm = format!("svc{i}").into_bytes();
                    let inview = set.as_ref().is_none_or(|s| s.contains(&nm));
                    let want = match model.get(&nm) {
                        _ if !inview => Err(Lookup::NotFound),
                        None => Err(Lookup::NotFound),
                        Some(&(_, true)) => Err(Lookup::Unavailable),
                        Some(&(l, false)) => Ok(l),
                    };
                    assert_eq!(
                        dir.lookup(v, &nm).copied(),
                        want,
                        "seed {seed} step {step}: lookup {i}"
                    );
                }
                let listed: Vec<Vec<u8>> = model
                    .keys()
                    .filter(|k| set.as_ref().is_none_or(|s| s.contains(*k)))
                    .cloned()
                    .collect();
                assert_eq!(dir.list(v), listed, "seed {seed} step {step}: list");
            }
            assert_eq!(dir.len(), model.len());
        }
    }
}

#[test]
fn names_are_checked_and_the_table_is_bounded() {
    assert!(valid_name(b"memfs"));
    assert!(valid_name(b"net.stack-1"));
    assert!(!valid_name(b""));
    assert!(!valid_name(b"a/b"));
    assert!(!valid_name(b"has space"));
    assert!(!valid_name(b"nul\0"));
    assert!(!valid_name(&[b'x'; 65]));
    let mut d: Directory<u32> = Directory::new();
    assert_eq!(d.publish(b"a/b", 1), Err(Refuse::Invalid));
    for i in 0..MAX_NAMES {
        d.publish(format!("n{i}").as_bytes(), i as u32).unwrap();
    }
    assert_eq!(d.publish(b"one-too-many", 0), Err(Refuse::Quota));
    // an existing name still behaves
    assert_eq!(d.publish(b"n0", 9), Err(Refuse::Invalid));
    d.unpublish(b"n0");
    d.publish(b"one-too-many", 0).unwrap();
}

#[test]
fn a_hidden_name_is_indistinguishable_from_a_missing_one() {
    let mut d: Directory<u32> = Directory::new();
    d.publish(b"secret", 1).unwrap();
    d.publish(b"open", 2).unwrap();
    d.mark_stale(b"secret");
    let v = View::all().restrict(&[b"open"]).unwrap();
    assert_eq!(
        d.lookup(&v, b"secret"),
        Err(Lookup::NotFound),
        "not even UNAVAILABLE leaks"
    );
    assert_eq!(d.lookup(&v, b"never-published"), Err(Lookup::NotFound));
    assert_eq!(d.lookup(&v, b"open"), Ok(&2));
    assert_eq!(d.list(&v), vec![b"open".to_vec()]);
    assert!(v.restrict(&[b"secret"]).is_err());
    assert!(v.restrict(&[b"open"]).is_ok());
    assert!(!v.is_all() && View::all().is_all());
}
