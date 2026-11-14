//! Self-tests for S2: kernel threads, the timer interrupt, preemption, the single
//! block/wake path. They run in the boot thread at real-time priority 40, with
//! worker threads below or above it as each test needs. Time is measured in timer
//! ticks (1 ms), counted by the interrupt handler, so results do not depend on how
//! fast the host runs the emulator.

use crate::thread::{self, IDLE_SPINS};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

type Outcome = Result<(), String>;
type Test = (&'static str, fn() -> Outcome);

pub const MAIN_PRIO: u8 = 40;
/// Bytes of heap a test may keep for the kernel's lazily grown containers.
const SLACK: usize = 1024;
/// How many ticks late a wake may be. Fixed priority wakes the thread at once; plain
/// round robin makes it queue behind the idle thread's slice.
const LATE_TICKS: u64 = if cfg!(feature = "sched-round-robin") {
    3 * kcore::sched::TIMESLICE_TICKS as u64 + 1
} else {
    1
};
const NORMAL: u8 = 10;

fn ensure(c: bool, what: &str) -> Outcome {
    if c {
        Ok(())
    } else {
        Err(String::from(what))
    }
}

fn ensure_eq<T: PartialEq + core::fmt::Debug>(got: T, want: T, what: &str) -> Outcome {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

/// Run `body`, then require that the heap got everything back (threads and their
/// stacks included) and no stack overflowed. The kernel's own containers (the thread
/// table, the timer set, wait queues) grow lazily and keep a node or two, so the
/// comparison allows a little; a leaked thread costs its 16 KiB stack.
fn clean(body: impl FnOnce() -> Outcome) -> Outcome {
    let before = thread::heap_free();
    let threads = (thread::thread_count(), thread::sched_thread_count());
    body()?;
    ensure(thread::stacks_intact(), "a stack guard was overwritten")?;
    ensure_eq(
        thread::thread_count(),
        threads.0,
        "live thread records after the test",
    )?;
    ensure_eq(
        thread::sched_thread_count(),
        threads.1,
        "scheduler-core thread records after the test (a missing reap)",
    )?;
    let after = thread::heap_free();
    ensure(
        after + SLACK >= before,
        &format!("heap bytes free fell from {before} to {after}: leak"),
    )
}

fn timer_ticks_advance() -> Outcome {
    let start = thread::ticks();
    let mut spins = 0u64;
    while thread::ticks() < start + 3 {
        spins += 1;
        ensure(spins < 2_000_000_000, "the timer interrupt never arrived")?;
        core::hint::spin_loop();
    }
    Ok(())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn adder(n: usize) {
    for _ in 0..1000 {
        COUNTER.fetch_add(n as u64, SeqCst);
        thread::yield_now();
    }
}

fn threads_start_run_and_finish() -> Outcome {
    clean(|| {
        COUNTER.store(0, SeqCst);
        let tids: Vec<_> = (1..=3)
            .map(|n| thread::spawn("adder", NORMAL, adder, n))
            .collect();
        for t in tids {
            thread::join(t);
        }
        ensure_eq(
            COUNTER.load(SeqCst),
            1000 * (1 + 2 + 3),
            "sum of the workers' additions",
        )
    })
}

fn many_threads_do_not_leak() -> Outcome {
    clean(|| {
        for _ in 0..100 {
            let t = thread::spawn("short", NORMAL, |_| {}, 0);
            thread::join(t);
        }
        Ok(())
    })
}

fn sleep_wakes_on_time() -> Outcome {
    let t0 = thread::ticks();
    thread::sleep_ticks(5);
    let elapsed = thread::ticks() - t0;
    ensure(
        (5..=6 + LATE_TICKS).contains(&elapsed),
        &format!("slept {elapsed} ticks, wanted 5"),
    )
}

fn idle_runs_when_nothing_else_does() -> Outcome {
    let before = IDLE_SPINS.load(SeqCst);
    thread::sleep_ticks(3);
    ensure(
        IDLE_SPINS.load(SeqCst) > before,
        "the idle thread did not run while everything slept",
    )
}

fn wake_before_block_is_not_lost() -> Outcome {
    let me = thread::current();
    let t0 = thread::ticks();
    thread::wake(me); // nobody is blocked: this leaves a token
    thread::wake(me); // and a second wake does not queue
    thread::block(); // consumes the token
    ensure(
        thread::ticks() - t0 <= 1,
        "block waited although a wake had already arrived",
    )?;
    // The token is gone: now block for real, woken by a thread that wakes us after 3 ticks.
    let waker = thread::spawn(
        "waker",
        NORMAL,
        |me| {
            thread::sleep_ticks(3);
            thread::wake(me as u32);
        },
        me as usize,
    );
    let t1 = thread::ticks();
    thread::block();
    let waited = thread::ticks() - t1;
    thread::join(waker);
    ensure(
        (3..=5 + LATE_TICKS).contains(&waited),
        &format!("blocked for {waited} ticks, wanted about 3"),
    )
}

static ITEMS: AtomicU64 = AtomicU64::new(0);
static TAKEN: AtomicU64 = AtomicU64::new(0);
static QUEUE: AtomicU64 = AtomicU64::new(u64::MAX);
const TOTAL: u64 = 200;

fn consumer(_: usize) {
    let q = QUEUE.load(SeqCst) as usize;
    while TAKEN.load(SeqCst) < TOTAL {
        thread::wait_until(q, || ITEMS.load(SeqCst) > TAKEN.load(SeqCst));
        TAKEN.fetch_add(1, SeqCst);
    }
}

fn producer_consumer_loses_no_wakeup() -> Outcome {
    clean(|| {
        ITEMS.store(0, SeqCst);
        TAKEN.store(0, SeqCst);
        let q = thread::new_queue();
        QUEUE.store(q as u64, SeqCst);
        let c = thread::spawn("consumer", NORMAL, consumer, 0);
        for i in 0..TOTAL {
            ITEMS.fetch_add(1, SeqCst);
            thread::wake_one(q);
            if i % 7 == 0 {
                thread::sleep_ticks(1); // let the consumer drain and block again
            }
        }
        thread::join(c);
        ensure_eq(TAKEN.load(SeqCst), TOTAL, "items consumed")
    })
}

static STOP: AtomicBool = AtomicBool::new(false);
static HOG_A: AtomicU64 = AtomicU64::new(0);
static HOG_B: AtomicU64 = AtomicU64::new(0);

fn hog(which: usize) {
    let counter = if which == 0 { &HOG_A } else { &HOG_B };
    while !STOP.load(SeqCst) {
        counter.fetch_add(1, SeqCst);
    }
}

fn equal_priority_hogs_share_the_cpu() -> Outcome {
    clean(|| {
        STOP.store(false, SeqCst);
        HOG_A.store(0, SeqCst);
        HOG_B.store(0, SeqCst);
        let a = thread::spawn("hog-a", NORMAL, hog, 0);
        let b = thread::spawn("hog-b", NORMAL, hog, 1);
        thread::sleep_ticks(80); // eight time slices each, give or take
        STOP.store(true, SeqCst);
        thread::join(a);
        thread::join(b);
        let (x, y) = (HOG_A.load(SeqCst), HOG_B.load(SeqCst));
        ensure(
            x > 0 && y > 0,
            &format!("a hog never ran (a={x}, b={y}): timer preemption is not working"),
        )?;
        ensure(
            x < 10 * y && y < 10 * x,
            &format!("unfair sharing: a={x}, b={y}"),
        )
    })
}

static RELEASED: AtomicU64 = AtomicU64::new(0);
static DONE_COUNT: AtomicU64 = AtomicU64::new(0);
static FIRST_DONE: AtomicU64 = AtomicU64::new(u64::MAX);

fn queue_waiter(id: usize) {
    let q = QUEUE.load(SeqCst) as usize;
    thread::wait_until(q, || RELEASED.load(SeqCst) > 0);
    RELEASED.fetch_sub(1, SeqCst);
    let _ = FIRST_DONE.compare_exchange(u64::MAX, id as u64, SeqCst, SeqCst);
    DONE_COUNT.fetch_add(1, SeqCst);
}

fn two_waiters_on_one_queue_wake_in_order() -> Outcome {
    clean(|| {
        RELEASED.store(0, SeqCst);
        DONE_COUNT.store(0, SeqCst);
        FIRST_DONE.store(u64::MAX, SeqCst);
        let q = thread::new_queue();
        QUEUE.store(q as u64, SeqCst);
        let a = thread::spawn("waiter-a", NORMAL, queue_waiter, 0);
        thread::sleep_ticks(2 + LATE_TICKS); // a is queued first
        let b = thread::spawn("waiter-b", NORMAL, queue_waiter, 1);
        thread::sleep_ticks(2 + LATE_TICKS);
        ensure_eq(
            DONE_COUNT.load(SeqCst),
            0,
            "a waiter ran before anything was released",
        )?;
        RELEASED.store(1, SeqCst);
        ensure(thread::wake_one(q), "wake_one found nobody waiting")?;
        thread::sleep_ticks(2 + LATE_TICKS);
        ensure_eq(
            DONE_COUNT.load(SeqCst),
            1,
            "waiters released by one wake_one",
        )?;
        ensure_eq(
            FIRST_DONE.load(SeqCst),
            0,
            "which waiter went first (first in, first out)",
        )?;
        RELEASED.store(1, SeqCst);
        thread::wake_all(q);
        thread::sleep_ticks(2 + LATE_TICKS);
        ensure_eq(
            DONE_COUNT.load(SeqCst),
            2,
            "waiters released after wake_all",
        )?;
        ensure(thread::join(a) && thread::join(b), "joining the waiters")
    })
}

static CHILD_RAN: AtomicBool = AtomicBool::new(false);

fn parent(_: usize) {
    let child = thread::spawn("child", NORMAL + 5, |_| CHILD_RAN.store(true, SeqCst), 0);
    // With fixed priorities the higher-priority child has already run; a plain
    // round-robin class does not preempt, so the child runs later.
    #[cfg(not(feature = "sched-round-robin"))]
    assert!(
        CHILD_RAN.load(SeqCst),
        "a higher-priority child did not run before spawn returned"
    );
    thread::join(child);
}

fn a_thread_can_spawn_threads() -> Outcome {
    clean(|| {
        CHILD_RAN.store(false, SeqCst);
        let p = thread::spawn("parent", NORMAL, parent, 0);
        ensure(thread::join(p), "joining the parent")?;
        ensure(CHILD_RAN.load(SeqCst), "the child never ran")
    })
}

fn joining_nothing_does_not_hang() -> Outcome {
    clean(|| {
        ensure(!thread::join(thread::current()), "a thread joined itself")?;
        ensure(!thread::join(99_999), "joined a thread that does not exist")?;
        let t = thread::spawn("once", NORMAL, |_| {}, 0);
        ensure(thread::join(t), "the first join")?;
        ensure(!thread::join(t), "a second join of the same thread")
    })
}

fn sleeper(_: usize) {
    thread::block();
}

fn stack_guard_damage_is_detected() -> Outcome {
    clean(|| {
        let t = thread::spawn("sleeper", NORMAL, sleeper, 0);
        thread::sleep_ticks(2 + LATE_TICKS); // it runs and blocks
        ensure(thread::stacks_intact(), "guards intact before the damage")?;
        thread::debug_set_guard(t, false);
        ensure(
            !thread::stacks_intact(),
            "damage to a stack guard went unnoticed",
        )?;
        thread::debug_set_guard(t, true);
        ensure(thread::stacks_intact(), "guards intact after the repair")?;
        thread::wake(t);
        ensure(thread::join(t), "joining the sleeper")
    })
}

fn fp_unit_is_off() -> Outcome {
    ensure(
        thread::fp_off(),
        "sstatus.FS is not Off: thread switches do not save FP state",
    )
}

// --- Tests that need the fixed-priority class. ---

#[cfg(not(feature = "sched-round-robin"))]
mod priority {
    use super::*;

    static FLAG: AtomicBool = AtomicBool::new(false);

    pub fn higher_priority_runs_at_once_on_spawn() -> Outcome {
        clean(|| {
            FLAG.store(false, SeqCst);
            let t = thread::spawn("rt", MAIN_PRIO + 5, |_| FLAG.store(true, SeqCst), 0);
            ensure(
                FLAG.load(SeqCst),
                "the higher-priority thread had not run when spawn returned",
            )?;
            thread::join(t);
            Ok(())
        })
    }

    pub fn lower_priority_waits_for_higher() -> Outcome {
        clean(|| {
            STOP.store(false, SeqCst);
            HOG_A.store(0, SeqCst);
            HOG_B.store(0, SeqCst);
            let high = thread::spawn("high", 20, hog, 0);
            let low = thread::spawn(
                "low",
                5,
                |_| {
                    HOG_B.fetch_add(1, SeqCst);
                },
                0,
            );
            thread::sleep_ticks(30);
            ensure_eq(
                HOG_B.load(SeqCst),
                0,
                "progress of the lower-priority thread while a higher one spins",
            )?;
            ensure(
                HOG_A.load(SeqCst) > 0,
                "the higher-priority thread did not run",
            )?;
            STOP.store(true, SeqCst);
            thread::join(high);
            thread::join(low);
            ensure_eq(
                HOG_B.load(SeqCst),
                1,
                "the lower-priority thread runs once the other has gone",
            )
        })
    }

    pub fn set_priority_takes_effect() -> Outcome {
        clean(|| {
            HOG_A.store(0, SeqCst);
            let t = thread::spawn(
                "raised",
                5,
                |_| {
                    HOG_A.fetch_add(1, SeqCst);
                },
                0,
            );
            // Main (priority 40) keeps the CPU: the priority-5 thread cannot run.
            let until = thread::ticks() + 5;
            while thread::ticks() < until {
                core::hint::spin_loop();
            }
            ensure_eq(
                HOG_A.load(SeqCst),
                0,
                "progress of a priority-5 thread while main (40) was running",
            )?;
            // Raised above main, it takes the CPU at once, before this call returns.
            thread::set_priority(t, MAIN_PRIO + 5);
            ensure_eq(
                HOG_A.load(SeqCst),
                1,
                "the raised thread did not run when its priority went up",
            )?;
            thread::join(t);
            Ok(())
        })
    }

    static MAX_LATE: AtomicU64 = AtomicU64::new(0);
    static LATE_DONE: AtomicBool = AtomicBool::new(false);

    /// The latency probe: sleep 3 ticks 50 times, recording how late each wake was.
    fn probe(_: usize) {
        let mut worst = 0;
        for _ in 0..50 {
            let target = thread::ticks() + 3;
            thread::sleep_ticks(3);
            worst = worst.max(thread::ticks().saturating_sub(target));
        }
        MAX_LATE.store(worst, SeqCst);
        LATE_DONE.store(true, SeqCst);
    }

    /// Worst lateness of the probe at `prio` while three hogs run at normal priority.
    fn lateness_under_load(prio: u8) -> Result<u64, String> {
        STOP.store(false, SeqCst);
        LATE_DONE.store(false, SeqCst);
        let hogs: Vec<_> = (0..3)
            .map(|_| {
                thread::spawn(
                    "load",
                    NORMAL,
                    |_| {
                        while !STOP.load(SeqCst) {
                            core::hint::spin_loop();
                        }
                    },
                    0,
                )
            })
            .collect();
        let p = thread::spawn("probe", prio, probe, 0);
        thread::join(p);
        STOP.store(true, SeqCst);
        for h in hogs {
            thread::join(h);
        }
        ensure(LATE_DONE.load(SeqCst), "the probe did not finish")?;
        Ok(MAX_LATE.load(SeqCst))
    }

    pub fn realtime_thread_stays_responsive_under_load() -> Outcome {
        clean(|| {
            let late = lateness_under_load(MAIN_PRIO + 10)?;
            ensure(
                late <= 1,
                &format!(
                    "a real-time thread woke {late} ticks late with three hogs running (bound: 1)"
                ),
            )
        })
    }

    /// The negative control: the same probe at the hogs' own priority must be delayed
    /// by them, or the bound above would prove nothing.
    pub fn the_latency_test_can_fail() -> Outcome {
        clean(|| {
            let late = lateness_under_load(NORMAL)?;
            ensure(
                late > 3,
                &format!(
                    "an equal-priority probe was only {late} ticks late: the load is not loading"
                ),
            )
        })
    }
}

pub const TESTS: &[Test] = &[
    ("timer_ticks_advance", timer_ticks_advance),
    ("threads_start_run_and_finish", threads_start_run_and_finish),
    ("many_threads_do_not_leak", many_threads_do_not_leak),
    ("sleep_wakes_on_time", sleep_wakes_on_time),
    (
        "idle_runs_when_nothing_else_does",
        idle_runs_when_nothing_else_does,
    ),
    (
        "wake_before_block_is_not_lost",
        wake_before_block_is_not_lost,
    ),
    (
        "producer_consumer_loses_no_wakeup",
        producer_consumer_loses_no_wakeup,
    ),
    (
        "equal_priority_hogs_share_the_cpu",
        equal_priority_hogs_share_the_cpu,
    ),
    (
        "two_waiters_on_one_queue_wake_in_order",
        two_waiters_on_one_queue_wake_in_order,
    ),
    ("a_thread_can_spawn_threads", a_thread_can_spawn_threads),
    (
        "joining_nothing_does_not_hang",
        joining_nothing_does_not_hang,
    ),
    (
        "stack_guard_damage_is_detected",
        stack_guard_damage_is_detected,
    ),
    ("fp_unit_is_off", fp_unit_is_off),
];

/// Only meaningful for the fixed-priority class.
#[cfg(not(feature = "sched-round-robin"))]
pub const PRIORITY_TESTS: &[Test] = &[
    (
        "higher_priority_runs_at_once_on_spawn",
        priority::higher_priority_runs_at_once_on_spawn,
    ),
    (
        "lower_priority_waits_for_higher",
        priority::lower_priority_waits_for_higher,
    ),
    (
        "set_priority_takes_effect",
        priority::set_priority_takes_effect,
    ),
    (
        "realtime_thread_stays_responsive_under_load",
        priority::realtime_thread_stays_responsive_under_load,
    ),
    (
        "the_latency_test_can_fail",
        priority::the_latency_test_can_fail,
    ),
];
#[cfg(feature = "sched-round-robin")]
pub const PRIORITY_TESTS: &[Test] = &[];
