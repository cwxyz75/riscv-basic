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
    (
        "teardown_with_threads_on_other_harts",
        teardown_with_threads_on_other_harts,
    ),
    ("unmap_while_siblings_touch", unmap_while_siblings_touch),
    (
        "kill_a_spinner_on_another_hart",
        kill_a_spinner_on_another_hart,
    ),
    ("smp_churn_stress", smp_churn_stress),
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
    // (other harts take the lock for their timer ticks, so "free" is a moment, not a state)
    if !wait_for(100, bkl::is_free) {
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
    if n < 2 || thread::class_name() != "fixed-priority" {
        return Ok(()); // (priorities mean nothing to the round-robin class)
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
    user::reset_user_mode_high_water();
    let a = user::start(user::CRASH_ELF, 15);
    let b = user::start(user::CRASH_ELF, 15);
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

/// A user process whose main thread unmaps and remaps a page over and over while two sibling
/// threads (on other harts) read it: a stale translation would show the previous mapping's data.
fn unmap_while_siblings_touch() -> Result<(), String> {
    use crate::user;
    let (pid, tid) = user::start(user::CRASH_ELF, 40).map_err(|e| format!("{e:?}"))?;
    let code = user::finish(pid, tid);
    if code != 0 {
        return Err(format!("the process ended with {code}: a worker saw stale data (round {code}) or the protocol broke (9xx)"));
    }
    Ok(())
}

/// Processes with sibling threads running on other harts are ended from outside, over and over:
/// the kernel must tear them down (page tables and frames freed and reused) without any hart
/// still translating through a dead root, and nothing may leak.
fn teardown_with_threads_on_other_harts() -> Result<(), String> {
    use crate::user;
    let frames_before = crate::irq::without(|| crate::kmem::get().mm.db.free_count());
    for round in 0..120 {
        let (pid, tid) = user::start(user::CRASH_ELF, 41).map_err(|e| format!("{e:?}"))?;
        thread::sleep_ticks(1 + (round % 3));
        user::end_process(pid, -9);
        let code = user::finish(pid, tid);
        if code != -9 {
            return Err(format!("round {round}: exit code {code}, wanted -9"));
        }
    }
    if !wait_for(2000, || user::process_count() == 0) {
        return Err(String::from("processes are left"));
    }
    if user::job_ledger() != (1, 0, 0, 0, 0) {
        return Err(format!(
            "the job ledger is not back to the root: {:?}",
            user::job_ledger()
        ));
    }
    let frames_after = crate::irq::without(|| crate::kmem::get().mm.db.free_count());
    if frames_after + 8 < frames_before {
        return Err(format!("frames {frames_before} -> {frames_after}"));
    }
    Ok(())
}

/// A spinner in user mode on another hart is ended promptly. The kill sends a reschedule IPI to the
/// other harts, in the same locked region that sets the victim's kill flag: the test counts the
/// IPIs the secondary harts handle between the call and the victim's end (the control build
/// without the kick sees none), and checks that the end came well within a timer tick (the
/// tick alone would also do it, so that bound is a sanity check, not the guard).
fn kill_a_spinner_on_another_hart() -> Result<(), String> {
    use crate::user;
    let n = smp::started();
    if n < 2 {
        return Ok(());
    }
    let ipis = || -> u64 {
        (1..n)
            .map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst))
            .sum()
    };
    let mut kicked = 0;
    let mut cycles = alloc::vec::Vec::new();
    const TRIALS: usize = 15;
    for _ in 0..TRIALS {
        let (pid, tid) = user::start(user::CRASH_ELF, 15).map_err(|e| format!("{e:?}"))?;
        // wait until some secondary hart is running user code (the spinner was placed there)
        if !wait_for(1000, || {
            (1..n).any(|i| percpu::hart(i).in_user.load(Ordering::SeqCst))
        }) {
            user::end_process(pid, -3);
            user::finish(pid, tid);
            return Err(String::from("the spinner never ran on a secondary hart"));
        }
        thread::sleep_ticks(2); // let the start-up IPIs settle
        let (t0, before) = (crate::timer::now(), ipis());
        user::end_process(pid, -3);
        let code = user::finish(pid, tid);
        let took = user::LAST_EXIT_CYCLES
            .load(Ordering::SeqCst)
            .saturating_sub(t0);
        if code != -3 {
            return Err(format!("exit code {code}, wanted -3"));
        }
        if ipis() > before {
            kicked += 1;
        }
        cycles.push(took);
    }
    if kicked < TRIALS - 1 {
        return Err(format!(
            "only {kicked} of {TRIALS} kills sent a reschedule IPI to the other harts"
        ));
    }
    cycles.sort_unstable();
    let median = cycles[cycles.len() / 2];
    if median > 10_000 {
        return Err(format!(
            "the median kill took {median} cycles (over one 1 ms tick)"
        ));
    }
    Ok(())
}

fn churn_thread(_: usize) {
    HI_DONE.fetch_add(1, Ordering::SeqCst);
}

/// Several processes churn objects, memory, mappings and threads at once on different harts,
/// while the boot hart spawns and joins free kernel threads; then every leak oracle must be clean.
fn smp_churn_stress() -> Result<(), String> {
    use crate::user;
    thread::sleep_ticks(10); // let earlier tests' exited threads be reaped before counting
    let frames_before = crate::irq::without(|| crate::kmem::get().mm.db.free_count());
    let threads_before = thread::thread_count();
    let vmos_before = crate::irq::without(|| crate::kmem::get().vmos.count());
    let mut procs = alloc::vec::Vec::new();
    for seed in 1..=6u64 {
        procs.push(user::start(user::CRASH_ELF, (seed << 8) | 42).map_err(|e| format!("{e:?}"))?);
    }
    // meanwhile: kernel-thread churn on free threads
    HI_DONE.store(0, Ordering::SeqCst);
    let mut spawned = 0;
    for _ in 0..300 {
        let t = thread::spawn_free("churn", 5, churn_thread, 0);
        thread::join(t);
        spawned += 1;
    }
    for (i, (pid, tid)) in procs.into_iter().enumerate() {
        let code = user::finish(pid, tid);
        if code != 0 {
            return Err(format!("churn process {} ended with {code}", i + 1));
        }
    }
    if HI_DONE.load(Ordering::SeqCst) != spawned {
        return Err(String::from("a churn thread was lost"));
    }
    if !wait_for(3000, || user::process_count() == 0) {
        return Err(String::from("processes are left"));
    }
    thread::sleep_ticks(5);
    if user::job_ledger() != (1, 0, 0, 0, 0) {
        return Err(format!("job ledger {:?}", user::job_ledger()));
    }
    if thread::thread_count() > threads_before {
        return Err(format!(
            "threads {threads_before} -> {}",
            thread::thread_count()
        ));
    }
    let vmos_after = crate::irq::without(|| crate::kmem::get().vmos.count());
    if vmos_after != vmos_before {
        return Err(format!("VMOs {vmos_before} -> {vmos_after}"));
    }
    let frames_after = crate::irq::without(|| crate::kmem::get().mm.db.free_count());
    if frames_after + 8 < frames_before {
        return Err(format!("frames {frames_before} -> {frames_after}"));
    }
    if crate::bkl::VIOLATIONS.load(Ordering::SeqCst) != 0 {
        return Err(String::from("lock-order violations during the stress"));
    }
    Ok(())
}
