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
        for step in 0..1500 {
            let ctx = format!("seed {seed} step {step}");
            let ti = rng.below(2) as usize;
            let handles: Vec<Handle> = {
                let mut v: Vec<Handle> = tables[ti].handle_values().collect();
                v.sort_by_key(|h| h.0);
                v
            };
            let pick = |rng: &mut Rng| handles[rng.below(handles.len() as u64) as usize];
            match rng.below(14) {
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
                        reg.sweep_roots_done();
                        let roots: Vec<_> = tables.iter().flat_map(|t| t.objects()).collect();
                        reg.sweep(&roots);
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
                    if let Some(ti) = unscanned.pop() {
                        for o in tables[ti].objects() {
                            reg.shade(o);
                        }
                        if unscanned.is_empty() {
                            reg.sweep_roots_done();
                        }
                    }
                    let budget = 1 + rng.below(4) as usize;
                    let _ = reg.sweep_step(budget);
                }
            }
            assert_reachable_is_alive(&reg, &[&tables[0], &tables[1]], &ctx);
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
