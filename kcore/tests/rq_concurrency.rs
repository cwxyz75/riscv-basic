//! L11b: the per-hart run-queue locks, under shuttle. Three harts, each a model thread, drive the real `kcore::sched_smp::Sched2`
//! over `Mutex`-backed run queues (one lock per hart, one for the thread table), exactly as the kernel drives it: ticks and
//! reschedule IPIs with no big kernel lock (the fast paths), everything else under a model big lock that is held until the hart has
//! "switched". Random schedules explore the interleavings of wake against pull against tick against block.
//!
//! The property is the one the per-hart locks can break: **no thread is executing on two harts**. Each hart records which thread
//! it executes (`exec`) before it calls `switched`, as the kernel does after a context switch; no other hart may be executing the
//! same thread at that moment. At the end every thread is in exactly one place (`Sched2::audit`, which takes every lock).
//!
//! Run by `make os-test` with `RUSTFLAGS="--cfg shuttle"`; without the cfg this file is empty.
#![cfg(shuttle)]

use kcore::sched::Tid;
use kcore::sched2::{FixedPrio, HartRq, SchedAttr, MS};
use kcore::sched_smp::{Rqs, Sched2, Table};
use shuttle::rand::Rng;
use shuttle::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use shuttle::sync::{Arc, Mutex, MutexGuard};
use shuttle::thread;

const HARTS: usize = 3;

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

struct World {
    s: Sched2<FixedPrio, SRqs>,
    /// The model big kernel lock: held by every path but the tick and the IPI, until the hart has switched.
    bkl: Mutex<()>,
    /// The thread each hart is executing (0: none yet).
    exec: Vec<AtomicU64>,
    /// The thread each hart last switched away from and has not yet looked at for reaping (the kernel's `percpu.prev`, T165): `reap_prev` takes the
    /// big lock again, after a variable delay, and frees it only if it exited and is not in transit.
    prev: Vec<AtomicU64>,
    /// Threads that exist (spawned and not yet reaped), and those that have exited and wait to be reaped.
    tids: Mutex<Vec<Tid>>,
    dead: Mutex<Vec<Tid>>,
    /// The reschedule IPI each hart has been sent (every poke, a poke for the hart itself included: the kernel sends itself one).
    ipi: Vec<AtomicBool>,
    /// Deferred mode: the kernel's `need_resched`, set where the kernel calls `note_resched` (after a wake, a spawn, a priority change)
    /// and by `finish_switch` when a mark is still set; the hart switches only when it sees the flag (or takes an IPI).
    need: Vec<AtomicBool>,
    deferred: bool,
    /// SCHED-SMP-SPEC G4: the big lock is released at the switch (the thread's last instructions run outside it).
    g4: bool,
}

impl World {
    /// What a hart does when it has finished a switch (or decided not to): it is now executing what is designated, and the thread
    /// it left is available to the other harts.
    fn finish(&self, h: usize) {
        let c = self.s.current_on(h);
        let old = self.exec[h].swap(c, Ordering::SeqCst);
        if old != 0 && old != c && old != self.s.idle(h) {
            self.prev[h].store(old, Ordering::SeqCst);
        }
        if c != self.s.idle(h) {
            for x in 0..HARTS {
                if x != h {
                    assert_ne!(
                        self.exec[x].load(Ordering::SeqCst),
                        c,
                        "thread {c} is executing on hart {h} while hart {x} still is"
                    );
                }
            }
        }
        self.s.switched(h);
        self.deliver(h);
        self.finish_flag(h);
    }

    /// The end of a path that held the big lock and switched: with the lock following the thread (K3) the lock is held until the
    /// switch is finished; with G4 it is released first, so other harts' wakes, pulls and reaps run while this hart is still executing
    /// the thread it is leaving.
    fn finish_g(&self, h: usize, g: MutexGuard<'_, ()>) {
        if self.g4 {
            drop(g);
            // (the switch itself: saving the old context and loading the new takes time during which other harts run)
            thread::yield_now();
            thread::yield_now();
            self.finish(h);
        } else {
            self.finish(h);
            drop(g);
        }
    }

    /// `finish_switch`: a mark still set when the switch is over is not left without its flag.
    fn finish_flag(&self, h: usize) {
        if self.deferred && self.s.resched_pending(h) {
            self.need[h].store(true, Ordering::SeqCst);
        }
    }

    /// `note_resched`: after a wake, a spawn or a priority change.
    fn note(&self, h: usize) {
        if self.deferred {
            self.deliver(h);
            self.finish_flag(h);
        }
    }

    /// `send_pokes`: an IPI to every poked hart; a poke for the hart itself becomes an IPI to itself.
    fn deliver(&self, _h: usize) {
        let pokes = self.s.take_pokes();
        for x in 0..HARTS {
            if pokes & (1 << x) != 0 {
                self.ipi[x].store(true, Ordering::SeqCst);
            }
        }
    }
}

fn hart(w: Arc<World>, h: usize, steps: usize, deferred: bool) {
    let mut now = 0u64;
    let mut rng = shuttle::rand::thread_rng();
    for _ in 0..steps {
        if w.ipi[h].swap(false, Ordering::SeqCst)
            || (deferred && w.need[h].swap(false, Ordering::SeqCst))
        {
            // (deferred mode: the defined point of a hart that a wake of its own marked, no IPI owed)
            w.s.reschedule_on(h);
            finish_fast(&w, h);
        }
        let exec = w.exec[h].load(Ordering::SeqCst);
        let running_user = exec != 0 && exec != w.s.idle(h);
        match rng.gen_range(0..12u32) {
            // the tick and the reschedule IPI: no big lock unless the hart must switch
            0 | 1 => {
                now += MS;
                w.s.tick_on(h, now);
                finish_fast(&w, h);
            }
            2 => {
                w.s.reschedule_on(h);
                finish_fast(&w, h);
            }
            3 => {
                let _g = w.bkl.lock().unwrap();
                let list = w.tids.lock().unwrap().clone();
                if !list.is_empty() {
                    let t = list[rng.gen_range(0..list.len())];
                    // a thread that exists is always found, whatever migrations race with the look
                    assert!(w.s.wake_on(h, t).is_ok(), "wake of live thread {t} failed");
                }
                w.note(h);
                w.finish_g(h, _g);
            }
            4 if running_user => {
                let _g = w.bkl.lock().unwrap();
                let _ = w.s.prepare_block_on(h);
                let _ = w.s.block_on(h);
                w.finish_g(h, _g);
            }
            5 if running_user => {
                let _g = w.bkl.lock().unwrap();
                let _ = w.s.yield_on(h);
                w.finish_g(h, _g);
            }
            6 if running_user => {
                let _g = w.bkl.lock().unwrap();
                let _ = w.s.sleep_until_on(h, now + 2 * MS);
                w.finish_g(h, _g);
            }
            7 if running_user && rng.gen_range(0..3u32) == 0 => {
                let _g = w.bkl.lock().unwrap();
                w.dead.lock().unwrap().push(exec);
                let _ = w.s.exit_on(h);
                if deferred {
                    // the kernel's `exit`: wake the joiners, then settle once more before the switch
                    let list = w.tids.lock().unwrap().clone();
                    if !list.is_empty() {
                        let t = list[rng.gen_range(0..list.len())];
                        let _ = w.s.wake_on(h, t);
                    }
                    w.s.reschedule_on(h);
                }
                w.finish_g(h, _g);
            }
            8 => {
                // the reaper: free an exited thread nobody executes any more
                let _g = w.bkl.lock().unwrap();
                let mut dead = w.dead.lock().unwrap();
                if let Some(i) = dead.iter().position(|&t| {
                    w.s.state(t) == Some(kcore::sched::State::Exited)
                        && (0..HARTS).all(|x| w.exec[x].load(Ordering::SeqCst) != t)
                }) {
                    let t = dead.remove(i);
                    w.tids.lock().unwrap().retain(|&x| x != t);
                    assert!(w.s.reap(t).is_ok(), "reap of {t} failed");
                }
            }
            11 => {
                // `reap_prev` (T165; dropping the `is_in_transit` test below is an equivalent mutant here: the model sets `prev` only once the hart's switch is
                // complete, so the thread is never in transit when it is consumed; the assertion on `exec` is what catches a premature free): the lock taken again after the switch, and the thread this hart left freed by the kernel's rule (exited, not in
                // transit): no hart may be executing it, whatever the other harts did between the switch and now
                let _g = w.bkl.lock().unwrap();
                let p = w.prev[h].swap(0, Ordering::SeqCst);
                if p != 0 {
                    let mut dead = w.dead.lock().unwrap();
                    if let Some(i) = dead.iter().position(|&t| t == p) {
                        if w.s.state(p) == Some(kcore::sched::State::Exited)
                            && !w.s.is_in_transit(p)
                        {
                            for x in 0..HARTS {
                                assert_ne!(
                                    w.exec[x].load(Ordering::SeqCst),
                                    p,
                                    "reap_prev: thread {p} is freed while hart {x} still executes it"
                                );
                            }
                            dead.remove(i);
                            w.tids.lock().unwrap().retain(|&x| x != p);
                            assert!(w.s.reap(p).is_ok(), "reap_prev of {p} failed");
                        }
                    }
                }
            }
            9 => {
                let _g = w.bkl.lock().unwrap();
                let list = w.tids.lock().unwrap().clone();
                if !list.is_empty() {
                    let t = list[rng.gen_range(0..list.len())];
                    let _ = w.s.set_priority_on(h, t, rng.gen_range(1..8u8));
                }
                w.note(h);
                w.finish_g(h, _g);
            }
            _ => {
                let _g = w.bkl.lock().unwrap();
                if w.tids.lock().unwrap().len() < 6 {
                    let prio = rng.gen_range(1..8u8);
                    if let Ok(t) = w.s.spawn_on(h, SchedAttr { prio, rr: false }, None) {
                        w.tids.lock().unwrap().push(t);
                    }
                }
                w.note(h);
                w.finish_g(h, _g);
            }
        }
    }
}

/// `timer_tick_fast` / `reschedule_fast`: if the designation is still what the hart executes, nothing is in transit; else it takes
/// the big lock and switches.
fn finish_fast(w: &World, h: usize) {
    if w.s.current_on(h) == w.exec[h].load(Ordering::SeqCst) {
        w.s.switched(h);
        w.deliver(h);
        w.finish_flag(h);
    } else {
        let _g = w.bkl.lock().unwrap();
        w.finish_g(h, _g);
    }
}

fn model(steps: usize, deferred: bool, g4: bool) {
    let rqs = SRqs {
        rq: (0..HARTS)
            .map(|_| Mutex::new(HartRq::new(FixedPrio::new())))
            .collect(),
        table: Mutex::new(Table::new()),
    };
    let w = Arc::new(World {
        s: Sched2::over(rqs),
        bkl: Mutex::new(()),
        exec: (0..HARTS).map(|_| AtomicU64::new(0)).collect(),
        prev: (0..HARTS).map(|_| AtomicU64::new(0)).collect(),
        tids: Mutex::new(Vec::new()),
        dead: Mutex::new(Vec::new()),
        ipi: (0..HARTS).map(|_| AtomicBool::new(false)).collect(),
        need: (0..HARTS).map(|_| AtomicBool::new(false)).collect(),
        deferred,
        g4,
    });
    w.s.set_deferred(deferred);
    for h in 0..HARTS {
        let i = w.s.idle(h);
        w.exec[h].store(i, Ordering::SeqCst);
    }
    let hs: Vec<_> = (0..HARTS)
        .map(|h| {
            let w = w.clone();
            thread::spawn(move || hart(w, h, steps, deferred))
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // quiesce: deliver every poke and look at the queues until nothing is pending; then GP1 as well as GP2 must hold
    for _ in 0..30 {
        let mut again = false;
        for h in 0..HARTS {
            if w.ipi[h].swap(false, Ordering::SeqCst)
                || (deferred && w.need[h].swap(false, Ordering::SeqCst))
            {
                w.s.reschedule_on(h);
                let _g = w.bkl.lock().unwrap();
                w.finish(h);
                again = true;
            }
        }
        if !again {
            break;
        }
    }
    // every mark owed an IPI got one (a mark whose poke was dropped leaves the hart pending for a whole tick)
    for h in 0..HARTS {
        assert!(
            !w.s.resched_pending(h),
            "hart {h} is marked but no IPI reached it"
        );
    }
    w.s.audit(true)
        .expect("every thread is in exactly one place, and GP1 holds at quiescence");
}

fn explore(steps: usize, iterations: usize, deferred: bool, g4: bool) {
    shuttle::check_random(move || model(steps, deferred, g4), iterations);
}

#[test]
fn no_thread_executes_on_two_harts_under_per_hart_locks() {
    explore(14, 20000, false, false);
}

/// X7: a wake never switches. The same model with the scheduler in deferred mode: wakes, spawns and priority changes mark the caller's own
/// hart instead of settling it, and the hart looks at its queues at its next step (its defined point). No thread may execute on two
/// harts, and GP1 must hold at quiescence once every hart has had its point.
#[test]
fn no_thread_executes_on_two_harts_when_wakes_are_deferred() {
    explore(14, 20000, true, false);
}

/// G4 (the lock released at the switch, the deployed kernel's mode from L12): the same model with the lock dropped before each switch
/// is finished. A thread that blocked or exited is still executing on its hart until then: no other hart may run, pull or reap it.
#[test]
fn no_thread_executes_on_two_harts_when_the_lock_is_released_at_the_switch() {
    explore(14, 20000, true, true);
}

/// Replay one recorded schedule (`REPLAY_SCHEDULE=<file>`): how a failure that CI printed is reproduced here (T162).
#[test]
fn replay_a_recorded_schedule() {
    if let Ok(f) = std::env::var("REPLAY_SCHEDULE") {
        let sched = std::fs::read_to_string(f).unwrap();
        let deferred = std::env::var("REPLAY_DEFERRED").is_ok();
        shuttle::replay(
            move || model(14, deferred, std::env::var("REPLAY_G4").is_ok()),
            sched.trim(),
        );
    }
}
