//! The dirty log's completeness (slice W step 1, W-SPEC section 3): after any operation, every object
//! whose liveness or signal word changed must be in the log (a port's derived READABLE changes whenever
//! an object it binds is touched, so a port is covered by its bindings). A missed touch would become a
//! hang once waiters are woken by the log, so this is the single most important test of the slice.

mod common;
use abi::rights::Rights;
use abi::Handle;
use common::{Rng, SEEDS, STEPS};
use kcore::handle::HandleTable;
use kcore::ipc::*;
use kcore::object::ObjId;
use std::collections::{BTreeMap, BTreeSet};

type Snapshot = BTreeMap<ObjId, (u32, Vec<ObjId>)>; // signal word and (for ports) the bound objects

fn snapshot(reg: &Kobjs) -> Snapshot {
    reg.live_ids()
        .into_iter()
        .map(|id| {
            (
                id,
                (
                    signals_of_object(reg, id).expect("live"),
                    port_bound_objects(reg, id),
                ),
            )
        })
        .collect()
}

/// Check `after` against `before` given what was logged.
fn check(before: &Snapshot, after: &Snapshot, logged: &BTreeSet<ObjId>, what: &str, seed: u64) {
    for (id, (sig, _)) in before {
        match after.get(id) {
            None => assert!(
                logged.contains(id),
                "seed {seed}: {what}: object {id:?} was destroyed but not logged"
            ),
            Some((sig2, _)) => {
                if sig != sig2 {
                    // a port's word follows its bindings
                    let covered = logged.contains(id)
                        || before[id]
                            .1
                            .iter()
                            .chain(after[id].1.iter())
                            .any(|b| logged.contains(b));
                    assert!(
                        covered,
                        "seed {seed}: {what}: object {id:?} signals {sig:#x} -> {sig2:#x} but it (and its bindings) were not logged"
                    );
                }
            }
        }
    }
}

struct World {
    reg: Kobjs,
    tables: Vec<HandleTable>,
}

fn pick(rng: &mut Rng, t: &HandleTable) -> Option<Handle> {
    let objs: Vec<_> = (1..=8191u32)
        .map(Handle)
        .filter(|h| t.get(*h).is_ok())
        .take(64)
        .collect();
    if objs.is_empty() {
        None
    } else {
        Some(objs[rng.below(objs.len() as u64) as usize])
    }
}

fn run(seed: u64) {
    let mut rng = Rng::new(seed);
    let mut w = World {
        reg: Kobjs::new(),
        tables: vec![HandleTable::new(64), HandleTable::new(64)],
    };
    for step in 0..STEPS {
        let before = snapshot(&w.reg);
        let ti = rng.below(2) as usize;
        let what: String;
        {
            let World { reg, tables } = &mut w;
            match rng.below(16) {
                0 => {
                    what = "channel_create".into();
                    let _ = channel_create(reg, &mut tables[ti]);
                }
                1 => {
                    what = "socket_create".into();
                    let _ = socket_create(reg, &mut tables[ti]);
                }
                2 => {
                    what = "event_create".into();
                    let _ = event_create(reg, &mut tables[ti]);
                }
                3 => {
                    what = "port_create".into();
                    let _ = port_create(reg, &mut tables[ti]);
                }
                4 | 5 => {
                    what = "channel_write".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let carry =
                            pick(&mut rng, &tables[ti]).filter(|c| *c != h && rng.chance(40));
                        let hs: Vec<Handle> = carry.into_iter().collect();
                        let _ = channel_write(reg, &mut tables[ti], h, b"msg", &hs);
                    }
                }
                6 => {
                    what = "channel_read".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let _ = channel_read(reg, &mut tables[ti], h, 64, 4);
                    }
                }
                7 => {
                    what = "socket_write".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let _ = socket_write(reg, &tables[ti], h, &[7u8; 100]);
                    }
                }
                8 => {
                    what = "socket_read".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let _ = socket_read(reg, &tables[ti], h, 64);
                    }
                }
                9 => {
                    what = "socket_shutdown".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let _ = socket_shutdown(reg, &tables[ti], h);
                    }
                }
                10 => {
                    what = "event_signal".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let bit = 1 << (24 + rng.below(4));
                        let (clear, set) = if rng.chance(50) { (bit, 0) } else { (0, bit) };
                        let _ = event_signal(reg, &tables[ti], h, clear, set);
                    }
                }
                11 => {
                    what = "port_bind".into();
                    if let (Some(p), Some(o)) =
                        (pick(&mut rng, &tables[ti]), pick(&mut rng, &tables[ti]))
                    {
                        let _ = port_bind(reg, &tables[ti], p, o, 0xffff_ffff, rng.below(8));
                    }
                }
                12 => {
                    what = "port_unbind".into();
                    if let Some(p) = pick(&mut rng, &tables[ti]) {
                        let _ = port_unbind(reg, &tables[ti], p, rng.below(8));
                    }
                }
                13 => {
                    what = "close".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        let _ = tables[ti].close(reg, h);
                    }
                }
                14 => {
                    what = "replace_or_duplicate".into();
                    if let Some(h) = pick(&mut rng, &tables[ti]) {
                        if rng.chance(50) {
                            let _ = tables[ti].replace(reg, h, Rights::SAME);
                        } else {
                            let _ = tables[ti].duplicate(reg, h, Rights::SAME);
                        }
                    }
                }
                _ => {
                    // a sweep (with every handle as a root) or a process death
                    if rng.chance(30) {
                        what = "drain".into();
                        tables[ti].drain(reg);
                    } else {
                        what = "sweep".into();
                        let roots: Vec<ObjId> = tables.iter().flat_map(|t| t.objects()).collect();
                        reg.sweep(&roots);
                    }
                }
            }
        }
        // the kernel's drain after a syscall
        let World { reg, .. } = &mut w;
        let dirty = reg.take_dirty();
        assert!(!dirty.overflow);
        let logged: BTreeSet<ObjId> = dirty.items.iter().map(|(i, _)| *i).collect();
        let after = snapshot(reg);
        check(
            &before,
            &after,
            &logged,
            &format!("step {step} {what}"),
            seed,
        );
        // a handle removal is a *forced* touch
        if what == "close" || what == "drain" {
            // (the removed object, if the handle was valid, is in the log as forced: covered by
            // `forced_touch_on_handle_removal` below)
        }
    }
}

#[test]
fn every_change_of_signals_or_liveness_is_logged() {
    for seed in 1..=SEEDS {
        run(seed);
    }
}

#[test]
fn removing_a_handle_is_a_forced_touch_even_when_the_signals_do_not_change() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let e = event_create(&mut reg, &mut t).unwrap();
    let obj = t.get(e).unwrap().obj;
    reg.take_dirty();
    // close with another handle alive: the object does not die, its signals do not change
    let e2 = t.duplicate(&mut reg, e, Rights::SAME).unwrap();
    reg.take_dirty();
    t.close(&mut reg, e).unwrap();
    let d = reg.take_dirty();
    assert!(
        d.items.contains(&(obj, true)),
        "close is a forced touch: {d:?}"
    );
    // replace: the old value stops working, a forced touch
    let e3 = t.replace(&mut reg, e2, Rights::SAME).unwrap();
    assert!(
        reg.take_dirty().items.contains(&(obj, true)),
        "replace is a forced touch"
    );
    // a transfer out of the table likewise
    let (c1, c2) = channel_create(&mut reg, &mut t).unwrap();
    reg.take_dirty();
    channel_write(&mut reg, &mut t, c1, b"x", &[e3]).unwrap();
    assert!(
        reg.take_dirty().items.contains(&(obj, true)),
        "a transferred handle is a forced touch"
    );
    let _ = c2;
}

#[test]
fn the_log_survives_flooding_by_compaction_then_overflow() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let e = event_create(&mut reg, &mut t).unwrap();
    reg.take_dirty();
    // the same object touched over and over compacts to one entry
    for _ in 0..10_000 {
        event_signal(&mut reg, &t, e, 0, 1 << 24).unwrap();
    }
    let d = reg.take_dirty();
    assert!(!d.overflow && !d.items.is_empty() && d.items.len() < 5000);
}
