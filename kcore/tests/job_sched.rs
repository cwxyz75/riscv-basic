//! PR1 and PR2 (SCHED-SMP-SPEC 4.9) in the job model: a job's `max_prio` and `rt_budget` never exceed its parent's,
//! whatever sequence of creates and `set_sched` calls; a call that would raise one above the parent's is refused and
//! changes nothing; lowering one lowers every descendant; new jobs inherit the parent's.
mod common;
use common::Rng;
use kcore::job::{JobId, Jobs, Limits, SchedError, SchedLimits};

#[test]
fn random_walk_keeps_every_job_within_its_parent() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut jobs = Jobs::new();
        let mut ids = vec![JobId::ROOT];
        for step in 0..200 {
            let what = format!("seed {seed} step {step}");
            match rng.next() % 3 {
                0 => {
                    let parent = ids[rng.next() as usize % ids.len()];
                    if let Ok(j) = jobs.create(parent, Limits::UNLIMITED) {
                        assert_eq!(
                            jobs.sched(j),
                            jobs.sched(parent),
                            "{what}: a new job inherits"
                        );
                        ids.push(j);
                    }
                }
                _ => {
                    let id = ids[rng.next() as usize % ids.len()];
                    let want = SchedLimits {
                        max_prio: (rng.next() % 66) as u8,
                        rt_budget: rng.next() % 150_000_000,
                        cpu_weight: (rng.next() % 70_000) as u32,
                    };
                    let before: Vec<_> = ids.iter().map(|&j| jobs.sched(j)).collect();
                    let parent_s = jobs.parent(id).and_then(|p| jobs.sched(p));
                    let r = jobs.set_sched(id, want);
                    let bad_value = want.max_prio > 63
                        || !(1..=65536).contains(&want.cpu_weight)
                        || want.rt_budget > 100_000_000;
                    let above = parent_s.is_some_and(|p| {
                        want.max_prio > p.max_prio || want.rt_budget > p.rt_budget
                    });
                    match r {
                        Err(SchedError::Invalid) => assert!(bad_value, "{what}"),
                        Err(SchedError::AboveParent) => assert!(!bad_value && above, "{what}"),
                        Err(e) => panic!("{what}: {e:?}"),
                        Ok(()) => assert!(
                            !bad_value && !above,
                            "{what}: accepted a value it should refuse"
                        ),
                    }
                    if r.is_err() {
                        let after: Vec<_> = ids.iter().map(|&j| jobs.sched(j)).collect();
                        assert_eq!(before, after, "{what}: a refusal changes nothing");
                    }
                }
            }
            // PR1 and PR2 for every job
            for &j in &ids {
                let s = jobs.sched(j).unwrap();
                if let Some(p) = jobs.parent(j) {
                    let ps = jobs.sched(p).unwrap();
                    assert!(
                        s.max_prio <= ps.max_prio && s.rt_budget <= ps.rt_budget,
                        "{what}: job {j:?} exceeds its parent"
                    );
                }
            }
        }
    }
}

#[test]
fn lowering_a_parent_lowers_its_whole_subtree() {
    let mut jobs = Jobs::new();
    let a = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let b = jobs.create(a, Limits::UNLIMITED).unwrap();
    assert_eq!(jobs.sched(b).unwrap().max_prio, 63);
    jobs.set_sched(
        a,
        SchedLimits {
            max_prio: 20,
            rt_budget: 0,
            cpu_weight: 1024,
        },
    )
    .unwrap();
    assert_eq!(
        (
            jobs.sched(a).unwrap().max_prio,
            jobs.sched(b).unwrap().max_prio
        ),
        (20, 20)
    );
    assert_eq!(jobs.sched(b).unwrap().rt_budget, 0);
    // a child cannot be raised above its parent afterwards
    assert_eq!(
        jobs.set_sched(
            b,
            SchedLimits {
                max_prio: 21,
                rt_budget: 0,
                cpu_weight: 1024
            }
        ),
        Err(SchedError::AboveParent)
    );
    assert!(jobs
        .set_sched(
            b,
            SchedLimits {
                max_prio: 5,
                rt_budget: 0,
                cpu_weight: 1
            }
        )
        .is_ok());
    jobs.release(b); // the last holder: the id is gone
    assert_eq!(
        jobs.set_sched(b, SchedLimits::ROOT),
        Err(SchedError::NoSuchJob)
    );
}
