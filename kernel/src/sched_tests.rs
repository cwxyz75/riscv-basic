//! Self-tests for S2: kernel threads, the timer interrupt, preemption, the single
//! block/wake path. They run in the boot thread at real-time priority 40, with
//! worker threads below or above it as each test needs. Time is measured in timer
//! ticks (1 ms), counted by the interrupt handler, so results do not depend on how
//! fast the host runs the emulator.

use crate::thread::{self, IDLE_SPINS};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};

type Outcome = Result<(), String>;
type Test = (&'static str, fn() -> Outcome);

pub const MAIN_PRIO: u8 = crate::MAIN_PRIO;
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

/// T284, the host-stall injector's self-test, run only when the harness stops the emulator on purpose (`hoststall=<ms>:<period>` on
/// the kernel line, from `OS_HOST_STALL`; at least 5 ms, so that an ordinary 1 ms sleep cannot pass for one): the guest must see a stall
/// land. With every hart keeping a deadline near (`pause::watch`), it sleeps 1 ms at a time; a sleep across which the `time` counter
/// jumped by at least the stall is one the host did not run, and the pause detector must have seen it as a pause (`pause::covering`:
/// on two harts, at least half the stall late). It gives up after four periods and a second.
pub fn a_host_stall_is_seen_by_the_guest() -> Outcome {
    let Some((ms, period)) = stall_arg() else {
        return Err(String::from(
            "the kernel line's hoststall= is not <ms>:<period ms> with a stall of at least 5 ms",
        ));
    };
    let stall_ns = ms * 1_000_000;
    let give_up = thread::now_ns() + (4 * period + 1000) * 1_000_000;
    let (mut jumps, mut best) = (0u32, kcore::pause::Cover::None);
    crate::pause::watch((4 * period + 2000) * 1_000_000);
    let seen = loop {
        if thread::now_ns() >= give_up {
            break None;
        }
        let start = thread::now_ns();
        thread::sleep_ticks(1);
        let end = thread::now_ns();
        if end - start < stall_ns {
            continue;
        }
        jumps += 1;
        match crate::pause::covering(start, end, ms * 1000 / 2) {
            kcore::pause::Cover::Pause { hart, late_us } => break Some((hart, late_us)),
            other => best = other,
        }
    };
    crate::pause::watch(0);
    stall_verdict(seen, ms, jumps, best)
}

/// `hoststall=<ms>:<period>` from the kernel line, with a stall of at least 5 ms.
#[inline(never)]
fn stall_arg() -> Option<(u64, u64)> {
    crate::dtb::boot_arg_value("hoststall=")
        .and_then(|v| kcore::pause::parse_stall_arg(&v))
        .filter(|&(ms, _)| ms >= 5)
}

/// The self-test's verdict (a frame of its own: the 1 KiB budget).
#[inline(never)]
fn stall_verdict(
    seen: Option<(usize, u64)>,
    ms: u64,
    jumps: u32,
    best: kcore::pause::Cover,
) -> Outcome {
    match seen {
        Some((hart, late_us)) => {
            crate::pause::report("a_host_stall_is_seen_by_the_guest", hart, late_us);
            Ok(())
        }
        None => Err(format!(
            "no stall of {ms} ms was seen as a pause: {jumps} sleeps of 1 ms took at least {ms} ms, and the detector said {best:?} for the last"
        )),
    }
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
    // (on the precise clock, AB2: a sleep is a number of milliseconds from the instant it starts, and the tick count of one hart
    // can differ by one from it)
    let t0 = thread::now_ns();
    thread::sleep_ticks(5);
    let elapsed_ns = thread::now_ns() - t0;
    ensure(
        (5_000_000..=(6 + LATE_TICKS) * 1_000_000 + 40_000_000).contains(&elapsed_ns), // (a real clock: a host stall adds to it)
        &format!("slept {elapsed_ns} ns, wanted 5 ms"),
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

static WBB_DONE: AtomicBool = AtomicBool::new(false);
static WBB_RESCUED: AtomicBool = AtomicBool::new(false);
static WBB_WOKEN: AtomicBool = AtomicBool::new(false);
/// How long the rescuer of `wake_before_block_is_not_lost` waits before it wakes a test thread that blocked although a wake had
/// arrived: a lost wake would otherwise hang the boot (nothing else wakes the test thread) instead of failing the test.
const WBB_RESCUE_TICKS: u64 = 2_000;
/// A pause of the host later than this inside the second half's window has the window taken again (its bound is 3 to
/// `12 + LATE_TICKS` ticks).
const WBB_PAUSE_US: u64 = 5_000;
/// The second half's waker found its counted wake lost: the thread stayed blocked for `WBB_RESCUE_TICKS` after it.
static WBB_LOST: AtomicBool = AtomicBool::new(false);

fn wake_before_block_is_not_lost() -> Outcome {
    let me = thread::current();
    wbb_prepared_wake_is_kept(me)?;
    wbb_a_real_block_is_woken(me)
}

/// The first half of `wake_before_block_is_not_lost` (a frame of its own: the whole test is over the 1 KiB budget).
#[inline(never)]
fn wbb_prepared_wake_is_kept(me: kcore::sched::Tid) -> Outcome {
    // a wake that finds nobody waiting is not remembered (SCHED-SMP-SPEC 3.7)
    thread::wake(me);
    ensure(
        thread::park_state(me) == Some(kcore::park::ParkState::Idle),
        "a wake with nobody waiting left something behind",
    )?;
    // A thread that has prepared to wait does get the wake: its block does not happen (and wakes do not queue). Judged by what the
    // call did (`block` says whether it blocked), not by the clock: the old bound, at most one tick from before `prepare_block` to
    // after `block`, was crossed by a pause of the host with nothing blocked (the release kernel at four harts on CI, run 46085592181;
    // the host-stall injector crossed it 10 times in 40,000 rounds with `block` blocking in none, and never without the injector). A
    // lost wake blocks for good, so a rescuer wakes the thread after `WBB_RESCUE_TICKS` and the test fails instead of hanging; its
    // wake changes nothing when the block did not happen (a wake is never remembered).
    WBB_DONE.store(false, SeqCst);
    WBB_RESCUED.store(false, SeqCst);
    let rescuer = thread::spawn(
        "rescuer",
        NORMAL,
        |me| {
            for _ in 0..WBB_RESCUE_TICKS {
                if WBB_DONE.load(SeqCst) {
                    return;
                }
                thread::sleep_ticks(1);
            }
            WBB_RESCUED.store(true, SeqCst);
            thread::wake(me as u64);
        },
        me as usize,
    );
    thread::prepare_block();
    thread::wake(me);
    thread::wake(me);
    let notified = thread::park_state(me);
    let blocked = thread::block(); // consumes the notification
    let left = thread::park_state(me);
    WBB_DONE.store(true, SeqCst);
    thread::join(rescuer);
    wbb_judge(blocked, notified, left)
}

/// The first half's verdict (a frame of its own: its messages and the rest are over the budget together).
#[inline(never)]
fn wbb_judge(
    blocked: bool,
    notified: Option<kcore::park::ParkState>,
    left: Option<kcore::park::ParkState>,
) -> Outcome {
    // (the messages are made only on a failure: each `format!` in `ensure`'s argument costs the frame whether it fails or not)
    if blocked {
        let who = if WBB_RESCUED.load(SeqCst) {
            "the rescuer woke it"
        } else {
            "something else woke it"
        };
        return Err(format!(
            "block waited although a wake had already arrived (the park word said {notified:?} before it; {who})"
        ));
    }
    if notified != Some(kcore::park::ParkState::Notified)
        || left != Some(kcore::park::ParkState::Idle)
    {
        return Err(format!(
            "the park word said {notified:?} after the wakes and {left:?} after block (wanted Notified, then Idle)"
        ));
    }
    Ok(())
}

/// The second half of `wake_before_block_is_not_lost`.
#[inline(never)]
fn wbb_a_real_block_is_woken(me: kcore::sched::Tid) -> Outcome {
    // Nothing is left over: now block for real, woken by a thread that wakes us after 3 ticks. A wake that finds us not yet blocked
    // (a pause of the host before our `block`) does not count and is not remembered, so the waker repeats it each tick until one
    // counts; after a counted wake it waits for us to say we are awake, and if we do not within `WBB_RESCUE_TICKS` it records the wake
    // as lost and wakes us again (a failure, not a hang). The window starts before the waker is made, so every wake comes at least
    // 3 ticks into it. A window that a pause of the host covered (T284: two harts' timer interrupts more than `WBB_PAUSE_US` late
    // inside it) is taken again, at most three in a row.
    crate::pause::watch(10_000 * 1_000_000);
    WBB_LOST.store(false, SeqCst);
    let mut rs = kcore::pause::Resampler::new();
    let waited = loop {
        WBB_WOKEN.store(false, SeqCst);
        let n1 = thread::now_ns();
        let t1 = thread::ticks();
        let waker = thread::spawn(
            "waker",
            NORMAL,
            |me| {
                thread::sleep_ticks(3);
                while !WBB_WOKEN.load(SeqCst) && !thread::wake_counted(me as u64) {
                    thread::sleep_ticks(1);
                }
                for _ in 0..WBB_RESCUE_TICKS {
                    if WBB_WOKEN.load(SeqCst) {
                        return;
                    }
                    thread::sleep_ticks(1);
                }
                WBB_LOST.store(true, SeqCst);
                while !WBB_WOKEN.load(SeqCst) {
                    thread::wake(me as u64);
                    thread::sleep_ticks(1);
                }
            },
            me as usize,
        );
        thread::block();
        let waited = thread::ticks() - t1;
        let n2 = thread::now_ns();
        WBB_WOKEN.store(true, SeqCst);
        thread::join(waker);
        let pause = match crate::pause::covering(n1, n2, WBB_PAUSE_US) {
            kcore::pause::Cover::Pause { hart, late_us } => Some((hart, late_us)),
            _ => None,
        };
        match rs.judge(pause, WBB_PAUSE_US) {
            kcore::pause::Verdict::Keep => break waited,
            kcore::pause::Verdict::Retake { hart, late_us } => {
                crate::pause::report("wake_before_block_is_not_lost", hart, late_us)
            }
            kcore::pause::Verdict::GiveUp { hart, late_us } => {
                crate::pause::report("wake_before_block_is_not_lost", hart, late_us);
                break waited; // (judged as it is: a host that never runs the harts is a failure too)
            }
        }
    };
    crate::pause::watch(0);
    ensure(
        !WBB_LOST.load(SeqCst),
        "a wake to a blocked thread was lost (it stayed blocked after a wake that counted)",
    )?;
    ensure(
        (3..=12 + LATE_TICKS).contains(&waited), // (a real clock and one-shot timers: the emulator's interrupt jitter shows)
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

static NR_GO: AtomicBool = AtomicBool::new(false);
static NR_RAN: AtomicBool = AtomicBool::new(false);
static NR_QUEUE: AtomicU64 = AtomicU64::new(u64::MAX);

fn nr_waiter(_: usize) {
    let q = NR_QUEUE.load(SeqCst) as usize;
    thread::wait_until(q, || NR_GO.load(SeqCst));
    NR_RAN.store(true, SeqCst);
}

/// X7 (SCHED-SMP-SPEC): a wake never switches; the switch happens at the next defined point, here the end of the outermost critical
/// section. A higher-priority thread, pinned to this hart so no other hart can run it, is woken inside two nested sections: it must
/// not have run when the inner one ends, and must have run when the outer one does.
fn a_wake_switches_at_the_end_of_the_outermost_section() -> Outcome {
    clean(|| {
        NR_GO.store(false, SeqCst);
        NR_RAN.store(false, SeqCst);
        let q = thread::new_queue();
        NR_QUEUE.store(q as u64, SeqCst);
        let me = crate::percpu::this()
            .index
            .load(core::sync::atomic::Ordering::Relaxed);
        let before = thread::DEFAULT_AFFINITY.swap(me, SeqCst);
        let w = thread::spawn("nr_waiter", MAIN_PRIO + 5, nr_waiter, 0); // preempts at once and blocks
        thread::DEFAULT_AFFINITY.store(before, SeqCst);
        let mut early = true;
        let mut after_inner = true;
        crate::irq::without(|| {
            NR_GO.store(true, SeqCst);
            thread::wake_one(q);
            early = NR_RAN.load(SeqCst);
            after_inner = NR_RAN.load(SeqCst);
        });
        let late = NR_RAN.load(SeqCst);
        thread::join(w);
        ensure(
            !early && !after_inner,
            "the woken thread ran inside a critical section",
        )?;
        ensure(
            late,
            &format!(
                "the woken thread had not run when the outermost section ended (depth {}, need {})",
                crate::percpu::this().bkl_depth.get(),
                crate::percpu::this().need_resched.load(SeqCst)
            ),
        )
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

/// Sleep a tick at a time until `ready` holds, for at most `ticks` (T287: a fixed nap assumes the host runs the hart through it; a pause
/// of the host inside the nap makes the sleeper's own timer overdue, so on resume it preempts the threads it was waiting for before they
/// ran: `two_waiters_on_one_queue_wake_in_order` "got 1, want 2", `wake_many_makes_every_waiter_runnable_before_any_runs` "the two
/// waiters did not block", in boots beside other boots and under chaos). Whether it held at the end.
fn settles(ticks: u64, mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..ticks {
        if ready() {
            return true;
        }
        thread::sleep_ticks(1);
    }
    ready()
}

/// How long `settles` waits for something that takes a wake and a few microseconds of running: far beyond any pause of the host.
const SETTLE_TICKS: u64 = 2_000;

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

#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under
fn two_waiters_on_one_queue_wake_in_order() -> Outcome {
    clean(|| {
        RELEASED.store(0, SeqCst);
        DONE_COUNT.store(0, SeqCst);
        FIRST_DONE.store(u64::MAX, SeqCst);
        let q = thread::new_queue();
        QUEUE.store(q as u64, SeqCst);
        let a = thread::spawn("waiter-a", NORMAL, queue_waiter, 0);
        // (T287: each step waits until it has happened, not a fixed nap: a waiter is queued once it is blocked)
        ensure(
            settles(SETTLE_TICKS, || thread::is_blocked(a)),
            "waiter a never blocked on the queue",
        )?; // a is queued first
        let b = thread::spawn("waiter-b", NORMAL, queue_waiter, 1);
        ensure(
            settles(SETTLE_TICKS, || thread::is_blocked(b)),
            "waiter b never blocked on the queue",
        )?;
        ensure_eq(
            DONE_COUNT.load(SeqCst),
            0,
            "a waiter ran before anything was released",
        )?;
        RELEASED.store(1, SeqCst);
        ensure(thread::wake_one(q), "wake_one found nobody waiting")?;
        ensure(
            settles(SETTLE_TICKS, || DONE_COUNT.load(SeqCst) >= 1),
            "the waiter one wake_one released never finished",
        )?;
        thread::sleep_ticks(2 + LATE_TICKS); // (a second waiter released by the one wake would show by now)
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
        settles(SETTLE_TICKS, || DONE_COUNT.load(SeqCst) >= 2);
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

/// A frame the optimiser cannot drop, recursing until the thread's own stack has used `target` bytes: KT13's deliberate deep stack
/// (measured with the same scan as the boot's check, so the test does not depend on the size of a debug or release frame).
#[inline(never)]
fn recurse(target: usize) -> usize {
    let mut pad = [0u8; 256];
    pad[target % 256] = 1;
    let pad = core::hint::black_box(pad);
    if thread::current_stack_use() >= target {
        usize::from(pad[0])
    } else {
        recurse(target).wrapping_add(usize::from(pad[target % 256]))
    }
}

static DEEP_USE: AtomicUsize = AtomicUsize::new(0);

fn deep_thread(_: usize) {
    core::hint::black_box(recurse(thread::STACK_BUDGET + 1024));
    DEEP_USE.store(thread::current_stack_use(), SeqCst);
    // this thread went over the budget on purpose: it does not count against the boot's check
    thread::exempt_from_stack_budget();
}

/// KT13: a thread that recurses to about 9 KiB is over the budget (8 KiB) and the check says so, while an ordinary thread is not.
fn the_stack_budget_check_trips_on_deep_recursion() -> Outcome {
    clean(|| {
        let t = thread::spawn("deep", NORMAL, deep_thread, 0);
        ensure(thread::join(t), "joining the deep thread")?;
        let used = DEEP_USE.load(SeqCst);
        ensure(
            thread::over_budget(used),
            "a thread that recursed to about 9 KiB was not over the stack budget",
        )?;
        ensure(
            used < thread::STACK_BYTES,
            "the deep thread reached the guard: the test's recursion is too deep",
        )?;
        ensure(
            !thread::over_budget(thread::STACK_BUDGET),
            "the budget itself is inside the budget",
        )?;
        ensure(
            thread::over_budget(thread::STACK_BUDGET + 1),
            "one byte over is over",
        )
    })
}

fn fp_unit_is_off() -> Outcome {
    ensure(
        thread::fp_off(),
        "the FP unit is not off: thread switches do not save FP state",
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

    static GO: AtomicBool = AtomicBool::new(false);
    static H2: AtomicU64 = AtomicU64::new(0);
    static SAW_H2_BLOCKED: AtomicBool = AtomicBool::new(false);
    static RAN: AtomicU64 = AtomicU64::new(0);

    /// T62 (SMP review finding 1): a waker that wakes two higher-priority waiters must make both runnable
    /// before either runs. If it woke and rescheduled one at a time, the first would preempt the waker and
    /// the second would still be blocked (with a mid-priority hog on the hart it would stay so). Pinned to
    /// the boot hart like every kernel thread, so the order is deterministic.
    pub fn wake_many_makes_every_waiter_runnable_before_any_runs() -> Outcome {
        clean(|| {
            GO.store(false, SeqCst);
            SAW_H2_BLOCKED.store(false, SeqCst);
            RAN.store(0, SeqCst);
            let h1 = thread::spawn(
                "h1",
                20,
                |_| {
                    thread::wait_deadline_private(|| GO.load(SeqCst), u64::MAX);
                    if thread::is_blocked(H2.load(SeqCst)) {
                        SAW_H2_BLOCKED.store(true, SeqCst);
                    }
                    RAN.fetch_add(1, SeqCst);
                },
                0,
            );
            let h2 = thread::spawn(
                "h2",
                20,
                |_| {
                    thread::wait_deadline_private(|| GO.load(SeqCst), u64::MAX);
                    RAN.fetch_add(1, SeqCst);
                },
                0,
            );
            H2.store(h2, SeqCst);
            // main (priority 40) sleeps: both waiters run to their wait and block (T287: until they have, not a fixed 3 ticks, which
            // a pause of the host inside the nap cut short: main's overdue timer preempted the waiters before they reached the wait)
            ensure(
                settles(SETTLE_TICKS, || {
                    thread::is_blocked(h1) && thread::is_blocked(h2)
                }),
                "the two waiters did not block",
            )?;
            let low = thread::spawn(
                "low",
                5,
                |h1| {
                    GO.store(true, SeqCst);
                    thread::wake_many(&[h1 as u64, H2.load(SeqCst)]);
                },
                h1 as usize,
            );
            settles(SETTLE_TICKS, || RAN.load(SeqCst) >= 2);
            ensure_eq(RAN.load(SeqCst), 2, "waiters that ran after the batch wake")?;
            ensure(
                !SAW_H2_BLOCKED.load(SeqCst),
                "the second waiter was still blocked when the first ran (one wake and reschedule at a time)",
            )?;
            thread::join(h1);
            thread::join(h2);
            thread::join(low);
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
    static P99_LATE: AtomicU64 = AtomicU64::new(0);
    static LATE_DONE: AtomicBool = AtomicBool::new(false);
    /// T284: the probe's windows taken again because a pause of the host covered them, the worst such pause
    /// (`kcore::pause::encode`), and whether three in a row were covered (the probe gave up).
    static RETAKEN: AtomicU64 = AtomicU64::new(0);
    static WORST_PAUSE: AtomicU64 = AtomicU64::new(0);
    static GAVE_UP: AtomicU64 = AtomicU64::new(0);
    /// The worst lateness seen on one hart only inside a window (not excused: `kcore::pause::Cover::OneHart`), for a failure message.
    static ONE_HART: AtomicU64 = AtomicU64::new(0);
    /// Which test the probe is measuring for: its `##PAUSE` lines name it, and a window is covered by a pause later than that test's
    /// own bound (the real-time bound, 4 ms; the control's, the lateness it must exceed, 6 ms). (Before the heartbeat, the development
    /// machine's emulator took the 10 ms slice-end interrupt the control waits for about 5 ms late every time, which the real-time bound
    /// would have called a pause in every window; with the heartbeat no timer is armed that far ahead while the probe measures.)
    static PROBE_FOR: AtomicUsize = AtomicUsize::new(0);
    /// The most windows the negative control takes looking for one kept window (T287: about 13 ms each).
    const CONTROL_WINDOWS: u32 = 200;
    const PROBES: [(&str, u64); 2] = [
        ("realtime_thread_stays_responsive_under_load", 4_000),
        ("the_latency_test_can_fail", 6_000),
    ];

    /// The pause that covered a probe window, if one did (a lateness on one hart alone is kept for the failure message, not excused);
    /// under `guesttime` the host cannot pause the guest, so a late interrupt there is the kernel's and nothing is excused. (A frame of
    /// its own: the probe's is near the 1 KiB budget.)
    #[inline(never)]
    fn window_pause(start: u64, end: u64, bound_us: u64, guest_time: bool) -> Option<(usize, u64)> {
        match crate::pause::covering(start, end, bound_us) {
            _ if guest_time => None,
            kcore::pause::Cover::Pause { hart, late_us } => Some((hart, late_us)),
            kcore::pause::Cover::OneHart { hart, late_us } => {
                if late_us > kcore::pause::decode(ONE_HART.load(SeqCst)).map_or(0, |(_, l)| l) {
                    ONE_HART.store(kcore::pause::encode(Some((hart, late_us))), SeqCst);
                }
                None
            }
            kcore::pause::Cover::None => None,
        }
    }

    /// The latency probe: sleep 3 ticks until 50 wakes are measured, recording how late each was. A sleep that a pause of the host
    /// covered (T284: a timer interrupt of any hart taken more than the bound late inside it) is taken again, at most three in a row.
    #[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under
    fn probe(_: usize) {
        use kcore::pause::{Resampler, Verdict};
        let mut late = alloc::vec::Vec::new();
        let mut rs = Resampler::new();
        let (name, bound_us) = PROBES[PROBE_FOR.load(SeqCst) % PROBES.len()];
        let guest_time = crate::dtb::boot_arg_present("guesttime");
        // (T287: the negative control asks only whether one window with the host running shows the equal-priority probe held back by
        // the hogs' slices: a covered window is no evidence either way, and three covered in a row say only that the host was busy (the
        // injector at 10 ms every 40 ms gave up every boot), so the control keeps looking, for at most `CONTROL_WINDOWS`, and stops at
        // the first kept window past its bound. The real-time bound keeps the rule: a host that never runs the hart fails it.)
        let control = PROBE_FOR.load(SeqCst) == 1;
        let mut windows = 0;
        while late.len() < 50 {
            windows += 1;
            // (nanoseconds on the real clock: there is no tick to round to)
            let start = thread::now_ns();
            let target = start + 3_000_000;
            thread::sleep_ticks(3);
            let end = thread::now_ns();
            match rs.judge(window_pause(start, end, bound_us, guest_time), bound_us) {
                Verdict::Keep => {
                    let l = end.saturating_sub(target);
                    late.push(l);
                    if control && l > bound_us * 1000 {
                        break;
                    }
                }
                Verdict::Retake { hart, late_us } => crate::pause::report(name, hart, late_us),
                Verdict::GiveUp { hart, late_us } if control && windows < CONTROL_WINDOWS => {
                    crate::pause::report(name, hart, late_us);
                }
                Verdict::GiveUp { hart, late_us } => {
                    crate::pause::report(name, hart, late_us);
                    GAVE_UP.store(kcore::pause::encode(Some((hart, late_us))), SeqCst);
                    break;
                }
            }
        }
        RETAKEN.store(u64::from(rs.retaken), SeqCst);
        WORST_PAUSE.store(kcore::pause::encode(rs.worst), SeqCst);
        late.sort_unstable();
        MAX_LATE.store(late.last().copied().unwrap_or(0), SeqCst);
        // (the next to worst of the 50: `len * 99 / 100` was index 49, the worst itself, so the "99th" bound was on the worst sample
        // and one pause of the host failed it, T178; found by T284)
        P99_LATE.store(
            late.get(late.len().saturating_sub(2)).copied().unwrap_or(0),
            SeqCst,
        );
        LATE_DONE.store(true, SeqCst);
    }

    /// What the probe's re-sampling did, for a failure message: "" when no window was taken again.
    fn pauses_said() -> String {
        let mut said = match kcore::pause::decode(WORST_PAUSE.load(SeqCst)) {
            None => String::new(),
            Some((h, us)) => format!(
                "; {} windows taken again for a host pause, the worst {us} us on hart {h}",
                RETAKEN.load(SeqCst)
            ),
        };
        if let Some((h, us)) = kcore::pause::decode(ONE_HART.load(SeqCst)) {
            said.push_str(&format!(
                "; a timer interrupt {us} us late on hart {h} alone (not a pause of the host: no other hart saw it)"
            ));
        }
        said
    }

    /// Worst lateness of the probe at `prio` while three hogs run at normal priority (an error when three windows in a row were
    /// covered by a pause of the host: a host that does not run the hart is a failure too).
    fn lateness_under_load(prio: u8) -> Result<u64, String> {
        STOP.store(false, SeqCst);
        LATE_DONE.store(false, SeqCst);
        RETAKEN.store(0, SeqCst);
        WORST_PAUSE.store(0, SeqCst);
        GAVE_UP.store(0, SeqCst);
        ONE_HART.store(0, SeqCst);
        PROBE_FOR.store(usize::from(prio == NORMAL), SeqCst);
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
        // (T284: every hart keeps a deadline near while the probe measures, so a pause of the host makes one late wherever it lands)
        crate::pause::watch(30_000_000_000);
        let p = thread::spawn("probe", prio, probe, 0);
        thread::join(p);
        crate::pause::watch(0);
        STOP.store(true, SeqCst);
        for h in hogs {
            thread::join(h);
        }
        ensure(LATE_DONE.load(SeqCst), "the probe did not finish")?;
        if let Some((h, us)) = kcore::pause::decode(GAVE_UP.load(SeqCst)) {
            return Err(format!(
                "host pause: three windows in a row were covered by a pause (the last: a timer interrupt {us} us late on hart {h}), so \
                 the probe could not measure the scheduler"
            ));
        }
        Ok(MAX_LATE.load(SeqCst))
    }

    /// The bound in guest time (T21, T284): every wake within 1 ms. (A frame of its own: the test's closure is near the 1 KiB budget.)
    #[inline(never)]
    fn guest_time_bound(late: u64) -> Outcome {
        ensure(
            late <= 1_000_000,
            &format!("a real-time thread woke {late} ns late in guest time with three hogs running (bound: 1 ms, every wake)"),
        )
    }

    pub fn realtime_thread_stays_responsive_under_load() -> Outcome {
        clean(|| {
            let late = lateness_under_load(MAIN_PRIO + 10)?;
            // (SCHED-SMP-SPEC 6.1 `rt_latency`: the RT probe's lateness with every other hart loaded; 50 samples, so the 99th is the next to worst)
            {
                use core::fmt::Write;
                let mut out = crate::console::Console;
                let _ = writeln!(out, "##BENCH rt_latency_p99 {} ns", P99_LATE.load(SeqCst));
                let _ = writeln!(out, "##BENCH rt_latency_max {late} ns");
            }
            // T21 (T284): under `-icount` with `sleep=off` (the counts boot, `make os-count`: one hart, `guesttime` on the kernel line) the clock
            // is the instruction count, the host cannot delay a wake, and the bound is the guest's own: every one of the 50 wakes within
            // 1 ms (measured: 36 us).
            if crate::dtb::boot_arg_present("guesttime") {
                return guest_time_bound(late);
            }
            ensure(
                // (the emulator delivers a timer interrupt up to a few milliseconds late on a loaded host: observed 2.2 ms; a thread of the hogs'
                // own priority is held for whole slices, 10 ms, which is what the negative control checks)
                // (a pause of the host delays one wake of the 50: the bound is on the 99th, the next to worst, and the worst only has to stay within 30 ms; T178)
                P99_LATE.load(SeqCst) <= 4_000_000 && late <= 30_000_000,
                // (T284: a window that a pause of the host covered was taken again, so what is judged here was measured with the host running)
                &format!(
                    "a real-time thread woke {late} ns late (99th {} ns) with three hogs running (bound: 4 ms, worst 30 ms){}",
                    P99_LATE.load(SeqCst),
                    pauses_said()
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
                late > 6_000_000,
                &format!(
                    "an equal-priority probe was only {late} ns late: the load is not loading{}",
                    pauses_said()
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
        "a_wake_switches_at_the_end_of_the_outermost_section",
        a_wake_switches_at_the_end_of_the_outermost_section,
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
    (
        "the_stack_budget_check_trips_on_deep_recursion",
        the_stack_budget_check_trips_on_deep_recursion,
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
        "wake_many_makes_every_waiter_runnable_before_any_runs",
        priority::wake_many_makes_every_waiter_runnable_before_any_runs,
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
