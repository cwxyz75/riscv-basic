//! The pause detector's rule (T284): how a timing test tells a pause of the host from a delay of the kernel.
//!
//! A bound on host time fails when the host does not run an emulated hart: QEMU's `time` counter follows the host clock, so a
//! hart that was not run for 20 ms finds its timer interrupt 20 ms late, and a wake it should have made is that late too. The kernel
//! can see this happen: every hart programs its own one-shot timer (Sstc `stimecmp`), and when the interrupt is taken the
//! difference between the `time` counter and the value programmed is how late the hart took it. An interrupt late by more than a
//! test's own bound is a **pause**: the host did not run the hart, or the hart ran with interrupts off for that long.
//!
//! Each hart keeps the pauses it saw in a small ring ([`HartRing`]), written only by that hart with interrupts off and read by any;
//! one slot is one word ([`pack`]), so a reader never sees half a sample. A test measures in **windows** (one sleep of a latency
//! probe, say): a window that a pause overlaps ([`covers`]) says nothing about the kernel and is taken again, and [`Resampler`]
//! decides, window by window, whether to keep a sample, take it again, or give up: three covered windows in a row are a failure (a
//! host that never runs the hart is still a failure, and so is a kernel that holds interrupts off at every wake), and a window with
//! no pause is judged by the test's bound as before.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Samples kept per hart: the newest win.
pub const RING: usize = 128;
/// Latenesses below this (microseconds) are not kept: they are the emulator's ordinary jitter, far below any test's bound, and
/// keeping them would push the pauses out of the ring.
pub const FLOOR_US: u64 = 200;
/// How many covered windows in a row make a test fail ([`Resampler`]).
pub const MAX_COVERED_IN_A_ROW: u32 = 3;

const LATE_BITS: u32 = 24;
const LATE_MAX: u64 = (1 << LATE_BITS) - 1;
const ARRIVAL_MAX: u64 = (1 << (64 - LATE_BITS)) - 1;

/// One sample as one word: when the interrupt was taken (microseconds since boot, 40 bits: about 12 days) and how late
/// (microseconds, 24 bits: about 16 s; longer saturates). 0 is the empty slot (no interrupt is taken at time 0 and 0 late).
pub fn pack(arrival_us: u64, late_us: u64) -> u64 {
    (arrival_us.min(ARRIVAL_MAX) << LATE_BITS) | late_us.min(LATE_MAX)
}

/// The inverse of [`pack`]: (arrival, late), microseconds.
pub fn unpack(word: u64) -> (u64, u64) {
    (word >> LATE_BITS, word & LATE_MAX)
}

/// Whether a timer interrupt taken at `arrival_us`, `late_us` late, overlaps the window `[start_us, end_us]`: the hart was not
/// taking its interrupt from `arrival - late` to `arrival`, and that span meets the window.
pub fn covers(arrival_us: u64, late_us: u64, start_us: u64, end_us: u64) -> bool {
    arrival_us >= start_us && arrival_us.saturating_sub(late_us) <= end_us
}

/// One hart's pauses. One writer (the hart, in its timer interrupt, interrupts off); readers on any hart.
pub struct HartRing {
    slots: [AtomicU64; RING],
    next: AtomicUsize,
}

impl Default for HartRing {
    fn default() -> Self {
        Self::new()
    }
}

impl HartRing {
    pub const fn new() -> HartRing {
        HartRing {
            slots: [const { AtomicU64::new(0) }; RING],
            next: AtomicUsize::new(0),
        }
    }

    /// Record a timer interrupt taken at `arrival_us`, `late_us` late: kept when it is at least [`FLOOR_US`] late. Only the owning
    /// hart calls this.
    pub fn record(&self, arrival_us: u64, late_us: u64) {
        if late_us < FLOOR_US {
            return;
        }
        let i = self.next.load(Ordering::Relaxed) % RING;
        if let Some(slot) = self.slots.get(i) {
            slot.store(pack(arrival_us, late_us), Ordering::Release);
        }
        self.next.store(i.wrapping_add(1) % RING, Ordering::Release);
    }

    /// How long (microseconds) the kept samples say the hart was not taking its interrupt inside `[start_us, end_us]`: the sum of
    /// each sample's span `[arrival - late, arrival]` clipped to the window.
    pub fn paused_in(&self, start_us: u64, end_us: u64) -> u64 {
        self.slots
            .iter()
            .map(|s| s.load(Ordering::Acquire))
            .filter(|&w| w != 0)
            .map(unpack)
            .map(|(at, late)| overlap(at.saturating_sub(late), at, start_us, end_us))
            .fold(0u64, u64::saturating_add)
    }

    /// The largest lateness (microseconds) of a kept sample that overlaps `[start_us, end_us]`, or 0.
    pub fn worst_in(&self, start_us: u64, end_us: u64) -> u64 {
        self.slots
            .iter()
            .map(|s| s.load(Ordering::Acquire))
            .filter(|&w| w != 0)
            .map(unpack)
            .filter(|&(at, late)| covers(at, late, start_us, end_us))
            .map(|(_, late)| late)
            .max()
            .unwrap_or(0)
    }
}

/// A hart writes `new` to `stimecmp` at counter value `now` (all in counter cycles; `u64::MAX`: nothing armed), and `old` was what it
/// had armed and not yet taken. Returns (when the interrupt it now waits for is due, how long the old one is overdue if this write
/// cancels it). A future value is due then. A value already past raises the interrupt at once, so the hardware is late only from
/// `now`, the moment it was written; but if the old interrupt was already due and is still pending, it has been due since `old`, and
/// a re-arm must not hide that (a hart back from a pause re-arms the deadline it missed before it takes the interrupt). A write that
/// moves a due interrupt into the future, or turns it off, cancels it: the hart never took it, and it is overdue by `now - old` (the
/// pause detector keeps that as a sample, like an interrupt taken that late).
pub fn rearm(old: u64, new: u64, now: u64) -> (u64, u64) {
    let pending = old != u64::MAX && old <= now;
    if new == u64::MAX || new > now {
        (new, if pending { now.saturating_sub(old) } else { 0 })
    } else if pending {
        (old, 0)
    } else {
        (now, 0)
    }
}

/// The worst pause over several harts' rings in a window: (hart, lateness in microseconds), or None when no kept sample overlaps it.
pub fn worst_of<'a>(
    rings: impl IntoIterator<Item = &'a HartRing>,
    start_us: u64,
    end_us: u64,
) -> Option<(usize, u64)> {
    rings
        .into_iter()
        .enumerate()
        .map(|(h, r)| (h, r.worst_in(start_us, end_us)))
        .filter(|&(_, late)| late > 0)
        .max_by_key(|&(_, late)| late)
}

/// The pause time of several harts' rings inside a window, summed (microseconds): what a test that bills by the clock must allow
/// for, since a hart that was not run was still charged to the thread it was running.
pub fn paused_total<'a>(
    rings: impl IntoIterator<Item = &'a HartRing>,
    start_us: u64,
    end_us: u64,
) -> u64 {
    rings
        .into_iter()
        .map(|r| r.paused_in(start_us, end_us))
        .fold(0u64, u64::saturating_add)
}

/// The length of `[a0, a1]` inside `[b0, b1]` (0 when they do not meet).
pub fn overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> u64 {
    a1.min(b1).saturating_sub(a0.max(b0))
}

/// What `debug_stat` 216 answers: 0 for no pause, else `(hart << 48) | late_us` (the lateness saturates at 48 bits).
pub fn encode(found: Option<(usize, u64)>) -> u64 {
    match found {
        None => 0,
        Some((h, late)) => ((h as u64 & 0xffff) << 48) | late.clamp(1, (1 << 48) - 1),
    }
}

/// The inverse of [`encode`].
pub fn decode(word: u64) -> Option<(usize, u64)> {
    (word != 0).then_some(((word >> 48) as usize, word & ((1 << 48) - 1)))
}

/// What to do with one window of a timing test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No pause covered it: the sample counts, judged by the test's bound.
    Keep,
    /// A pause of the host covered it (hart, microseconds late): take the window again.
    Retake { hart: usize, late_us: u64 },
    /// [`MAX_COVERED_IN_A_ROW`] windows in a row were covered: the test fails (the last pause: hart, microseconds).
    GiveUp { hart: usize, late_us: u64 },
}

/// The re-sampling rule, one window at a time: a window covered by a pause later than the test's own bound is taken again, at most
/// [`MAX_COVERED_IN_A_ROW`] in a row; a kept window starts the count again.
#[derive(Clone, Copy, Debug, Default)]
pub struct Resampler {
    in_a_row: u32,
    /// Windows taken again so far.
    pub retaken: u32,
    /// The largest pause that made a window be taken again: (hart, microseconds).
    pub worst: Option<(usize, u64)>,
}

impl Resampler {
    pub const fn new() -> Resampler {
        Resampler {
            in_a_row: 0,
            retaken: 0,
            worst: None,
        }
    }

    /// Judge a window: `pause` is the worst pause that overlapped it ([`worst_of`]), `bound_us` the test's own bound. Only a pause
    /// later than the bound covers a window: a smaller one could not have failed it on its own.
    pub fn judge(&mut self, pause: Option<(usize, u64)>, bound_us: u64) -> Verdict {
        match pause {
            Some((hart, late_us)) if late_us > bound_us => {
                self.in_a_row = self.in_a_row.saturating_add(1);
                if self.worst.is_none_or(|(_, w)| late_us > w) {
                    self.worst = Some((hart, late_us));
                }
                if self.in_a_row >= MAX_COVERED_IN_A_ROW {
                    Verdict::GiveUp { hart, late_us }
                } else {
                    self.retaken = self.retaken.saturating_add(1);
                    Verdict::Retake { hart, late_us }
                }
            }
            _ => {
                self.in_a_row = 0;
                Verdict::Keep
            }
        }
    }
}

/// The value of `hoststall=<ms>:<period ms>` on the kernel line (the harness adds it when `OS_HOST_STALL` is set): (ms, period), or
/// None for anything else (both whole numbers, both positive).
pub fn parse_stall_arg(value: &str) -> Option<(u64, u64)> {
    let (ms, period) = value.split_once(':')?;
    let (ms, period) = (ms.parse::<u64>().ok()?, period.parse::<u64>().ok()?);
    (ms > 0 && period > 0).then_some((ms, period))
}
