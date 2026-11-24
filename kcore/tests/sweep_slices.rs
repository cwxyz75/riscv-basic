//! MM10: the reachability sweep in slices, against the full object graph. Random operations (make events and channels, send handles through
//! queues, read them back out, duplicate, close, move handles between two tables, make cycles) are interleaved with one-object slices of a
//! running sweep. The properties:
//!  * nothing reachable is ever freed: after every operation, every handle's object is alive and so is everything an alive object holds;
//!  * every unreachable cycle is freed: once the handles are closed and two sweeps have run, nothing is left;
//!  * the barrier is what makes the first one true: `HandleTable::insert` shades its object (mutation-checked: without it a handle moved
//!    out of a queue mid-sweep loses its object).
mod common;
use abi::rights::Rights;
use abi::Handle;
use common::Rng;
use kcore::handle::HandleTable;
use kcore::ipc::{channel_create, channel_read, channel_write, event_create, Kobjs};
use kcore::object::{Object, SweepStep};

fn assert_reachable_is_alive(reg: &Kobjs, tables: &[&HandleTable], ctx: &str) {
    let mut work: Vec<_> = tables.iter().flat_map(|t| t.objects()).collect();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = work.pop() {
        if !seen.insert(id) {
            continue;
        }
        let obj = reg.get(id).unwrap_or_else(|| {
            panic!("{ctx}: an object reachable from a handle was freed: {id:?}")
        });
        obj.held(&mut work);
    }
}

#[test]
fn nothing_reachable_is_freed_and_every_cycle_is_between_two_sweeps() {
    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed);
        let mut reg = Kobjs::new();
        let mut tables = [HandleTable::new(300), HandleTable::new(300)];
        // the tables whose roots a pending sweep has yet to shade (T206)
        let mut unscanned: Vec<usize> = Vec::new();
        let mut root_cursor = 0usize;
        // a process that has died and whose table is being closed in slices: out of the roots (T206 plan 1, item 6), its handles still hold objects
        let mut dying: Option<HandleTable> = None;
        for step in 0..1500 {
            let ctx = format!("seed {seed} step {step}");
            let ti = rng.below(2) as usize;
            let handles: Vec<Handle> = {
                let mut v: Vec<Handle> = tables[ti].handle_values().collect();
                v.sort_by_key(|h| h.0);
                v
            };
            let pick = |rng: &mut Rng| handles[rng.below(handles.len() as u64) as usize];
            match rng.below(16) {
                0 => {
                    let _ = event_create(&mut reg, &mut tables[ti]);
                }
                1 => {
                    let _ = channel_create(&mut reg, &mut tables[ti]);
                }
                2 | 3 if handles.len() >= 2 => {
                    // send a handle (maybe an end of the same channel: cycles) through a channel
                    let (c, h) = (pick(&mut rng), pick(&mut rng));
                    let _ = channel_write(&mut reg, &mut tables[ti], c, b"", &[h]);
                }
                4 | 5 if !handles.is_empty() => {
                    // read a message: its handles come out into the table (the handle the barrier must shade)
                    let c = pick(&mut rng);
                    let _ = channel_read(&mut reg, &mut tables[ti], c, 16, 8);
                }
                6 if !handles.is_empty() => {
                    let h = pick(&mut rng);
                    let _ = tables[ti].duplicate(&mut reg, h, Rights::SAME);
                }
                7 | 8 if !handles.is_empty() => {
                    let h = pick(&mut rng);
                    let _ = tables[ti].close(&mut reg, h);
                }
                9 if !handles.is_empty() => {
                    // move a handle to the other table
                    let h = pick(&mut rng);
                    if let Ok(e) = tables[ti].remove_for_transfer(&mut reg, h) {
                        tables[1 - ti]
                            .insert(&mut reg, e.obj, e.rights)
                            .expect("room");
                    }
                }
                10 => {
                    // a process dies: its table is drained (every handle closed at once)
                    if rng.below(8) == 0 {
                        tables[ti].drain(&mut reg);
                    }
                }
                11 => {
                    // the teardown's whole sweep, in the middle of a sliced one (it finishes that one first)
                    if rng.below(4) == 0 {
                        for ti in unscanned.drain(..) {
                            for o in tables[ti].objects() {
                                reg.shade(o);
                            }
                        }
                        root_cursor = 0;
                        reg.sweep_roots_done();
                        let roots: Vec<_> = tables.iter().flat_map(|t| t.objects()).collect();
                        reg.sweep(&roots);
                    }
                }
                14 if dying.is_none() => {
                    // a process dies: its table leaves the roots, and is closed a few slots at a time from now on
                    dying = Some(std::mem::replace(&mut tables[ti], HandleTable::new(300)));
                }
                15 => {
                    if let Some(t) = dying.as_mut() {
                        if t.drain_some(&mut reg, 1 + rng.below(3) as usize) {
                            dying = None;
                        }
                    }
                }
                _ => {
                    // a slice of the sweep: start one if none runs and some garbage may exist
                    if !reg.sweep_active() {
                        if rng.below(2) == 0 {
                            reg.sweep_begin_pending();
                            unscanned = vec![0, 1];
                            if rng.below(2) == 0 {
                                unscanned.reverse();
                            }
                        } else {
                            let roots: Vec<_> = tables.iter().flat_map(|t| t.objects()).collect();
                            reg.sweep_begin(&roots);
                        }
                    }
                    // (a pending sweep's roots arrive one table per slice)
                    if let Some(&ti) = unscanned.last() {
                        // (T206: a table's slots a few at a time, the cursor kept between slices)
                        let len = tables[ti].slot_count();
                        let to = len.min(root_cursor + 1 + rng.below(4) as usize);
                        for o in tables[ti].objects_in(root_cursor, to) {
                            reg.shade(o);
                        }
                        if to >= len {
                            unscanned.pop();
                            root_cursor = 0;
                            if unscanned.is_empty() {
                                reg.sweep_roots_done();
                            }
                        } else {
                            root_cursor = to;
                        }
                    }
                    let budget = 1 + rng.below(4) as usize;
                    let _ = reg.sweep_step(budget);
                }
            }
            assert_reachable_is_alive(&reg, &[&tables[0], &tables[1]], &ctx);
        }
        // (the dying table's closes go on: a stale id of an object the sweep freed first is skipped, never another object's)
        if let Some(t) = dying.as_mut() {
            t.drain(&mut reg);
        }
        // everything is closed: what counting cannot free, two sweeps do
        for t in tables.iter_mut() {
            let hs: Vec<Handle> = t.handle_values().collect();
            for h in hs {
                let _ = t.close(&mut reg, h);
            }
        }
        for ti in unscanned.drain(..) {
            for o in tables[ti].objects() {
                reg.shade(o);
            }
        }
        reg.sweep_roots_done();
        for _ in 0..2 {
            while reg.sweep_active() {
                let _ = reg.sweep_step(3);
            }
            let roots: Vec<_> = Vec::new();
            reg.sweep_begin(&roots);
            while let SweepStep::More = reg.sweep_step(3) {}
        }
        assert_eq!(
            reg.live(),
            0,
            "seed {seed}: every unreachable object is freed"
        );
    }
}

#[test]
fn an_object_made_while_a_sweep_runs_is_born_marked() {
    use kcore::ipc::KObj;
    let mut reg = Kobjs::new();
    assert!(reg.sweep_begin(&[]));
    // made during marking, with no handle at all: only the birth mark keeps it from being freed as unreachable
    let id = reg.create(KObj::Vmo(0));
    while let SweepStep::More = reg.sweep_step(1) {}
    assert!(
        reg.get(id).is_some(),
        "an object made during a sweep survives that sweep"
    );
    // the next sweep, started after its birth, finds it unreachable
    assert_eq!(reg.sweep(&[]), 1);
    assert!(reg.get(id).is_none());
}

/// T206 B: closing the head of a chain of 3,000 channels (each queues both ends of the next) is many bounded sections, not one: no call destroys
/// more than twice the unit budget (the owed references of the last call, then the one it was given), the owed list shrinks to nothing, and
/// everything is freed at the end. A stale owed id (a sweep freed the object first) is skipped.
#[test]
fn a_close_that_frees_a_chain_is_paid_in_bounded_slices() {
    for with_sweep in [false, true] {
        let n = 3000;
        let mut reg = Kobjs::new();
        let mut t = HandleTable::new(16);
        let mut next: Option<(Handle, Handle)> = None;
        for _ in 0..n {
            let (a, b) = channel_create(&mut reg, &mut t).unwrap();
            if let Some((na, nb)) = next {
                channel_write(&mut reg, &mut t, a, b"", &[na, nb]).unwrap();
            }
            // (the table keeps only the head of what it has built so far: the previous pair is in `b`'s queue)
            if let Some((na, nb)) = next {
                let _ = (na, nb);
            }
            next = Some((a, b));
        }
        let created = reg.live();
        assert_eq!(created, 2 * n);
        let mut calls = 0;
        loop {
            if with_sweep && calls == 3 {
                // a sweep (no roots but this table's objects) runs between the slices: it may free owed targets first
                let roots: Vec<_> = t.objects().collect();
                reg.sweep(&roots);
            }
            let before = reg.live();
            let done = t.drain_some(&mut reg, 4);
            calls += 1;
            let freed = before.saturating_sub(reg.live());
            assert!(
                freed <= 2 * kcore::object::RELEASE_UNITS,
                "freed {freed} in one call"
            );
            if done && reg.owed() == 0 {
                break;
            }
            assert!(calls < 10_000, "no progress");
        }
        assert!(
            with_sweep || calls > created / (2 * kcore::object::RELEASE_UNITS),
            "{calls} calls for a chain of {created}"
        );
        assert_eq!(reg.live(), 0);
    }
}
