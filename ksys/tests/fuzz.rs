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
use kcore::object::ObjId;
use kfuzz_gen::{Gen, Kind, Model, Window};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

mod common;
use common::iohost::{Host, IoMem, BOOT_FRAMES};
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
        Some(KObj::Resource) => Kind::Resource,
        Some(KObj::IoDomain(_)) => Kind::IoDomain,
        _ => Kind::Any,
    }
}

/// One sequence. Returns a hash of every call and its reply (two runs of a seed must agree), or the first problem.
fn run_seed(seed: u64, calls: usize, at: &Cell<(&'static str, usize)>) -> Result<u64, String> {
    at.set(("setup", 0));
    // (one seed in four runs short of frames, so a commit that cannot get one rolls back: T176)
    // (IOMMU6: on the IOMMU host, whose bring-up takes `BOOT_FRAMES` of the world's frames)
    let world = if seed % 4 == 3 {
        world_with_frames(256, BOOT_FRAMES + 24 + (seed as usize / 4) % 20)
    } else {
        world_with_frames(256, BOOT_FRAMES + 400)
    };
    let iomem = IoMem::default();
    let mut io = Host::with_world(&iomem, world);
    let mut waits = kcore::wait::WaitTable::new();
    let mut masks: BTreeMap<u64, (ObjId, u32)> = BTreeMap::new(); // what each registered thread waits for
    for t in 1..=WAITERS {
        waits.admit(t).map_err(|_| "admit".to_string())?;
    }
    // a handle to the root job, to give the job calls something to act on (the generator never closes it or sets limits on it)
    let jh = {
        let mut q = Quota {
            jobs: &mut io.w.jobs,
            job: Some(JobId::ROOT),
        };

        ipc::job_object_create(
            &mut io.w.objs,
            &mut io.w.handles,
            JobId::ROOT,
            Rights::JOB,
            &mut q,
        )
        .map_err(|e| format!("{e:?}"))?
        .0
    };
    // a root resource, so the device calls can make domains and DMA VMOs (IOMMU6)
    let res = io.resource();
    let mut model = Model::with(&[(jh, Kind::Job), (res, Kind::Resource)]);
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
            io.w.job = jobs
                .get(i / 40 % (jobs.len() + 1))
                .and_then(|&h| {
                    ipc::job_of(&io.w.objs, &io.w.handles, Handle(h), Rights::MANAGE).ok()
                })
                .unwrap_or(JobId::ROOT);
        }
        at.set(("poke", i));
        for (p, bytes) in &call.pokes {
            if *p >= BASE && p + bytes.len() as u64 <= BASE + 0x10000 {
                // (the pokes can fail once the fake machine is out of frames: the call then runs on whatever the buffer held)
                let _ = kcore::usercopy::copy_to_user(
                    &mut io.w.aspace,
                    &mut io.w.mm,
                    &mut io.w.vmos,
                    &mut io.w.jobs,
                    *p,
                    bytes,
                );
            }
        }
        at.set(("call", i));
        // (T176 6: what the call may have changed is in the dirty log, which the kernel's wake-up pass reads: a signal word that moved, or an object that went, with no entry would never wake its waiters)
        let before: Vec<(ObjId, u32)> =
            io.w.objs
                .live_ids()
                .into_iter()
                .filter_map(|o| ipc::signals_of_object(&io.w.objs, o).map(|s| (o, s)))
                .collect();
        let r = match ksys::wide_argument_error(call.nr, &call.args) {
            Some(st) => Some(Err((st, None, None))), // refused before it runs, as in the kernel
            None => io.dispatch(call.nr, call.args),
        };
        format!("{} {:?} {r:?}", call.nr, call.args).hash(&mut hasher);
        let (ok, a1, a2) = match &r {
            Some(Ok((a1, a2))) => (true, *a1, *a2),
            _ => (false, None, None),
        };
        if std::env::var("KFUZZ_TRACE").is_ok() {
            eprintln!("{i}: {call:?} -> {r:?}");
        }
        at.set(("dirty", i));
        {
            let mut logged: std::collections::HashSet<ObjId> =
                io.w.objs.peek_dirty().iter().map(|&(o, _)| o).collect();
            // (a port's READABLE follows what it binds: the wait table releases a port's waiters when something it binds is touched, L21a)
            for (o, _) in &before {
                if ipc::port_bound_objects(&io.w.objs, *o)
                    .iter()
                    .any(|b| logged.contains(b))
                {
                    logged.insert(*o);
                }
            }
            for (o, was) in &before {
                let now = ipc::signals_of_object(&io.w.objs, *o);
                // (a signal that is only cleared wakes nobody: a waiter wants a bit that is set)
                if now.is_none_or(|n| n & !*was != 0)
                    && !logged.contains(o)
                    && !io.w.objs.dirty_overflowed()
                {
                    return Err(format!(
                        "seed {seed}, call {i} ({call:?}): {o:?} gained signals ({was:#x} to {now:?}) or died and the dirty log does not name it"
                    ));
                }
            }
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
                    &mut io.w.aspace,
                    &mut io.w.mm,
                    &mut io.w.vmos,
                    &mut io.w.jobs,
                    list,
                    n * 4,
                ) {
                    let got: Vec<(u64, Kind)> = raw
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u64::from(u32::from_le_bytes(*c)))
                        .map(|h| (h, kind_of(&io.w, h)))
                        .collect();
                    model.received(&got);
                }
            }
        }
        // (T176 2: `port_wait` lives in the kernel, so the harness polls the ports the process holds the way the wait does, one binding or a batch: the cursor,
        // the one-shot disarm and the pruning of bindings whose object went all run, and what comes back must be a delivered signal)
        if i % 5 == 2 {
            at.set(("poll", i));
            let ports: Vec<u64> = model
                .live()
                .iter()
                .filter(|&&(_, k)| k == Kind::Port)
                .map(|&(h, _)| h)
                .collect();
            if let Some(&p) = ports.get(i / 5 % ports.len().max(1)) {
                if i % 10 == 2 {
                    if let Ok(Some((key, seen))) =
                        ipc::port_poll_in(&mut io.w.objs, &io.w.handles, Handle(p), &mut io.w.jobs)
                    {
                        if seen == 0 {
                            return Err(format!("seed {seed}, call {i}: a poll of port {p} delivered key {key} with no signal"));
                        }
                    }
                } else if let Ok(got) = ipc::port_poll_many_in(
                    &mut io.w.objs,
                    &io.w.handles,
                    Handle(p),
                    &mut io.w.jobs,
                    1 + i % 4,
                ) {
                    if got.len() > 1 + i % 4 || got.iter().any(|&(_, seen)| seen == 0) {
                        return Err(format!(
                            "seed {seed}, call {i}: a batch poll of port {p} gave {got:?}"
                        ));
                    }
                }
            }
        }
        at.set(("cleanup", i));
        let dirty = cleanup_keep(&mut io.w);
        // (the kernel's tail: a dead domain's destruction is begun and completed)
        io.settle_domains();
        at.set(("waits", i));
        // (T176 7: the threads of the wait table, which the kernel keeps outside `ksys`, wait on what the fuzzer makes and breaks)
        check_waits(&mut io.w, &mut waits, &mut masks, dirty, seed, i)?;
        at.set(("audit", i));
        audit(&mut io.w).map_err(|e| format!("seed {seed}, call {i} ({call:?}): {e}"))?;
    }
    at.set(("baseline", calls));
    // (what is left waiting is released by the destruction of everything: one last commit, then every thread leaves)
    let dirty = cleanup_keep(&mut io.w);
    check_waits(&mut io.w, &mut waits, &mut masks, dirty, seed, calls)?;
    for t in 1..=WAITERS {
        waits.forget(t);
        waits.retire(t);
    }
    waits
        .audit(|o| io.w.objs.is_alive(o))
        .map_err(|e| format!("seed {seed}, after the waiters left: {e}"))?;
    io.w.job = JobId::ROOT;
    // (every handle closed and the sweep run, the dead domains are destroyed before the baseline counts the frames)
    io.w.handles.drain(&mut io.w.objs);
    cleanup(&mut io.w);
    io.w.objs.sweep(&[]);
    cleanup(&mut io.w);
    io.settle_domains();
    let st = io.store.stats();
    if (st.domains, st.io_pages, st.holds_leaked) != (0, 0, 0) || !io.timeouts.is_empty() {
        return Err(format!(
            "seed {seed}, at the end: domains {}, I/O-mapped pages {}, leaked holds {}, fence timeouts {:?}",
            st.domains, st.io_pages, st.holds_leaked, io.timeouts
        ));
    }
    baseline(&mut io.w).map_err(|e| format!("seed {seed}, at the end: {e}"))?;
    (io.w.objs.live(), io.w.handles.len()).hash(&mut hasher);
    Ok(hasher.finish())
}

/// How many threads wait on what the fuzzer makes.
const WAITERS: u64 = 6;

/// The kernel's commit to the waiters after a call, and what must hold of it (W3, W4, as `kcore/tests/wait_model.rs` checks them over the IPC layer
/// alone): every woken thread had a cause, none that is left has a true predicate, a dead object or a removed handle, and the table's own audit. Then
/// some idle threads start waiting, only when their predicate is false, on an object that is not a port (a port's baton is that model's).
fn check_waits(
    w: &mut World,
    waits: &mut kcore::wait::WaitTable,
    masks: &mut BTreeMap<u64, (ObjId, u32)>,
    dirty: kcore::object::Dirty,
    seed: u64,
    i: usize,
) -> Result<(), String> {
    let forced: BTreeSet<ObjId> = dirty
        .items
        .iter()
        .filter(|(_, f)| *f)
        .map(|(o, _)| *o)
        .collect();
    let overflow = dirty.overflow;
    let woken = {
        let objs = &w.objs;
        waits.commit(
            |id| ipc::signals_of_object(objs, id),
            |id, f| objs.for_each_binder(id, f),
            |_, _| false,
            dirty,
        )
    };
    for t in &woken {
        let Some((obj, mask)) = masks.remove(t) else {
            return Err(format!(
                "seed {seed}, call {i}: thread {t} was woken but was not waiting"
            ));
        };
        let sig = ipc::signals_of_object(&w.objs, obj);
        if !(overflow || sig.is_none_or(|s| s & mask != 0) || forced.contains(&obj)) {
            return Err(format!(
                "seed {seed}, call {i}: thread {t} was woken without cause (signals {sig:?}, mask {mask:#x})"
            ));
        }
        if waits.is_registered(*t) {
            return Err(format!(
                "seed {seed}, call {i}: woken thread {t} is still registered"
            ));
        }
    }
    waits.recycle(woken);
    for (t, (obj, mask)) in masks.iter() {
        if !waits.is_registered(*t) {
            return Err(format!(
                "seed {seed}, call {i}: thread {t} lost its registration without a wake"
            ));
        }
        if forced.contains(obj) {
            return Err(format!(
                "seed {seed}, call {i}: thread {t} still waits on {obj:?} though a handle to it was removed"
            ));
        }
        match ipc::signals_of_object(&w.objs, *obj) {
            None => {
                return Err(format!("seed {seed}, call {i}: thread {t} still waits on a dead object"))
            }
            Some(s) if s & mask != 0 => {
                return Err(format!(
                    "seed {seed}, call {i}: thread {t} was not woken though its signals {s:#x} match {mask:#x}"
                ))
            }
            Some(_) => {}
        }
    }
    waits
        .audit(|o| w.objs.is_alive(o))
        .map_err(|e| format!("seed {seed}, call {i}: the wait table: {e}"))?;
    // some idle threads start waiting
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (i as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
        | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for t in 1..=WAITERS {
        if masks.contains_key(&t) || next() % 100 >= 30 {
            continue;
        }
        let ids: Vec<ObjId> = w
            .objs
            .live_ids()
            .into_iter()
            .filter(|&id| !matches!(w.objs.get(id), Some(KObj::Port(_))))
            .collect();
        if ids.is_empty() {
            continue;
        }
        let obj = ids[(next() % ids.len() as u64) as usize];
        let mask = 1u32 << (next() % 4);
        if ipc::signals_of_object(&w.objs, obj).is_some_and(|s| s & mask == 0) {
            waits.register(t, obj, mask);
            masks.insert(t, (obj, mask));
        }
    }
    Ok(())
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
    let calls = if cfg!(miri) { 30 } else { 200 };
    let a = run_seed(5, calls, &at).unwrap();
    let b = run_seed(5, calls, &at).unwrap();
    let c = run_seed(6, calls, &at).unwrap();
    assert_eq!(a, b, "the same seed, the same calls and replies");
    assert_ne!(a, c);
}
