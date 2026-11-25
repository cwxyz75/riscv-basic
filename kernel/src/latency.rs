//! The latency trace (slice B, SCHED-SMP-SPEC 3.12 and 6.1, task 2): with the `latency-trace` feature the kernel
//! records, per hart, the **longest interrupts-off span and where it began**, and, per syscall number, how long the
//! big kernel lock was held and how long a take waited for it. Without the feature every function here is empty and
//! inlined away, so the hot paths (`irq::without`, `bkl::lock`) cost nothing.
//!
//! Interrupts-off spans are measured at `irq::without` (the kernel's critical section, by far the common way) and
//! around every trap; the site is the caller's `Location`. The numbers are `time` counter cycles (the counter's rate is the device tree's, `timer::timebase`).

#![allow(dead_code)]

use core::panic::Location;

#[cfg(feature = "latency-trace")]
mod on {
    use super::*;
    use crate::percpu::MAX_HARTS;
    use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering::Relaxed};

    pub const SYSCALLS: usize = 128;

    static OFF_MAX: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
    /// When the hart's current interrupts-off span began (0: none). A span ends when the code that began it
    /// returns *or when the hart switches threads* (a thread that blocks inside a critical section sleeps
    /// with the span still open in its own frame; the hart's span is what counts).
    static OFF_START: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
    static OFF_SITE: [AtomicPtr<Location<'static>>; MAX_HARTS] =
        [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_HARTS];
    /// When the hart's current BKL hold began (0: not holding), and the syscall it is serving (0: none).
    static ACQUIRED: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
    static CURRENT: [AtomicU32; MAX_HARTS] = [const { AtomicU32::new(0) }; MAX_HARTS];
    static HOLDS: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];
    static HOLD_TOTAL: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];
    static HOLD_MAX: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];
    static WAIT_TOTAL: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];
    static WAIT_MAX: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];

    fn hart() -> usize {
        crate::percpu::this().index.load(Relaxed)
    }

    /// Interrupts were just turned off by this code: if they were on, a span begins (the token says so).
    #[inline(always)]
    pub fn off_begin(was_enabled: bool) -> u64 {
        if was_enabled {
            OFF_START[hart()].store(crate::timer::now(), Relaxed);
            1
        } else {
            0
        }
    }

    fn record(h: usize, start: u64, site: &'static Location<'static>) {
        if start == 0 {
            return;
        }
        let span = crate::timer::now().saturating_sub(start);
        if span > OFF_MAX[h].load(Relaxed) {
            OFF_MAX[h].store(span, Relaxed);
            OFF_SITE[h].store(site as *const _ as *mut _, Relaxed);
        }
    }

    #[inline(always)]
    pub fn off_end(token: u64, site: &'static Location<'static>) {
        if token == 0 {
            return;
        }
        let h = hart();
        record(h, OFF_START[h].swap(0, Relaxed), site);
    }

    /// The hart is about to switch threads: the span so far ends here (the next thread starts its own), at
    /// the caller's site.
    #[track_caller]
    pub fn switch_out() {
        let h = hart();
        record(h, OFF_START[h].swap(0, Relaxed), Location::caller());
    }

    /// Interrupts are about to be turned on by something other than the end of a critical section (the idle loop,
    /// the return to user mode): the open span ends here.
    #[track_caller]
    pub fn interrupts_on() {
        let h = hart();
        record(h, OFF_START[h].swap(0, Relaxed), Location::caller());
    }

    /// A thread has just been resumed (or started) on this hart with interrupts still off: a span begins.
    pub fn switch_in() {
        OFF_START[hart()].store(crate::timer::now(), Relaxed);
    }

    pub fn set_syscall(n: usize) {
        CURRENT[hart()].store(n.min(SYSCALLS - 1) as u32, Relaxed);
    }

    pub fn clear_syscall() {
        CURRENT[hart()].store(0, Relaxed);
    }

    /// A take of the big lock finished: it waited `waited` cycles; the hold begins now.
    pub fn bkl_acquired(waited: u64) {
        let n = CURRENT[hart()].load(Relaxed) as usize;
        WAIT_TOTAL[n].fetch_add(waited, Relaxed);
        WAIT_MAX[n].fetch_max(waited, Relaxed);
        ACQUIRED[hart()].store(crate::timer::now(), Relaxed);
    }

    /// The outermost release: record the hold against the syscall being served.
    pub fn bkl_released() {
        let t0 = ACQUIRED[hart()].swap(0, Relaxed);
        if t0 == 0 {
            return;
        }
        let held = crate::timer::now().saturating_sub(t0);
        let n = CURRENT[hart()].load(Relaxed) as usize;
        HOLDS[n].fetch_add(1, Relaxed);
        HOLD_TOTAL[n].fetch_add(held, Relaxed);
        HOLD_MAX[n].fetch_max(held, Relaxed);
        // (the syscall that was being served ended with this trap)
        CURRENT[hart()].store(0, Relaxed);
    }

    /// The longest interrupts-off span of any hart since the last `reset`: cycles and where it began.
    pub fn irq_off_max() -> (u64, Option<&'static Location<'static>>) {
        let mut best = (0, None);
        for h in 0..MAX_HARTS {
            let v = OFF_MAX[h].load(Relaxed);
            if v > best.0 {
                let p = OFF_SITE[h].load(Relaxed);
                // SAFETY: the pointer was made from a `&'static Location` in `off_end` (or is null).
                best = (v, unsafe { p.as_ref() });
            }
        }
        best
    }

    /// The longest interrupts-off span of hart `h` since the last `reset`.
    pub fn irq_off_max_on(h: usize) -> (u64, Option<&'static Location<'static>>) {
        let p = OFF_SITE[h].load(Relaxed);
        // SAFETY: as in `irq_off_max`.
        (OFF_MAX[h].load(Relaxed), unsafe { p.as_ref() })
    }

    pub fn this_hart() -> usize {
        hart()
    }

    /// The window of a test: say the longest big-lock hold and the longest interrupts-off span so far (with where the span ended: a trap
    /// runs with interrupts off from entry to return unless a switch splits it, so it is the whole call when nothing else wants the hart; the
    /// hold is what the chunking of T207 bounds, X4), return the hold, and start again.
    pub fn take_window() -> u64 {
        use core::fmt::Write;
        let (m, site) = irq_off_max();
        let hold = (0..SYSCALLS)
            .map(|n| HOLD_MAX[n].swap(0, Relaxed))
            .max()
            .unwrap_or(0);
        let _ = writeln!(
            crate::console::Console,
            "latency: window: big-lock hold max {} ns; interrupts-off max {} ns ending at {}",
            hold * 100,
            m * 100,
            site.map_or(alloc::string::String::from("?"), |s| alloc::format!(
                "{}:{}",
                s.file(),
                s.line()
            ))
        );
        reset();
        hold
    }

    pub fn reset() {
        for h in 0..MAX_HARTS {
            OFF_MAX[h].store(0, Relaxed);
        }
    }

    /// The same figures as `##BENCH` lines for the benchmark harness (SCHED-SMP-SPEC 6.1: `irq_off_max`, `bkl_profile`): the
    /// interrupts-off maximum, the longest BKL hold of any syscall, and the maximum hold and wait of the ten worst.
    #[inline(never)]
    fn bench_lines(irq_off: u64, worst: &[(u64, usize)]) {
        use core::fmt::Write;
        let mut out = crate::console::Console;
        let _ = writeln!(out, "##BENCH irq_off_max {} ns", irq_off * 100);
        if let Some((v, _)) = worst.first() {
            let _ = writeln!(out, "##BENCH bkl_hold_max {} ns", v * 100);
        }
        for (v, n) in worst.iter().take(10) {
            let _ = writeln!(out, "##BENCH bkl_hold_max_sys{n} {} ns", v * 100);
            let _ = writeln!(
                out,
                "##BENCH bkl_wait_max_sys{n} {} ns",
                WAIT_MAX[*n].load(Relaxed) * 100
            );
        }
    }

    /// Print the summary (the end of a run): the interrupts-off maximum, and the busiest and longest BKL holds.
    pub fn report() {
        use core::fmt::Write;
        let mut out = crate::console::Console;
        let (m, site) = irq_off_max();
        let _ = writeln!(
            out,
            "latency: interrupts-off max {} ns at {}",
            m * 100,
            site.map_or(alloc::string::String::from("?"), |s| alloc::format!(
                "{}:{}",
                s.file(),
                s.line()
            ))
        );
        let mut worst: alloc::vec::Vec<(u64, usize)> = (0..SYSCALLS)
            .map(|n| (HOLD_MAX[n].load(Relaxed), n))
            .filter(|(v, _)| *v > 0)
            .collect();
        worst.sort_unstable_by(|a, b| b.cmp(a));
        bench_lines(m, &worst);
        for (v, n) in worst.into_iter().take(10) {
            let _ = writeln!(
                out,
                "latency: bkl hold syscall {n}: max {} ns, {} holds, total {} us; wait max {} ns, total {} us",
                v * 100,
                HOLDS[n].load(Relaxed),
                HOLD_TOTAL[n].load(Relaxed) / 10,
                WAIT_MAX[n].load(Relaxed) * 100,
                WAIT_TOTAL[n].load(Relaxed) / 10
            );
        }
    }
}

#[cfg(feature = "latency-trace")]
pub use on::*;

#[cfg(not(feature = "latency-trace"))]
mod off {
    use super::*;
    #[inline(always)]
    pub fn off_begin(_was_enabled: bool) -> u64 {
        0
    }
    #[inline(always)]
    pub fn off_end(_token: u64, _site: &'static Location<'static>) {}
    #[inline(always)]
    pub fn switch_out() {}
    #[inline(always)]
    pub fn switch_in() {}
    #[inline(always)]
    pub fn interrupts_on() {}
    #[inline(always)]
    pub fn set_syscall(_n: usize) {}
    #[inline(always)]
    pub fn clear_syscall() {}
    #[inline(always)]
    pub fn bkl_acquired(_waited: u64) {}
    #[inline(always)]
    pub fn bkl_released() {}
    pub fn irq_off_max() -> (u64, Option<&'static Location<'static>>) {
        (0, None)
    }
    pub fn take_window() -> u64 {
        0
    }
    pub fn reset() {}
    pub fn report() {}
}

#[cfg(not(feature = "latency-trace"))]
pub use off::*;

/// A test section of known length is reported within 20% (and at the right place): the trace can see what it claims to.
#[cfg(feature = "latency-trace")]
pub fn the_latency_trace_can_see_a_long_section() -> Result<(), alloc::string::String> {
    let len = crate::timer::cycles_from_ns(5_000_000); // 5 ms of the counter
    reset();
    // (this hart's maximum: another hart waiting for the big lock with interrupts off can record a longer span of its
    // own between the reset and the read, which says nothing about this section)
    let h = crate::irq::without(|| {
        let start = crate::timer::now();
        while crate::timer::now() < start + len {
            core::hint::spin_loop();
        }
        this_hart()
    });
    let (cycles, site) = irq_off_max_on(h);
    if !(len..=len + len / 5).contains(&cycles) {
        return Err(alloc::format!(
            "a {len}-cycle interrupts-off section was reported as {cycles} cycles"
        ));
    }
    match site {
        Some(s) if s.file().ends_with("latency.rs") => Ok(()),
        other => Err(alloc::format!("the span was attributed to {other:?}")),
    }
}
