//! PM1: harts go offline and come back (KERNEL-V1-SPEC 5.3). HT1: no thread is homed on, and nothing is placed on, a hart that is not online. HT2: a thread that
//! could run is never stranded by a hart going offline, and a sleeper homed there still wakes on time.

use kcore::sched::{State, Tid};
use kcore::sched2::{Error, FixedPrio, SchedAttr, MS};
use kcore::sched_smp::Sched2;

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

/// What the kernel does for hart `h` after a change: look at the queues, finish the switch, and (offline) move what is homed there. Returns what is left there.
#[allow(clippy::needless_range_loop)] // (the hart index is what the scheduler takes)
fn settle(s: &Sched2<FixedPrio>, harts: usize, offline: &[bool]) {
    for _ in 0..4 {
        for h in 0..harts {
            s.reschedule_on(h);
            s.switched(h);
            if offline[h] {
                s.evacuate(h);
            }
        }
    }
}

fn homed_on(s: &Sched2<FixedPrio>, h: usize) -> Vec<Tid> {
    s.tids()
        .into_iter()
        .filter(|&t| s.home(t) == Some(h) && t != s.idle(h))
        .collect()
}

#[test]
fn a_hart_goes_offline_with_runners_queued_and_sleepers_and_comes_back() {
    let s = Sched2::new(4, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    // three threads of 10, one per idle hart; one of them sleeps with a deadline, one is queued behind a better thread
    let a = s.spawn_on(0, attr(10), None).unwrap();
    let b = s.spawn_on(0, attr(10), None).unwrap();
    let c = s.spawn_on(0, attr(10), None).unwrap();
    let mut off = [false; 4];
    settle(&s, 4, &off);
    let hb = s.home(b).unwrap();
    // b sleeps until 5 ms
    s.sleep_until_on(hb, 5 * MS).unwrap();
    settle(&s, 4, &off);
    assert_eq!(s.state(b), Some(State::Blocked));
    // take the hart b is homed on offline
    s.begin_offline(hb).unwrap();
    off[hb] = true;
    settle(&s, 4, &off);
    assert!(
        homed_on(&s, hb).is_empty(),
        "HT1: nothing is homed on the offline hart: {:?}",
        homed_on(&s, hb)
    );
    for t in [a, b, c] {
        assert_ne!(s.home(t), Some(hb));
    }
    assert_eq!(
        s.current_on(hb),
        s.idle(hb),
        "an offline hart runs only its idle thread"
    );
    // b's timer moved with it: it wakes on time on the hart it went to
    let hb2 = s.home(b).unwrap();
    s.tick_on(hb2, 4 * MS);
    assert_eq!(s.state(b), Some(State::Blocked), "not yet");
    s.tick_on(hb2, 6 * MS);
    settle(&s, 4, &off);
    assert!(
        matches!(s.state(b), Some(State::Runnable | State::Running)),
        "b woke at its deadline: {:?}",
        s.state(b)
    );
    // a new thread is never placed on the offline hart
    for _ in 0..6 {
        let t = s.spawn_on(0, attr(10), None).unwrap();
        settle(&s, 4, &off);
        assert_ne!(s.home(t), Some(hb), "HT1: placed on an offline hart");
    }
    s.audit(true).unwrap();
    // and it comes back: placement may choose it again
    s.finish_online(hb, 7 * MS);
    off[hb] = false;
    settle(&s, 4, &off);
    s.audit(true).unwrap();
}

#[test]
fn the_last_online_hart_and_a_hart_with_a_pinned_thread_cannot_go_offline() {
    let s = Sched2::new(3, FixedPrio::new);
    s.adopt_current_on(0, attr(20), None).unwrap();
    let pinned = s.spawn_on(0, attr(10), Some(2)).unwrap();
    assert_eq!(s.home(pinned), Some(2));
    assert_eq!(
        s.begin_offline(2),
        Err(Error::BadState),
        "a thread is pinned there"
    );
    s.begin_offline(1).unwrap();
    assert_eq!(
        s.begin_offline(0).map(|_| ()),
        Ok(()),
        "hart 0 may go while hart 2 is online"
    );
    assert_eq!(
        s.begin_offline(2),
        Err(Error::BadState),
        "the last online hart (and pinned)"
    );
    let t = Sched2::new(2, FixedPrio::new);
    t.begin_offline(1).unwrap();
    assert_eq!(
        t.begin_offline(0),
        Err(Error::BadState),
        "the last online hart"
    );
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[test]
#[allow(clippy::needless_range_loop)] // (the hart index is what the scheduler takes)
fn random_walks_with_harts_going_offline_and_online_keep_ht1_and_ht2() {
    for seed in 1..=80u64 {
        let harts = 4usize;
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let s = Sched2::new(harts, FixedPrio::new);
        s.adopt_current_on(0, attr(25), None).unwrap();
        let mut off = [false; 4];
        let mut now = 0u64;
        let mut threads: Vec<Tid> = Vec::new();
        let mut sleeping: Vec<(Tid, u64)> = Vec::new();
        for step in 0..300 {
            match r.below(8) {
                0 | 1 => {
                    if threads.len() < 12 {
                        threads.push(s.spawn_on(0, attr(1 + r.below(20) as u8), None).unwrap());
                    }
                }
                2 => {
                    // a running thread sleeps with a deadline
                    let h = r.below(harts as u64) as usize;
                    let cur = s.current_on(h);
                    if threads.contains(&cur)
                        && s.sleep_until_on(h, now + (1 + r.below(8)) * MS).is_ok()
                    {
                        sleeping.push((cur, now));
                    }
                }
                3 => {
                    // time passes
                    now += r.below(3) * MS;
                    for h in 0..harts {
                        s.tick_on(h, now);
                    }
                }
                4 => {
                    let h = r.below(harts as u64) as usize;
                    if !off[h] && s.begin_offline(h).is_ok() {
                        off[h] = true;
                    }
                }
                5 => {
                    let h = r.below(harts as u64) as usize;
                    if off[h] && homed_on(&s, h).is_empty() {
                        s.finish_online(h, now);
                        off[h] = false;
                    }
                }
                _ => {}
            }
            settle(&s, harts, &off);
            // HT1: nothing homed on an offline hart once it has been drained, and nothing runs there but its idle thread
            for h in 0..harts {
                if off[h] {
                    assert_eq!(
                        s.current_on(h),
                        s.idle(h),
                        "seed {seed} step {step}: an offline hart runs a thread"
                    );
                    assert!(
                        homed_on(&s, h).is_empty(),
                        "seed {seed} step {step}: HT1 on hart {h}: {:?}",
                        homed_on(&s, h)
                    );
                }
            }
            // HT2: every runnable thread is running or queued on an online hart
            for &t in &threads {
                if matches!(s.state(t), Some(State::Runnable)) {
                    let h = s.home(t).unwrap();
                    assert!(
                        !off[h],
                        "seed {seed} step {step}: a runnable thread is homed on offline hart {h}"
                    );
                }
            }
            s.audit(true)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
        }
        // every sleeper wakes once its deadline has passed: advance far and settle
        now += 20 * MS;
        for h in 0..harts {
            s.tick_on(h, now);
        }
        settle(&s, harts, &off);
        for (t, _) in sleeping {
            assert_ne!(
                s.state(t),
                Some(State::Blocked),
                "seed {seed}: a sleeper never woke (HT2)"
            );
        }
    }
}
