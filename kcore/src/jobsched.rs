//! Per-job CPU accounting (SCHED-SMP-SPEC 4.10, task 8): what each job's threads ran, in nanoseconds, and how much of that was
//! real-time-band time in the current 100 ms window (`rt_used`, the number 4.5's budget is compared with; enforcement is task 25).
//!
//! One record per job slot, all atomics: a hart charges the running thread's job on the switch and timer paths **without a lock**
//! (the scheduler's own lock is the hart's run queue). `cpu` only grows. `rt_used` is packed with the window it belongs to in one word, so that
//! a charge at a window boundary is a single compare-and-swap: two harts charging the same job across the boundary cannot lose an
//! addition or keep one from the old window (the window id has 28 bits, 310 days at 100 ms; a longer boot saturates it, which only
//! stops the used figure from restarting).

use crate::job::MAX_JOBS;
use crate::sched2::{Time, RT_WINDOW};
use crate::sync::{AtomicU64, Ordering};
use alloc::vec::Vec;

const USED_BITS: u32 = 36; // up to 68 s of RT time in a window: 64 harts at 100 ms is 6.4 s
const USED_MASK: u64 = (1 << USED_BITS) - 1;
const WINDOW_MAX: u64 = (1 << (64 - USED_BITS)) - 1;

struct JobSched {
    /// Bumped when the slot is reused: a thread of the earlier job that is charged late carries the old generation and is dropped.
    generation: AtomicU64,
    cpu: AtomicU64,
    /// `window << USED_BITS | used`.
    rt: AtomicU64,
}

/// The accounting records of every job slot.
pub struct JobSchedTable {
    recs: Vec<JobSched>,
}

impl Default for JobSchedTable {
    fn default() -> Self {
        Self::new()
    }
}

impl JobSchedTable {
    pub fn new() -> JobSchedTable {
        JobSchedTable {
            recs: (0..MAX_JOBS)
                .map(|_| JobSched {
                    generation: AtomicU64::new(0),
                    cpu: AtomicU64::new(0),
                    rt: AtomicU64::new(0),
                })
                .collect(),
        }
    }

    fn rec(&self, job: u32) -> Option<&JobSched> {
        self.recs.get(job as usize)
    }

    /// What a thread of the job in slot `job` keeps as its `ThreadSched.job`: the slot and the slot's generation now (8 bits of slot,
    /// `MAX_JOBS` is 256). A charge whose tag is not the slot's current one belongs to a job that has gone and is dropped.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the generation is masked to 24 bits and the slot is below 256
    pub fn tag(&self, job: u32) -> u32 {
        let g = self.rec(job).map_or(0, |r| {
            r.generation.load(Ordering::Relaxed) as u32 & 0xff_ffff
        });
        (g << 8) | (job & 0xff)
    }

    /// A job slot is (re)used: start from nothing (and charges tagged before this are dropped).
    pub fn reset(&self, job: u32) {
        if let Some(r) = self.rec(job) {
            r.generation.fetch_add(1, Ordering::Relaxed);
            r.cpu.store(0, Ordering::Relaxed);
            r.rt.store(0, Ordering::Relaxed);
        }
    }

    /// A thread of `job` ran for `ran` nanoseconds that ended at `now`; `rt_base`: its **base** priority is in the real-time band
    /// (time run on a donated priority is not the job's, 4.5). The part of the interval before the window that `now` is in belongs
    /// to the window that is over and is not counted in `rt_used`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: nanosecond counts far below u64::MAX within a boot; the window id is clamped
    pub fn charge(&self, tag: u32, ran: Time, rt_base: bool, now: Time) {
        let Some(r) = self.rec(tag & 0xff) else {
            return;
        };
        if r.generation.load(Ordering::Relaxed) as u32 & 0xff_ffff != tag >> 8 {
            return; // the job this thread belonged to is gone and the slot is someone else's
        }
        r.cpu.fetch_add(ran, Ordering::Relaxed);
        if !rt_base || ran == 0 {
            return;
        }
        let window = (now / RT_WINDOW).min(WINDOW_MAX);
        let inside = ran.min(now - now / RT_WINDOW * RT_WINDOW);
        let mut cur = r.rt.load(Ordering::Relaxed);
        loop {
            let (w, used) = (cur >> USED_BITS, cur & USED_MASK);
            if w > window {
                return; // a charge for a window that is already over (this hart's clock reads a little behind another's): dropped
            }
            // (the part before the boundary belongs to the window that is over: nothing reads it again)
            let kept = if w == window { used + inside } else { inside };
            let next = (window << USED_BITS) | kept.min(USED_MASK);
            match r
                .rt
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    /// The job's total CPU time, nanoseconds (its own threads only; a subtree is summed by the caller).
    pub fn cpu(&self, job: u32) -> u64 {
        self.rec(job).map_or(0, |r| r.cpu.load(Ordering::Relaxed))
    }

    /// RT-band nanoseconds the job's threads have run in the window that `now` is in.
    pub fn rt_used(&self, job: u32, now: Time) -> u64 {
        let Some(r) = self.rec(job) else { return 0 };
        let v = r.rt.load(Ordering::Relaxed);
        if v >> USED_BITS == (now / RT_WINDOW).min(WINDOW_MAX) {
            v & USED_MASK
        } else {
            0
        }
    }
}
