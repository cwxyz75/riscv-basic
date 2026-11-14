//! The wait table against the registry (slice W step 3): after every random operation, committing the
//! dirty log releases exactly the registered threads whose wait the operation can have ended. Checks W4
//! (no registered thread is left with a true predicate or a dead object), no spurious wakes (a thread is
//! released only if its predicate holds, its object died, or a handle to it was removed), and the audit.
//! The reference is the old design's rule (OD43: re-evaluate everyone) computed directly.

mod common;
use abi::rights::Rights;
use abi::Handle;
use common::{Rng, SEEDS, STEPS};
use kcore::handle::HandleTable;
use kcore::ipc::*;
use kcore::object::ObjId;
use kcore::wait::WaitTable;
use std::collections::{BTreeMap, BTreeSet};

fn is_port(reg: &Kobjs, id: ObjId) -> bool {
    !port_bound_objects(reg, id).is_empty() || matches!(reg.get(id), Some(KObj::Port(_)))
}

fn pick(rng: &mut Rng, t: &HandleTable) -> Option<Handle> {
    let hs: Vec<_> = (1..=8191u32)
        .map(Handle)
        .filter(|h| t.get(*h).is_ok())
        .take(64)
        .collect();
    if hs.is_empty() {
        None
    } else {
        Some(hs[rng.below(hs.len() as u64) as usize])
    }
}

fn run(seed: u64) {
    let mut rng = Rng::new(seed);
    let mut reg = Kobjs::new();
    let mut tables = [HandleTable::new(64), HandleTable::new(64)];
    let mut waits = WaitTable::new();
    let mut masks: BTreeMap<u32, (ObjId, u32)> = BTreeMap::new(); // what each registered thread waits for
    for step in 0..STEPS {
        let ti = rng.below(2) as usize;
        let mut what = String::new();
        match rng.below(14) {
            0 => {
                let _ = channel_create(&mut reg, &mut tables[ti]);
            }
            1 => {
                let _ = socket_create(&mut reg, &mut tables[ti]);
            }
            2 => {
                let _ = event_create(&mut reg, &mut tables[ti]);
            }
            3 => {
                let _ = port_create(&mut reg, &mut tables[ti]);
            }
            4 | 5 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let carry = pick(&mut rng, &tables[ti]).filter(|c| *c != h && rng.chance(30));
                    let hs: Vec<Handle> = carry.into_iter().collect();
                    let _ = channel_write(&mut reg, &mut tables[ti], h, b"m", &hs);
                }
            }
            6 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let _ = channel_read(&mut reg, &mut tables[ti], h, 64, 4);
                }
            }
            7 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let _ = socket_write(&mut reg, &tables[ti], h, &[1u8; 200]);
                }
            }
            8 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let _ = socket_read(&mut reg, &tables[ti], h, 100);
                }
            }
            9 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let bit = 1 << (24 + rng.below(4));
                    let (clear, set) = if rng.chance(50) { (bit, 0) } else { (0, bit) };
                    let _ = event_signal(&mut reg, &tables[ti], h, clear, set);
                }
            }
            10 => {
                if let (Some(p), Some(o)) =
                    (pick(&mut rng, &tables[ti]), pick(&mut rng, &tables[ti]))
                {
                    let masks = [
                        abi::signals::READABLE,
                        abi::signals::PEER_CLOSED,
                        1 << 24,
                        1 << 25,
                    ];
                    let mask = masks[rng.below(4) as usize];
                    let _ = port_bind(&mut reg, &tables[ti], p, o, u64::from(mask), rng.below(6));
                }
            }
            11 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    what = "close".into();
                    let _ = tables[ti].close(&mut reg, h);
                }
            }
            12 => {
                if let Some(h) = pick(&mut rng, &tables[ti]) {
                    let _ = tables[ti].replace(h, Rights::SAME);
                }
            }
            _ => {
                if rng.chance(40) {
                    tables[ti].drain(&mut reg);
                } else {
                    let roots: Vec<ObjId> = tables.iter().flat_map(|t| t.objects()).collect();
                    reg.sweep(&roots);
                }
            }
        }
        // the kernel's commit
        for t in tables.iter_mut() {
            t.flush_removed(&mut reg);
        }
        let dirty = reg.take_dirty();
        let forced: BTreeSet<ObjId> = dirty
            .items
            .iter()
            .filter(|(_, f)| *f)
            .map(|(i, _)| *i)
            .collect();
        let woken = waits.commit(
            |id| signals_of_object(&reg, id),
            |id| is_port(&reg, id),
            dirty,
        );
        for t in &woken {
            let (obj, mask) = masks.remove(t).expect("a woken thread was registered");
            let sig = signals_of_object(&reg, obj);
            let justified = sig.is_none_or(|s| s & mask != 0) || forced.contains(&obj);
            assert!(justified, "seed {seed} step {step}: thread {t} woken without cause (signals {sig:?}, mask {mask:#x})");
            assert!(!waits.is_registered(*t));
        }
        // a removed handle is a forced touch: every thread waiting on that object was released (so
        // that it can re-validate its handle and report BAD_HANDLE)
        for (t, (obj, _)) in &masks {
            assert!(
                !forced.contains(obj),
                "seed {seed} step {step}: thread {t} still waits on {obj:?} though a handle to it was removed"
            );
        }
        // W4: nobody left waiting has a true predicate or a dead object
        for (t, (obj, mask)) in &masks {
            assert!(waits.is_registered(*t));
            match signals_of_object(&reg, *obj) {
                None => panic!("seed {seed} step {step} {what}: thread {t} still waits on a dead object"),
                Some(s) => assert!(
                    s & mask == 0,
                    "seed {seed} step {step} {what}: thread {t} was not woken though its signals {s:#x} match {mask:#x}"
                ),
            }
        }
        waits.audit(|id| reg.is_alive(id)).unwrap();
        // some threads start waiting (only when their predicate is false, as the kernel does)
        for t in 1..=6u32 {
            if !masks.contains_key(&t) && rng.chance(30) {
                let ids = reg.live_ids();
                if ids.is_empty() {
                    continue;
                }
                // half the time wait for READABLE on a port (whose signal follows its bindings)
                let ports: Vec<ObjId> = ids
                    .iter()
                    .copied()
                    .filter(|i| matches!(reg.get(*i), Some(KObj::Port(_))))
                    .collect();
                let (obj, mask) = if !ports.is_empty() && rng.chance(50) {
                    (
                        ports[rng.below(ports.len() as u64) as usize],
                        abi::signals::READABLE,
                    )
                } else {
                    (ids[rng.below(ids.len() as u64) as usize], 1 << rng.below(6))
                };
                if let Some(s) = signals_of_object(&reg, obj) {
                    if s & mask == 0 {
                        waits.register(t, obj, mask);
                        masks.insert(t, (obj, mask));
                    }
                }
            }
        }
        // a thread may also give up (timeout or kill)
        if rng.chance(10) {
            if let Some((&t, _)) = masks.iter().next() {
                assert!(waits.forget(t));
                masks.remove(&t);
            }
        }
    }
}

#[test]
fn the_wait_table_releases_exactly_the_threads_a_change_can_have_ended() {
    for seed in 1..=SEEDS {
        run(seed);
    }
}

#[test]
fn register_replaces_and_forget_is_idempotent() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(8);
    let a = event_create(&mut reg, &mut t).unwrap();
    let b = event_create(&mut reg, &mut t).unwrap();
    let (oa, ob) = (t.get(a).unwrap().obj, t.get(b).unwrap().obj);
    let mut w = WaitTable::new();
    w.register(1, oa, 1 << 24);
    w.register(1, ob, 1 << 25); // a thread waits on one object at a time
    assert_eq!(w.len(), 1);
    reg.take_dirty();
    event_signal(&mut reg, &t, a, 0, 1 << 24).unwrap(); // the old object: nobody waits there now
    let d = reg.take_dirty();
    assert!(w
        .commit(|i| signals_of_object(&reg, i), |_| false, d)
        .is_empty());
    event_signal(&mut reg, &t, b, 0, 1 << 25).unwrap();
    let d = reg.take_dirty();
    assert_eq!(
        w.commit(|i| signals_of_object(&reg, i), |_| false, d),
        vec![1]
    );
    assert!(!w.forget(1));
}
