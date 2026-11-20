//! The host fuzzer of the syscall layer (KT8): sequences of calls from `kfuzz-gen` (70 % valid, 25 % near-valid, 5 % garbage) run against
//! `ksys` over the fake machine, with the audits after every call. No call may panic; the frame database must stay consistent; every
//! handle must name a live object; and the job ledger must equal what is actually held (each job's object usage is the number of live
//! objects charged to it).
//!
//! `KFUZZ_SEEDS` (default `0..16`, `A..B` or `A,B,C`) and `KFUZZ_CALLS` (default 400 per sequence) size the run; the nightly run uses
//! thousands of seeds. A failure names the seed, so `KFUZZ_SEEDS=<seed> cargo test -p ksys --test fuzz` replays it exactly. Under Miri
//! the default is two short sequences.
use abi::rights::Rights;
use kcore::ipc::{self, KObj, Quota};
use kcore::job::{JobId, Resource};
use kfuzz_gen::{Gen, Kind, Model, Window};

mod common;
use common::*;

fn seeds() -> Vec<u64> {
    let default = if cfg!(miri) { "0..2" } else { "0..16" };
    let text = std::env::var("KFUZZ_SEEDS").unwrap_or_else(|_| default.to_string());
    if let Some((a, b)) = text.split_once("..") {
        (a.parse::<u64>().unwrap()..b.parse::<u64>().unwrap()).collect()
    } else {
        text.split(',').map(|x| x.parse().unwrap()).collect()
    }
}

fn calls_per_sequence() -> usize {
    std::env::var("KFUZZ_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(miri) { 60 } else { 400 })
}

/// Every audit; the first problem found.
fn audit(w: &mut World) -> Result<(), String> {
    w.mm.db.audit()?;
    for o in w.handles.objects() {
        if !w.objs.is_alive(o) {
            return Err(format!("a handle names the dead object {o:?}"));
        }
    }
    // the ledger: after the credits of destroyed objects are applied, a job's object usage is exactly the live objects charged to it or to a
    // job below it (a charge walks up to every ancestor)
    ipc::settle(&mut w.objs, &mut w.vmos.jobs);
    let mut charged: Vec<JobId> = Vec::new();
    let mut jobs = vec![JobId::ROOT, w.job];
    for id in w.objs.live_ids() {
        if let Some(KObj::Job(j)) = w.objs.get(id) {
            jobs.push(*j);
        }
        if let Some(j) = w.objs.job_of(id) {
            charged.push(j);
            jobs.push(j);
        }
    }
    jobs.sort_by_key(|j| format!("{j:?}"));
    jobs.dedup();
    for j in jobs {
        let held = charged
            .iter()
            .filter(|&&c| w.vmos.jobs.is_within(c, j))
            .count() as u64;
        let used = w.vmos.jobs.usage(j).map_or(0, |u| u.get(Resource::Objects));
        if used != held {
            return Err(format!("job {j:?}: the ledger says {used} objects, {held} are alive and charged to it or below"));
        }
    }
    Ok(())
}

fn run_seed(seed: u64, calls: usize) -> Result<(), String> {
    let mut w = world_with(48);
    // a handle to the root job, to give the job calls something to act on
    let jh = {
        let mut q = Quota {
            jobs: &mut w.vmos.jobs,
            job: Some(JobId::ROOT),
        };
        u64::from(
            ipc::job_object_create(
                &mut w.objs,
                &mut w.handles,
                JobId::ROOT,
                Rights::JOB,
                &mut q,
            )
            .map_err(|e| format!("{e:?}"))?
            .0,
        )
    };
    let mut model = Model::with(&[(jh, Kind::Job)]);
    let window = Window {
        base: BASE,
        len: 0x10000,
        read_only: RO,
    };
    let mut gen = Gen::new(seed, window);
    for i in 0..calls {
        let call = gen.next(&model);
        for (at, bytes) in &call.pokes {
            if *at >= BASE && at + bytes.len() as u64 <= BASE + 0x10000 {
                // (the pokes can fail once the fake machine is out of frames: the call then runs on whatever the buffer held)
                let _ = kcore::usercopy::copy_to_user(
                    &mut w.aspace,
                    &mut w.mm,
                    &mut w.vmos,
                    *at,
                    bytes,
                );
            }
        }
        let r = w.call(call.nr, call.args);
        let (ok, a1, a2) = match &r {
            Some(Ok((a1, a2))) => (true, *a1, *a2),
            _ => (false, None, None),
        };
        if std::env::var("KFUZZ_TRACE").is_ok() {
            eprintln!("{i}: {call:?} -> {r:?}");
        }
        model.observe(&call, ok, a1, a2);
        if std::env::var("KFUZZ_TRACE").is_ok() {
            ipc::settle(&mut w.objs, &mut w.vmos.jobs);
            let used = w
                .vmos
                .jobs
                .usage(JobId::ROOT)
                .map_or(0, |u| u.get(Resource::Objects));
            let held = w
                .objs
                .live_ids()
                .into_iter()
                .filter(|&id| w.objs.job_of(id).is_some())
                .count();
            let live = w.objs.live();
            eprintln!(
                "   root used {used} held {held} live {live} handles {}",
                w.handles.len()
            );
        }
        audit(&mut w).map_err(|e| format!("seed {seed}, call {i} ({:?}): {e}", call))?;
    }
    Ok(())
}

#[test]
fn sequences_of_calls_keep_every_audit_clean() {
    let calls = calls_per_sequence();
    let mut failures = Vec::new();
    for seed in seeds() {
        // (a panic is a finding too: name its seed so it replays)
        match std::panic::catch_unwind(|| run_seed(seed, calls)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failures.push(e),
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                failures.push(format!("seed {seed}: a call panicked: {msg}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing seeds; the first: {}",
        failures.len(),
        failures[0]
    );
}

#[test]
fn a_seed_replays_to_the_same_result() {
    let a = run_seed(5, 200).map_err(|e| e.to_string());
    let b = run_seed(5, 200).map_err(|e| e.to_string());
    assert_eq!(a, b);
}
