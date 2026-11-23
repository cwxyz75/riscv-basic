//! The scheduler of SCHED-SMP-SPEC section 4, **stage one** (task 6): the new structure, with behaviour as today.
//!
//! What is new compared with [`crate::sched`]: one priority number orders everything and its **band** (idle,
//! normal 1 to 31, real-time 32 to 63) picks the class that queues the thread ([`band`], [`SchedAttr`]); every
//! thread has the spec's record ([`ThreadSched`]); time is in **nanoseconds**; the class interface is the spec's
//! (`enqueue(how)`, `dequeue(why)`, `pick`, `top`, `preempts`, `account`, `next_event`); and a hart's run queue,
//! [`HartRq`], hosts an RT class, a normal class and the hart's idle thread with **strict precedence in the core**
//! (`pick`: RT, then normal, then idle). The RT class is FIFO within a level (a preempted thread returns to the
//! front; optional round robin with `rr`); the normal class is today's fixed priority with a 10 ms round robin
//! slice. RT budgets are enforced (task 25): a hart that has run its RT band for `RT_HART_MAX` of a window gives the normal band the rest
//! of it when the normal band has work (NM1), and a job that has used its budget has its RT-base threads demoted to `DEMOTED_PRIO` until
//! the window ends. Donation exists as a field only.
//!
//! This stage runs **one hart's queue** (the model the differential test drives against the old scheduler
//! `sched::Sched`, which stays as the oracle: for priorities in the normal band the two must have the same
//! `current` after every operation). Wiring it into the kernel, the per-hart queues of several harts, push and pull
//! belong to the later tasks (6b, 11). Pure: no locks, a simulated clock.

use crate::sched::{State, Tid, PRIO_MAX};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

pub type Prio = u8;
pub type Time = u64;

pub const MS: Time = 1_000_000;
/// The default timer slack of a thread in the normal band (SCHED-SMP-SPEC 4.11): 50 microseconds; 0 in the real-time band; at most `MAX_SLACK`.
pub const DEFAULT_SLACK: Time = 50_000;
pub const MAX_SLACK: Time = MS;
/// The normal band's round robin slice (OD33's 10 ticks, now in nanoseconds).
pub const TIMESLICE: Time = 10 * MS;
/// RT window and limits (SCHED-SMP-SPEC 4.5); accounted, not enforced yet.
pub const RT_WINDOW: Time = 100 * MS;
pub const RT_HART_MAX: Time = 80 * MS;
pub const RT_RR_SLICE: Time = 10 * MS;
/// While a real-time-base thread runs, its hart accounts at least this often (the job's budget is shared by every hart).
pub const RT_ACCT_PERIOD: Time = 10 * MS;
pub const RT_ENFORCE: bool = true;
/// What a thread of a job that has used its RT budget counts as until the window ends (the top of the normal band, round robin).
pub const DEMOTED_PRIO: Prio = 31;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Band {
    Idle,
    Normal,
    Rt,
}

/// The band of a priority: 0 idle, 1 to 31 normal, 32 to 63 real-time.
pub fn band(p: Prio) -> Band {
    match p {
        0 => Band::Idle,
        1..=31 => Band::Normal,
        _ => Band::Rt,
    }
}

/// A thread's base scheduling attribute. `rr`: rotate among equals (real-time only; the normal band always does).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SchedAttr {
    pub prio: Prio,
    pub rr: bool,
}

/// Which hart holds a thread's record: an atomic that travels in the record, so that a hart that moves the record (a pull, a
/// migrating wake) updates it under the run-queue locks it holds, and a waker that holds no run-queue lock can read it (and
/// re-check it under the lock, L11 design note section 3.1). The thread table (`sched_smp`) maps each thread to its `Home`.
pub struct Home(crate::sync::AtomicU8);

impl Home {
    pub fn new(hart: usize) -> Home {
        Home(crate::sync::AtomicU8::new(hart as u8))
    }
    pub fn get(&self) -> usize {
        usize::from(self.0.load(crate::sync::Ordering::Acquire))
    }
    pub fn set(&self, hart: usize) {
        self.0
            .store(hart as u8, core::sync::atomic::Ordering::Release);
    }
}

impl core::fmt::Debug for Home {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Home({})", self.get())
    }
}

/// The per-thread record (SCHED-SMP-SPEC 4.3).
#[derive(Clone, Debug)]
pub struct ThreadSched {
    /// Shared with the thread table; set by `HartRq::attach` and at creation.
    pub home: alloc::sync::Arc<Home>,
    pub tid: Tid,
    pub base: SchedAttr,
    /// Copy of the donation graph's answer (`kcore::donate`, task 7); equal to `base.prio` until then.
    pub eff: Prio,
    /// 0: not demoted; else the id of the RT window it was demoted in, plus one.
    pub demoted_window: u64,
    /// The effective priority to go back to when the demotion ends.
    pub undemoted: Prio,
    pub state: State,
    pub affinity: Option<u8>,
    pub last_hart: u8,
    pub slice_left: Time,
    pub deadline: Option<Time>,
    /// The hard expiry of `deadline` (soft expiry plus the slack at the time it was set): the hardware timer is programmed to the
    /// earliest of these; every entry whose soft expiry has passed is expired in the same interrupt.
    pub timer_hard: Time,
    /// This thread's timer slack when it sleeps in the normal band (nanoseconds; 0 in the real-time band whatever this says).
    pub timer_slack: Time,
    pub runnable_since: Time,
    pub started: Time,
    pub cpu: Time,
    pub group: u32,
    pub job: u32,
    pub class_words: [u64; 4],
    /// The park word (SCHED-SMP-SPEC 3.7): whether a wake counts. (Plain here: this core runs under one lock; the
    /// atomic protocol is `kcore::park`.)
    pub park: crate::park::ParkState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Enq {
    New,
    Wake { slept: Time },
    Preempted,
    SliceEnd,
    Yield,
    Migrated,
    AttrChanged,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deq {
    Migrate,
    AttrChange,
    Gone,
}

/// One deque per level and a 64-bit occupancy mask (best level in one instruction), and an index from thread to level so
/// that `contains` is a lookup and `remove` touches one level.
pub struct PrioQueues {
    levels: Vec<VecDeque<Tid>>,
    mask: u64,
    at: BTreeMap<Tid, Prio>,
}

impl Default for PrioQueues {
    fn default() -> Self {
        Self::new()
    }
}

impl PrioQueues {
    pub fn new() -> PrioQueues {
        PrioQueues {
            levels: (0..=PRIO_MAX as usize).map(|_| VecDeque::new()).collect(),
            mask: 0,
            at: BTreeMap::new(),
        }
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: p <= PRIO_MAX = 63 and `levels` has 64 entries
    pub fn push_back(&mut self, p: Prio, t: Tid) {
        self.levels[p as usize].push_back(t);
        self.mask |= 1 << p;
        self.at.insert(t, p);
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: p <= PRIO_MAX = 63 and `levels` has 64 entries
    pub fn push_front(&mut self, p: Prio, t: Tid) {
        self.levels[p as usize].push_front(t);
        self.mask |= 1 << p;
        self.at.insert(t, p);
    }
    /// The best queued level.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the mask is non-zero, so leading_zeros <= 63
    pub fn top(&self) -> Option<Prio> {
        (self.mask != 0).then(|| 63 - self.mask.leading_zeros() as Prio)
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: p comes from the mask, so it is below 64
    pub fn pop_best(&mut self) -> Option<Tid> {
        let p = self.top()?;
        let t = self.levels[p as usize].pop_front();
        if self.levels[p as usize].is_empty() {
            self.mask &= !(1 << p);
        }
        if let Some(t) = t {
            self.at.remove(&t);
        }
        t
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: p <= PRIO_MAX = 63 and `levels` has 64 entries
    pub fn remove(&mut self, t: Tid) -> bool {
        let Some(p) = self.at.remove(&t) else {
            return false;
        };
        let level = &mut self.levels[p as usize];
        if let Some(i) = level.iter().position(|&x| x == t) {
            level.remove(i);
        }
        if level.is_empty() {
            self.mask &= !(1 << p);
        }
        true
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: p comes from the mask, so it is below 64
    pub fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)> {
        (0..=PRIO_MAX).rev().find_map(|p| {
            self.levels[p as usize]
                .iter()
                .find(|&&t| ok(t))
                .map(|&t| (t, p))
        })
    }
    pub fn contains(&self, t: Tid) -> bool {
        self.at.contains_key(&t)
    }
    pub fn len(&self) -> usize {
        self.at.len()
    }
    pub fn is_empty(&self) -> bool {
        self.mask == 0
    }
}

/// A band's class (SCHED-SMP-SPEC 4.3).
pub trait Class {
    fn name(&self) -> &'static str;
    /// `t` is runnable here. `Preempted` goes to the front of its level (keeping its slice), everything else to the back.
    fn enqueue(&mut self, t: &mut ThreadSched, how: Enq, now: Time);
    fn dequeue(&mut self, t: &mut ThreadSched, why: Deq, now: Time);
    /// Remove and return the best queued thread.
    fn pick(&mut self, now: Time) -> Option<Tid>;
    /// The effective priority of the best queued thread.
    fn top(&self) -> Option<Prio>;
    fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)>;
    /// Should a queued thread of this class replace `cur` right now?
    fn preempts(&self, cur: &ThreadSched, now: Time) -> bool;
    /// Would a thread of priority `cand`, queued here, displace a running thread of effective priority `cur_eff`?
    /// (Placement, push and pull ask this: moving a thread to a hart where it would only wait is churn.) Fixed priority:
    /// a higher number wins. A class that never preempts says no.
    fn displaces(&self, cand: Prio, cur_eff: Prio) -> bool {
        cand > cur_eff
    }
    /// Charge `ran` to the running thread; true: its slice is over and an equal or better peer waits.
    fn account(&mut self, cur: &mut ThreadSched, ran: Time, now: Time) -> bool;
    /// When this class next needs the hart's timer while `cur` runs (slice end), if at all.
    fn next_event(&self, cur: &ThreadSched, now: Time) -> Option<Time>;
    fn contains(&self, tid: Tid) -> bool;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn enqueue_common(q: &mut PrioQueues, t: &mut ThreadSched, how: Enq, slice: Time) {
    match how {
        Enq::Preempted => {
            t.slice_left = t.slice_left.max(1);
            q.push_front(t.eff, t.tid);
        }
        // a priority change keeps the slice the thread had (it moves to the back of its new level, not a new turn); so does a
        // migration: moving harts is not a new turn either
        Enq::AttrChanged | Enq::Migrated => {
            t.slice_left = t.slice_left.max(1);
            q.push_back(t.eff, t.tid);
        }
        _ => {
            t.slice_left = slice;
            q.push_back(t.eff, t.tid);
        }
    }
}

/// The normal band today: fixed priority, round robin within a level, a 10 ms slice (OD33).
pub struct FixedPrio {
    q: PrioQueues,
}

impl FixedPrio {
    pub fn new() -> FixedPrio {
        FixedPrio {
            q: PrioQueues::new(),
        }
    }
}

impl Default for FixedPrio {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for FixedPrio {
    fn name(&self) -> &'static str {
        "fixed-prio"
    }
    fn enqueue(&mut self, t: &mut ThreadSched, how: Enq, _now: Time) {
        enqueue_common(&mut self.q, t, how, TIMESLICE);
    }
    fn dequeue(&mut self, t: &mut ThreadSched, _why: Deq, _now: Time) {
        self.q.remove(t.tid);
    }
    fn pick(&mut self, _now: Time) -> Option<Tid> {
        self.q.pop_best()
    }
    fn top(&self) -> Option<Prio> {
        self.q.top()
    }
    fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)> {
        self.q.top_where(ok)
    }
    fn preempts(&self, cur: &ThreadSched, _now: Time) -> bool {
        self.q.top().is_some_and(|p| p > cur.eff)
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: cpu time and slices are nanosecond counts far below u64::MAX within a boot
    fn account(&mut self, cur: &mut ThreadSched, ran: Time, _now: Time) -> bool {
        cur.cpu += ran;
        cur.slice_left = cur.slice_left.saturating_sub(ran);
        if cur.slice_left > 0 {
            return false;
        }
        if self.q.top().is_some_and(|p| p >= cur.eff) {
            true
        } else {
            cur.slice_left = TIMESLICE;
            false
        }
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: now plus a slice is far below u64::MAX
    fn next_event(&self, cur: &ThreadSched, now: Time) -> Option<Time> {
        // (a slice end matters only when an equal or better peer waits: otherwise the thread would just get a new slice)
        self.q
            .top()
            .filter(|&p| p >= cur.eff)
            .map(|_| now + cur.slice_left)
    }
    fn contains(&self, tid: Tid) -> bool {
        self.q.contains(tid)
    }
    fn len(&self) -> usize {
        self.q.len()
    }
}

/// The normal band as plain round robin: the queue is one FIFO whatever the priorities (a second policy, to show classes
/// are pluggable and the conformance suite is about every one of them). It still reports the best queued priority, so
/// placement and pull can see what waits; it never preempts and it rotates when a slice ends and anything waits.
pub struct RoundRobin {
    q: alloc::collections::VecDeque<(Tid, Prio)>,
}

impl RoundRobin {
    pub fn new() -> RoundRobin {
        RoundRobin {
            q: alloc::collections::VecDeque::new(),
        }
    }
}

impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for RoundRobin {
    fn name(&self) -> &'static str {
        "round-robin"
    }
    fn enqueue(&mut self, t: &mut ThreadSched, how: Enq, _now: Time) {
        match how {
            Enq::Preempted => {
                t.slice_left = t.slice_left.max(1);
                self.q.push_front((t.tid, t.eff));
            }
            Enq::AttrChanged | Enq::Migrated => {
                t.slice_left = t.slice_left.max(1);
                self.q.push_back((t.tid, t.eff));
            }
            _ => {
                t.slice_left = TIMESLICE;
                self.q.push_back((t.tid, t.eff));
            }
        }
    }
    fn dequeue(&mut self, t: &mut ThreadSched, _why: Deq, _now: Time) {
        self.q.retain(|&(x, _)| x != t.tid);
    }
    fn pick(&mut self, _now: Time) -> Option<Tid> {
        self.q.pop_front().map(|(t, _)| t)
    }
    fn top(&self) -> Option<Prio> {
        self.q.iter().map(|&(_, p)| p).max()
    }
    fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)> {
        self.q
            .iter()
            .filter(|&&(t, _)| ok(t))
            .max_by_key(|&&(_, p)| p)
            .map(|&(t, p)| (t, p))
    }
    fn preempts(&self, _cur: &ThreadSched, _now: Time) -> bool {
        false
    }
    fn displaces(&self, _cand: Prio, _cur_eff: Prio) -> bool {
        false // round robin never preempts: a waiting thread gets its turn at a slice end
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: cpu time and slices are nanosecond counts far below u64::MAX within a boot
    fn account(&mut self, cur: &mut ThreadSched, ran: Time, _now: Time) -> bool {
        cur.cpu += ran;
        cur.slice_left = cur.slice_left.saturating_sub(ran);
        if cur.slice_left > 0 {
            return false;
        }
        if self.q.is_empty() {
            cur.slice_left = TIMESLICE;
            false
        } else {
            true
        }
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: now plus a slice is far below u64::MAX
    fn next_event(&self, cur: &ThreadSched, now: Time) -> Option<Time> {
        (!self.q.is_empty()).then(|| now + cur.slice_left)
    }
    fn contains(&self, tid: Tid) -> bool {
        self.q.iter().any(|&(t, _)| t == tid)
    }
    fn len(&self) -> usize {
        self.q.len()
    }
}

/// The real-time band: fixed priority 32 to 63, FIFO within a level; a preempted thread returns to the front of its
/// level; with `rr` a thread also rotates every `RT_RR_SLICE` when an equal-priority peer waits.
pub struct RtClass {
    q: PrioQueues,
}

impl RtClass {
    pub fn new() -> RtClass {
        RtClass {
            q: PrioQueues::new(),
        }
    }
}

impl Default for RtClass {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for RtClass {
    fn name(&self) -> &'static str {
        "rt"
    }
    fn enqueue(&mut self, t: &mut ThreadSched, how: Enq, _now: Time) {
        enqueue_common(&mut self.q, t, how, RT_RR_SLICE);
    }
    fn dequeue(&mut self, t: &mut ThreadSched, _why: Deq, _now: Time) {
        self.q.remove(t.tid);
    }
    fn pick(&mut self, _now: Time) -> Option<Tid> {
        self.q.pop_best()
    }
    fn top(&self) -> Option<Prio> {
        self.q.top()
    }
    fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)> {
        self.q.top_where(ok)
    }
    fn preempts(&self, cur: &ThreadSched, _now: Time) -> bool {
        self.q.top().is_some_and(|p| p > cur.eff)
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: cpu time and slices are nanosecond counts far below u64::MAX within a boot
    fn account(&mut self, cur: &mut ThreadSched, ran: Time, _now: Time) -> bool {
        cur.cpu += ran;
        if !cur.base.rr {
            return false; // FIFO: runs until it blocks, yields or is preempted
        }
        cur.slice_left = cur.slice_left.saturating_sub(ran);
        if cur.slice_left > 0 {
            return false;
        }
        if self.q.top().is_some_and(|p| p >= cur.eff) {
            true
        } else {
            cur.slice_left = RT_RR_SLICE;
            false
        }
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: now plus a slice is far below u64::MAX
    fn next_event(&self, cur: &ThreadSched, now: Time) -> Option<Time> {
        (cur.base.rr && self.q.top().is_some_and(|p| p >= cur.eff)).then(|| now + cur.slice_left)
    }
    fn contains(&self, tid: Tid) -> bool {
        self.q.contains(tid)
    }
    fn len(&self) -> usize {
        self.q.len()
    }
}

/// Shared-queue mode (task 6b): the two bands as one [`crate::sched::SchedClass`], so the kernel's multi-hart core
/// (`sched::Sched`, one set of queues for all harts, a per-hart current thread) runs the spec's band structure until
/// per-hart queues (task 11). Real-time (32 to 63) above normal (0 to 31, idle at 0) with strict precedence; the
/// normal band rotates at the end of a slice as before; a **real-time thread never rotates** (FIFO within its level:
/// it runs until it blocks, yields or is preempted by a higher level; the round-robin flag arrives with the per-hart
/// classes).
pub struct TwoBand {
    rt: PrioQueues,
    normal: PrioQueues,
}

impl Default for TwoBand {
    fn default() -> Self {
        Self::new()
    }
}

impl TwoBand {
    pub fn new() -> TwoBand {
        TwoBand {
            rt: PrioQueues::new(),
            normal: PrioQueues::new(),
        }
    }

    fn best_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<Prio> {
        let rt = self.rt.top_where(ok).map(|(_, p)| p);
        rt.or_else(|| self.normal.top_where(ok).map(|(_, p)| p))
    }
}

impl crate::sched::SchedClass for TwoBand {
    fn name(&self) -> &'static str {
        "fixed-priority"
    }
    fn enqueue_tail(&mut self, tid: Tid, prio: Prio) {
        match band(prio) {
            Band::Rt => self.rt.push_back(prio, tid),
            _ => self.normal.push_back(prio, tid),
        }
    }
    fn enqueue_head(&mut self, tid: Tid, prio: Prio) {
        match band(prio) {
            Band::Rt => self.rt.push_front(prio, tid),
            _ => self.normal.push_front(prio, tid),
        }
    }
    fn pick_next_where(&mut self, ok: &dyn Fn(Tid) -> bool) -> Option<Tid> {
        if let Some((t, _)) = self.rt.top_where(ok) {
            self.rt.remove(t);
            return Some(t);
        }
        let (t, _) = self.normal.top_where(ok)?;
        self.normal.remove(t);
        Some(t)
    }
    fn remove(&mut self, tid: Tid) {
        if !self.rt.remove(tid) {
            self.normal.remove(tid);
        }
    }
    fn contains(&self, tid: Tid) -> bool {
        self.rt.contains(tid) || self.normal.contains(tid)
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: queue lengths are bounded by the thread limit
    fn len(&self) -> usize {
        self.rt.len() + self.normal.len()
    }
    fn would_preempt_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool {
        self.best_where(ok).is_some_and(|p| p > running)
    }
    fn would_rotate_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool {
        // a real-time thread runs until it blocks, yields or is preempted: no slice rotation
        band(running) != Band::Rt && self.best_where(ok).is_some_and(|p| p >= running)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    NoSuchThread,
    NoCurrent,
    BadPriority,
    NotExited,
    /// The thread id counter reached its limit (CNT1: a counter never wraps; the creation is refused).
    Exhausted,
    /// A hart cannot go offline: it is the last online hart, or a thread is pinned to it (PM1).
    BadState,
}

/// One hart's run queue: an RT class, a normal class and the hart's idle thread, strict precedence in `pick`.
pub struct HartRq<N: Class> {
    rt: RtClass,
    normal: N,
    idle: Option<Tid>,
    cur: Option<Tid>,
    /// The hart is offline (PM1): it runs only its idle thread, nothing queued here would run here, and what is homed here is moved away.
    offline: bool,
    threads: BTreeMap<Tid, ThreadSched>,
    now: Time,
    timers: BTreeSet<(Time, Tid)>,
    /// The same entries ordered by hard expiry (soft expiry plus slack): the first is when the hardware timer must fire.
    hard_timers: BTreeSet<(Time, Tid)>,
    next_tid: Tid,
    pub bad_picks: u32,
    /// Where the running thread's job is charged (SCHED-SMP-SPEC 4.10): the table is shared by every hart's queue.
    jobs: Option<alloc::sync::Arc<crate::jobsched::JobSchedTable>>,
    /// The RT window (`now / RT_WINDOW`) the figures below belong to, and the RT-band time this hart has run in it (NM1's throttle).
    win: u64,
    rt_ran: Time,
    /// Threads demoted here in the window `win`: the window's end puts them back.
    demoted: BTreeSet<Tid>,
}

impl<N: Class> HartRq<N> {
    pub fn new(normal: N) -> HartRq<N> {
        HartRq {
            rt: RtClass::new(),
            normal,
            idle: None,
            cur: None,
            offline: false,
            threads: BTreeMap::new(),
            now: 0,
            timers: BTreeSet::new(),
            hard_timers: BTreeSet::new(),
            next_tid: 1,
            bad_picks: 0,
            jobs: None,
            win: 0,
            rt_ran: 0,
            demoted: BTreeSet::new(),
        }
    }

    /// Charge every thread's CPU time to its job in `table` from now on.
    pub fn set_job_table(&mut self, table: alloc::sync::Arc<crate::jobsched::JobSchedTable>) {
        self.jobs = Some(table);
    }

    /// Which job a thread's time is charged to (a kernel thread keeps job 0, the root's).
    pub fn set_job(&mut self, tid: Tid, job: u32) -> Result<(), Error> {
        self.threads.get_mut(&tid).ok_or(Error::NoSuchThread)?.job = job;
        Ok(())
    }

    /// When the budgets next need this hart's timer (4.5): the end of the window while anything is throttled or demoted (to clear it),
    /// and, while an RT thread runs, the moment this hart's RT share would be spent with the normal band waiting, and the moment the
    /// thread's job would use its budget.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: now plus a window is far below u64::MAX
    fn enforce_event(&self) -> Option<Time> {
        if !RT_ENFORCE {
            return None;
        }
        let window_end = (self.now / RT_WINDOW + 1) * RT_WINDOW;
        let mut ev: Option<Time> = None;
        let mut put = |t: Time| ev = Some(ev.map_or(t, |e| e.min(t)));
        if self.rt_blocked() || !self.demoted.is_empty() {
            put(window_end);
        }
        if let Some(t) = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .and_then(|c| self.threads.get(&c))
            .filter(|t| band(t.eff) == Band::Rt)
        {
            if !self.normal.is_empty()
                && self.win == self.now / RT_WINDOW
                && self.rt_ran < RT_HART_MAX
            {
                put((self.now + (RT_HART_MAX - self.rt_ran)).min(window_end));
            }
            if band(t.base.prio) == Band::Rt {
                if let Some(left) = self.jobs.as_ref().and_then(|j| j.rt_left(t.job, self.now)) {
                    // (other harts drain the same budget and this hart cannot see them: look again every `RT_ACCT_PERIOD`, so an exhausted job
                    // is found within that, on every hart that runs one of its real-time threads)
                    put((self.now + left.clamp(1, RT_ACCT_PERIOD)).min(window_end));
                }
            }
        }
        ev
    }

    /// Is the RT band held back on this hart right now (the throttle of 4.5)?
    pub fn rt_throttled(&self) -> bool {
        self.rt_blocked()
    }

    /// Is the RT band held back on this hart? It has run `RT_HART_MAX` of the window and the normal band has a thread to run
    /// (SCHED-SMP-SPEC 4.5, NM1): work-conserving, so with the normal band empty RT runs on.
    fn rt_blocked(&self) -> bool {
        RT_ENFORCE
            && self.win == self.now / RT_WINDOW
            && self.rt_ran >= RT_HART_MAX
            && self.normal_has_work()
    }

    /// A thread of the normal band is queued here or is the one running (a hart that has just switched to it must not hand the hart back).
    fn normal_has_work(&self) -> bool {
        !self.normal.is_empty()
            || self
                .cur
                .filter(|&c| Some(c) != self.idle)
                .and_then(|c| self.threads.get(&c))
                .is_some_and(|t| band(t.eff) == Band::Normal)
    }

    /// Time moved on by `dt` (now `self.now`); `ran` is the thread that ran for it, if any (not the idle thread). Rolls the RT window, adds
    /// the hart's RT time (by effective band) and demotes the thread if its job has used its budget. Returns whether the designation may
    /// now be wrong.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: nanosecond counts far below u64::MAX within a boot
    fn enforce_tick(&mut self, ran: Option<Tid>, dt: Time) -> bool {
        if !RT_ENFORCE {
            return false;
        }
        let w = self.now / RT_WINDOW;
        let mut changed = false;
        // (the part of the interval that lies in the window `now` is in)
        let share = dt.min(self.now - w * RT_WINDOW);
        if w != self.win {
            self.win = w;
            self.rt_ran = 0;
            changed = self.restore_demoted();
        }
        let Some(tid) = ran else {
            return changed;
        };
        // (one lookup for the common thread, which has nothing to do with real-time budgets)
        let Some((rt, involved)) = self.threads.get(&tid).map(|t| {
            let rt = band(t.eff) == Band::Rt;
            (rt, rt || band(t.base.prio) == Band::Rt)
        }) else {
            return changed;
        };
        if !involved {
            return changed;
        }
        if rt {
            self.rt_ran += share;
            if self.rt_blocked() {
                changed = true; // the running RT thread must give way to the normal band
            }
        }
        match self.threads.get(&tid).map(|t| t.state) {
            Some(State::Running) => {
                if let Some(mut t) = self.threads.remove(&tid) {
                    changed |= self.demote_if_exhausted(&mut t);
                    self.threads.insert(tid, t);
                }
            }
            Some(State::Runnable)
                if self
                    .threads
                    .get(&tid)
                    .is_some_and(|t| self.is_exhausted_rt(t)) =>
            {
                // (queued: a requeue demotes it on the way in)
                self.class_dequeue(tid, Deq::AttrChange);
                self.class_enqueue(tid, Enq::AttrChanged);
            }
            _ => {}
        }
        changed
    }

    /// Is `t` a real-time-base thread, queued or running in the RT band, whose job has used its budget in this window?
    fn is_exhausted_rt(&self, t: &ThreadSched) -> bool {
        t.demoted_window == 0
            && band(t.base.prio) == Band::Rt
            && band(t.eff) == Band::Rt
            && self
                .jobs
                .as_ref()
                .is_some_and(|j| j.exhausted(t.job, self.now))
    }

    /// Demote `t` (a thread that is running or about to be queued) if its base is real-time and its job has used its budget in this
    /// window. The thread is not in a class queue. Returns whether it was demoted.
    fn demote_if_exhausted(&mut self, t: &mut ThreadSched) -> bool {
        let Some(j) = self.jobs.as_ref() else {
            return false;
        };
        if t.demoted_window != 0 || band(t.base.prio) != Band::Rt || band(t.eff) != Band::Rt {
            return false;
        }
        if !j.exhausted(t.job, self.now) {
            return false;
        }
        t.undemoted = t.eff;
        t.eff = DEMOTED_PRIO;
        t.demoted_window = (self.now / RT_WINDOW).saturating_add(1);
        t.slice_left = TIMESLICE;
        self.demoted.insert(t.tid);
        true
    }

    /// Put a thread back after its window is over (a lazy check at enqueue, like the spec says).
    fn undemote_if_over(&self, t: &mut ThreadSched) {
        if t.demoted_window != 0 && t.demoted_window != (self.now / RT_WINDOW).saturating_add(1) {
            t.eff = t.undemoted.max(t.base.prio);
            t.demoted_window = 0;
        }
    }

    /// The window rolled: every thread demoted here goes back to its priority (queued ones move class). Returns whether anything did.
    #[allow(clippy::expect_used)] // INVARIANT: a tid in `demoted` was looked up in the same iteration
    fn restore_demoted(&mut self) -> bool {
        let tids: Vec<Tid> = core::mem::take(&mut self.demoted).into_iter().collect();
        let mut any = false;
        for tid in tids {
            let Some(t) = self.threads.get(&tid) else {
                continue;
            };
            if t.demoted_window == 0 {
                continue;
            }
            let state = t.state;
            if state == State::Runnable {
                self.class_dequeue(tid, Deq::AttrChange);
            }
            let t = self.threads.get_mut(&tid).expect("exists");
            t.eff = t.undemoted.max(t.base.prio);
            t.demoted_window = 0;
            if state == State::Runnable {
                self.class_enqueue(tid, Enq::AttrChanged);
            }
            any = true;
        }
        any
    }

    fn charge_job(&self, t: &ThreadSched, ran: Time) {
        if let Some(j) = &self.jobs {
            // (real-time time is what a thread of real-time **base** priority runs *in the band*: a demoted thread runs as a normal one, and
            // counting it would keep the figure growing for the rest of the window after the budget is spent)
            j.charge(
                t.job,
                ran,
                band(t.base.prio) == Band::Rt && t.demoted_window == 0,
                self.now,
            );
        }
    }

    pub fn now(&self) -> Time {
        self.now
    }

    /// The thread that should be running now (the idle thread when nothing else is runnable).
    pub fn current(&self) -> Option<Tid> {
        self.cur
    }

    pub fn state(&self, tid: Tid) -> Option<State> {
        self.threads.get(&tid).map(|t| t.state)
    }

    pub fn record(&self, tid: Tid) -> Option<&ThreadSched> {
        self.threads.get(&tid)
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn fresh(&mut self, attr: SchedAttr, state: State) -> Result<Tid, Error> {
        if attr.prio > PRIO_MAX {
            return Err(Error::BadPriority);
        }
        let tid = self.next_tid;
        self.next_tid = tid.checked_add(1).ok_or(Error::Exhausted)?;
        self.threads.insert(
            tid,
            ThreadSched {
                home: alloc::sync::Arc::new(Home::new(0)),
                tid,
                base: attr,
                eff: attr.prio,
                demoted_window: 0,
                undemoted: 0,
                state,
                affinity: None,
                last_hart: 0,
                slice_left: TIMESLICE,
                deadline: None,
                timer_hard: 0,
                timer_slack: DEFAULT_SLACK,
                runnable_since: self.now,
                started: self.now,
                cpu: 0,
                group: 0,
                job: 0,
                class_words: [0; 4],
                park: crate::park::ParkState::Idle,
            },
        );
        Ok(tid)
    }

    /// The hart's idle thread: it runs when nothing else can, is never queued, and is pinned here.
    #[allow(clippy::expect_used)] // INVARIANT: the idle thread was just created
    pub fn adopt_idle(&mut self) -> Tid {
        let tid = self
            .fresh(SchedAttr { prio: 0, rr: false }, State::Running)
            .expect("priority 0 is valid");
        self.idle = Some(tid);
        self.cur = Some(tid);
        tid
    }

    #[allow(clippy::expect_used, clippy::unreachable)] // INVARIANT: the thread was found in `threads` by every caller before this call
    fn class_enqueue(&mut self, tid: Tid, how: Enq) {
        let mut t = self.threads.remove(&tid).expect("thread exists");
        if RT_ENFORCE {
            self.undemote_if_over(&mut t);
            self.demote_if_exhausted(&mut t);
            if t.demoted_window != 0 {
                self.demoted.insert(tid); // (it may have come from another hart: this one ends the demotion at the window's end)
            }
        }
        match band(t.eff) {
            Band::Rt => self.rt.enqueue(&mut t, how, self.now),
            Band::Normal => self.normal.enqueue(&mut t, how, self.now),
            Band::Idle => unreachable!("the idle thread is never queued"),
        }
        self.threads.insert(tid, t);
    }

    #[allow(clippy::expect_used)] // INVARIANT: the thread was found in `threads` by every caller before this call
    fn class_dequeue(&mut self, tid: Tid, why: Deq) {
        let mut t = self.threads.remove(&tid).expect("thread exists");
        match band(t.eff) {
            Band::Rt => self.rt.dequeue(&mut t, why, self.now),
            Band::Normal => self.normal.dequeue(&mut t, why, self.now),
            Band::Idle => {}
        }
        self.threads.insert(tid, t);
    }

    /// Strict precedence (core mechanism, not policy): RT, then normal, then idle. Rejects a pick that is not a
    /// runnable thread (the guard against a faulty class).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a counter of bad picks (a test statistic)
    fn pick(&mut self) {
        debug_assert!(self.cur.is_none() || self.cur == self.idle);
        if self.offline {
            self.cur = self.idle;
            return;
        }
        loop {
            let picked = if self.rt_blocked() {
                None
            } else {
                self.rt.pick(self.now)
            }
            .or_else(|| self.normal.pick(self.now));
            let Some(tid) = picked else {
                self.cur = self.idle;
                return;
            };
            match self.threads.get_mut(&tid) {
                Some(t) if t.state == State::Runnable => {
                    t.state = State::Running;
                    t.started = self.now;
                    self.cur = Some(tid);
                    return;
                }
                _ => self.bad_picks += 1,
            }
        }
    }

    /// Anything queued that should run instead of the current thread? (An idle current is replaced by anything.)
    #[allow(clippy::indexing_slicing)] // INVARIANT: `c` is the current thread, which has a record
    fn better_waits(&self) -> bool {
        let Some(c) = self.cur else {
            return true;
        };
        if self.offline {
            return Some(c) != self.idle; // (an offline hart displaces what runs and picks nothing but its idle thread)
        }
        let blocked = self.rt_blocked();
        if Some(c) == self.idle {
            return (!self.rt.q.is_empty() && !blocked) || self.normal.top().is_some();
        }
        let t = &self.threads[&c];
        if blocked && band(t.eff) == Band::Rt {
            return true; // the throttle: the normal band has work and this hart's RT share is spent
        }
        (!blocked && self.rt.preempts(t, self.now)) || self.normal.preempts(t, self.now)
    }

    /// Make the designation right after something changed: dispatch if there is no current thread, preempt the
    /// current one (to the front of its level, keeping its slice) if something better waits.
    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    fn reschedule(&mut self) {
        if !self.better_waits() {
            return;
        }
        if let Some(c) = self.cur.filter(|&c| Some(c) != self.idle) {
            let t = self.threads.get_mut(&c).expect("current exists");
            t.state = State::Runnable;
            self.cur = None;
            self.class_enqueue(c, Enq::Preempted);
        }
        self.cur = None;
        self.pick();
    }

    pub fn spawn(&mut self, attr: SchedAttr) -> Result<Tid, Error> {
        let tid = self.fresh(attr, State::Runnable)?;
        self.class_enqueue(tid, Enq::New);
        self.reschedule();
        Ok(tid)
    }

    pub fn wake(&mut self, tid: Tid) -> Result<(), Error> {
        let t = self.threads.get_mut(&tid).ok_or(Error::NoSuchThread)?;
        match t.state {
            State::Blocked => {
                if let Some(d) = t.deadline.take() {
                    self.timers.remove(&(d, tid));
                    self.hard_timers.remove(&(t.timer_hard, tid));
                }
                let slept = self.now.saturating_sub(t.started);
                t.park = crate::park::ParkState::Idle;
                t.state = State::Runnable;
                t.runnable_since = self.now;
                self.class_enqueue(tid, Enq::Wake { slept });
                self.reschedule();
            }
            // counts only for a prepared thread; nothing is remembered otherwise
            State::Running | State::Runnable => {
                if t.park == crate::park::ParkState::Prepared {
                    t.park = crate::park::ParkState::Notified;
                }
            }
            State::Exited => {}
        }
        Ok(())
    }

    /// The current thread is about to wait: a wake now counts (`prepare` before registering, as in 3.7).
    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn prepare_block(&mut self) -> Result<(), Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        self.threads.get_mut(&c).expect("current exists").park = crate::park::ParkState::Prepared;
        Ok(())
    }

    /// The condition was already true: not waiting after all (a standing preparation or notification is dropped).
    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn cancel_block(&mut self) -> Result<(), Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        self.threads.get_mut(&c).expect("current exists").park = crate::park::ParkState::Idle;
        Ok(())
    }

    /// Block the current thread unless a notification is waiting (then it is consumed). Returns whether it blocked.
    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn block_current(&mut self) -> Result<bool, Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        if t.park == crate::park::ParkState::Notified {
            t.park = crate::park::ParkState::Idle;
            return Ok(false);
        }
        t.park = crate::park::ParkState::Prepared;
        t.state = State::Blocked;
        self.cur = None;
        self.pick();
        Ok(true)
    }

    /// Sleep until `deadline` (a passed deadline does not block and leaves nothing prepared).
    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn sleep_until(&mut self, deadline: Time) -> Result<bool, Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        if t.park == crate::park::ParkState::Notified {
            t.park = crate::park::ParkState::Idle;
            return Ok(false); // a wake already arrived after `prepare_block`
        }
        t.park = crate::park::ParkState::Prepared;
        if deadline <= self.now {
            t.park = crate::park::ParkState::Idle;
            return Ok(false);
        }
        t.deadline = Some(deadline);
        let slack = if band(t.eff) == Band::Rt {
            0
        } else {
            t.timer_slack.min(MAX_SLACK)
        };
        t.timer_hard = deadline.saturating_add(slack);
        self.timers.insert((deadline, c));
        self.hard_timers.insert((t.timer_hard, c));
        self.block_current()
    }

    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn yield_now(&mut self) -> Result<(), Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        self.threads.get_mut(&c).expect("current exists").state = State::Runnable;
        self.cur = None;
        self.class_enqueue(c, Enq::Yield);
        self.pick();
        Ok(())
    }

    #[allow(clippy::expect_used)] // INVARIANT: `c` is the current thread, which has a record
    pub fn exit_current(&mut self) -> Result<(), Error> {
        let c = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .ok_or(Error::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        t.state = State::Exited;
        t.park = crate::park::ParkState::Idle;
        self.cur = None;
        self.pick();
        Ok(())
    }

    pub fn reap(&mut self, tid: Tid) -> Result<(), Error> {
        match self.threads.get(&tid) {
            Some(t) if t.state == State::Exited => {
                self.threads.remove(&tid);
                Ok(())
            }
            Some(_) => Err(Error::NotExited),
            None => Err(Error::NoSuchThread),
        }
    }

    /// Change a thread's base priority (its band may change; `eff` follows until donation exists).
    #[allow(clippy::expect_used)] // INVARIANT: the thread was looked up by the caller in the same call
    pub fn set_priority(&mut self, tid: Tid, prio: Prio) -> Result<(), Error> {
        if prio > PRIO_MAX {
            return Err(Error::BadPriority);
        }
        let t = self.threads.get(&tid).ok_or(Error::NoSuchThread)?;
        let (state, old) = (t.state, t.eff);
        if state == State::Runnable && old != prio {
            self.class_dequeue(tid, Deq::AttrChange);
        }
        let t = self.threads.get_mut(&tid).expect("exists");
        t.base.prio = prio;
        t.eff = prio;
        t.demoted_window = 0; // (a new priority ends a demotion; the budget demotes again if it must)
        self.demoted.remove(&tid);
        match state {
            State::Runnable if old != prio => {
                self.class_enqueue(tid, Enq::AttrChanged);
                self.reschedule();
            }
            State::Running => self.reschedule(),
            _ => {}
        }
        Ok(())
    }

    /// Set a thread's *effective* priority (what donation, `kcore::donate`, computes) without touching its base: a
    /// queued thread moves to where it now belongs (possibly into the other band's class), the running one may now
    /// be preempted, and the base is what it falls back to. Returns whether the thread is known.
    #[allow(clippy::expect_used)] // INVARIANT: the thread was looked up by the caller in the same call
    pub fn set_effective(&mut self, tid: Tid, eff: Prio) -> bool {
        let Some(t) = self.threads.get(&tid) else {
            return false;
        };
        if eff > PRIO_MAX || t.eff == eff {
            return t.eff == eff;
        }
        let state = t.state;
        if state == State::Runnable {
            self.class_dequeue(tid, Deq::AttrChange);
        }
        let t = self.threads.get_mut(&tid).expect("exists");
        t.eff = eff;
        t.demoted_window = 0; // (a donation ends a demotion: the donor's priority is what counts, and the budget demotes again if it must)
        self.demoted.remove(&tid);
        match state {
            State::Runnable => {
                self.class_enqueue(tid, Enq::AttrChanged);
                self.reschedule();
            }
            State::Running => self.reschedule(),
            _ => {}
        }
        true
    }

    /// When this hart's one-shot hardware timer must fire next (SCHED-SMP-SPEC 4.11, KERNEL-V1-SPEC 5.1): the earliest **hard** expiry of
    /// a sleeper's deadline, or the running thread's slice end when an equal or better peer waits; `None` is "never" (a hart with one
    /// runnable thread and no deadline takes no timer interrupt). Absolute nanoseconds on the hart's clock. The caller programs the
    /// hardware to it whenever it may have changed (after an interrupt, a switch, an enqueue).
    pub fn next_event(&self) -> Option<Time> {
        let timer = self.hard_timers.first().map(|&(h, _)| h);
        let slice = self
            .cur
            .filter(|&c| Some(c) != self.idle)
            .and_then(|c| self.threads.get(&c))
            .and_then(|t| match band(t.eff) {
                Band::Rt => self.rt.next_event(t, self.now),
                _ => self.normal.next_event(t, self.now),
            });
        let timer_slice = match (timer, slice) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (timer_slice, self.enforce_event()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Bring the hart's clock to `now` and charge the elapsed time to the thread that is running, without firing timers or changing any
    /// designation. Tickless kernels (L13) call it before every change of designation, so a switch between two interrupts charges each
    /// thread what it used; the rotation decision itself waits for the next timer event.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the nanosecond clock is far below u64::MAX within a boot
    /// Returns whether the budgets now want this hart to look at its queues (the throttle began, a thread was demoted, a window rolled).
    pub fn charge(&mut self, now: Time) -> bool {
        let dt = now.saturating_sub(self.now);
        if dt == 0 {
            return false;
        }
        self.now += dt;
        if let Some(c) = self.cur.filter(|&c| Some(c) != self.idle) {
            if let Some(mut t) = self.threads.remove(&c) {
                t.cpu += dt;
                t.slice_left = t.slice_left.saturating_sub(dt);
                self.charge_job(&t, dt);
                self.threads.insert(c, t);
            }
        }
        let ran = self.cur.filter(|&c| Some(c) != self.idle);
        self.enforce_tick(ran, dt)
    }

    /// Set a thread's timer slack (nanoseconds, clamped to `MAX_SLACK`); it applies to its next sleep.
    pub fn set_timer_slack(&mut self, tid: Tid, slack: Time) -> Result<(), Error> {
        self.threads
            .get_mut(&tid)
            .ok_or(Error::NoSuchThread)?
            .timer_slack = slack.min(MAX_SLACK);
        Ok(())
    }

    /// Advance the clock by `dt`: fire the timers that are due, account the running thread's slice (rotating it
    /// when its slice is over and an equal or better peer waits), then make the designation right.
    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the nanosecond clock is far below u64::MAX within a boot
    pub fn advance(&mut self, dt: Time) {
        // The thread that ran during this tick: it is the one whose slice the tick uses up, even when a timer wake below
        // preempts it. (Charging whoever is current *after* the wakes made a thread that is woken at every tick, such as
        // the interactive boot's serial bridge, pay for every tick, so the thread it preempted never used up its slice and
        // its equal-priority peers never ran: T127.)
        let ran = self.cur.filter(|&c| Some(c) != self.idle);
        self.now += dt;
        self.enforce_tick(None, dt); // (the window rolls before anything is queued)
        let due: Vec<(Time, Tid)> = self.timers.range(..(self.now + 1, 0)).copied().collect();
        for (d, tid) in due {
            self.timers.remove(&(d, tid));
            if !self.threads.contains_key(&tid) {
                // (a thread reaped with its timer pending: its hard entry must not outlive it, or the hardware timer would be armed for
                // a time that is already past for ever)
                self.hard_timers.retain(|&(_, x)| x != tid);
            }
            if let Some(t) = self.threads.get_mut(&tid) {
                self.hard_timers.remove(&(t.timer_hard, tid));
                t.deadline = None;
                if t.state == State::Blocked {
                    t.park = crate::park::ParkState::Idle;
                    t.state = State::Runnable;
                    t.runnable_since = self.now;
                    self.class_enqueue(tid, Enq::Wake { slept: 0 });
                }
            }
        }
        // (v1 fires the timers and makes the designation right for them; the slice is accounted to the thread that ran)
        self.reschedule();
        if let Some(c) = ran {
            let now = self.now;
            let mut t = self.threads.remove(&c).expect("the thread that ran exists");
            let rotate = match band(t.eff) {
                Band::Rt => self.rt.account(&mut t, dt, now),
                _ => self.normal.account(&mut t, dt, now),
            };
            self.charge_job(&t, dt);
            self.threads.insert(c, t);
            if rotate {
                if self.cur == Some(c) {
                    self.threads.get_mut(&c).expect("current exists").state = State::Runnable;
                    self.cur = None;
                    self.class_enqueue(c, Enq::SliceEnd);
                    self.pick();
                } else if self
                    .threads
                    .get(&c)
                    .is_some_and(|t| t.state == State::Runnable)
                {
                    // preempted this very tick with its slice used up: it goes to the back of its level
                    self.class_dequeue(c, Deq::AttrChange);
                    self.class_enqueue(c, Enq::SliceEnd);
                }
            }
            self.enforce_tick(Some(c), dt);
        }
        self.reschedule();
    }

    // ---- what `kcore::sched_smp` (task 11) needs to run several of these as one machine: record migration, a quiet
    // priority change, queue views for placement and pull, and the same operations without the immediate reschedule
    // (a remote hart's current thread is changed only by that hart).

    /// A fresh thread record with a caller-chosen id (the multi-hart scheduler owns the id space).
    pub(crate) fn fresh_as(
        &mut self,
        tid: Tid,
        attr: SchedAttr,
        state: State,
        affinity: Option<u8>,
        last_hart: u8,
    ) -> Result<(), Error> {
        if attr.prio > PRIO_MAX {
            return Err(Error::BadPriority);
        }
        self.threads.insert(
            tid,
            ThreadSched {
                home: alloc::sync::Arc::new(Home::new(usize::from(last_hart))),
                tid,
                base: attr,
                eff: attr.prio,
                demoted_window: 0,
                undemoted: 0,
                state,
                affinity,
                last_hart,
                slice_left: TIMESLICE,
                deadline: None,
                timer_hard: 0,
                timer_slack: DEFAULT_SLACK,
                runnable_since: self.now,
                started: self.now,
                cpu: 0,
                group: 0,
                job: 0,
                class_words: [0; 4],
                park: crate::park::ParkState::Idle,
            },
        );
        Ok(())
    }

    /// This hart's idle thread, with a caller-chosen id.
    #[allow(clippy::expect_used)] // INVARIANT: the priority 0 record is always valid
    pub(crate) fn adopt_idle_as(&mut self, tid: Tid, hart: u8) {
        self.fresh_as(
            tid,
            SchedAttr { prio: 0, rr: false },
            State::Running,
            Some(hart),
            hart,
        )
        .expect("priority 0 is valid");
        self.idle = Some(tid);
        self.cur = Some(tid);
    }

    /// Adopt the code already running on this hart (the boot thread) as a normal thread.
    pub(crate) fn adopt_current_as(
        &mut self,
        tid: Tid,
        attr: SchedAttr,
        affinity: Option<u8>,
        hart: u8,
    ) -> Result<(), Error> {
        if self.cur.is_some_and(|c| Some(c) != self.idle) {
            return Err(Error::NoCurrent);
        }
        self.fresh_as(tid, attr, State::Running, affinity, hart)?;
        self.cur = Some(tid);
        Ok(())
    }

    pub(crate) fn idle_tid(&self) -> Option<Tid> {
        self.idle
    }
    pub(crate) fn tids(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads.keys().copied()
    }
    /// The effective priority of what runs here (0 for the idle thread).
    #[allow(clippy::indexing_slicing)] // INVARIANT: `c` is the current thread, which has a record
    pub(crate) fn cur_eff(&self) -> Prio {
        match self.cur {
            Some(c) if Some(c) != self.idle => self.threads[&c].eff,
            _ => 0,
        }
    }
    /// Would a thread of priority `cand` queued on this hart run instead of what runs now? An idle hart takes anything;
    /// otherwise the thread must outrank the current one and its band's class must be willing to preempt.
    pub(crate) fn would_run(&self, cand: Prio) -> bool {
        if self.offline {
            return false;
        }
        if band(cand) == Band::Rt && self.rt_blocked() {
            return false; // this hart's RT share is spent and the normal band has work (4.5)
        }
        if self.cur == self.idle {
            return cand > 0;
        }
        if band(cand) != Band::Rt
            && self.cur_eff() >= 32
            && self.win == self.now / RT_WINDOW
            && self.rt_ran >= RT_HART_MAX
            && !self.normal.is_empty()
            && RT_ENFORCE
        {
            return true; // the running real-time thread has spent this hart's share and the normal band has work here (NM1)
        }
        let cur = self.cur_eff();
        if cand <= cur {
            return false;
        }
        match band(cand) {
            Band::Rt => self.rt.displaces(cand, cur),
            _ => self.normal.displaces(cand, cur),
        }
    }
    /// What `would_run` depends on: whether the real-time band is held back, whether the hart is idle, the effective priority that runs, and
    /// whether a running real-time thread has spent the hart's share with a normal thread queued here (then any normal candidate takes it).
    fn would_run_state(&self) -> (bool, bool, Prio, bool) {
        let blocked = self.rt_blocked();
        let idle = self.cur == self.idle;
        let cur = self.cur_eff();
        let spent = !idle
            && RT_ENFORCE
            && cur >= 32
            && self.win == self.now / RT_WINDOW
            && self.rt_ran >= RT_HART_MAX
            && !self.normal.is_empty();
        (blocked, idle, cur, spent)
    }

    /// The state of `would_run_mask` packed into a number: the mask is the same whenever the key is (the classes' `displaces` are functions of
    /// the two priorities alone), so a hart that publishes after a change that left the key alone need not compute the 63 bits again.
    pub(crate) fn would_run_key(&self) -> u64 {
        let (blocked, idle, cur, spent) = self.would_run_state();
        u64::from(idle)
            | u64::from(cur) << 1
            | u64::from(blocked) << 9
            | u64::from(spent) << 10
            | u64::from(self.offline) << 11
    }

    /// `would_run(p)` for every priority 1 to 63 at once, bit `p` (what a hart publishes after every change): the state `would_run` looks at
    /// is read once, not 63 times.
    pub(crate) fn would_run_mask(&self) -> u64 {
        if self.offline {
            return 0;
        }
        let (blocked, idle, cur, spent) = self.would_run_state();
        let mut mask = 0u64;
        for p in 1..=PRIO_MAX {
            let rt = band(p) == Band::Rt;
            let yes = if rt && blocked {
                false
            } else if idle || (!rt && spent) {
                true
            } else if p <= cur {
                false
            } else if rt {
                self.rt.displaces(p, cur)
            } else {
                self.normal.displaces(p, cur)
            };
            if yes {
                mask |= 1 << p;
            }
        }
        mask
    }
    /// The best queued effective priority, if anything is queued here.
    pub(crate) fn queued_top(&self) -> Option<Prio> {
        if self.rt_blocked() {
            return self.normal.top(); // (the RT queue waits for the window to end: it is not what this hart will run next)
        }
        self.rt.top().or_else(|| self.normal.top())
    }
    /// The best queued thread that satisfies `ok`.
    pub(crate) fn queued_top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)> {
        self.rt.top_where(ok).or_else(|| self.normal.top_where(ok))
    }
    /// Set the clock without running timers (a hart joining the scheduler).
    pub(crate) fn set_now(&mut self, now: Time) {
        self.now = now;
    }
    pub(crate) fn is_queued(&self, tid: Tid) -> bool {
        self.rt.contains(tid) || self.normal.contains(tid)
    }
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: queue lengths are bounded by the thread limit
    pub(crate) fn queued_len(&self) -> usize {
        self.rt.len() + self.normal.len()
    }
    pub(crate) fn wants_resched(&self) -> bool {
        self.better_waits()
    }
    /// Make the designation right (dispatch, or preempt for something better that waits). Only this hart calls it.
    pub(crate) fn resched(&mut self) {
        self.reschedule();
    }

    /// Queue a thread whose record is already here and that is `Runnable`.
    pub(crate) fn enqueue_runnable(&mut self, tid: Tid, how: Enq) {
        self.class_enqueue(tid, how);
    }

    /// Take a queued thread (Runnable) off this hart's queues, record and all, to move it to another hart.
    pub(crate) fn detach_queued(&mut self, tid: Tid) -> Option<ThreadSched> {
        if !self.is_queued(tid) {
            return None;
        }
        self.class_dequeue(tid, Deq::Migrate);
        self.threads.remove(&tid)
    }

    /// Take a blocked thread's record (cancelling its timer), made `Runnable`, to place it on another hart. It is not
    /// queued anywhere; the caller attaches it. Returns the record and how long it slept.
    #[allow(clippy::expect_used)] // INVARIANT: the record was found by the lookup just above
    pub(crate) fn detach_woken(&mut self, tid: Tid) -> Result<(ThreadSched, Time), Error> {
        let t = self.threads.get_mut(&tid).ok_or(Error::NoSuchThread)?;
        if t.state != State::Blocked {
            return Err(Error::NoCurrent);
        }
        if let Some(d) = t.deadline.take() {
            self.timers.remove(&(d, tid));
            self.hard_timers.remove(&(t.timer_hard, tid));
        }
        let slept = self.now.saturating_sub(t.started);
        t.park = crate::park::ParkState::Idle;
        t.state = State::Runnable;
        t.runnable_since = self.now;
        Ok((self.threads.remove(&tid).expect("exists"), slept))
    }

    /// The records homed here that can move away now (PM1): queued runnable threads and blocked ones, but not the idle thread, the running thread and the threads
    /// `skip` names (a thread in transit, whose switch is not done).
    pub(crate) fn movable(&self, skip: &dyn Fn(Tid) -> bool) -> Vec<(Tid, State)> {
        self.threads
            .values()
            .filter(|t| {
                Some(t.tid) != self.idle
                    && Some(t.tid) != self.cur
                    && !skip(t.tid)
                    && ((t.state == State::Runnable && self.is_queued(t.tid))
                        || t.state == State::Blocked)
            })
            .map(|t| (t.tid, t.state))
            .collect()
    }

    /// Take a blocked thread's record, timers cancelled, to move it to another hart (PM1); the deadline stays in the record.
    pub(crate) fn detach_blocked(&mut self, tid: Tid) -> Option<ThreadSched> {
        let t = self.threads.get(&tid)?;
        if t.state != State::Blocked {
            return None;
        }
        if let Some(d) = t.deadline {
            self.timers.remove(&(d, tid));
            self.hard_timers.remove(&(t.timer_hard, tid));
        }
        self.threads.remove(&tid)
    }

    /// Put a blocked thread's record here with its timers (PM1: the other end of `detach_blocked`). A deadline already passed fires at the next advance.
    pub(crate) fn attach_blocked(&mut self, mut rec: ThreadSched, hart: u8) {
        rec.last_hart = hart;
        rec.home.set(usize::from(hart));
        let tid = rec.tid;
        if let Some(d) = rec.deadline {
            self.timers.insert((d, tid));
            self.hard_timers.insert((rec.timer_hard, tid));
        }
        self.threads.insert(tid, rec);
    }

    pub(crate) fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    /// How many records are homed here besides the idle thread's (PM1: an offline hart is empty when this is zero).
    pub(crate) fn homed_count(&self) -> usize {
        self.threads
            .keys()
            .filter(|&&t| Some(t) != self.idle)
            .count()
    }

    /// Is a thread other than the idle thread pinned to this hart? (PM1: it makes going offline fail.)
    pub(crate) fn has_pinned(&self, hart: usize) -> bool {
        self.threads.values().any(|t| {
            Some(t.tid) != self.idle
                && t.affinity.is_some_and(|a| a as usize == hart)
                && t.state != State::Exited
        })
    }

    /// Put a record here and queue it.
    pub(crate) fn attach(&mut self, mut rec: ThreadSched, how: Enq, hart: u8) {
        rec.last_hart = hart;
        rec.home.set(usize::from(hart));
        let tid = rec.tid;
        self.threads.insert(tid, rec);
        self.class_enqueue(tid, how);
    }

    /// `wake` without the reschedule, for a thread whose record is here (the caller does the designation or marks the
    /// hart). Returns whether it was made runnable (it was blocked); a prepared one is notified.
    pub(crate) fn wake_quiet(&mut self, tid: Tid) -> Result<bool, Error> {
        let t = self.threads.get_mut(&tid).ok_or(Error::NoSuchThread)?;
        match t.state {
            State::Blocked => {
                if let Some(d) = t.deadline.take() {
                    self.timers.remove(&(d, tid));
                    self.hard_timers.remove(&(t.timer_hard, tid));
                }
                let slept = self.now.saturating_sub(t.started);
                t.park = crate::park::ParkState::Idle;
                t.state = State::Runnable;
                t.runnable_since = self.now;
                self.class_enqueue(tid, Enq::Wake { slept });
                Ok(true)
            }
            State::Running | State::Runnable => {
                if t.park == crate::park::ParkState::Prepared {
                    t.park = crate::park::ParkState::Notified;
                }
                Ok(false)
            }
            State::Exited => Ok(false),
        }
    }

    /// `set_priority` without the reschedule: returns whether this hart's designation may now be wrong.
    #[allow(clippy::expect_used)] // INVARIANT: the thread was looked up in the same call
    pub(crate) fn set_priority_quiet(&mut self, tid: Tid, prio: Prio) -> Result<bool, Error> {
        if prio > PRIO_MAX {
            return Err(Error::BadPriority);
        }
        let t = self.threads.get(&tid).ok_or(Error::NoSuchThread)?;
        let (state, old) = (t.state, t.eff);
        if state == State::Runnable && old != prio {
            self.class_dequeue(tid, Deq::AttrChange);
        }
        let t = self.threads.get_mut(&tid).expect("exists");
        t.base.prio = prio;
        t.eff = prio;
        t.demoted_window = 0; // (a new priority ends a demotion; the budget demotes again if it must)
        self.demoted.remove(&tid);
        Ok(match state {
            State::Runnable if old != prio => {
                self.class_enqueue(tid, Enq::AttrChanged);
                true
            }
            State::Running => true,
            _ => false,
        })
    }

    /// `set_effective` without the reschedule (donation, L23): returns whether this hart's designation may now be wrong.
    #[allow(clippy::expect_used)] // INVARIANT: the thread was looked up in the same call
    pub(crate) fn set_effective_quiet(&mut self, tid: Tid, eff: Prio) -> Result<bool, Error> {
        if eff > PRIO_MAX {
            return Err(Error::BadPriority);
        }
        let t = self.threads.get(&tid).ok_or(Error::NoSuchThread)?;
        let (state, old) = (t.state, t.eff);
        if old == eff {
            return Ok(false);
        }
        if state == State::Runnable {
            self.class_dequeue(tid, Deq::AttrChange);
        }
        let t = self.threads.get_mut(&tid).expect("exists");
        t.eff = eff;
        t.demoted_window = 0; // (a donation ends a demotion: the donor's priority is what counts, and the budget demotes again if it must)
        self.demoted.remove(&tid);
        Ok(match state {
            State::Runnable => {
                self.class_enqueue(tid, Enq::AttrChanged);
                true
            }
            State::Running => true,
            _ => false,
        })
    }

    /// The timers and the slice accounting of `advance`, up to `now`, without the final designation by a remote caller
    /// (this is the hart's own tick). Same as `advance`; the name says whose call it is.
    pub(crate) fn tick_to(&mut self, now: Time) {
        let dt = now.saturating_sub(self.now);
        self.advance(dt);
    }

    /// The per-hart rules (the single-hart form of GP2): every thread is in exactly one place.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a count of running threads, at most the thread count
    pub fn audit(&self) -> Result<(), String> {
        let mut running = 0;
        for (&tid, t) in &self.threads {
            if Some(tid) == self.idle {
                continue; // the idle thread is never queued and runs when nothing else does
            }
            let queued = self.rt.contains(tid) || self.normal.contains(tid);
            let timed = self.timers.iter().filter(|&&(_, x)| x == tid).count();
            match t.state {
                State::Running => {
                    running += 1;
                    if self.cur != Some(tid) || queued || timed != 0 {
                        return Err(alloc::format!("running thread {tid} is also elsewhere"));
                    }
                }
                State::Runnable if !queued => {
                    return Err(alloc::format!("runnable thread {tid} is not queued"));
                }
                State::Blocked if queued => {
                    return Err(alloc::format!("blocked thread {tid} is queued"));
                }
                State::Blocked if timed > 1 || (timed == 1) != t.deadline.is_some() => {
                    return Err(alloc::format!("blocked thread {tid}: timer bookkeeping"));
                }
                State::Exited if queued || timed != 0 => {
                    return Err(alloc::format!(
                        "exited thread {tid} is still queued or timed"
                    ));
                }
                _ => {}
            }
        }
        let want = usize::from(self.cur != self.idle);
        if running != want {
            return Err(alloc::format!("{running} running threads, expected {want}"));
        }
        if self.bad_picks != 0 {
            return Err(alloc::format!("{} bad picks", self.bad_picks));
        }
        Ok(())
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    /// `would_run_mask` is `would_run` for every priority (the publish of a hart's hints reads the state once instead of 63 times).
    #[test]
    fn the_mask_is_would_run_for_every_priority() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..300 {
            let mut h = HartRq::new(FixedPrio::new());
            h.adopt_idle();
            for _ in 0..(next() % 6) {
                let _ = h.spawn(SchedAttr {
                    prio: 1 + (next() % 63) as u8,
                    rr: false,
                });
            }
            // run for a while so the real-time share can be spent
            for _ in 0..(next() % 120) {
                h.advance(MS);
            }
            let mask = h.would_run_mask();
            for p in 1..=PRIO_MAX {
                assert_eq!(h.would_run(p), mask & (1 << p) != 0, "priority {p}");
            }
        }
    }
}
