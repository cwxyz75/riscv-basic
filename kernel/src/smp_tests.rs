//! SMP bring-up checks (S5a): the secondary harts came online with their own state, their own
//! timers and working IPIs.

use crate::percpu;
use crate::{smp, thread};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering;

type Test = (&'static str, fn() -> Result<(), String>);

/// `wfi` must return when an enabled interrupt is pending even though `sstatus.SIE` is clear (privileged spec): the
/// idle protocol relies on it to close the window between "nothing to run" and "sleep" (SCHED-SMP-SPEC 3.9, task 4).
fn wfi_returns_with_interrupts_masked() -> Result<(), String> {
    let start = crate::timer::now();
    crate::irq::without(|| {
        // the timer interrupt is enabled (STIE) but globally masked here; program it one tick ahead
        crate::timer::program(Some(crate::timer::now_ns() + 1_000_000));
        crate::arch::cpu_idle(); // returns at once if an interrupt is already pending
    });
    let waited = crate::timer::now() - start;
    if waited > 200 * 10_000 {
        return Err(alloc::format!("idle took {waited} cycles to return"));
    }
    Ok(())
}

static RT_ARRIVED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RT_HARTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RT_WAIT_FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn rt_probe(n: usize) {
    RT_HARTS.fetch_or(
        1 << percpu::this().index.load(Ordering::Relaxed),
        Ordering::SeqCst,
    );
    RT_ARRIVED.fetch_add(1, Ordering::SeqCst);
    let deadline = thread::ticks() + 2000; // (well above the watchdog's 500 ms)
    while RT_ARRIVED.load(Ordering::SeqCst) < n {
        if thread::ticks() > deadline {
            RT_WAIT_FAILED.store(true, Ordering::SeqCst);
            return;
        }
        core::hint::spin_loop();
    }
}

/// SCHED-SMP-SPEC task 11: as many real-time threads as there are harts, all above the thread that starts them, all free to
/// run anywhere, take every hart at once (each waits for the others to arrive, so one hart running two of them in turn would
/// time out): the push marks and pulls spread them, and the starting thread is preempted to make room.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn rt_threads_take_the_harts() -> Result<(), String> {
    let n = smp::started();
    RT_ARRIVED.store(0, Ordering::SeqCst);
    RT_HARTS.store(0, Ordering::SeqCst);
    RT_WAIT_FAILED.store(false, Ordering::SeqCst);
    let prio = crate::sched_tests::MAIN_PRIO + 10;
    let mut tids = alloc::vec::Vec::new();
    // (all in one critical section: a spawn that preempted its caller at once would leave the rest unstarted)
    crate::irq::without(|| {
        for _ in 0..n {
            tids.push(thread::spawn_free("rt_probe", prio, rt_probe, n));
        }
    });
    for t in tids {
        thread::join(t);
    }
    if RT_WAIT_FAILED.load(Ordering::SeqCst) {
        return Err(format!(
            "the real-time threads did not run at once on {n} harts (harts used: {:#b}; ipis {:?}; timer {:?}; online {:?})",
            RT_HARTS.load(Ordering::SeqCst),
            (0..n).map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
            (0..n).map(|i| percpu::hart(i).timer_ticks.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
            (0..n).map(|i| percpu::hart(i).online.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>()
        ));
    }
    let used = RT_HARTS.load(Ordering::SeqCst);
    if used.count_ones() as usize != n {
        return Err(format!("{n} threads ran on harts {used:#b}"));
    }
    Ok(())
}

static SLEEPER_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn short_sleeper(_: usize) {
    while !SLEEPER_STOP.load(Ordering::Relaxed) {
        thread::sleep_ns(100_000); // 100 microseconds
    }
}

/// PM2 (KERNEL-V1-SPEC 5.5): a hart with nothing to do for a long time enters the firmware's retentive suspend, and a hart whose next
/// event is a hundred microseconds away uses `wfi`. On QEMU the two are equivalent in effect (OpenSBI's default retentive suspend is
/// a `wfi`); the counters are what tells them apart.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn long_idle_suspends_and_a_short_sleeper_does_not() -> Result<(), String> {
    let n = smp::started();
    if n < 3 {
        return Ok(());
    }
    // hart 1 idle for 100 ms: its entries are suspends (when the firmware has the call)
    let (s0, w0) = (
        percpu::hart(1).idle_suspend.load(Ordering::Relaxed),
        percpu::hart(1).idle_wfi.load(Ordering::Relaxed),
    );
    thread::sleep_ticks(100);
    // (one long idle is one entry, begun before the window: the count since boot is what shows it)
    let suspends = percpu::hart(1).idle_suspend.load(Ordering::Relaxed);
    let _ = s0;
    let _wfis = percpu::hart(1).idle_wfi.load(Ordering::Relaxed) - w0;
    if smp::suspend_supported() && suspends == 0 {
        return Err(format!(
            "an idle hart with no timer event never entered the retentive suspend in 100 ms (idle-wait entries {_wfis}, idle ns {})",
            percpu::hart(1).idle_ns.load(Ordering::Relaxed)
        ));
    }
    // hart 2 runs a thread that sleeps 100 us at a time: its idles are short: no suspend entries
    SLEEPER_STOP.store(false, Ordering::Relaxed);
    let before = thread::DEFAULT_AFFINITY.swap(2, Ordering::SeqCst);
    let t = thread::spawn("short_sleeper", 20, short_sleeper, 0);
    thread::DEFAULT_AFFINITY.store(before, Ordering::SeqCst);
    thread::sleep_ticks(10); // (let it settle in)
    let s1 = percpu::hart(2).idle_suspend.load(Ordering::Relaxed);
    let w1 = percpu::hart(2).idle_wfi.load(Ordering::Relaxed);
    thread::sleep_ticks(100);
    let suspends2 = percpu::hart(2).idle_suspend.load(Ordering::Relaxed) - s1;
    let wfis2 = percpu::hart(2).idle_wfi.load(Ordering::Relaxed) - w1;
    SLEEPER_STOP.store(true, Ordering::Relaxed);
    thread::join(t);
    if wfis2 < 20 {
        return Err(format!(
            "hart 2, with a 100 us sleeper, idled through the wait instruction only {wfis2} times in 100 ms"
        ));
    }
    if suspends2 > 2 {
        return Err(format!("hart 2, with a 100 us sleeper, entered the long-idle suspend {suspends2} times in 100 ms"));
    }
    Ok(())
}

pub const TESTS: &[Test] = &[
    (
        "wfi_returns_with_interrupts_masked",
        wfi_returns_with_interrupts_masked,
    ),
    ("rt_threads_take_the_harts", rt_threads_take_the_harts),
    (
        "long_idle_suspends_and_a_short_sleeper_does_not",
        long_idle_suspends_and_a_short_sleeper_does_not,
    ),
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
        "decommit_while_siblings_touch",
        decommit_while_siblings_touch,
    ),
    ("protect_while_siblings_write", protect_while_siblings_write),
    (
        "kill_a_spinner_on_another_hart",
        kill_a_spinner_on_another_hart,
    ),
    (
        "user_mode_ticks_take_no_big_lock",
        user_mode_ticks_take_no_big_lock,
    ),
    (
        "user_clock_get_and_yield_take_no_big_lock",
        user_clock_get_and_yield_take_no_big_lock,
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

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
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
    // (no hart has a periodic timer any more, L13: the timer tests are `idle_harts_take_no_big_lock_for_their_ticks` and the one-shot checks)
    Ok(())
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
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
        if h.timer_ticks.load(Ordering::SeqCst) > ticks + 3 {
            return Err(format!(
                "hart {i} takes timer interrupts nobody asked for after the IPIs"
            ));
        }
    }
    // PL6: the interrupts above were messages to the harts' IMSIC files; the firmware's IPI extension was never called
    if crate::arch::sbi::ipi_calls() != 0 {
        return Err(format!(
            "{} calls were made to the SBI IPI extension",
            crate::arch::sbi::ipi_calls()
        ));
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
        if smp::wait_work(i, 5 * crate::timer::timebase()) != Some(M) {
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
        (Class::Heap, Class::Obj, true),
        (Class::Uart, Class::Obj, true),
        (Class::Uart, Class::Heap, true),
        (Class::Obj, Class::Heap, false),
        (Class::Obj, Class::Uart, false),
        (Class::Heap, Class::Uart, false),
        // the planned classes, through the same checker (the pure table is in `klock/tests/order.rs`)
        (Class::Heap, Class::Frame, true),
        (Class::Frame, Class::Heap, false),
        // the memory locks (L16 part b): none may be held when the big lock is taken, nor taken in the wrong order
        (Class::Vmo, Class::Obj, true),
        (Class::Jobs, Class::Obj, true),
        (Class::Frame, Class::Obj, true),
        (Class::Jobs, Class::Vmo, true),
        (Class::Frame, Class::Jobs, true),
        (Class::Vmo, Class::Jobs, false),
        (Class::Jobs, Class::Frame, false),
        (Class::Procs, Class::Futex, false),
        (Class::Futex, Class::Procs, true),
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
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
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
    // (one reschedule IPI per wake, except that the secondary's own tick may take the woken thread first: the pull at a tick
    // is as good a way to displace the spinner, and the IPI it makes redundant is then not sent)
    if ipis < (TRIALS as u64) * 2 / 3 {
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

/// MM6: the same for `vmo_decommit`: two sibling threads read a page whose frame the main thread decommits and gives to another VMO;
/// through a stale translation a worker would read that VMO's bytes where the decommitted page reads zeros.
fn decommit_while_siblings_touch() -> Result<(), String> {
    use crate::user;
    let (pid, tid) = user::start(user::CRASH_ELF, 102).map_err(|e| format!("{e:?}"))?;
    let code = user::finish(pid, tid);
    if code != 0 {
        return Err(format!("the process ended with {code}: a worker read through a stale translation (round {code}) or the protocol broke (9xx)"));
    }
    Ok(())
}

/// MM7: a protect that takes write permission away reaches the other harts: a worker that has written the page (so its hart caches a
/// writable translation) and writes it again after the protect must fault, and a fault ends the process (-1001). Through a stale
/// translation the write would go through and the process would end normally.
fn protect_while_siblings_write() -> Result<(), String> {
    use crate::user;
    // (twenty processes: a worker that is switched out loses its cached translation anyway, so one round proves little; with the
    // shootdown skipped, at least one of twenty must see the stale entry and exit 0)
    for round in 0..20 {
        let (pid, tid) = user::start(user::CRASH_ELF, 103).map_err(|e| format!("{e:?}"))?;
        let code = user::finish(pid, tid);
        if code != -1001 {
            return Err(format!("round {round}: the process ended with {code}, not -1001: a worker wrote through a stale writable translation (0), or the protocol broke (9xx)"));
        }
    }
    Ok(())
}

/// Processes with sibling threads running on other harts are ended from outside, over and over:
/// the kernel must tear them down (page tables and frames freed and reused) without any hart
/// still translating through a dead root, and nothing may leak.
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn teardown_with_threads_on_other_harts() -> Result<(), String> {
    use crate::user;
    let frames_before = crate::irq::without(crate::kmem::free_frames_accounted);
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
    if user::job_ledger() != (1, 0, 0, 0, 0, 0) {
        return Err(format!(
            "the job ledger is not back to the root: {:?}",
            user::job_ledger()
        ));
    }
    let frames_after = crate::irq::without(crate::kmem::free_frames_accounted);
    if frames_after != frames_before {
        return Err(format!("frames {frames_before} -> {frames_after}"));
    }
    Ok(())
}

/// L12b (SCHED-SMP-SPEC task 12, acceptance): a hart running a user thread that does nothing but spin takes no big kernel lock for its
/// timer ticks. A secondary hart's count of outermost big-lock takes must hardly move over 80 ticks; it took one per tick when every
/// trap from user mode took the lock.
fn user_mode_ticks_take_no_big_lock() -> Result<(), String> {
    no_big_lock_for(15, "a spinning user thread")
}

/// L12c: the same for a user thread that does nothing but call `clock_get`, and for one that only calls `thread_yield`.
fn user_clock_get_and_yield_take_no_big_lock() -> Result<(), String> {
    no_big_lock_for(80, "a user thread calling clock_get")?;
    no_big_lock_for(81, "a user thread calling thread_yield")?;
    no_big_lock_for(82, "a user thread calling futex_wake")
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn no_big_lock_for(program: u64, what: &str) -> Result<(), String> {
    use crate::user;
    let n = smp::started();
    if n < 2 {
        return Ok(());
    }
    let (pid, tid) = user::start(user::CRASH_ELF, program).map_err(|e| format!("{e:?}"))?;
    let mut host = None;
    for _ in 0..5000 {
        // (the hart that is running the thread: `in_user` is no test for one that traps all the time; the test thread itself sleeps, so
        // hart 0 is as good a host as any other)
        host = (0..n).find(|&i| percpu::hart(i).running.load(Ordering::Relaxed) == tid);
        if host.is_some() {
            break;
        }
        thread::sleep_ticks(1);
    }
    let result = (|| {
        let h = host.ok_or_else(|| {
            format!(
                "{what} (thread {tid}) never ran on any hart (state {}; running {:?})",
                thread::debug_state(tid),
                (0..n)
                    .map(|i| percpu::hart(i).running.load(Ordering::Relaxed))
                    .collect::<alloc::vec::Vec<_>>()
            )
        })?;
        thread::sleep_ticks(20); // the start-up traps settle (the first tick's full path records the epoch)
        let (takes0, ticks0) = (
            percpu::hart(h).bkl_takes.load(Ordering::Relaxed),
            percpu::hart(h).timer_ticks.load(Ordering::Relaxed),
        );
        thread::sleep_ticks(80);
        let takes = percpu::hart(h).bkl_takes.load(Ordering::Relaxed) - takes0;
        let ticks = percpu::hart(h).timer_ticks.load(Ordering::Relaxed) - ticks0;
        // (a thread that traps all the time is often between `user_mode_left` and `user_mode_entered`: `in_user` is no test of it)
        // (still on the hart it was measured on: a thread that moved away leaves the hart idle and the count proves nothing)
        if percpu::hart(h).running.load(Ordering::Relaxed) != tid {
            return Err(format!("{what} left hart {h} during the measurement"));
        }
        // (L13: no periodic tick: a hart with one runnable thread and no deadline takes no timer interrupt at all)
        if ticks > 3 {
            return Err(format!(
                "hart {h}, running {what} alone, took {ticks} timer interrupts in 80 ms"
            ));
        }
        if takes > 8 {
            return Err(format!(
                "hart {h}, running {what}, took the big kernel lock {takes} times for {ticks} timer interrupts"
            ));
        }
        Ok(())
    })();
    user::end_process(pid, -3);
    user::finish(pid, tid);
    result
}

/// A spinner in user mode on another hart is ended promptly. The kill sends a reschedule IPI to the
/// other harts, in the same locked region that sets the victim's kill flag: the test counts the
/// IPIs the secondary harts handle between the call and the victim's end (the control build
/// without the kick sees none), and checks that the end came well within a timer tick (the
/// tick alone would also do it, so that bound is a sanity check, not the guard).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
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
    for trial in 0..TRIALS {
        let (pid, tid) = user::start(user::CRASH_ELF, 15).map_err(|e| format!("{e:?}"))?;
        // wait until some secondary hart is running user code (the spinner was placed there)
        // (a hart's ticks are phase-locked with hart 0's, so one sample per tick can land every time in the window in which
        // the other hart is inside its own tick trap, `in_user` clear: look several times, a little apart)
        if !wait_for(1000, || {
            (0..50).any(|_| {
                let up = (1..n).any(|i| percpu::hart(i).in_user.load(Ordering::SeqCst));
                if !up {
                    for _ in 0..3000 {
                        core::hint::spin_loop();
                    }
                }
                up
            })
        }) {
            let why = format!(
                "the spinner never ran on a secondary hart (trial {trial}; spinner {}; in user mode {:?})",
                thread::debug_state(tid),
                (0..n)
                    .map(|i| percpu::hart(i).in_user.load(Ordering::SeqCst))
                    .collect::<alloc::vec::Vec<_>>()
            ) + &format!(
                " ipis {:?} ticks {:?} bkl takes {:?}",
                (0..n).map(|i| percpu::hart(i).resched_ipis.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
                (0..n).map(|i| percpu::hart(i).timer_ticks.load(Ordering::SeqCst)).collect::<alloc::vec::Vec<_>>(),
                (0..n).map(|i| thread::wake_stat(16 + i as u64)).collect::<alloc::vec::Vec<_>>()
            );
            user::end_process(pid, -3);
            user::finish(pid, tid);
            return Err(why);
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
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn smp_churn_stress() -> Result<(), String> {
    use crate::user;
    thread::sleep_ticks(10); // let earlier tests' exited threads be reaped before counting
    let frames_before = crate::irq::without(crate::kmem::free_frames_accounted);
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
    if user::job_ledger() != (1, 0, 0, 0, 0, 0) {
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
    let frames_after = crate::irq::without(crate::kmem::free_frames_accounted);
    if frames_after != frames_before {
        return Err(format!("frames {frames_before} -> {frames_after}"));
    }
    if crate::bkl::VIOLATIONS.load(Ordering::SeqCst) != 0 {
        return Err(String::from("lock-order violations during the stress"));
    }
    Ok(())
}
