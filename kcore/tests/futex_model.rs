//! The futex table against an independent reference (a plain list of (key, waiter) in arrival
//! order), over random operation sequences.
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::futex::Futexes;

#[test]
fn random_waits_wakes_and_forgets_agree_with_the_list() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut f = Futexes::new();
        let mut model: Vec<((u32, u64), u64)> = vec![]; // arrival order
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            let key = (rng.below(3) as u32, rng.below(3) * 4);
            match rng.below(5) {
                0 | 1 => {
                    let who = rng.below(12);
                    let existed = model.iter().any(|&(_, w)| w == who);
                    model.retain(|&(_, w)| w != who);
                    model.push((key, who));
                    assert_eq!(f.wait(key, who), !existed, "{ctx}");
                }
                2 => {
                    let n = rng.below(4);
                    let want: Vec<u64> = model
                        .iter()
                        .filter(|&&(k, _)| k == key)
                        .map(|&(_, w)| w)
                        .take(n as usize)
                        .collect();
                    model.retain(|&(_, w)| !want.contains(&w));
                    assert_eq!(f.wake(key, n), want, "{ctx}");
                }
                3 => {
                    let who = rng.below(12);
                    let had = model.iter().any(|&(_, w)| w == who);
                    model.retain(|&(_, w)| w != who);
                    assert_eq!(f.forget(who), had, "{ctx}");
                }
                _ => {
                    let pid = rng.below(3) as u32;
                    let mut want: Vec<u64> = model
                        .iter()
                        .filter(|&&((p, _), _)| p == pid)
                        .map(|&(_, w)| w)
                        .collect();
                    model.retain(|&((p, _), _)| p != pid);
                    let mut got = f.forget_process(pid);
                    want.sort();
                    got.sort();
                    assert_eq!(got, want, "{ctx}");
                }
            }
            assert_eq!(f.len(), model.len(), "{ctx}");
            for &(_, w) in &model {
                assert!(f.contains(w), "{ctx}");
            }
        }
    }
}
