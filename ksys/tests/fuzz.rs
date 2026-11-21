//! The host fuzzer of the syscall layer (KT8): sequences of calls from `kfuzz-gen` (70 % valid, 25 % near-valid, 5 % garbage) run against
//! `ksys` over the fake machine, doing around each call what the kernel does (the wide-argument check first, the cleanup of dead objects and
//! the sweep of unreachable ones after), with audits after every call:
//!
//! * no call may panic (the panic is attributed to a phase and a call index);
//! * `FrameDb::audit` holds, every handle names a live object, a peer is alive and points back;
//! * every live object's reference count equals the handles that name it plus the references held by live objects (a leaked or a missing
//!   `retain`/`release` shows here);
//! * the job ledger equals what is held: each job's object usage is the number of live objects charged to it or to a job below it, and its queued
//!   bytes the sum of what those objects hold;
//!
//! and at the end of every sequence, after every handle is closed and the sweep has run, nothing is left alive or charged.
//!
//! `KFUZZ_SEEDS` (default `0..16`, `A..B` or `A,B,C`) and `KFUZZ_CALLS` (default 400 per sequence) size the run; the overnight run uses
//! thousands of seeds. A failure prints the exact command that replays it. Under Miri the default is two short sequences.
use abi::rights::Rights;
use abi::Handle;
use kcore::ipc::{self, KObj, Quota};
use kcore::job::JobId;
use kfuzz_gen::{Gen, Kind, Model, Window};
use std::cell::Cell;
use std::hash::{Hash, Hasher};

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

fn replay(seed: u64, calls: usize) -> String {
    format!("replay: KFUZZ_SEEDS={seed} KFUZZ_CALLS={calls} cargo test --profile release-checked -p ksys --test fuzz sequences (KFUZZ_TRACE=1 prints every call)")
}

/// The kind the model should give a handle the process just received.
fn kind_of(w: &World, h: u64) -> Kind {
    let Ok(e) = w.handles.get(Handle(h)) else {
        return Kind::Any;
    };
    match w.objs.get(e.obj) {
        Some(KObj::Channel(_)) => Kind::Channel,
        Some(KObj::Socket(_)) => Kind::Socket,
        Some(KObj::Event(_)) => Kind::Event,
        Some(KObj::Port(_)) => Kind::Port,
        Some(KObj::Vmo(_)) => Kind::Vmo,
        Some(KObj::Job(_)) => Kind::Job,
        _ => Kind::Any,
    }
}

/// One sequence. Returns a hash of every call and its reply (two runs of a seed must agree), or the first problem.
fn run_seed(seed: u64, calls: usize, at: &Cell<(&'static str, usize)>) -> Result<u64, String> {
    at.set(("setup", 0));
    let mut w = world_with(256);
    // a handle to the root job, to give the job calls something to act on (the generator never closes it or sets limits on it)
    let jh = {
        let mut q = Quota {
            jobs: &mut w.vmos.jobs,
            job: Some(JobId::ROOT),
        };

        ipc::job_object_create(
            &mut w.objs,
            &mut w.handles,
            JobId::ROOT,
            Rights::JOB,
            &mut q,
        )
        .map_err(|e| format!("{e:?}"))?
        .0
    };
    let mut model = Model::with(&[(jh, Kind::Job)]);
    model.pin(jh);
    let window = Window {
        base: BASE,
        len: 0x10000,
        read_only: RO,
    };
    let mut gen = Gen::new(seed, window);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for i in 0..calls {
        at.set(("generate", i));
        let call = gen.next(&model);
        // every so often the process acts for a job it holds (a child job: its limits and its ancestors' now matter), or for its own again
        if i % 40 == 39 {
            let jobs: Vec<u64> = model
                .live()
                .iter()
                .filter(|&&(_, k)| k == Kind::Job)
                .map(|&(h, _)| h)
                .collect();
            w.job = jobs
                .get(i / 40 % (jobs.len() + 1))
                .and_then(|&h| ipc::job_of(&w.objs, &w.handles, Handle(h), Rights::MANAGE).ok())
                .unwrap_or(JobId::ROOT);
        }
        at.set(("poke", i));
        for (p, bytes) in &call.pokes {
            if *p >= BASE && p + bytes.len() as u64 <= BASE + 0x10000 {
                // (the pokes can fail once the fake machine is out of frames: the call then runs on whatever the buffer held)
                let _ =
                    kcore::usercopy::copy_to_user(&mut w.aspace, &mut w.mm, &mut w.vmos, *p, bytes);
            }
        }
        at.set(("call", i));
        let r = match ksys::wide_argument_error(call.nr, &call.args) {
            Some(st) => Some(Err((st, None, None))), // refused before it runs, as in the kernel
            None => w.call(call.nr, call.args),
        };
        format!("{} {:?} {r:?}", call.nr, call.args).hash(&mut hasher);
        let (ok, a1, a2) = match &r {
            Some(Ok((a1, a2))) => (true, *a1, *a2),
            _ => (false, None, None),
        };
        if std::env::var("KFUZZ_TRACE").is_ok() {
            eprintln!("{i}: {call:?} -> {r:?}");
        }
        at.set(("observe", i));
        model.observe(&call, ok, a1, a2);
        if ok
            && matches!(
                call.nr,
                abi::syscall::CHANNEL_READ | abi::syscall::CHANNEL_READ_TXID
            )
        {
            let (list, n) = (
                if call.nr == abi::syscall::CHANNEL_READ {
                    call.args[3]
                } else {
                    call.args[4]
                },
                a2.unwrap_or(0) as usize,
            );
            if n > 0 {
                if let Ok(raw) = kcore::usercopy::copy_from_user(
                    &mut w.aspace,
                    &mut w.mm,
                    &mut w.vmos,
                    list,
                    n * 4,
                ) {
                    let got: Vec<(u64, Kind)> = raw
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u64::from(u32::from_le_bytes(*c)))
                        .map(|h| (h, kind_of(&w, h)))
                        .collect();
                    model.received(&got);
                }
            }
        }
        at.set(("cleanup", i));
        cleanup(&mut w);
        at.set(("audit", i));
        audit(&mut w).map_err(|e| format!("seed {seed}, call {i} ({call:?}): {e}"))?;
    }
    at.set(("baseline", calls));
    w.job = JobId::ROOT;
    baseline(&mut w).map_err(|e| format!("seed {seed}, at the end: {e}"))?;
    (w.objs.live(), w.handles.len()).hash(&mut hasher);
    Ok(hasher.finish())
}

#[test]
fn sequences_of_calls_keep_every_audit_clean() {
    let calls = calls_per_sequence();
    let mut failures = Vec::new();
    for seed in seeds() {
        let at = Cell::new(("start", 0));
        // (a panic is a finding too: name its seed, phase and call so it replays; a panic in the harness's own phases says so)
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_seed(seed, calls, &at)))
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => failures.push(format!("{e}\n  {}", replay(seed, calls))),
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                let (phase, i) = at.get();
                let who = if phase == "call" {
                    "the syscall layer"
                } else {
                    "the harness"
                };
                failures.push(format!(
                    "seed {seed}: {who} panicked in phase {phase} at call {i}: {msg}\n  {}",
                    replay(seed, calls)
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing seeds:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn a_seed_replays_to_the_same_hash_and_another_seed_does_not() {
    let at = Cell::new(("", 0));
    let a = run_seed(5, 200, &at).unwrap();
    let b = run_seed(5, 200, &at).unwrap();
    let c = run_seed(6, 200, &at).unwrap();
    assert_eq!(a, b, "the same seed, the same calls and replies");
    assert_ne!(a, c);
}
