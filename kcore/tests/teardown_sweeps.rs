//! T265 (T278): many processes that end at once share the sweeps that collect what they leave. Each dying process holds an unreachable
//! cycle once its handles are drained (a channel end queued in its own queue); its teardown is a slice-at-a-time actor that drains its
//! table and then follows `kcore::object::collect_step` until it is covered. The actors run interleaved in a seeded random order, with a
//! living table whose objects must survive. The properties:
//!  * an actor that is covered has its cycle freed (safety: the shared sweep really collected it);
//!  * nothing a living handle reaches is freed;
//!  * twenty teardowns whose drains end close together begin a handful of sweeps, not twenty (the old protocol, in which each teardown
//!    finishes the running sweep and then begins its own, begins exactly one per teardown: the comparison run below).
mod common;
use common::Rng;
use kcore::handle::HandleTable;
use kcore::ipc::{channel_create, channel_write, event_create, Kobjs};
use kcore::object::{collect_step, CollectStep, ObjId, SweepStep};

/// One dying process's teardown.
struct Actor {
    table: HandleTable,
    cycle: Vec<ObjId>,
    /// The sweep number read when the drain finished.
    mark: Option<u64>,
    /// The old protocol only: whether this actor has begun its own sweep.
    began: bool,
    done: bool,
}

fn dying_process(reg: &mut Kobjs) -> Actor {
    let mut table = HandleTable::new(64);
    let (a, b) = channel_create(reg, &mut table).unwrap();
    let (c, d) = channel_create(reg, &mut table).unwrap();
    let ids: Vec<ObjId> = table.objects().collect();
    // d travels to b's queue and b to d's: b and d hold each other, and once a and c are closed only a sweep frees them
    channel_write(reg, &mut table, a, b"", &[d]).unwrap();
    channel_write(reg, &mut table, c, b"", &[b]).unwrap();
    // a few more handles, so the drain takes several slices
    for _ in 0..5 {
        let _ = event_create(reg, &mut table);
    }
    Actor {
        table,
        cycle: ids,
        mark: None,
        began: false,
        done: false,
    }
}

/// Run `n` teardowns to the end, interleaved by `rng`; `shared` picks the protocol. Returns the number of sweeps begun.
fn run(seed: u64, n: usize, shared: bool) -> u64 {
    let mut rng = Rng::new(seed);
    let mut reg = Kobjs::new();
    let mut living = HandleTable::new(64);
    let (keep_a, _keep_b) = channel_create(&mut reg, &mut living).unwrap();
    let _ = keep_a;
    let mut actors: Vec<Actor> = (0..n).map(|_| dying_process(&mut reg)).collect();
    let start = reg.sweep_epoch();
    let roots = |living: &HandleTable| -> Vec<ObjId> { living.objects().collect() };
    let mut turns = 0u64;
    while actors.iter().any(|a| !a.done) {
        turns += 1;
        assert!(
            turns < 1_000_000,
            "seed {seed}: the teardowns did not finish"
        );
        // the living process works meanwhile: it makes objects (born marked while a sweep runs) and moves a handle through a channel
        // (shaded by the barrier when it lands), so the shared sweeps run against a changing graph
        match rng.below(8) {
            0 => {
                let _ = event_create(&mut reg, &mut living);
            }
            1 => {
                if let Ok((x, y)) = channel_create(&mut reg, &mut living) {
                    let other = living.handle_values().find(|&h| h != x && h != y);
                    if let Some(h) = other {
                        let _ = channel_write(&mut reg, &mut living, x, b"", &[h]);
                    }
                    let _ = kcore::ipc::channel_read(&mut reg, &mut living, y, 8, 4);
                }
            }
            _ => {}
        }
        let i = rng.below(n as u64) as usize;
        if actors[i].done {
            continue;
        }
        let ctx = format!("seed {seed} turn {turns} actor {i} shared {shared}");
        if actors[i].mark.is_none() {
            let budget = 1 + rng.below(3) as usize;
            if actors[i].table.drain_some(&mut reg, budget) {
                actors[i].mark = Some(reg.sweep_epoch());
                // (the test means something: with the handles gone the cycle is still there, for a sweep to find; a sweep that began
                // earlier may have freed it already, since a dying table is not a root, as in the kernel)
                if reg.sweep_epoch() == start {
                    assert!(
                        actors[i].cycle.iter().any(|&id| reg.get(id).is_some()),
                        "{ctx}: the cycle went with the handles"
                    );
                }
            }
            continue;
        }
        let mark = actors[i].mark.unwrap_or(0);
        let budget = 1 + rng.below(4) as usize;
        if shared {
            match collect_step(mark, reg.sweep_epoch(), reg.sweep_active()) {
                CollectStep::Covered => {
                    for &id in &actors[i].cycle {
                        assert!(
                            reg.get(id).is_none(),
                            "{ctx}: covered, but its cycle {id:?} is alive"
                        );
                    }
                    actors[i].done = true;
                }
                CollectStep::Help => {
                    let _ = reg.sweep_step(budget);
                }
                CollectStep::Start => {
                    assert!(reg.sweep_begin(&roots(&living)), "{ctx}: no sweep ran");
                }
            }
        } else {
            // the protocol before T265: finish a sweep another began, then begin one's own and run it to the end
            if reg.sweep_active() {
                if let SweepStep::Done(_) = reg.sweep_step(budget) {
                    if actors[i].began {
                        actors[i].done = true;
                    }
                }
            } else if actors[i].began {
                actors[i].done = true;
            } else {
                assert!(reg.sweep_begin(&roots(&living)));
                actors[i].began = true;
            }
        }
        // nothing the living table reaches is ever freed (its objects, and what they hold in their queues)
        let mut work: Vec<ObjId> = living.objects().collect();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(id) = work.pop() {
            if !seen.insert(id) {
                continue;
            }
            let obj = reg
                .get(id)
                .unwrap_or_else(|| panic!("{ctx}: a living object {id:?} was freed"));
            kcore::object::Object::held(obj, &mut work);
        }
    }
    reg.sweep_epoch() - start
}

#[test]
fn concurrent_teardowns_share_sweeps_and_each_cycle_is_collected() {
    for seed in 1..=200u64 {
        let n = 1 + (seed % 20) as usize;
        let sweeps = run(seed, n, true);
        assert!(sweeps >= 1, "seed {seed}: no sweep collected the cycles");
        // (at most one per actor, as before, and in practice far fewer)
        assert!(
            sweeps <= n as u64,
            "seed {seed}: {sweeps} sweeps for {n} teardowns"
        );
    }
}

#[test]
fn twenty_teardowns_at_once_begin_a_handful_of_sweeps_not_twenty() {
    let mut shared_total = 0;
    let mut old_total = 0;
    for seed in 1..=50u64 {
        shared_total += run(seed, 20, true);
        old_total += run(seed, 20, false);
    }
    println!(
        "sweeps begun over 50 runs of 20 teardowns: {shared_total} shared, {old_total} before T265"
    );
    // the old protocol begins one sweep per teardown: 20 per run
    assert_eq!(
        old_total,
        50 * 20,
        "the comparison: one sweep per teardown before T265"
    );
    assert!(
        shared_total <= 50 * 5,
        "the shared protocol began {shared_total} sweeps over 50 runs of 20 teardowns (want a handful per run)"
    );
}
