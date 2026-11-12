//! SMP bring-up checks (S5a): the secondary harts came online with their own state, their own
//! timers and working IPIs.

use crate::percpu;
use crate::{smp, thread};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering;

type Test = (&'static str, fn() -> Result<(), String>);

pub const TESTS: &[Test] = &[
    ("harts_come_online", harts_come_online),
    ("ipi_reaches_every_hart", ipi_reaches_every_hart),
    (
        "bkl_is_exclusive_under_contention",
        bkl_is_exclusive_under_contention,
    ),
    (
        "lock_order_violations_are_detected",
        lock_order_violations_are_detected,
    ),
    (
        "a_wake_preempts_a_lower_priority_hart",
        a_wake_preempts_a_lower_priority_hart,
    ),
    ("user_threads_run_in_parallel", user_threads_run_in_parallel),
];

/// Wait up to `ms` milliseconds for `ready`.
fn wait_for(ms: u64, mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..ms {
        if ready() {
            return true;
        }
        thread::sleep_ticks(1);
    }
    ready()
}

fn harts_come_online() -> Result<(), String> {
    let n = smp::started();
    if smp::online() != n {
        return Err(format!(
            "{} of {n} started harts came online",
            smp::online()
        ));
    }
    // each hart has its own record: distinct addresses, the index it was given
    for i in 0..n {
        let h = percpu::hart(i);
        if h.index.load(Ordering::Relaxed) != i {
            return Err(format!(
                "hart {i} has index {}",
                h.index.load(Ordering::Relaxed)
            ));
        }
        for j in 0..i {
            if core::ptr::eq(h, percpu::hart(j)) {
                return Err(format!("harts {i} and {j} share a record"));
            }
            if h.hart_id.load(Ordering::Relaxed) == percpu::hart(j).hart_id.load(Ordering::Relaxed)
            {
                return Err(format!("harts {i} and {j} share SBI id"));
            }
        }
    }
    // each secondary saw its own record's address in `tp` when it bound itself
    for i in 1..n {
        let h = percpu::hart(i);
        if h.tp_seen.load(Ordering::Relaxed) != h as *const percpu::PerHart as usize {
            return Err(format!(
                "hart {i} bound tp to {:#x}",
                h.tp_seen.load(Ordering::Relaxed)
            ));
        }
    }
    if !smp::stack_canaries_intact() {
        return Err(String::from(
            "a hart's boot stack overflowed into its canary",
        ));
    }
    // the calling hart's `tp` is its own record
    let me = percpu::this();
    if !(0..n).any(|i| core::ptr::eq(me, percpu::hart(i))) {
        return Err(String::from("tp is not one of the hart records"));
    }
    // each hart takes its own timer interrupts
    for i in 0..n {
        let h = percpu::hart(i);
        if !wait_for(1000, || h.timer_ticks.load(Ordering::SeqCst) >= 3) {
            return Err(format!(
                "hart {i} took only {} timer interrupts in a second",
                h.timer_ticks.load(Ordering::SeqCst)
            ));
        }
    }
    Ok(())
}

fn ipi_reaches_every_hart() -> Result<(), String> {
    let n = smp::started();
    for i in 1..n {
        let h = percpu::hart(i);
        let before = h.ipis.load(Ordering::SeqCst);
        smp::send_ipi(i, percpu::IPI_RESCHED);
        if !wait_for(1000, || h.ipis.load(Ordering::SeqCst) > before) {
            return Err(format!("hart {i} did not take the IPI"));
        }
        if h.pending.load(Ordering::SeqCst) != 0 {
            return Err(format!("hart {i} left its pending bits set"));
        }
    }
    // a second round, all at once: every hart counts at least one more
    let before: alloc::vec::Vec<u64> = (0..n)
        .map(|i| percpu::hart(i).ipis.load(Ordering::SeqCst))
        .collect();
    for i in 1..n {
        smp::send_ipi(i, percpu::IPI_RESCHED);
    }
    for (i, &was) in before.iter().enumerate().skip(1) {
        let h = percpu::hart(i);
        if !wait_for(1000, || h.ipis.load(Ordering::SeqCst) > was) {
            return Err(format!("hart {i} missed the second IPI"));
        }
    }
    // no interrupt storm afterwards (a software interrupt that is never cleared would re-fire
    // for ever and starve the timer): the counts hold still and the timers keep running
    let after: alloc::vec::Vec<(u64, u64)> = (0..n)
        .map(|i| {
            let h = percpu::hart(i);
            (
                h.ipis.load(Ordering::SeqCst),
                h.timer_ticks.load(Ordering::SeqCst),
            )
        })
        .collect();
    thread::sleep_ticks(30);
    for (i, &(ipis, ticks)) in after.iter().enumerate().skip(1) {
        let h = percpu::hart(i);
        if h.ipis.load(Ordering::SeqCst) != ipis {
            return Err(format!("hart {i} keeps taking IPIs nobody sent"));
        }
        if h.timer_ticks.load(Ordering::SeqCst) < ticks + 10 {
            return Err(format!("hart {i}'s timer stalled after the IPIs"));
        }
    }
    Ok(())
}

fn bkl_is_exclusive_under_contention() -> Result<(), String> {
    use crate::bkl;
    const M: u64 = 20_000;
    let n = smp::started();
    smp::LOCK_COUNTER.store(0, Ordering::SeqCst);
    for i in 1..n {
        smp::start_work(i, smp::WORK_BKL_COUNT, M);
    }
    // the boot hart does the same, through the kernel's critical section
    for _ in 0..M {
        crate::irq::without(smp::racy_increment);
    }
    for i in 1..n {
        if smp::wait_work(i, 5 * 10_000_000) != Some(M) {
            return Err(format!("hart {i} did not finish its increments"));
        }
    }
    let got = smp::LOCK_COUNTER.load(Ordering::SeqCst);
    if got != M * n as u64 {
        return Err(format!(
            "{got} increments counted of {}: the lock did not exclude",
            M * n as u64
        ));
    }
    if !bkl::is_free() {
        return Err(String::from("the lock is still held"));
    }
    Ok(())
}

fn lock_order_violations_are_detected() -> Result<(), String> {
    use crate::bkl::{order_check, Class};
    // the order is BKL, then HEAP, then UART: taking an earlier class while holding a later one
    // is flagged, taking a later one while holding an earlier one is not
    for (held, probe, bad) in [
        (Class::Heap, Class::Bkl, true),
        (Class::Uart, Class::Bkl, true),
        (Class::Uart, Class::Heap, true),
        (Class::Bkl, Class::Heap, false),
        (Class::Bkl, Class::Uart, false),
        (Class::Heap, Class::Uart, false),
    ] {
        if order_check(held, probe) != bad {
            return Err(format!(
                "holding {held:?}, taking {probe:?}: expected flagged={bad}"
            ));
        }
    }
    if crate::bkl::VIOLATIONS.load(Ordering::SeqCst) != 0 {
        return Err(format!(
            "{} lock-order violations in the real run so far",
            crate::bkl::VIOLATIONS.load(Ordering::SeqCst)
        ));
    }
    Ok(())
}

static STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static SPINNERS_UP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static HI_DONE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static HI_ON_SECONDARY: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn low_spinner(_: usize) {
    SPINNERS_UP.fetch_add(1, Ordering::SeqCst);
    while !STOP.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
}

fn high_runner(_: usize) {
    if percpu::this().index.load(Ordering::Relaxed) != 0 {
        HI_ON_SECONDARY.fetch_add(1, Ordering::SeqCst);
    }
    HI_DONE.fetch_add(1, Ordering::SeqCst);
}

/// Low-priority spinners occupy every secondary hart; a higher-priority thread woken from the
/// boot hart (which is busy with a still higher one) must displace one of them: the hart is
/// told by a reschedule IPI and the thread runs there.
fn a_wake_preempts_a_lower_priority_hart() -> Result<(), String> {
    let n = smp::started();
    if n < 2 {
        return Ok(());
    }
    const TRIALS: usize = 12;
    STOP.store(false, Ordering::SeqCst);
    SPINNERS_UP.store(0, Ordering::SeqCst);
    HI_DONE.store(0, Ordering::SeqCst);
    HI_ON_SECONDARY.store(0, Ordering::SeqCst);
    let spinners: alloc::vec::Vec<_> = (1..n)
        .map(|_| thread::spawn_free("low-spinner", 3, low_spinner, 0))
        .collect();
    let ipis_before: u64 = (1..n)
        .map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst))
        .sum();
    // wait (spinning: this thread must keep hart 0 busy) until every secondary runs a spinner
    let mut spins = 0u64;
    while SPINNERS_UP.load(Ordering::SeqCst) < n - 1 {
        spins += 1;
        if spins > 200_000_000 {
            STOP.store(true, Ordering::SeqCst);
            return Err(format!(
                "the spinners never got onto the other harts ({} of {} up; joined {}; ticks {:?}; resched ipis {:?}; running {:?})",
                SPINNERS_UP.load(Ordering::SeqCst),
                n - 1,
                thread::joined(),
                (0..n).map(|i| percpu::hart(i).timer_ticks.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
                (0..n).map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
                spinners.iter().map(|&t| thread::debug_state(t)).collect::<alloc::vec::Vec<_>>()
            ));
        }
        core::hint::spin_loop();
    }
    let mut highs = alloc::vec::Vec::new();
    for _ in 0..TRIALS {
        let before = HI_DONE.load(Ordering::SeqCst);
        highs.push(thread::spawn_free("high", 20, high_runner, 0));
        let mut waited = 0u64;
        while HI_DONE.load(Ordering::SeqCst) == before {
            waited += 1;
            if waited > 200_000_000 {
                STOP.store(true, Ordering::SeqCst);
                return Err(String::from(
                    "a higher-priority thread never ran: nothing displaced a spinner",
                ));
            }
            core::hint::spin_loop();
        }
    }
    STOP.store(true, Ordering::SeqCst);
    for t in spinners.into_iter().chain(highs) {
        thread::join(t);
    }
    let on_secondary = HI_ON_SECONDARY.load(Ordering::SeqCst);
    if on_secondary != TRIALS {
        return Err(format!(
            "only {on_secondary} of {TRIALS} woken threads ran on a secondary hart"
        ));
    }
    let ipis: u64 = (1..n)
        .map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst))
        .sum::<u64>()
        - ipis_before;
    if ipis < TRIALS as u64 {
        return Err(format!("{ipis} reschedule IPIs for {TRIALS} wake-ups"));
    }
    Ok(())
}

/// Two user processes, free to run anywhere, spin in user mode: at some moment two harts were in
/// user mode at the same time (the kernel counts the harts in user mode; recorded hart ids would
/// only show migration).
fn user_threads_run_in_parallel() -> Result<(), String> {
    use crate::user;
    let n = smp::started();
    if n < 2 {
        return Ok(());
    }
    thread::DEFAULT_AFFINITY.store(usize::MAX, Ordering::SeqCst);
    user::reset_user_mode_high_water();
    let a = user::start(user::CRASH_ELF, 15);
    let b = user::start(user::CRASH_ELF, 15);
    thread::DEFAULT_AFFINITY.store(0, Ordering::SeqCst);
    let (a, b) = (
        a.map_err(|e| format!("{e:?}"))?,
        b.map_err(|e| format!("{e:?}"))?,
    );
    thread::sleep_ticks(80);
    let high = user::max_harts_in_user_mode();
    for (pid, tid) in [a, b] {
        user::end_process(pid, -7);
        user::finish(pid, tid);
    }
    if high < 2 {
        return Err(format!("never more than {high} hart in user mode at once"));
    }
    Ok(())
}
