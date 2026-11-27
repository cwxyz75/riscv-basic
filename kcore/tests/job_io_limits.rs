//! IOMMU3: the two device limits of a job (IOMMU-SPEC 5.3: I/O-mapped pages and interrupt vectors) and the named-job DMA charge obey
//! J1 (usage counted on the job and every ancestor, all or nothing) and J2 (credited exactly once). Both limits are hierarchical: the
//! smallest set on the job and its ancestors applies, a child with none of its own inherits its parent's, and there is no default.
mod common;
use common::{FakeMem, Rng, SEEDS};
use kcore::aspace::{VmoError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{IoResource, JobError, JobId, Jobs, Limits, Resource, UNLIMITED};
use kcore::Mm;
use std::collections::BTreeMap;

const BOTH: [IoResource; 2] = [IoResource::MappedPages, IoResource::Vectors];

fn set(jobs: &mut Jobs, job: JobId, r: IoResource, limit: u64) {
    let pages = jobs.io_own_limit(job, IoResource::MappedPages).unwrap();
    let vectors = jobs.io_own_limit(job, IoResource::Vectors).unwrap();
    match r {
        IoResource::MappedPages => jobs.set_io_limits(job, limit, vectors),
        IoResource::Vectors => jobs.set_io_limits(job, pages, limit),
    }
    .unwrap();
}

#[test]
fn a_parents_limit_binds_a_child_and_a_grandchild_with_nothing_charged_on_refusal() {
    for r in BOTH {
        let mut jobs = Jobs::new();
        let parent = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        let child = jobs.create(parent, Limits::UNLIMITED).unwrap();
        let grandchild = jobs.create(child, Limits::UNLIMITED).unwrap();
        // no default: with no limit anywhere the charge is unlimited
        assert_eq!(jobs.io_limit(grandchild, r), UNLIMITED);
        jobs.charge_io(grandchild, r, 1 << 40).unwrap();
        jobs.credit_io(grandchild, r, 1 << 40);
        set(&mut jobs, parent, r, 10);
        for j in [child, grandchild] {
            assert_eq!(
                jobs.io_own_limit(j, r),
                Some(UNLIMITED),
                "{r:?}: none of its own"
            );
            assert_eq!(jobs.io_limit(j, r), 10, "{r:?}: inherited");
        }
        jobs.charge_io(grandchild, r, 7).unwrap();
        assert_eq!(
            jobs.charge_io(child, r, 4),
            Err(JobError::Exceeded),
            "{r:?}: refused at the parent's number"
        );
        assert_eq!(jobs.charge_io(grandchild, r, 4), Err(JobError::Exceeded));
        for j in [parent, child, grandchild] {
            assert_eq!(
                jobs.io_usage(j, r),
                Some(7),
                "{r:?}: nothing charged on any job of the chain"
            );
        }
        assert_eq!(
            jobs.io_usage(JobId::ROOT, r),
            Some(7),
            "{r:?}: the root counts everything"
        );
        jobs.charge_io(child, r, 3).unwrap();
        assert_eq!(
            jobs.io_usage(parent, r),
            Some(10),
            "{r:?}: exactly the limit"
        );
        assert_eq!(
            jobs.io_usage(grandchild, r),
            Some(7),
            "{r:?}: a sibling's charge is not the grandchild's"
        );
        // the other resource is untouched
        let other = BOTH.into_iter().find(|o| *o != r).unwrap();
        assert_eq!(jobs.io_usage(grandchild, other), Some(0));
        assert_eq!(jobs.io_limit(grandchild, other), UNLIMITED);
        jobs.credit_io(child, r, 3);
        jobs.credit_io(grandchild, r, 7);
        for j in [JobId::ROOT, parent, child, grandchild] {
            assert_eq!(jobs.io_usage(j, r), Some(0), "{r:?}: J2");
        }
        // jobs with nothing charged can go
        jobs.release(grandchild);
        jobs.release(child);
        jobs.release(parent);
        assert!(!jobs.exists(parent));
    }
}

#[test]
fn a_childs_smaller_limit_binds_the_child_only() {
    for r in BOTH {
        let mut jobs = Jobs::new();
        let parent = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        set(&mut jobs, parent, r, 100);
        let small = jobs.create(parent, Limits::UNLIMITED).unwrap();
        let other = jobs.create(parent, Limits::UNLIMITED).unwrap();
        set(&mut jobs, small, r, 5);
        assert_eq!(jobs.io_limit(small, r), 5);
        assert_eq!(jobs.io_limit(other, r), 100);
        jobs.charge_io(small, r, 5).unwrap();
        assert_eq!(jobs.charge_io(small, r, 1), Err(JobError::Exceeded));
        jobs.charge_io(other, r, 95).unwrap();
        assert_eq!(jobs.io_usage(parent, r), Some(100));
        // a child's larger limit does not lift the parent's
        let big = jobs.create(parent, Limits::UNLIMITED).unwrap();
        set(&mut jobs, big, r, 1000);
        assert_eq!(jobs.io_limit(big, r), 100);
        assert_eq!(jobs.charge_io(big, r, 1), Err(JobError::Exceeded));
        // a limit set below the usage refuses later charges and takes nothing away
        set(&mut jobs, other, r, 10);
        assert_eq!(jobs.io_usage(other, r), Some(95));
        jobs.credit_io(other, r, 90);
        assert_eq!(jobs.charge_io(other, r, 6), Err(JobError::Exceeded));
        jobs.charge_io(other, r, 5).unwrap();
    }
}

/// A random walk over a tree of jobs: charges and credits of both resources, limits set and changed, against a ledger that recomputes
/// every job's usage from the charges below it and every refusal from the limits above it.
#[test]
fn random_io_charges_follow_the_ledger() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut jobs = Jobs::new();
        let mut tree: Vec<(JobId, Option<usize>)> = vec![(JobId::ROOT, None)];
        let mut own: BTreeMap<(usize, usize), u64> = BTreeMap::new(); // (job index, resource) -> its own charges
        let mut limits: BTreeMap<(usize, usize), u64> = BTreeMap::new();
        let mut charges: Vec<(usize, usize, u64)> = vec![];
        for step in 0..400 {
            match rng.below(6) {
                0 if tree.len() < 12 => {
                    let p = rng.below(tree.len() as u64) as usize;
                    let j = jobs.create(tree[p].0, Limits::UNLIMITED).unwrap();
                    tree.push((j, Some(p)));
                }
                1 if tree.len() > 1 => {
                    let j = 1 + rng.below(tree.len() as u64 - 1) as usize;
                    let ri = rng.below(2) as usize;
                    let lim = if rng.chance(20) {
                        UNLIMITED
                    } else {
                        rng.below(60)
                    };
                    set(&mut jobs, tree[j].0, BOTH[ri], lim);
                    limits.insert((j, ri), lim);
                }
                2 | 3 => {
                    let j = rng.below(tree.len() as u64) as usize;
                    let ri = rng.below(2) as usize;
                    let n = 1 + rng.below(12);
                    // the ledger: refused iff some job on the chain would pass its own limit
                    let mut refused = false;
                    let mut c = Some(j);
                    while let Some(k) = c {
                        let used: u64 = (0..tree.len())
                            .filter(|&d| is_within(&tree, d, k))
                            .map(|d| own.get(&(d, ri)).copied().unwrap_or(0))
                            .sum();
                        let lim = limits.get(&(k, ri)).copied().unwrap_or(UNLIMITED);
                        refused |= lim != UNLIMITED && used + n > lim;
                        c = tree[k].1;
                    }
                    let r = jobs.charge_io(tree[j].0, BOTH[ri], n);
                    assert_eq!(
                        r.is_err(),
                        refused,
                        "seed {seed} step {step}: charge {n} to job {j}"
                    );
                    if r.is_ok() {
                        *own.entry((j, ri)).or_default() += n;
                        charges.push((j, ri, n));
                    }
                }
                _ if !charges.is_empty() => {
                    let (j, ri, n) = charges.swap_remove(rng.below(charges.len() as u64) as usize);
                    jobs.credit_io(tree[j].0, BOTH[ri], n);
                    *own.get_mut(&(j, ri)).unwrap() -= n;
                }
                _ => {}
            }
            // J1: every job's usage is the sum of its own charges and its descendants'
            for k in 0..tree.len() {
                for (ri, &res) in BOTH.iter().enumerate() {
                    let want: u64 = (0..tree.len())
                        .filter(|&d| is_within(&tree, d, k))
                        .map(|d| own.get(&(d, ri)).copied().unwrap_or(0))
                        .sum();
                    assert_eq!(
                        jobs.io_usage(tree[k].0, res),
                        Some(want),
                        "seed {seed} step {step}: job {k}"
                    );
                    let lim = std::iter::successors(Some(k), |&x| tree[x].1)
                        .map(|x| limits.get(&(x, ri)).copied().unwrap_or(UNLIMITED))
                        .min()
                        .unwrap();
                    assert_eq!(jobs.io_limit(tree[k].0, res), lim);
                }
            }
        }
        // J2: everything credited, everything back to zero, and the jobs can be released
        for (j, ri, n) in charges.drain(..) {
            jobs.credit_io(tree[j].0, BOTH[ri], n);
        }
        for r in BOTH {
            assert_eq!(jobs.io_usage(JobId::ROOT, r), Some(0));
        }
        for (j, _) in tree.iter().skip(1).rev() {
            jobs.release(*j);
        }
    }
}

fn is_within(tree: &[(JobId, Option<usize>)], d: usize, k: usize) -> bool {
    std::iter::successors(Some(d), |&x| tree[x].1).any(|x| x == k)
}

#[test]
fn the_named_job_dma_charge_obeys_j1_and_j2() {
    let mut mm = Mm {
        db: FrameDb::new(2000, 400),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let parent = jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 50,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let child = jobs.create(parent, Limits::UNLIMITED).unwrap();
    let pages = |j: &Jobs, id| j.usage(id).unwrap().get(Resource::Pages);
    let kb = |j: &Jobs, id| j.usage(id).unwrap().get(Resource::KernelBytes);
    let a = vmos.create_dma_in(&mut mm, &mut jobs, 40, child).unwrap();
    assert_eq!(
        (pages(&jobs, child), pages(&jobs, parent)),
        (40, 40),
        "J1: the chain is charged"
    );
    assert_eq!(kb(&jobs, child), kb(&jobs, parent));
    let free = mm.db.free_count();
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 11, child),
        Err(VmoError::NoMemory),
        "the parent's limit binds the named child"
    );
    assert_eq!(
        (pages(&jobs, child), pages(&jobs, parent)),
        (40, 40),
        "nothing charged on refusal"
    );
    assert_eq!(mm.db.free_count(), free, "and no frame taken");
    // the job record outlives its last handle while the VMO holds it; the release credits once and lets it go
    vmos.release(&mut mm, &mut jobs, a).unwrap();
    assert_eq!((pages(&jobs, child), pages(&jobs, parent)), (0, 0), "J2");
    assert_eq!((kb(&jobs, child), kb(&jobs, parent)), (0, 0));
    jobs.release(child);
    assert!(!jobs.exists(child));
}
