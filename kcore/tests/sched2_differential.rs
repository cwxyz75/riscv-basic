//! Task 6 (SCHED-SMP-SPEC 4.6): "behaviour unchanged" is tested, not asserted. The old scheduler `sched::Sched` is
//! the oracle; the same random operation sequences drive it (one hart, fixed priority, 1 ms ticks) and the new
//! `sched2::HartRq` (nanosecond clock, one tick = 1 ms); after every operation the thread that should be running and
//! every thread's state must be identical. Priorities are in the normal band (1 to 31): the real-time class differs
//! on purpose (FIFO, no slice rotation) and has its own tests below.
mod common;
use common::Rng;
use kcore::sched::{FixedPriority, Sched, Tid, PRIO_IDLE};
use kcore::sched2::{FixedPrio, HartRq, SchedAttr, MS};

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

#[test]
fn the_new_normal_band_behaves_like_the_old_scheduler() {
    for seed in 0..400u64 {
        let mut rng = Rng::new(seed);
        let mut old = Sched::with_harts(FixedPriority::new(), 1);
        let mut new = HartRq::new(FixedPrio::new());
        let idle_old = old.adopt_current_on(0, PRIO_IDLE, None).unwrap();
        let idle_new = new.adopt_idle();
        // threads created so far, as (old tid, new tid)
        let mut pairs: Vec<(Tid, Tid)> = vec![(idle_old, idle_new)];
        let map_old = |pairs: &[(Tid, Tid)], t: Option<Tid>| {
            t.map(|t| pairs.iter().find(|p| p.0 == t).expect("known thread").1)
        };
        for step in 0..250 {
            let cur_new = new.current();
            let on_idle = cur_new == Some(idle_new);
            let desc;
            match rng.next() % 10 {
                0 | 1 => {
                    let p = 1 + (rng.next() % 8) as u8;
                    desc = format!("spawn {p}");
                    let a = old.spawn(p).unwrap();
                    let b = new.spawn(attr(p)).unwrap();
                    pairs.push((a, b));
                }
                2 | 3 if pairs.len() > 1 => {
                    let (a, b) = pairs[1 + rng.next() as usize % (pairs.len() - 1)];
                    desc = format!("wake {b}");
                    old.wake(a).unwrap();
                    new.wake(b).unwrap();
                }
                4 if !on_idle => {
                    desc = "block".into();
                    assert_eq!(
                        old.block_current().unwrap(),
                        new.block_current().unwrap(),
                        "seed {seed} step {step}: {desc}"
                    );
                }
                5 if !on_idle => {
                    let ticks = 1 + rng.next() % 25;
                    desc = format!("sleep {ticks}");
                    let a = old.sleep_until(old.now() + ticks).unwrap();
                    let b = new.sleep_until(new.now() + ticks * MS).unwrap();
                    assert_eq!(a, b, "seed {seed} step {step}: {desc}");
                }
                6 if !on_idle => {
                    desc = "yield".into();
                    old.yield_current().unwrap();
                    new.yield_now().unwrap();
                }
                7 if !on_idle && rng.next().is_multiple_of(3) => {
                    desc = "exit".into();
                    old.exit_current().unwrap();
                    new.exit_current().unwrap();
                }
                8 if pairs.len() > 1 => {
                    let (a, b) = pairs[1 + rng.next() as usize % (pairs.len() - 1)];
                    let p = 1 + (rng.next() % 12) as u8;
                    desc = format!("set_priority {b} {p}");
                    let ra = old.set_priority(a, p);
                    let rb = new.set_priority(b, p);
                    assert_eq!(ra.is_ok(), rb.is_ok(), "seed {seed} step {step}: {desc}");
                }
                _ => {
                    let ticks = 1 + rng.next() % 4;
                    desc = format!("advance {ticks}");
                    for _ in 0..ticks {
                        old.tick();
                        new.advance(MS);
                    }
                }
            }
            assert_eq!(
                map_old(&pairs, old.current()),
                new.current(),
                "seed {seed} step {step} ({desc}): the thread that should run differs"
            );
            for &(a, b) in &pairs[1..] {
                let (so, sn) = (old.state(a), new.state(b));
                let same = match (so, sn) {
                    (Some(x), Some(y)) => x == y,
                    (None, None) => true,
                    _ => false,
                };
                assert!(same, "seed {seed} step {step} ({desc}): thread {b} is {so:?} in the old, {sn:?} in the new");
            }
            old.audit().unwrap_or_else(|e| panic!("old audit: {e}"));
            new.audit()
                .unwrap_or_else(|e| panic!("seed {seed} step {step} ({desc}): new audit: {e}"));
        }
    }
}

/// The real-time band's own rules: strictly above the normal band, FIFO within a level, a preempted thread back
/// at the front of its level, no rotation unless `rr`, and rotation with it.
#[test]
fn the_real_time_band_is_strict_fifo_and_rr_is_opt_in() {
    let mut rq = HartRq::new(FixedPrio::new());
    let idle = rq.adopt_idle();
    let n = rq.spawn(attr(31)).unwrap();
    assert_eq!(
        rq.current(),
        Some(n),
        "a normal thread runs on an idle hart"
    );
    let r1 = rq.spawn(attr(40)).unwrap();
    assert_eq!(
        rq.current(),
        Some(r1),
        "RT preempts any normal thread, even priority 31"
    );
    let r2 = rq.spawn(attr(40)).unwrap();
    assert_eq!(
        rq.current(),
        Some(r1),
        "an equal RT thread does not preempt: FIFO"
    );
    for _ in 0..50 {
        rq.advance(MS);
    }
    assert_eq!(
        rq.current(),
        Some(r1),
        "no slice rotation for a FIFO RT thread"
    );
    let r3 = rq.spawn(attr(50)).unwrap();
    assert_eq!(rq.current(), Some(r3));
    rq.block_current().unwrap();
    assert_eq!(
        rq.current(),
        Some(r1),
        "the preempted thread resumes first: it went to the front of its level"
    );
    rq.exit_current().unwrap();
    assert_eq!(rq.current(), Some(r2));
    rq.exit_current().unwrap();
    assert_eq!(
        rq.current(),
        Some(n),
        "the normal band runs when no RT thread is runnable"
    );
    rq.exit_current().unwrap();
    assert_eq!(rq.current(), Some(idle));
    rq.audit().unwrap();
    // round robin among equals when asked for
    let a = rq.spawn(SchedAttr { prio: 40, rr: true }).unwrap();
    let b = rq.spawn(SchedAttr { prio: 40, rr: true }).unwrap();
    assert_eq!(rq.current(), Some(a));
    for _ in 0..10 {
        rq.advance(MS);
    }
    assert_eq!(
        rq.current(),
        Some(b),
        "an rr thread rotates after its 10 ms"
    );
    rq.audit().unwrap();
}

#[test]
fn bands_and_queues_are_what_the_spec_says() {
    use kcore::sched2::{band, Band, PrioQueues};
    assert_eq!(
        (band(0), band(1), band(31), band(32), band(63)),
        (Band::Idle, Band::Normal, Band::Normal, Band::Rt, Band::Rt)
    );
    let mut q = PrioQueues::new();
    assert!(q.is_empty() && q.top().is_none());
    q.push_back(5, 1);
    q.push_back(5, 2);
    q.push_front(5, 3);
    q.push_back(40, 4);
    assert_eq!(q.top(), Some(40));
    assert_eq!(q.pop_best(), Some(4));
    assert_eq!(q.pop_best(), Some(3), "front first");
    assert!(q.remove(2));
    assert!(!q.remove(2));
    assert_eq!(q.top_where(&|t| t == 1), Some((1, 5)));
    assert_eq!(q.pop_best(), Some(1));
    assert!(q.is_empty() && q.top().is_none());
}
