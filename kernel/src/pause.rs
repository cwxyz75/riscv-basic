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
//! What tells the two apart is that a pause of the host stops every vCPU at once: a window counts as covered only when two harts took
//! an interrupt that late inside it (`kcore::pause::covering`; on a one-hart machine the one hart), while a kernel that holds
//! interrupts off, waits for a lock in the timer handler or delays a wake does it on one hart. Two helpers keep every hart able to be
//! late: after taking a timer interrupt a hart arms a sentinel 1 ms ahead until its handler arms the next event, and while a test
//! watches (`watch`) every hart keeps a deadline at most 1 ms ahead (a heartbeat). The recording runs in every kernel (one `time` read
//! and a few relaxed atomics per arm); only the readout is for test kernels: `debug_stat` 216 to 218 (SYSCALLS.md).

use crate::percpu::MAX_HARTS;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::pause::HartRing;

/// When each hart's timer interrupt is due by what it programmed (`time` counter cycles, `kcore::pause::rearm`; `u64::MAX`: nothing
/// armed). Written by `armed` before the CSR, with interrupts off, so an interrupt taken afterwards is measured against the value that
/// raised it or a later one (a lateness is never overstated).
static ARMED: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(u64::MAX) }; MAX_HARTS];
static RINGS: [HartRing; MAX_HARTS] = [const { HartRing::new() }; MAX_HARTS];
/// While a timing test measures (until this `now_ns` time), every hart arms its timer at most `HEARTBEAT_NS` ahead (`cap`), so a hart
/// that the host stops is late for an interrupt whatever it was doing: without it a hart that runs a thread with nothing else armed
/// (tickless, L13) has no deadline that a pause could make late.
static WATCH_UNTIL: AtomicU64 = AtomicU64::new(0);
const HEARTBEAT_NS: u64 = 1_000_000;
/// How far ahead the sentinel is that `timer_taken` arms until the handler arms the real next event.
const SENTINEL_NS: u64 = 1_000_000;
/// The same in counter cycles, computed once (the counter's rate is fixed at boot).
static SENTINEL_CYCLES: AtomicU64 = AtomicU64::new(0);

fn hart() -> usize {
    crate::percpu::this().index.load(Ordering::Relaxed) % MAX_HARTS
}

fn record(h: usize, now: u64, late_cycles: u64) {
    let late_us = crate::timer::ns_from_cycles(late_cycles) / 1000;
    if let Some(r) = RINGS.get(h) {
        r.record(crate::timer::ns_at(now) / 1000, late_us);
    }
}

/// This hart is about to program `stimecmp` with `cycles`: record when the interrupt it raises is due, and keep as a sample an interrupt
/// this write cancels after it fell due (the hart never took it: `kcore::pause::rearm`). A value already past is due from the moment it
/// is written (the scheduler arms an event that is already due whenever its clock lags the counter; that is not the hardware's
/// lateness), unless an interrupt still pending was due earlier, which a re-arm must not hide.
pub fn armed(cycles: u64) {
    let h = hart();
    if let Some(a) = ARMED.get(h) {
        let now = crate::timer::now();
        let (due, missed) = kcore::pause::rearm(a.load(Ordering::Relaxed), cycles, now);
        a.store(due, Ordering::Relaxed);
        if missed > 0 {
            record(h, now, missed);
        }
    }
}

/// This hart has just taken its timer interrupt (called first in the trap handler's timer branch, before any lock is taken: a wait
/// for a lock is the kernel's delay, not the interrupt's). Records how late it came, then arms a sentinel `SENTINEL_NS` ahead until
/// the handler arms the real next event (every path of the timer branch does, `thread::program_timer`): a hart stopped between the two
/// would otherwise have nothing armed to be late for. (The stalls of the host-stall injector were seen to land just there, between a
/// taken interrupt and the re-arm, again and again: three of three undetected stalls in one boot of the planted-delay kernel.)
pub fn timer_taken() {
    let h = hart();
    let now = crate::timer::now();
    let Some(a) = ARMED.get(h) else {
        return;
    };
    let armed = a.load(Ordering::Relaxed);
    if armed != u64::MAX && now >= armed {
        record(h, now, now.saturating_sub(armed));
    }
    let mut ahead = SENTINEL_CYCLES.load(Ordering::Relaxed);
    if ahead == 0 {
        ahead = crate::timer::cycles_from_ns(SENTINEL_NS).max(1);
        SENTINEL_CYCLES.store(ahead, Ordering::Relaxed);
    }
    let sentinel = now.saturating_add(ahead);
    a.store(sentinel, Ordering::Relaxed);
    crate::timer::write_raw(sentinel);
}

/// A timing test measures for the next `ns` nanoseconds (0: it has finished): every hart keeps a deadline at most 1 ms ahead meanwhile.
/// The other harts are told at once (a reschedule interrupt, whose handler re-arms through `thread::program_timer`): a hart that runs a
/// lone thread with nothing armed, or idles with nothing armed, would otherwise not re-arm until something else woke it. This hart
/// re-arms at its next switch or interrupt.
pub fn watch(ns: u64) {
    WATCH_UNTIL.store(
        if ns == 0 {
            0
        } else {
            crate::timer::now_ns().saturating_add(ns).max(1)
        },
        Ordering::Relaxed,
    );
    if ns != 0 {
        crate::smp::kick_others();
    }
}

/// The event a hart arms (`thread::program_timer`): `ev`, or at most a heartbeat ahead while a test watches.
pub fn cap(ev: Option<u64>) -> Option<u64> {
    let until = WATCH_UNTIL.load(Ordering::Relaxed);
    if until == 0 {
        return ev; // (no test watches: no clock read on the common path)
    }
    let now = crate::timer::now_ns();
    if now >= until {
        return ev;
    }
    let beat = now.saturating_add(HEARTBEAT_NS);
    Some(ev.map_or(beat, |e| e.min(beat)))
}

fn rings() -> &'static [HartRing] {
    RINGS
        .get(..crate::smp::started().clamp(1, MAX_HARTS))
        .unwrap_or(&[])
}

/// Whether a pause covers `[start_ns, end_ns]` (the `now_ns` clock) for a test whose bound is `bound_us`: seen on two harts (on a
/// one-hart machine, the one), `kcore::pause::covering_with`. Besides the interrupts already taken late, a hart that is late **now**
/// counts (`overdue_now`): a test judges its window as it ends, when a hart that the host still has stopped has not yet taken the
/// interrupt that would record it (T287).
pub fn covering(start_ns: u64, end_ns: u64, bound_us: u64) -> kcore::pause::Cover {
    let overdue = overdue_now();
    kcore::pause::covering_with(
        rings(),
        &overdue,
        start_ns / 1000,
        end_ns.div_ceil(1000),
        bound_us,
    )
}

/// How long (microseconds) each online hart other than this one has had a timer interrupt due and not taken, now (0: none). This hart
/// is excluded: it is running this code. An offline hart is excluded: it takes no wakes, and what it has armed says nothing.
pub fn overdue_now() -> alloc::vec::Vec<u64> {
    let me = hart();
    let now = crate::timer::now();
    (0..crate::smp::started().clamp(1, MAX_HARTS))
        .map(|h| {
            if h == me || !crate::percpu::hart(h).online.load(Ordering::Acquire) {
                return 0;
            }
            let armed = ARMED.get(h).map_or(u64::MAX, |a| a.load(Ordering::Relaxed));
            if armed != u64::MAX && now > armed {
                crate::timer::ns_from_cycles(now - armed) / 1000
            } else {
                0
            }
        })
        .collect()
}

/// What each hart has armed, relative to now in microseconds (a minus sign: due that long ago and not taken; "none": nothing armed),
/// for a failure message (`harts_go_offline_and_come_back`, T287: whether the sleepers' deadlines sit on a hart that is not taking them).
pub fn armed_report() -> alloc::string::String {
    use core::fmt::Write as _;
    let mut out = alloc::string::String::new();
    let now = crate::timer::now();
    for h in 0..crate::smp::started().clamp(1, MAX_HARTS) {
        let armed = ARMED.get(h).map_or(u64::MAX, |a| a.load(Ordering::Relaxed));
        if armed == u64::MAX {
            let _ = write!(out, " {h}: none");
        } else if armed >= now {
            let _ = write!(
                out,
                " {h}: +{}",
                crate::timer::ns_from_cycles(armed - now) / 1000
            );
        } else {
            let _ = write!(
                out,
                " {h}: -{}",
                crate::timer::ns_from_cycles(now - armed) / 1000
            );
        }
    }
    out
}

/// Samples less late than this are not counted by `paused_in`: the emulator's ordinary lateness is a hart that ran.
const PAUSED_MIN_US: u64 = 2_000;

/// How long, summed over the harts, the harts were not taking their timer interrupts inside `[start_ns, end_ns]` (microseconds),
/// counting only samples at least 2 ms late.
pub fn paused_in(start_ns: u64, end_ns: u64) -> u64 {
    kcore::pause::paused_total(
        rings(),
        start_ns / 1000,
        end_ns.div_ceil(1000),
        PAUSED_MIN_US,
    )
}

/// Print `##PAUSE <test> hart <h> late <us> us` (`kcore::testlog::pause`): a window of `test` that a pause covered.
pub fn report(test: &str, hart: usize, late_us: u64) {
    let _ = kcore::testlog::pause(&mut crate::console::Console, test, hart, late_us);
}
