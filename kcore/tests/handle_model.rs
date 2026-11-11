//! The handle table against a reference model, plus adversarial values.
mod common;
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use common::{Rng, SEEDS, STEPS};
use kcore::handle::HandleTable;
use kcore::ipc::{event_create, event_signal, Kobjs};
use std::collections::HashMap;

fn random_rights(rng: &mut Rng) -> Rights {
    match rng.below(6) {
        0 => Rights::SAME,
        1 => Rights(rng.below(0x400) as u32), // any valid subset
        2 => Rights(rng.below(0x400) as u32 | 1 << 20), // an unassigned bit
        3 => Rights::NONE,
        4 => Rights::EVENT,
        _ => Rights::ALL,
    }
}

#[test]
fn random_operations_agree_with_the_model() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut reg = Kobjs::new();
        let limit = 3 + rng.below(30) as usize;
        let mut table = HandleTable::new(limit);
        // model: live handle -> (object number, rights); object number -> reference count
        let mut live: HashMap<Handle, (u32, Rights)> = HashMap::new();
        let mut refs: HashMap<u32, u32> = HashMap::new();
        let mut closed: Vec<Handle> = Vec::new();
        let mut next_obj = 0u32;
        let mut ids = HashMap::new(); // object number -> ObjId, found through the table

        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            let mut handles: Vec<Handle> = live.keys().copied().collect();
            handles.sort_by_key(|h| h.0); // HashMap order is random; a failing seed must replay
            let pick = |rng: &mut Rng| handles[rng.below(handles.len() as u64) as usize];
            match rng.below(8) {
                0 | 1 => {
                    // create an event: the table inserts with the event's default rights
                    let result = event_create(&mut reg, &mut table);
                    if live.len() >= limit {
                        assert_eq!(result, Err(Status::NoMemory), "{ctx}");
                    } else {
                        let h = result.unwrap_or_else(|e| panic!("{ctx}: {e:?}"));
                        assert!(h.0 != 0, "{ctx}: zero handle");
                        assert!(
                            live.insert(h, (next_obj, Rights::EVENT)).is_none(),
                            "{ctx}: handle value reused while live"
                        );
                        refs.insert(next_obj, 1);
                        ids.insert(next_obj, table.get(h).unwrap().obj);
                        next_obj += 1;
                    }
                }
                2 if !handles.is_empty() => {
                    let h = pick(&mut rng);
                    assert_eq!(table.close(&mut reg, h), Ok(()), "{ctx}");
                    let (o, _) = live.remove(&h).unwrap();
                    *refs.get_mut(&o).unwrap() -= 1;
                    closed.push(h);
                }
                3 | 4 if !handles.is_empty() => {
                    let h = pick(&mut rng);
                    let want = random_rights(&mut rng);
                    let (o, have) = live[&h];
                    let result = table.duplicate(&mut reg, h, want);
                    let expect = if !have.contains(Rights::DUPLICATE) {
                        Err(Status::AccessDenied)
                    } else {
                        let new = if want == Rights::SAME {
                            Some(have)
                        } else if want.is_valid() && want.is_subset_of(have) {
                            Some(want)
                        } else {
                            None
                        };
                        match new {
                            None => Err(Status::AccessDenied),
                            Some(_) if live.len() >= limit => Err(Status::NoMemory),
                            Some(r) => Ok(r),
                        }
                    };
                    match (result, expect) {
                        (Ok(nh), Ok(r)) => {
                            assert!(live.insert(nh, (o, r)).is_none(), "{ctx}");
                            *refs.get_mut(&o).unwrap() += 1;
                            assert!(r.is_subset_of(have), "{ctx}: rights widened");
                        }
                        (Err(got), Err(want)) => assert_eq!(got, want, "{ctx}"),
                        (got, want) => panic!("{ctx}: duplicate gave {got:?}, model says {want:?}"),
                    }
                }
                5 if !handles.is_empty() => {
                    let h = pick(&mut rng);
                    let want = random_rights(&mut rng);
                    let (o, have) = live[&h];
                    let result = table.replace(h, want);
                    let new = if want == Rights::SAME {
                        Some(have)
                    } else if want.is_valid() && want.is_subset_of(have) {
                        Some(want)
                    } else {
                        None
                    };
                    match (result, new) {
                        (Ok(nh), Some(r)) => {
                            live.remove(&h);
                            closed.push(h);
                            assert!(live.insert(nh, (o, r)).is_none(), "{ctx}");
                            assert_ne!(nh, h, "{ctx}: replace returned the same value");
                        }
                        (Err(e), None) => {
                            assert_eq!(e, Status::AccessDenied, "{ctx}");
                            assert!(
                                table.get(h).is_ok(),
                                "{ctx}: a refused replace must leave the handle valid"
                            );
                        }
                        (got, want) => panic!("{ctx}: replace gave {got:?}, model says {want:?}"),
                    }
                }
                6 if !handles.is_empty() => {
                    // transfer out and straight back in (what a channel message does)
                    let h = pick(&mut rng);
                    let (o, have) = live[&h];
                    match table.remove_for_transfer(h) {
                        Ok(entry) => {
                            assert!(have.contains(Rights::TRANSFER), "{ctx}");
                            assert_eq!(entry.rights, have, "{ctx}");
                            live.remove(&h);
                            closed.push(h);
                            let nh = table.insert(entry.obj, entry.rights).unwrap();
                            live.insert(nh, (o, have));
                        }
                        Err(e) => {
                            assert!(!have.contains(Rights::TRANSFER), "{ctx}");
                            assert_eq!(e, Status::AccessDenied, "{ctx}");
                            assert!(table.get(h).is_ok(), "{ctx}");
                        }
                    }
                }
                _ => {}
            }
            // Adversarial values never resolve unless live.
            for _ in 0..4 {
                let v = Handle(rng.next() as u32);
                assert_eq!(
                    table.get(v).is_ok(),
                    live.contains_key(&v),
                    "{ctx}: forged {v:?}"
                );
            }
            assert_eq!(table.get(Handle::INVALID), Err(Status::BadHandle), "{ctx}");
            if !closed.is_empty() {
                let old = closed[rng.below(closed.len() as u64) as usize];
                assert_eq!(
                    table.get(old).is_ok(),
                    live.contains_key(&old),
                    "{ctx}: stale {old:?}"
                );
                if !live.contains_key(&old) {
                    assert_eq!(table.get(old), Err(Status::BadHandle), "{ctx}");
                }
            }
            // Agreement: every model handle is live with the same rights, and counts match.
            assert_eq!(table.len(), live.len(), "{ctx}");
            for (&h, &(o, r)) in &live {
                let e = table.get(h).unwrap();
                assert_eq!(e.rights, r, "{ctx}");
                assert_eq!(e.obj, ids[&o], "{ctx}: object identity");
            }
            let alive_objects = refs.values().filter(|&&n| n > 0).count();
            assert_eq!(reg.live(), alive_objects, "{ctx}: live objects");
            for (&o, &n) in &refs {
                assert_eq!(
                    reg.refs(ids[&o]).unwrap_or(0),
                    n,
                    "{ctx}: reference count of object {o}"
                );
            }
        }
        table.drain(&mut reg);
        assert!(table.is_empty());
        assert_eq!(
            reg.live(),
            0,
            "seed {seed}: objects leaked after draining the table"
        );
    }
}

#[test]
fn rights_escalation_is_refused() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let e = event_create(&mut reg, &mut t).unwrap(); // DUPLICATE TRANSFER SIGNAL WAIT
                                                     // duplicate: cannot ask for a right the source lacks
    assert_eq!(
        t.duplicate(&mut reg, e, Rights::READ),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        t.duplicate(&mut reg, e, Rights(Rights::SIGNAL.0 | Rights::WRITE.0)),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        t.duplicate(&mut reg, e, Rights(1 << 20)),
        Err(Status::AccessDenied),
        "unassigned bit"
    );
    // drop DUPLICATE with replace: the new handle cannot duplicate
    let weak = t.replace(e, Rights(Rights::SIGNAL.0)).unwrap();
    assert_eq!(
        t.duplicate(&mut reg, weak, Rights::SAME),
        Err(Status::AccessDenied),
        "no DUPLICATE right"
    );
    // replace cannot widen either, and an error leaves the handle valid
    assert_eq!(t.replace(weak, Rights::EVENT), Err(Status::AccessDenied));
    assert!(t.get(weak).is_ok());
    // transfer needs the TRANSFER right
    assert_eq!(t.remove_for_transfer(weak), Err(Status::AccessDenied));
    assert!(
        t.get(weak).is_ok(),
        "a refused transfer leaves the handle in place"
    );
    // signalling needs SIGNAL
    let wait_only = t.replace(weak, Rights::NONE).unwrap();
    assert_eq!(
        event_signal(&mut reg, &t, wait_only, 0, 1 << 24),
        Err(Status::AccessDenied)
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn the_table_limit_is_enforced_and_changes_nothing() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(2);
    let a = event_create(&mut reg, &mut t).unwrap();
    let _b = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(event_create(&mut reg, &mut t), Err(Status::NoMemory));
    assert_eq!(
        reg.live(),
        2,
        "a refused create must not leave an object behind"
    );
    assert_eq!(
        t.duplicate(&mut reg, a, Rights::SAME),
        Err(Status::NoMemory)
    );
    assert_eq!(
        reg.refs(t.get(a).unwrap().obj),
        Some(1),
        "a refused duplicate takes no reference"
    );
    t.close(&mut reg, a).unwrap();
    assert!(
        event_create(&mut reg, &mut t).is_ok(),
        "room again after a close"
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn handle_values_encode_slot_and_generation() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let h1 = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(h1.0, 1, "first slot, generation 0: value 0x0000_0001");
    t.close(&mut reg, h1).unwrap();
    let h2 = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(h2.0, 0x0001_0001, "same slot, generation 1");
    assert_eq!(
        t.get(h1),
        Err(Status::BadHandle),
        "the old value no longer works"
    );
    let h3 = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(h3.0, 2, "second slot");
    assert_eq!(
        t.get(Handle(0x0000_ffff)),
        Err(Status::BadHandle),
        "slot far beyond the table"
    );
    t.drain(&mut reg);
}
