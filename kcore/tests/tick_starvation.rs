//! T127 (found while building the interactive shell): a periodic wake of a higher-priority thread at every tick (the
//! interactive boot's serial bridge sleeps one tick in a loop) must not stop equal-priority threads from taking turns.
//! The scheduler used to charge the whole tick to the thread the wake had just preempted, so the thread that really ran
//! never used up its slice and its equal-priority peers starved (one hart, one spinner, the console server runnable but
//! never run).
use kcore::sched2::{FixedPrio, HartRq, SchedAttr, MS};
use std::collections::HashMap;

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

#[test]
fn a_periodic_high_priority_waker_does_not_starve_equal_peers() {
    let mut rq = HartRq::new(FixedPrio::new());
    rq.adopt_idle();
    let a = rq.spawn(attr(10)).unwrap();
    let b = rq.spawn(attr(10)).unwrap();
    let waker = rq.spawn(attr(20)).unwrap();
    let mut now = 0u64;
    let mut ran: HashMap<u32, u32> = HashMap::new();
    for _ in 0..200 {
        // the waker runs briefly and sleeps until the next tick
        if rq.current() == Some(waker) {
            rq.sleep_until(now + MS).unwrap();
        }
        let who = rq.current().expect("something runs");
        *ran.entry(who as u32).or_default() += 1;
        now += MS;
        rq.advance(MS);
    }
    let (ra, rb) = (
        ran.get(&(a as u32)).copied().unwrap_or(0),
        ran.get(&(b as u32)).copied().unwrap_or(0),
    );
    assert!(
        ra >= 60 && rb >= 60,
        "the two equal-priority threads got {ra} and {rb} of 200 ticks"
    );
}
