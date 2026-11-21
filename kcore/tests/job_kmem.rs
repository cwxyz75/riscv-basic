//! MM5: the kernel-bytes resource of the job ledger against a recomputed sum. Random charges and credits on a tree of jobs with page
//! limits: a job's kernel-byte usage is always the sum of the live charges made at or below it, a charge is refused exactly when some
//! ancestor would pass its derived limit (an eighth of its page limit, in bytes), a refusal changes nothing, and crediting everything
//! returns the ledger to zero.
mod common;
use common::Rng;
use kcore::job::{JobId, Jobs, Limits, Resource};

const SEEDS: u64 = 60;

fn limit_of(pages: u64) -> u64 {
    if pages == u64::MAX {
        u64::MAX
    } else {
        pages * 512
    }
}

#[test]
fn kernel_bytes_follow_a_recomputed_sum_and_stop_at_an_eighth_of_the_pages() {
    for seed in 1..=SEEDS {
        let mut rng = Rng::new(seed);
        let mut jobs = Jobs::new();
        // root (unlimited) -> a (page limit 40) -> b (page limit 16), and root -> c (page limit 100)
        let mk = |jobs: &mut Jobs, parent: JobId, pages: u64| {
            let mut l = Limits::UNLIMITED;
            l.pages = pages;
            jobs.create(parent, l).unwrap()
        };
        let a = mk(&mut jobs, JobId::ROOT, 40);
        let b = mk(&mut jobs, a, 16);
        let c = mk(&mut jobs, JobId::ROOT, 100);
        let all = [JobId::ROOT, a, b, c];
        let pages = [u64::MAX, 40, 16, 100];
        let parent = [None, Some(0usize), Some(1usize), Some(0usize)];
        let mut live: Vec<(usize, u64)> = vec![];
        let sum_below = |live: &[(usize, u64)], i: usize| -> u64 {
            live.iter()
                .filter(|(j, _)| {
                    let mut cur = Some(*j);
                    while let Some(x) = cur {
                        if x == i {
                            return true;
                        }
                        cur = parent[x];
                    }
                    false
                })
                .map(|(_, n)| n)
                .sum()
        };
        for _ in 0..800 {
            if live.is_empty() || !rng.next().is_multiple_of(3) {
                let j = (rng.next() % 4) as usize;
                let n = 1 + rng.next() % 3000;
                // the model: refused iff some ancestor (or the job) would pass its limit
                let mut would_pass = false;
                let mut cur = Some(j);
                while let Some(x) = cur {
                    if sum_below(&live, x).saturating_add(n) > limit_of(pages[x]) {
                        would_pass = true;
                    }
                    cur = parent[x];
                }
                let before: Vec<u64> = all
                    .iter()
                    .map(|&id| jobs.usage(id).unwrap().get(Resource::KernelBytes))
                    .collect();
                let r = jobs.charge(all[j], Resource::KernelBytes, n);
                assert_eq!(r.is_err(), would_pass, "seed {seed}: charge {n} to job {j}");
                if r.is_ok() {
                    live.push((j, n));
                } else {
                    let after: Vec<u64> = all
                        .iter()
                        .map(|&id| jobs.usage(id).unwrap().get(Resource::KernelBytes))
                        .collect();
                    assert_eq!(before, after, "a refusal changes nothing");
                }
            } else {
                let k = (rng.next() as usize) % live.len();
                let (j, n) = live.swap_remove(k);
                jobs.credit(all[j], Resource::KernelBytes, n);
            }
            for (i, &id) in all.iter().enumerate() {
                assert_eq!(
                    jobs.usage(id).unwrap().get(Resource::KernelBytes),
                    sum_below(&live, i),
                    "seed {seed}: job {i}"
                );
            }
        }
        for (j, n) in live.drain(..) {
            jobs.credit(all[j], Resource::KernelBytes, n);
        }
        for &id in &all {
            assert_eq!(jobs.usage(id).unwrap().get(Resource::KernelBytes), 0);
        }
    }
}

#[test]
fn the_kernel_byte_limit_is_an_eighth_of_the_page_limit_and_none_without_one() {
    let mut l = Limits::UNLIMITED;
    assert_eq!(l.effective(Resource::KernelBytes), u64::MAX);
    l.pages = 64;
    assert_eq!(l.effective(Resource::KernelBytes), 64 * 512);
}
