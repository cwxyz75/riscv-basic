//! The per-hart timer queue (SCHED-SMP-SPEC 4.11, task 13): the hardware timer is programmed to the earliest *hard* expiry
//! (deadline plus slack); when it fires, every entry whose *soft* expiry has passed is expired in the same interrupt. Invariants: an
//! entry never fires before its deadline; it fires no later than its hard expiry when the timer is served on time; the number of
//! interrupts never exceeds the number of distinct hard expiries; a hart with one runnable thread and no deadline has no event at all.
//! A negative control (a timer programmed to the soft expiry of the first entry only) must raise the interrupt count.
use kcore::sched::Tid;
use kcore::sched2::{FixedPrio, SchedAttr, DEFAULT_SLACK, MS};
use kcore::sched_smp::Sched2;
use std::collections::BTreeMap;

const US: u64 = 1_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn attr(prio: u8) -> SchedAttr {
    SchedAttr { prio, rr: false }
}

/// One hart; `entries` (soft deadline, slack) threads each put to sleep. Returns (tid -> (soft, slack)) and the scheduler, all asleep.
fn sleepers(entries: &[(u64, u64)], prio: u8) -> (Sched2<FixedPrio>, BTreeMap<Tid, (u64, u64)>) {
    let s = Sched2::new(1, FixedPrio::new);
    let mut tids = Vec::new();
    for _ in entries {
        tids.push(s.spawn_on(0, attr(prio), Some(0)).unwrap());
    }
    s.reschedule_on(0);
    s.switched(0);
    let mut by = BTreeMap::new();
    // each thread runs in turn (FIFO at one priority) and goes to sleep with its slack and deadline
    for (i, _) in entries.iter().enumerate() {
        let c = s.current_on(0);
        assert_ne!(c, s.idle(0), "a thread should be running to sleep");
        let k = tids.iter().position(|&t| t == c).unwrap();
        let (soft, slack) = entries[k];
        s.set_timer_slack(c, slack).unwrap();
        s.prepare_block_on(0).unwrap();
        assert!(s.sleep_until_on(0, soft).unwrap(), "entry {i} blocks");
        s.switched(0);
        by.insert(c, (soft, slack));
    }
    assert_eq!(s.current_on(0), s.idle(0));
    (s, by)
}

/// Run the hardware: serve the timer when it is due (late by `lateness(r)`), returning (interrupts, wake times by tid).
fn run(
    s: &Sched2<FixedPrio>,
    by: &BTreeMap<Tid, (u64, u64)>,
    mut program: impl FnMut(
        &Sched2<FixedPrio>,
        &BTreeMap<Tid, (u64, u64)>,
        &BTreeMap<Tid, u64>,
    ) -> Option<u64>,
    late: impl Fn() -> u64,
) -> (usize, BTreeMap<Tid, u64>) {
    let mut woke: BTreeMap<Tid, u64> = BTreeMap::new();
    let mut now = 0u64;
    let mut interrupts = 0;
    for _ in 0..10_000 {
        if woke.len() == by.len() {
            break;
        }
        let at = program(s, by, &woke).expect("sleepers remain: the timer is armed");
        now = now.max(at) + late();
        s.set_clock(0, now.saturating_sub(1)); // (the tick below advances it)
        s.tick_on(0, now);
        interrupts += 1;
        // whatever woke runs and ends (so it is counted once)
        loop {
            let c = s.current_on(0);
            if c == s.idle(0) {
                break;
            }
            woke.insert(c, now);
            s.exit_on(0).unwrap();
            s.switched(0);
        }
    }
    (interrupts, woke)
}

fn real_program(
    s: &Sched2<FixedPrio>,
    _: &BTreeMap<Tid, (u64, u64)>,
    _: &BTreeMap<Tid, u64>,
) -> Option<u64> {
    s.next_event_on(0)
}

#[test]
fn an_entry_never_fires_early_and_fires_by_its_hard_expiry_when_served_on_time() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    for round in 0..300 {
        let n = 1 + r.below(12) as usize;
        let entries: Vec<(u64, u64)> = (0..n)
            .map(|_| {
                (
                    MS + r.below(5 * MS),
                    [0, 10 * US, DEFAULT_SLACK, 200 * US, MS][r.below(5) as usize],
                )
            })
            .collect();
        let (s, by) = sleepers(&entries, 10);
        let (interrupts, woke) = run(&s, &by, real_program, || 0);
        assert_eq!(woke.len(), n, "round {round}: everything woke");
        for (t, &(soft, slack)) in &by {
            let at = woke[t];
            assert!(
                at >= soft,
                "round {round}: thread {t} woke {} ns early",
                soft - at
            );
            assert!(
                at <= soft + slack,
                "round {round}: thread {t} woke {} ns after its hard expiry",
                at - (soft + slack)
            );
        }
        let mut hards: Vec<u64> = by.values().map(|&(a, b)| a + b).collect();
        hards.sort_unstable();
        hards.dedup();
        assert!(
            interrupts <= hards.len(),
            "round {round}: {interrupts} interrupts for {} distinct hard expiries",
            hards.len()
        );
    }
}

#[test]
fn nearby_wakes_share_one_interrupt() {
    // three sleepers within each other's slack: one interrupt at the first hard expiry wakes all three
    let (s, by) = sleepers(
        &[
            (1000 * US, 50 * US),
            (1010 * US, 50 * US),
            (1020 * US, 50 * US),
        ],
        10,
    );
    let (interrupts, woke) = run(&s, &by, real_program, || 0);
    assert_eq!(woke.len(), 3);
    assert_eq!(interrupts, 1, "coalesced into one interrupt");
}

#[test]
fn negative_control_a_timer_at_the_soft_expiry_of_the_first_entry_raises_the_interrupt_count() {
    let entries = [
        (1000 * US, 50 * US),
        (1010 * US, 50 * US),
        (1020 * US, 50 * US),
    ];
    let (s, by) = sleepers(&entries, 10);
    let soft_first =
        |_: &Sched2<FixedPrio>, by: &BTreeMap<Tid, (u64, u64)>, woke: &BTreeMap<Tid, u64>| {
            by.iter()
                .filter(|(t, _)| !woke.contains_key(t))
                .map(|(_, &(soft, _))| soft)
                .min()
        };
    let (interrupts, woke) = run(&s, &by, soft_first, || 0);
    assert_eq!(woke.len(), 3);
    assert_eq!(
        interrupts, 3,
        "without slack coalescing every wake is its own interrupt"
    );
}

#[test]
fn a_real_time_thread_has_no_slack_and_wakes_at_its_deadline() {
    // the band decides: priority 60 is real time, whatever slack was asked for
    let (s, by) = sleepers(&[(2 * MS, 500 * US)], 60);
    assert_eq!(
        s.next_event_on(0),
        Some(2 * MS),
        "the hard expiry equals the deadline"
    );
    let (_, woke) = run(&s, &by, real_program, || 0);
    assert_eq!(*woke.values().next().unwrap(), 2 * MS);
}

#[test]
fn one_runnable_thread_and_no_deadline_means_no_timer_event() {
    let s = Sched2::new(1, FixedPrio::new);
    let t = s.spawn_on(0, attr(10), Some(0)).unwrap();
    s.reschedule_on(0);
    s.switched(0);
    assert_eq!(s.current_on(0), t);
    assert_eq!(
        s.next_event_on(0),
        None,
        "alone on the hart: no slice timer, no interrupt"
    );
    // a peer of the same priority makes the slice end an event; a lower one does not
    let low = s.spawn_on(0, attr(5), Some(0)).unwrap();
    s.switched(0);
    assert_eq!(
        s.next_event_on(0),
        None,
        "a lower-priority waiter never rotates the running thread"
    );
    let _ = low;
    let peer = s.spawn_on(0, attr(10), Some(0)).unwrap();
    s.switched(0);
    let _ = peer;
    assert!(
        s.next_event_on(0).is_some(),
        "an equal peer waits: the slice end is an event"
    );
}

#[test]
fn an_idle_hart_with_nothing_sleeping_has_no_event() {
    let s = Sched2::new(2, FixedPrio::new);
    assert_eq!(s.next_event_on(0), None);
    assert_eq!(s.next_event_on(1), None);
}
