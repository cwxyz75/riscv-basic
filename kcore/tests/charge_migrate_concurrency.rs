//! AC1 across a migration (SCHED-SMP-SPEC 4.10): a thread is never charged the same stretch of time on two harts. Hart 0 runs `T`
//! and yields it to `U` (pinned there); hart 1, idle, looks at its queues and pulls `T`. Hart 1's look starts with a charge, and if
//! the designation used that reading, a yield on hart 0 that came after it (charging `T` up to a later time) would leave the stretch
//! between the two billed to `T` twice: once on hart 0 for its run there, and once on hart 1 from the older reading. Random schedules
//! (shuttle) put hart 0's yield anywhere inside hart 1's look. The clock moves on at every reading, so every interleaving has a
//! stretch that could be billed twice.
//!
//! The property: what `T` is charged over the run is at most the time that passed. (The 2026-11-27 smoke of the iommu-drivers
//! epic saw a 100 ms spin charged 106.5 ms at four harts.)
//!
//! Run by `make os-test` with `RUSTFLAGS="--cfg shuttle"`; without the cfg this file is empty.
#![cfg(shuttle)]

use kcore::sched2::{FixedPrio, HartRq, SchedAttr};
use kcore::sched_smp::{Rqs, Sched2, Table};
use shuttle::sync::{Arc, Mutex, MutexGuard};
use shuttle::thread;
use std::sync::atomic::{AtomicU64, Ordering};

const HARTS: usize = 2;
/// The clock: every reading is one microsecond after the one before (a plain atomic: a reading is not a scheduling point, and the
/// model threads run one at a time, so the readings are ordered as the code makes them).
static NOW: AtomicU64 = AtomicU64::new(0);
/// (one clock for the whole file: the tests take turns)
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn clock() -> u64 {
    NOW.fetch_add(1_000, Ordering::SeqCst) + 1_000
}

struct SRqs {
    rq: Vec<Mutex<HartRq<FixedPrio>>>,
    table: Mutex<Table>,
}

impl Rqs<FixedPrio> for SRqs {
    type Guard<'a> = MutexGuard<'a, HartRq<FixedPrio>>;
    type TableGuard<'a> = MutexGuard<'a, Table>;
    fn len(&self) -> usize {
        self.rq.len()
    }
    fn lock(&self, x: usize) -> Self::Guard<'_> {
        self.rq[x].lock().unwrap()
    }
    fn table(&self) -> Self::TableGuard<'_> {
        self.table.lock().unwrap()
    }
}

fn attr() -> SchedAttr {
    prio(10)
}

fn prio(p: u8) -> SchedAttr {
    SchedAttr { prio: p, rr: false }
}

fn machine() -> Arc<Sched2<FixedPrio, SRqs>> {
    NOW.store(0, Ordering::SeqCst);
    let rqs = SRqs {
        rq: (0..HARTS)
            .map(|_| Mutex::new(HartRq::new(FixedPrio::new())))
            .collect(),
        table: Mutex::new(Table::new()),
    };
    Arc::new(Sched2::over(rqs).with_clock(clock))
}

/// The pull: hart 1's look (`settle`) takes `T` from hart 0's queue after hart 0 yielded it.
fn model() {
    let s = machine();
    let t = s.adopt_current_on(0, attr(), None).unwrap();
    let _u = s.spawn_on(0, attr(), Some(0)).unwrap();
    s.switched(0);
    assert_eq!(s.current_on(0), t);
    s.charge_all();
    let (cpu0, t0) = (s.record(t).unwrap().cpu, NOW.load(Ordering::SeqCst));
    let h0 = {
        let s = s.clone();
        thread::spawn(move || {
            s.yield_on(0).unwrap();
            s.switched(0);
        })
    };
    let h1 = {
        let s = s.clone();
        thread::spawn(move || {
            for _ in 0..4 {
                s.reschedule_on(1);
                s.switched(1);
                if s.current_on(1) == t {
                    break;
                }
                thread::yield_now();
            }
        })
    };
    h0.join().unwrap();
    h1.join().unwrap();
    s.charge_all();
    let (cpu1, t1) = (s.record(t).unwrap().cpu, NOW.load(Ordering::SeqCst));
    assert!(
        cpu1 - cpu0 <= t1 - t0,
        "T was charged {} ns in {} ns (on hart {})",
        cpu1 - cpu0,
        t1 - t0,
        if s.current_on(1) == t { 1 } else { 0 }
    );
}

#[test]
fn a_pulled_thread_is_not_charged_twice() {
    let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
    shuttle::check_random(model, 3000);
}
