//! The debug detectors' rules (T266): when the starvation watchdog may blame the scheduler, and what the big-lock spin report says
//! about the owner. Both are pure, so the rules are tested on the host (`kcore/tests/sched_watchdog.rs`, `kcore/tests/spin_report.rs`);
//! the kernel only feeds them its counters (`kernel/src/thread.rs`, `kernel/src/bkl.rs`).
//!
//! **The starvation watchdog** (SCHED-SMP-SPEC 3.9) reports a thread queued for more than 500 ms on one hart while a hart that may
//! run it idles ([`crate::sched_smp::Sched2::starved`]). Read alone, that finding cannot tell a scheduler that forgot the thread from
//! a hart that did not run at all: the host deschedules an emulated hart's thread (an oversubscribed host, T197) or stops the whole
//! emulator (the host-stall injector, T284), and a queued thread then waits however good the scheduler is. [`Watchdog`] adds a
//! progress rule: a finding counts toward the report only when the hart the thread is queued on has taken an interrupt since the
//! previous sample (its progress counter moved). A hart that took none did not execute, and the sample is **void**: neither counted
//! nor a reset of the count. A hart that ticks, has the thread queued, and neither runs it nor gives it to the idle hart is still
//! reported, on the third sample in a row ([`REPORT_AT`]), as before. A hart that shows no progress at all for [`STALL_NS`] while
//! the thread waits is reported differently ([`Verdict::Stalled`]): a hart stuck with interrupts off, or a lost poke on a hart that
//! armed no timer, is a real defect too, and far beyond any pause of a host.
//!
//! **The spin report** (T212): a hart that spins 30 million times for the big lock panics and names the owner and where the owner
//! took the lock. [`spin_verdict`] adds how many times the owner hart took the lock while this one waited: none is one long
//! section (the site named is the culprit), some is a waiter that lost every race (an unfair hand-over, or a waiter the host did
//! not run while the owner kept going).

/// The rule needs this many samples of one finding in a row, the first included (the same three as before T266).
pub const REPORT_AT: u32 = 3;
/// A hart that shows no progress for this long (by the sampling hart's clock) while a thread waits on it is reported as stalled
/// (10 s: far beyond any host pause seen, 1.6 s at most in T284's runs; short against CI's job timeout).
pub const STALL_NS: u64 = 10_000_000_000;

/// What one sample decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to report.
    Quiet,
    /// The same thread was found starved in [`REPORT_AT`] samples, each but the first after an interrupt on its hart: report it.
    Starved { tid: u64, on: usize, idle: usize },
    /// The thread's hart took no interrupt for `quiet_ns` while the thread waited on it ([`STALL_NS`] or more): report the hart.
    Stalled { tid: u64, on: usize, quiet_ns: u64 },
}

/// One sampling hart's memory between its samples: the finding it is following and how often it was confirmed. Plain values, so
/// the kernel can keep it in atomics ([`Watchdog::to_words`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Watchdog {
    /// The thread followed (0: none).
    pub tid: u64,
    /// The hart it is queued on.
    pub on: usize,
    /// That hart's progress counter at the last sample that counted.
    pub progress: u64,
    /// The samples that counted, the first sighting included (0: none).
    pub seen: u32,
    /// The sampler's clock when that hart last showed progress (or the thread was first seen).
    pub moved_at: u64,
}

impl Watchdog {
    /// A sample at `now` (the sampling hart's clock, ns). `found` is what [`crate::sched_smp::Sched2::starved`] returned: (thread, the
    /// hart it is queued on, an idle hart that may run it); `progress(h)` is hart `h`'s progress counter (interrupts taken), read now.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: `seen` stops at REPORT_AT; the clock difference saturates
    pub fn sample(
        &mut self,
        now: u64,
        found: Option<(u64, usize, usize)>,
        progress: impl Fn(usize) -> u64,
    ) -> Verdict {
        let Some((tid, on, idle)) = found else {
            *self = Self::default();
            return Verdict::Quiet;
        };
        let p = progress(on);
        if self.seen == 0 || tid != self.tid || on != self.on {
            // a new finding: the first sample of it counts (it has no baseline to have moved from)
            *self = Self {
                tid,
                on,
                progress: p,
                seen: 1,
                moved_at: now,
            };
        } else if p != self.progress {
            // the hart took an interrupt since the last counted sample: it executed, and the thread is still left there
            self.progress = p;
            self.moved_at = now;
            self.seen = (self.seen + 1).min(REPORT_AT);
        } else {
            // void: the hart did not run since (a host pause, or a hart stuck with interrupts off); neither counted nor reset
            let quiet_ns = now.saturating_sub(self.moved_at);
            if quiet_ns >= STALL_NS {
                return Verdict::Stalled { tid, on, quiet_ns };
            }
            return Verdict::Quiet;
        }
        if self.seen >= REPORT_AT {
            Verdict::Starved { tid, on, idle }
        } else {
            Verdict::Quiet
        }
    }

    /// The state as five words (for a kernel that keeps it in atomics).
    pub fn to_words(&self) -> [u64; 5] {
        [
            self.tid,
            self.on as u64,
            self.progress,
            u64::from(self.seen),
            self.moved_at,
        ]
    }

    /// The inverse of [`Watchdog::to_words`].
    pub fn from_words(w: [u64; 5]) -> Self {
        Self {
            tid: w[0],
            on: usize::try_from(w[1]).unwrap_or(usize::MAX),
            progress: w[2],
            seen: u32::try_from(w[3]).unwrap_or(REPORT_AT),
            moved_at: w[4],
        }
    }
}

/// What the waiter saw of the big lock's owner when it began to spin: who owned it (`owner`: hart + 1, 0 none), how many outermost
/// takes the owner's hart had made, and the waiter's clock (ns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpinStart {
    pub owner: u64,
    pub owner_takes: u64,
    pub at_ns: u64,
}

/// What the spin report says about the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpinVerdict {
    /// The owner at the start still holds the lock and took it no more since: one section held the whole time (the site the report
    /// names is where it was taken).
    OneLongSection { waited_ms: u64 },
    /// The lock changed hands while this hart waited (`takes`: the start owner's outermost takes since, or 0 when another hart owns
    /// it now): the waiter was starved, not the lock held for ever.
    Starved { takes: u64, waited_ms: u64 },
}

/// The verdict at the report: `start` as recorded at the first spin, `owner` and `owner_takes` (of the start owner's hart) now, and
/// the clock now. The takes counter is read without a lock from another hart: a hint, used for this report only.
pub fn spin_verdict(start: SpinStart, owner: u64, owner_takes: u64, now_ns: u64) -> SpinVerdict {
    let waited_ms = now_ns.saturating_sub(start.at_ns) / 1_000_000;
    let takes = owner_takes.saturating_sub(start.owner_takes);
    if owner == start.owner && takes == 0 {
        SpinVerdict::OneLongSection { waited_ms }
    } else {
        SpinVerdict::Starved { takes, waited_ms }
    }
}

impl core::fmt::Display for SpinVerdict {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            SpinVerdict::OneLongSection { waited_ms } => write!(
                f,
                "owner takes since: 0, waited {waited_ms} ms: one long section"
            ),
            SpinVerdict::Starved { takes, waited_ms } => write!(
                f,
                "owner takes since: {takes}, waited {waited_ms} ms: the lock changed hands, a starved waiter"
            ),
        }
    }
}
