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
    let hs: Vec<_> = (1..=8191u64)
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
    let mut masks: BTreeMap<u64, (ObjId, u32)> = BTreeMap::new(); // what each registered thread waits for
    let mut jobs = kcore::job::Jobs::new();
    let mut pollers: BTreeSet<u64> = BTreeSet::new(); // the port waiters that poll when they wake (`port_wait`): only they take the baton
    for step in 0..STEPS {
        let ti = rng.below(2) as usize;
        let mut what = String::new();
        match rng.below(16) {
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
                    let _ = tables[ti].replace(&mut reg, h, Rights::SAME);
                }
            }
            13 => {
                if let Some(p) = pick(&mut rng, &tables[ti]) {
                    what = "port_unbind".into();
                    let _ = port_unbind(&mut reg, &tables[ti], p, rng.below(6));
                }
            }
            14 => {
                // a poll by any thread (a waiter that woke, or a pool member that never waited): it logs the port and may disarm a one-shot
                if let Some(p) = pick(&mut rng, &tables[ti]) {
                    what = "port_poll".into();
                    let _ = port_poll(&mut reg, &tables[ti], p);
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
        // the kernel's commit: bindings made through a handle that went end first (L21c), the ports they leave are touched
        let gone = reg.take_gone();
        end_bindings_made_through(&mut reg, &mut jobs, &gone);
        let dirty = reg.take_dirty();
        let forced: BTreeSet<ObjId> = dirty
            .items
            .iter()
            .filter(|(_, f)| *f)
            .map(|(i, _)| *i)
            .collect();
        let woken = waits.commit(
            |id| signals_of_object(&reg, id),
            |id, f| reg.for_each_binder(id, f),
            |_, _| false,
            dirty,
        );
        waits
            .audit(|o| reg.get(o).is_some())
            .unwrap_or_else(|e| panic!("seed {seed} step {step}: audit: {e}"));
        let _ = &woken;
        // the ports that woke a waiter in this commit (the baton, L21b): the woken thread polls, which logs the port, so the next commit looks at
        // it again; the model does the same
        let mut baton: BTreeMap<ObjId, usize> = BTreeMap::new();
        for t in &woken {
            let (obj, mask) = masks.remove(t).expect("a woken thread was registered");
            let sig = signals_of_object(&reg, obj);
            let justified = sig.is_none_or(|s| s & mask != 0) || forced.contains(&obj);
            assert!(justified, "seed {seed} step {step}: thread {t} woken without cause (signals {sig:?}, mask {mask:#x})");
            assert!(!waits.is_registered(*t));
            if is_port(&reg, obj) && sig.is_some() && !forced.contains(&obj) && pollers.remove(t) {
                *baton.entry(obj).or_default() += 1;
                reg.touch(obj); // the woken thread's poll
            }
        }
        for (obj, n) in &baton {
            // PW: a port wakes one waiter at a time, not all of them
            assert_eq!(
                *n, 1,
                "seed {seed} step {step}: a port woke {n} waiters for one readiness"
            );
            let _ = obj;
        }
        // a removed handle is a forced touch: every thread waiting on that object was released (so
        // that it can re-validate its handle and report BAD_HANDLE)
        for (t, (obj, _)) in &masks {
            assert!(
                !forced.contains(obj),
                "seed {seed} step {step}: thread {t} still waits on {obj:?} though a handle to it was removed"
            );
        }
        // W4: nobody left waiting has a true predicate or a dead object, except a thread behind the one the port woke (PW: the baton passes
        // when that thread has polled; the model's poll above logged the port, so the next commit wakes the next waiter)
        for (t, (obj, mask)) in &masks {
            assert!(waits.is_registered(*t));
            match signals_of_object(&reg, *obj) {
                None => panic!("seed {seed} step {step} {what}: thread {t} still waits on a dead object"),
                Some(s) => assert!(
                    s & mask == 0 || baton.contains_key(obj),
                    "seed {seed} step {step} {what}: thread {t} was not woken though its signals {s:#x} match {mask:#x}"
                ),
            }
        }
        waits.audit(|id| reg.is_alive(id)).unwrap();
        // some threads start waiting (only when their predicate is false, as the kernel does)
        for t in 1..=6u64 {
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
                        if is_port(&reg, obj) {
                            if rng.chance(70) {
                                waits.admit(t).unwrap();
                                waits.register_poller(t, obj, mask);
                                pollers.insert(t);
                            } else {
                                waits.admit(t).unwrap();
                                waits.register_port(t, obj, mask);
                                pollers.remove(&t);
                            }
                        } else {
                            waits.admit(t).unwrap();
                            waits.register(t, obj, mask);
                            pollers.remove(&t);
                        }
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
    w.admit(1).unwrap();
    w.register(1, oa, 1 << 24);
    w.admit(1).unwrap();
    w.register(1, ob, 1 << 25); // a thread waits on one object at a time
    assert_eq!(w.len(), 1);
    reg.take_dirty();
    event_signal(&mut reg, &t, a, 0, 1 << 24).unwrap(); // the old object: nobody waits there now
    let d = reg.take_dirty();
    assert!(w
        .commit(|i| signals_of_object(&reg, i), |_, _| {}, |_, _| false, d)
        .is_empty());
    event_signal(&mut reg, &t, b, 0, 1 << 25).unwrap();
    let d = reg.take_dirty();
    assert_eq!(
        w.commit(|i| signals_of_object(&reg, i), |_, _| {}, |_, _| false, d),
        vec![1]
    );
    assert!(!w.forget(1));
}

#[test]
fn an_overflowed_log_releases_every_registration() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let e = event_create(&mut reg, &mut t).unwrap();
    let obj = t.get(e).unwrap().obj;
    let mut w = WaitTable::new();
    w.admit(7).unwrap();
    w.register(7, obj, 1 << 24);
    reg.take_dirty();
    // (the cap is a safety net the kernel's per-syscall drain never reaches; force the flag directly)
    let overflowed = kcore::object::Dirty {
        items: Vec::new(),
        overflow: true,
    };
    let woken = w.commit(
        |i| signals_of_object(&reg, i),
        |_, _| {},
        |_, _| false,
        overflowed,
    );
    assert_eq!(
        woken,
        vec![7],
        "everyone is released: a forced touch may have been lost"
    );
}

#[test]
fn a_port_waiter_costs_an_unrelated_commit_nothing() {
    // (L21a) a port with a waiter is evaluated only when an object it binds, or the port itself, is touched: a thousand bindings and a sleeping
    // waiter used to be walked by every commit on the system
    use std::cell::Cell;
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(2048);
    let port = port_create(&mut reg, &mut t).unwrap();
    let pobj = t.get(port).unwrap().obj;
    let mut events = Vec::new();
    for k in 0..1000u64 {
        let e = event_create(&mut reg, &mut t).unwrap();
        events.push(e);
        port_bind(&mut reg, &t, port, e, u64::from(abi::signals::USER_MASK), k).unwrap();
    }
    let other = event_create(&mut reg, &mut t).unwrap();
    let mut w = WaitTable::new();
    w.admit(1).unwrap();
    w.register_port(1, pobj, abi::signals::READABLE);
    reg.take_dirty();
    let calls = Cell::new(0usize);
    let commit = |w: &mut WaitTable, reg: &mut Kobjs| {
        calls.set(0);
        let d = reg.take_dirty();
        w.commit(
            |i| {
                calls.set(calls.get() + 1);
                signals_of_object(reg, i)
            },
            |i, f| reg.for_each_binder(i, f),
            |_, _| false,
            d,
        )
    };
    // a touch of an object the port does not bind: nothing is evaluated (nobody waits on it, and the port's thousand bindings are not walked)
    event_signal(&mut reg, &t, other, 0, 1 << 24).unwrap();
    assert!(commit(&mut w, &mut reg).is_empty());
    assert_eq!(
        calls.get(),
        0,
        "nothing was evaluated for an unrelated touch"
    );
    // a touch of a bound object that makes the port readable: the port is evaluated once, and the waiter is released
    event_signal(&mut reg, &t, events[500], 0, 1 << 24).unwrap();
    assert_eq!(commit(&mut w, &mut reg), vec![1]);
    assert_eq!(calls.get(), 1, "one evaluation for one bound touch");
    assert!(
        !w.is_registered(1),
        "the registration is gone with the waiter"
    );
}

#[test]
fn a_call_waits_for_its_own_reply_not_for_any_message() {
    // (L21d) `channel_call` registers for the reply with its transaction id: an unrelated message queued on the end does not release it, the
    // reply does, and so does the peer going away
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let oa = t.get(a).unwrap().obj;
    let mut w = WaitTable::new();
    let commit = |w: &mut WaitTable, reg: &mut Kobjs| {
        let d = reg.take_dirty();
        w.commit(
            |i| signals_of_object(reg, i),
            |_, _| {},
            |ch, tx| channel_has_txid(reg, ch, tx),
            d,
        )
    };
    reg.take_dirty();
    w.admit(1).unwrap();
    w.register_call(1, oa, 7);
    // an unrelated message (transaction id 9) arrives on the end: READABLE is up, the call is not released
    channel_write(&mut reg, &mut t, b, &9u32.to_le_bytes(), &[]).unwrap();
    assert!(
        commit(&mut w, &mut reg).is_empty(),
        "an unrelated message released a call"
    );
    // its reply (transaction id 7)
    channel_write(&mut reg, &mut t, b, &7u32.to_le_bytes(), &[]).unwrap();
    assert_eq!(
        commit(&mut w, &mut reg),
        vec![1],
        "the reply did not release the call"
    );
    // the peer closing releases a call that is still waiting
    w.admit(2).unwrap();
    w.register_call(2, oa, 8);
    reg.take_dirty();
    t.close(&mut reg, b).unwrap();
    assert_eq!(
        commit(&mut w, &mut reg),
        vec![2],
        "the peer going did not release the call"
    );
    w.audit(|id| reg.is_alive(id)).unwrap();
}

#[test]
fn a_port_wakes_one_waiter_and_the_poll_passes_the_baton() {
    // (L21b) two threads wait on a port; one ready binding wakes one of them (the first), and the next commit after that one's poll wakes the
    // other only if a binding is still ready
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let port = port_create(&mut reg, &mut t).unwrap();
    let pobj = t.get(port).unwrap().obj;
    let e = event_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, e, u64::from(1u32 << 24), 1).unwrap();
    let mut w = WaitTable::new();
    let commit = |w: &mut WaitTable, reg: &mut Kobjs| {
        let d = reg.take_dirty();
        w.commit(
            |i| signals_of_object(reg, i),
            |i, f| reg.for_each_binder(i, f),
            |_, _| false,
            d,
        )
    };
    reg.take_dirty();
    w.admit(1).unwrap();
    w.register_poller(1, pobj, abi::signals::READABLE);
    w.admit(2).unwrap();
    w.register_poller(2, pobj, abi::signals::READABLE);
    // (a plain wait on the port's READABLE, an `object_wait`, never polls: it is released with the first and takes no baton)
    w.admit(3).unwrap();
    w.register_port(3, pobj, abi::signals::READABLE);
    event_signal(&mut reg, &t, e, 0, 1 << 24).unwrap();
    let mut first = commit(&mut w, &mut reg);
    first.sort();
    assert_eq!(
        first,
        vec![1, 3],
        "the first poller and the plain waiter are woken"
    );
    assert!(w.is_registered(2), "the other poller stays asleep");
    // thread 1 polls (the binding stays ready: it is level-triggered): the port is logged, and the baton passes
    assert!(port_poll(&mut reg, &t, port).unwrap().is_some());
    assert_eq!(
        commit(&mut w, &mut reg),
        vec![2],
        "the poll passed the baton to the next waiter"
    );
    // the signal goes: a poll that finds nothing leaves nobody woken
    event_signal(&mut reg, &t, e, 1 << 24, 0).unwrap();
    assert!(commit(&mut w, &mut reg).is_empty());
}

#[test]
fn a_binding_ends_with_the_handle_it_was_made_through() {
    // (L21c, T61) closing, replacing or sending the handle a binding was made through ends the binding; a binding made through another handle to the
    // same object stays; the object's own handle table entries are unaffected
    let mut reg = Kobjs::new();
    let mut jobs = kcore::job::Jobs::new();
    let mut t = HandleTable::new(32);
    let mut other = HandleTable::new(32);
    let port = port_create(&mut reg, &mut t).unwrap();
    let ev = event_create(&mut reg, &mut t).unwrap();
    let dup = t.duplicate(&mut reg, ev, Rights::SAME).unwrap();
    port_bind(&mut reg, &t, port, ev, u64::from(1u32 << 24), 1).unwrap();
    port_bind(&mut reg, &t, port, dup, u64::from(1u32 << 25), 2).unwrap();
    let pobj = t.get(port).unwrap().obj;
    let bound = |reg: &Kobjs| port_bound_objects(reg, pobj).len();
    assert_eq!(bound(&reg), 2);
    let settle = |reg: &mut Kobjs, jobs: &mut kcore::job::Jobs| {
        let gone = reg.take_gone();
        end_bindings_made_through(reg, jobs, &gone);
    };
    // close the first handle: its binding (key 1) ends, the one through the duplicate stays
    t.close(&mut reg, ev).unwrap();
    settle(&mut reg, &mut jobs);
    assert_eq!(
        bound(&reg),
        1,
        "the binding made through the closed handle ended"
    );
    event_signal(&mut reg, &t, dup, 0, 1 << 24).unwrap(); // the ended binding's signal: the port is not readable for it
    assert!(
        port_poll(&mut reg, &t, port).unwrap().is_none(),
        "an ended binding delivered"
    );
    event_signal(&mut reg, &t, dup, 0, 1 << 25).unwrap();
    assert_eq!(
        port_poll(&mut reg, &t, port).unwrap().map(|(k, _)| k),
        Some(2)
    );
    // replace the duplicate: the old value stops working, and so does the binding made through it
    let new = t.replace(&mut reg, dup, Rights::SAME).unwrap();
    settle(&mut reg, &mut jobs);
    assert_eq!(bound(&reg), 0, "a replaced handle's binding ended");
    // a binding made through a handle that is then sent away ends too
    let moved = t.duplicate(&mut reg, new, Rights::SAME).unwrap();
    port_bind(&mut reg, &t, port, moved, u64::from(1u32 << 24), 3).unwrap();
    assert_eq!(bound(&reg), 1);
    let entry = t.remove_for_transfer(&mut reg, moved).unwrap();
    settle(&mut reg, &mut jobs);
    assert_eq!(bound(&reg), 0, "a transferred handle's binding ended");
    let _ = other.insert(&mut reg, entry.obj, entry.rights);
    // a process's death (drain) ends every binding made through its handles
    port_bind(&mut reg, &t, port, new, u64::from(1u32 << 24), 4).unwrap();
    assert_eq!(bound(&reg), 1);
    t.drain(&mut reg);
    settle(&mut reg, &mut jobs);
    assert_eq!(bound(&reg), 0);
}

#[test]
fn the_watch_count_of_a_bound_object_returns_to_zero_when_the_port_dies() {
    // (L21c) the count that lets a handle removal skip the ports must not stay high for objects that were polled through a temporary port (the usual
    // idiom): it is given back when the port is destroyed, by a release and by a sweep
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(16);
    let ev = event_create(&mut reg, &mut t).unwrap();
    let eobj = t.get(ev).unwrap().obj;
    let port = port_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, ev, u64::from(1u32 << 24), 1).unwrap();
    assert_eq!(reg.watch_count(eobj), 1);
    t.close(&mut reg, port).unwrap(); // the port's last handle: released
    assert_eq!(
        reg.watch_count(eobj),
        0,
        "a released port gave its counts back"
    );
    // the same through a sweep: a port that is only reachable from itself (a doomed cycle) dies in the sweep
    let port = port_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, ev, u64::from(1u32 << 24), 1).unwrap();
    assert_eq!(reg.watch_count(eobj), 1);
    let pobj = t.get(port).unwrap().obj;
    // (the registry's own bookkeeping is what the sweep frees: roots without the port's handle doom it)
    t.close(&mut reg, port).unwrap();
    reg.sweep(&[eobj]);
    assert!(!reg.is_alive(pobj));
    assert_eq!(reg.watch_count(eobj), 0);
}

/// KT9 (T177): admission is the only place the table allocates. Registering, forgetting and committing, with many threads admitted after the table has
/// rehashed, change nothing about its room; and a rehash keeps each object's registrations in arrival order (the baton of L21b depends on it).
#[test]
fn registering_forgetting_and_committing_allocate_nothing_and_a_rehash_keeps_order() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(64);
    let ev = event_create(&mut reg, &mut t).unwrap();
    let obj = t.get(ev).unwrap().obj;
    let mut w = WaitTable::new();
    // arrivals interleaved with admissions that grow the buckets, so a chain is moved at least twice
    for th in 1..=40u64 {
        w.admit(th).unwrap();
        w.register(th, obj, 1 << 24);
    }
    w.audit(|o| reg.get(o).is_some()).unwrap();
    assert_eq!(w.len(), 40);
    event_signal(&mut reg, &t, ev, 0, 1 << 24).unwrap();
    let d = reg.take_dirty();
    let woken = w.commit(
        |i| signals_of_object(&reg, i),
        |i, f| reg.for_each_binder(i, f),
        |_, _| false,
        d,
    );
    assert_eq!(
        woken,
        (1..=40u64).collect::<Vec<_>>(),
        "arrival order survives the rehashes"
    );
    w.audit(|o| reg.get(o).is_some()).unwrap();
    w.recycle(woken);
    // retire frees the record; a re-admitted id works again
    w.retire(7);
    assert_eq!(w.admitted(), 39);
    w.admit(7).unwrap();
    w.register(7, obj, 1 << 24);
    assert!(w.forget(7));
    assert!(!w.forget(7));
    w.audit(|o| reg.get(o).is_some()).unwrap();
}
