//! The pause detector (T284), the kernel's half: each hart records how late it took its own timer interrupt against the value it
//! programmed into `stimecmp`, and a timing test asks whether a pause covered its window. The rule (what is kept, what covers a
//! window, when to take a window again) is `kcore::pause`, tested on the host.
//!
//! A timer interrupt late by more than a test's bound means that the hart did not take it for that long: the host did not run the
//! emulated hart (QEMU's `time` follows the host clock, so a stopped vCPU thread finds its deadline long past), or the hart ran with
//! interrupts off for that long. The second is a kernel delay that this detector cannot tell from the first in one sample; a kernel
//! that does it at every wake still fails, because three covered windows in a row fail a test (`kcore::pause::Resampler`), and the
//! `latency-trace` build measures interrupts-off spans directly. What the detector does tell apart is the case that matters for the
//! timing tests: a wake that the scheduler delayed after an interrupt taken on time (no pause), from a wake that came late because the
//! interrupt itself came late (a pause).
//!
//! `debug_stat` 216 and 217 read it from user mode (test kernels only; SYSCALLS.md).

use crate::percpu::MAX_HARTS;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::pause::HartRing;

/// When each hart's timer interrupt is due by what it programmed (`time` counter cycles: the later of the value written to `stimecmp`
/// and the moment it was written; `u64::MAX`: nothing armed). Written by `timer::program` before the CSR, with interrupts off, so an
/// interrupt taken afterwards is measured against the value that raised it or a later one (never an earlier one: a lateness is never
/// overstated).
static ARMED: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(u64::MAX) }; MAX_HARTS];
static RINGS: [HartRing; MAX_HARTS] = [const { HartRing::new() }; MAX_HARTS];

fn hart() -> usize {
    crate::percpu::this().index.load(Ordering::Relaxed) % MAX_HARTS
}

/// This hart is about to program `stimecmp` with `cycles`. A value already past raises the interrupt at once: the hardware is late
/// only from the moment it was programmed, so that moment is what is recorded (the scheduler arms an event that is already due
/// whenever its clock lags the counter: measured against the value itself, the control of `the_latency_test_can_fail` showed 5 ms
/// "pauses" that were the scheduler's own lag, T284).
pub fn armed(cycles: u64) {
    if let Some(a) = ARMED.get(hart()) {
        a.store(
            if cycles == u64::MAX {
                cycles
            } else {
                cycles.max(crate::timer::now())
            },
            Ordering::Relaxed,
        );
    }
}

/// This hart has just taken its timer interrupt (called first in the trap handler's timer branch, before any lock is taken: a wait
/// for a lock is the kernel's delay, not the interrupt's).
pub fn timer_taken() {
    let h = hart();
    let now = crate::timer::now();
    let Some(armed) = ARMED.get(h).map(|a| a.load(Ordering::Relaxed)) else {
        return;
    };
    if armed == u64::MAX || now < armed {
        return; // (re-armed later since it was raised: nothing to measure)
    }
    let late_us = crate::timer::ns_from_cycles(now.saturating_sub(armed)) / 1000;
    let at_us = crate::timer::ns_at(now) / 1000;
    if let Some(r) = RINGS.get(h) {
        r.record(at_us, late_us);
    }
}

fn rings() -> &'static [HartRing] {
    RINGS
        .get(..crate::smp::started().clamp(1, MAX_HARTS))
        .unwrap_or(&[])
}

/// The worst pause of any hart that overlaps `[start_ns, end_ns]` (the `now_ns` clock): (hart, microseconds late), or None.
pub fn worst_in(start_ns: u64, end_ns: u64) -> Option<(usize, u64)> {
    kcore::pause::worst_of(rings(), start_ns / 1000, end_ns.div_ceil(1000))
}

/// How long, summed over the harts, the harts were not taking their timer interrupts inside `[start_ns, end_ns]` (microseconds).
pub fn paused_in(start_ns: u64, end_ns: u64) -> u64 {
    kcore::pause::paused_total(rings(), start_ns / 1000, end_ns.div_ceil(1000))
}

/// Print `##PAUSE <test> hart <h> late <us> us` (`kcore::testlog::pause`): a window of `test` that a pause covered.
pub fn report(test: &str, hart: usize, late_us: u64) {
    let _ = kcore::testlog::pause(&mut crate::console::Console, test, hart, late_us);
}
