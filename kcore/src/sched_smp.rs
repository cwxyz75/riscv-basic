//! The multi-hart scheduler of SCHED-SMP-SPEC section 4.4 (task 11, pure part): one [`HartRq`] per hart, each with its
//! own queues, **placement** by global priority, **push** marks and **pull**.
//!
//! The kernel wraps each `HartRq` in a lock of class `RQ`; this type is the single value the model tests drive, written
//! so that every operation touches the caller's hart or names the other hart it touches (the shape the locks take).
//! The rules it keeps:
//!
//! * `cur[h]` is changed only by hart `h` (`reschedule_on`): a wake, spawn or priority change that finds something
//!   better for another hart *marks* it (`resched_pending`, one poke owed) and leaves its designation alone.
//! * **Placement** (`select_hart`): a pinned thread goes to its hart; else its last hart if that hart's current thread
//!   is lower; else the hart with the lowest current priority if lower; else it queues on its last hart.
//! * **Push**: whenever a thread is left queued and some other hart runs something lower that may run it, that hart is
//!   marked. **Pull**: a hart about to pick first takes the best queued thread of any other hart that outranks both its
//!   own current thread and its own queue top (and may run here).
//! * **GP1** (at quiescence: no mark pending): no hart runs a thread while a higher-priority thread that may run there
//!   is queued anywhere. **GP2**: every thread is in exactly one place. [`Sched2::audit`] checks both.
//!
//! Pure: no locks, a simulated clock (`tick_on`).

use crate::sched::{State, Tid, PRIO_MAX};
use crate::sched2::{Class, Enq, Error, HartRq, Home, Prio, SchedAttr, ThreadSched, Time};
use crate::sync::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};
use core::ops::{Deref, DerefMut};

/// The most harts (the poke set is a bitmask).
pub const MAX_HARTS: usize = 64;

/// The thread table (the spec's `TT`): each thread's `Home`.
pub type Table = BTreeMap<Tid, Arc<Home>>;

/// What `Sched2` needs of the locks around the per-hart run queues (L11b). `lock(x)` takes hart `x`'s run-queue lock (class `RQ`,
/// instance `x`); the scheduler takes two only in ascending order, and never takes `table` while it holds one. The kernel
/// implements this with `klock::Spin`; [`Local`] with `RefCell`s, where taking a lock that is already held (a deadlock in the
/// kernel) panics, so the single-threaded model tests check the lock choreography too.
pub trait Rqs<N: Class> {
    type Guard<'a>: DerefMut<Target = HartRq<N>>
    where
        Self: 'a;
    type TableGuard<'a>: DerefMut<Target = Table>
    where
        Self: 'a;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn lock(&self, x: usize) -> Self::Guard<'_>;
    fn table(&self) -> Self::TableGuard<'_>;
}

/// The run queues of one scheduler value, for the model tests.
pub struct Local<N: Class> {
    rq: Vec<RefCell<HartRq<N>>>,
    table: RefCell<Table>,
}

impl<N: Class> Local<N> {
    pub fn new(harts: usize, make: impl Fn() -> N) -> Local<N> {
        Local {
            rq: (0..harts)
                .map(|_| RefCell::new(HartRq::new(make())))
                .collect(),
            table: RefCell::new(Table::new()),
        }
    }
}

impl<N: Class> Rqs<N> for Local<N> {
    type Guard<'a>
        = RefMut<'a, HartRq<N>>
    where
        Self: 'a;
    type TableGuard<'a>
        = RefMut<'a, Table>
    where
        Self: 'a;
    fn len(&self) -> usize {
        self.rq.len()
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count (callers pass harts they were given)
    fn lock(&self, x: usize) -> Self::Guard<'_> {
        self.rq[x].borrow_mut()
    }
    fn table(&self) -> Self::TableGuard<'_> {
        self.table.borrow_mut()
    }
}

/// What the other harts may read about hart `h` without its lock: published when `h`'s lock is released after a change, **read
/// only as a hint** whose every use is re-checked under the locks (SCHED-SMP-SPEC 4.4).
struct Hint {
    /// The effective priority of what runs (0: idle).
    cur_eff: AtomicU8,
    /// The length of the queues.
    queued_len: AtomicUsize,
    /// Bit `p` (1 to 63): a thread of priority `p` queued here would run instead of what runs now (`HartRq::would_run`).
    would_run: AtomicU64,
}

impl Hint {
    fn new() -> Hint {
        Hint {
            cur_eff: AtomicU8::new(0),
            queued_len: AtomicUsize::new(0),
            would_run: AtomicU64::new(0),
        }
    }
    fn publish<N: Class>(&self, rq: &HartRq<N>) {
        self.cur_eff.store(rq.cur_eff(), Ordering::Release);
        self.queued_len.store(rq.queued_len(), Ordering::Release);
        let mut mask = 0u64;
        for p in 1..=PRIO_MAX {
            if rq.would_run(p) {
                mask |= 1 << p;
            }
        }
        self.would_run.store(mask, Ordering::Release);
    }
}

/// A run-queue guard that publishes the hart's hints when it is released after a change (a `DerefMut` marks it changed).
struct Hq<'a, N: Class, R: Rqs<N> + 'a> {
    g: R::Guard<'a>,
    hint: &'a Hint,
    changed: bool,
}

impl<N: Class, R: Rqs<N>> Deref for Hq<'_, N, R> {
    type Target = HartRq<N>;
    fn deref(&self) -> &HartRq<N> {
        &self.g
    }
}
impl<N: Class, R: Rqs<N>> DerefMut for Hq<'_, N, R> {
    fn deref_mut(&mut self) -> &mut HartRq<N> {
        self.changed = true;
        &mut self.g
    }
}
impl<N: Class, R: Rqs<N>> Drop for Hq<'_, N, R> {
    fn drop(&mut self) {
        if self.changed {
            self.hint.publish(&self.g);
        }
    }
}

/// The scheduler: a thread table, the shared atomics, and the per-hart run queues behind `R`'s locks. Every method takes `&self`:
/// the kernel calls them from several harts at once, each holding at most the locks the method says. `R = Local` is the one-value
/// form the model tests drive.
pub struct Sched2<N: Class, R: Rqs<N> = Local<N>> {
    rqs: R,
    next_tid: AtomicU32,
    /// `resched_pending[h]`: set by whoever owes hart `h` one poke (compare-and-swap clear to set).
    pending: Vec<AtomicBool>,
    /// Harts that are part of the scheduler: only they are chosen to run or hold threads (a hart that has not joined
    /// yet is skipped by placement and by push). Every hart starts online; the kernel clears the others until they join.
    online: Vec<AtomicBool>,
    hint: Vec<Hint>,
    /// `transit[h]`: the thread (0: none) hart `h` has just been told to stop running and has queued (preempted, rotated, yielded) but
    /// whose context the kernel has not finished saving. It is not available to any other hart (no pull, no balance, no
    /// push) until `switched(h)`: the spec's "the switch completes under the lock" (3.7), for kernels whose lock is not held
    /// across the switch.
    transit: Vec<AtomicU32>,
    /// Deferred mode (SCHED-SMP-SPEC X7: a wake never switches): a wake, a spawn or a priority change that would make the caller's own
    /// hart look at its queues marks it (`pending`) instead of settling at once, so `current_on` stays the thread that is running
    /// until the hart's next defined point calls `reschedule_on`. The kernel sets it; the pure models run both ways.
    deferred: AtomicBool,
    /// The time source of a tickless kernel (L13): when set, every settle first charges the hart's running thread up to it.
    clock: Option<fn() -> Time>,
    pokes: AtomicU64,
    pulls: AtomicU64,
    settles: AtomicU64,
    marks: AtomicU64,
    _n: core::marker::PhantomData<N>,
}

fn allowed(rec: &ThreadSched, hart: usize) -> bool {
    rec.affinity.is_none_or(|a| a as usize == hart)
}

impl<N: Class> Sched2<N, Local<N>> {
    /// `harts` run queues (1 to [`MAX_HARTS`]), each with a normal class made by `make`. Every hart starts with its idle
    /// thread running (`idle(h)` gives its id).
    pub fn new(harts: usize, make: impl Fn() -> N) -> Sched2<N, Local<N>> {
        assert!((1..=MAX_HARTS).contains(&harts), "1 to 64 harts");
        Sched2::over(Local::new(harts, make))
    }
}

impl<N: Class, R: Rqs<N>> Sched2<N, R> {
    /// The scheduler over run queues that already exist (`rqs.len()` harts), each given its idle thread.
    #[allow(clippy::expect_used)] // INVARIANT: the record was just created on this hart
    pub fn over(rqs: R) -> Sched2<N, R> {
        let harts = rqs.len();
        assert!((1..=MAX_HARTS).contains(&harts), "1 to 64 harts");
        let s = Sched2 {
            rqs,
            next_tid: AtomicU32::new(1),
            pending: (0..harts).map(|_| AtomicBool::new(false)).collect(),
            online: (0..harts).map(|_| AtomicBool::new(true)).collect(),
            hint: (0..harts).map(|_| Hint::new()).collect(),
            transit: (0..harts).map(|_| AtomicU32::new(0)).collect(),
            deferred: AtomicBool::new(false),
            clock: None,
            pokes: AtomicU64::new(0),
            pulls: AtomicU64::new(0),
            settles: AtomicU64::new(0),
            marks: AtomicU64::new(0),
            _n: core::marker::PhantomData,
        };
        for h in 0..harts {
            let tid = s.alloc_tid();
            let home = {
                let mut q = s.lq(h);
                q.adopt_idle_as(tid, h as u8);
                q.record(tid).expect("just made").home.clone()
            };
            s.rqs.table().insert(tid, home);
        }
        s
    }

    fn alloc_tid(&self) -> Tid {
        self.next_tid.fetch_add(1, Ordering::Relaxed)
    }

    /// Take hart `x`'s run-queue lock (it publishes the hart's hints when released after a change).
    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count
    fn lq(&self, x: usize) -> Hq<'_, N, R> {
        Hq {
            g: self.rqs.lock(x),
            hint: &self.hint[x],
            changed: false,
        }
    }

    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count
    fn is_online(&self, x: usize) -> bool {
        self.online[x].load(Ordering::Acquire)
    }

    /// The hint for `would_run`: would a thread of priority `p` queued on hart `x` run instead of what runs there?
    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count; p <= PRIO_MAX = 63, so the shift is below 64
    fn would_run_hint(&self, x: usize, p: Prio) -> bool {
        p <= PRIO_MAX && self.hint[x].would_run.load(Ordering::Acquire) & (1 << p) != 0
    }

    fn home_of(&self, tid: Tid) -> Option<usize> {
        self.rqs.table().get(&tid).map(|h| h.get())
    }

    /// How many threads were pulled from another hart (a counter for tests and the kernel's statistics).
    pub fn pulls(&self) -> u64 {
        self.pulls.load(Ordering::Relaxed)
    }
    /// Statistics: calls to `settle`, marks turned on.
    pub fn settles(&self) -> u64 {
        self.settles.load(Ordering::Relaxed)
    }
    pub fn marks(&self) -> u64 {
        self.marks.load(Ordering::Relaxed)
    }

    pub fn harts(&self) -> usize {
        self.rqs.len()
    }
    /// Take `hart` into the scheduler or out of it (placement and push skip a hart that is not online).
    #[allow(clippy::indexing_slicing)] // INVARIANT: hart is a hart index below the hart count
    pub fn set_online(&self, hart: usize, online: bool) {
        self.online[hart].store(online, Ordering::Release);
    }
    /// Set `hart`'s clock without running any timers (when it joins).
    pub fn set_clock(&self, hart: usize, now: Time) {
        self.lq(hart).set_now(now);
    }
    pub fn now_on(&self, hart: usize) -> Time {
        self.lq(hart).now()
    }
    /// The base priority of a thread.
    pub fn priority(&self, tid: Tid) -> Option<Prio> {
        self.record(tid).map(|r| r.base.prio)
    }
    /// The only hart a thread may run on (`None`: any).
    pub fn affinity(&self, tid: Tid) -> Option<usize> {
        self.record(tid)?.affinity.map(usize::from)
    }
    /// The park word of a thread (a test hook).
    pub fn park_state(&self, tid: Tid) -> Option<crate::park::ParkState> {
        self.record(tid).map(|r| r.park)
    }
    /// The idle thread of `hart`.
    #[allow(clippy::expect_used)] // INVARIANT: every hart has an idle thread from `over`
    pub fn idle(&self, hart: usize) -> Tid {
        self.lq(hart)
            .idle_tid()
            .expect("every hart has an idle thread")
    }
    /// The thread designated to run on `hart` (its idle thread when nothing else does).
    #[allow(clippy::expect_used)] // INVARIANT: a hart always has a current thread (its idle thread at least)
    pub fn current_on(&self, hart: usize) -> Tid {
        self.lq(hart)
            .current()
            .expect("a hart always has a current thread")
    }
    #[allow(clippy::indexing_slicing)] // INVARIANT: hart is a hart index below the hart count
    pub fn resched_pending(&self, hart: usize) -> bool {
        self.pending[hart].load(Ordering::Acquire)
    }
    /// The harts to send a reschedule IPI to (bitmask), clearing the set.
    pub fn take_pokes(&self) -> u64 {
        self.pokes.swap(0, Ordering::AcqRel)
    }
    pub fn state(&self, tid: Tid) -> Option<State> {
        self.read_record(tid, |r| r.state)
    }
    /// A copy of the thread's record.
    pub fn record(&self, tid: Tid) -> Option<ThreadSched> {
        self.read_record(tid, |r| r.clone())
    }
    /// Read a thread's record under its hart's lock. The home is read first and re-checked under the lock: a record that moved
    /// in between is looked for again (a pull or a migrating wake changes it only holding both harts' locks).
    fn read_record<T>(&self, tid: Tid, f: impl Fn(&ThreadSched) -> T) -> Option<T> {
        loop {
            let h = self.home_of(tid)?;
            {
                let q = self.lq(h);
                if let Some(r) = q.record(tid).filter(|r| r.home.get() == h) {
                    return Some(f(r));
                }
            }
            // (the table is looked at with no run-queue lock held.) An entry means the thread exists and is moving, or is being
            // reaped (`reap` takes the record out and then the entry): look again; only a missing entry says it is gone. Deciding
            // "not moving" from an unchanged home would be fooled by a thread that moved away and back in between.
            self.home_of(tid)?;
        }
    }
    /// The hart that holds `tid`'s record.
    pub fn home(&self, tid: Tid) -> Option<usize> {
        self.home_of(tid)
    }
    /// Does hart `h` have a reason to leave idle: a mark is set for it, or something is queued on it? The idle loop asks this with
    /// interrupts off, before `wfi` (SCHED-SMP-SPEC 3.9: the check is made under `RQ[h]`, so a wake that lands after it has enqueued
    /// and sent an IPI that `wfi` will see).
    #[allow(clippy::indexing_slicing)] // INVARIANT: h is a hart index below the hart count
    pub fn idle_has_work(&self, h: usize) -> bool {
        {
            let q = self.lq(h);
            if self.pending[h].load(Ordering::Acquire) || q.queued_len() > 0 {
                return true;
            }
        }
        // A hart that is about to sleep looks at the others' queues first (the spec's `overloaded` mask, read the slow way): with no
        // periodic tick nothing else would make it pull work whose mark was spent while it was busy or already asleep (a thread queued
        // behind others on a busy hart, with every mark for it consumed). Only a thread this hart may take counts, so an idle hart does
        // not spin on work that is pinned elsewhere or in transit.
        for g in 0..self.harts() {
            if g == h || !self.is_online(g) {
                continue;
            }
            let src = self.lq(g);
            if src
                .queued_top_where(&|t| {
                    src.record(t).is_some_and(|r| allowed(r, h)) && !self.is_in_transit(t)
                })
                .is_some()
            {
                return true;
            }
        }
        false
    }

    /// The starvation watchdog's check (a debug detector, SCHED-SMP-SPEC 3.9; `now` is the caller's clock, since a hart that takes no
    /// tick has a stale one): a thread queued (or woken: `runnable_since` is the last wake) for more than `limit` ns while a hart
    /// that may run it is idle. Returns (thread, the hart it is queued on, the idle hart). A thread in transit is not counted. Takes
    /// one run-queue lock at a time and reads the others' hints, so it is cheap enough for a timer handler in a debug build.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: runnable_since is not after the clock that stamped it (saturating_sub)
    pub fn starved(&self, now: Time, limit: Time) -> Option<(Tid, usize, usize)> {
        for g in 0..self.harts() {
            let late: Vec<(Tid, Option<u8>)> = {
                let q = self.lq(g);
                q.tids()
                    .filter(|&t| q.is_queued(t))
                    .filter_map(|t| q.record(t).map(|r| (t, r)))
                    .filter(|(_, r)| now.saturating_sub(r.runnable_since) > limit)
                    .map(|(t, r)| (t, r.affinity))
                    .collect()
            };
            for (t, aff) in late {
                if self.is_in_transit(t) {
                    continue;
                }
                let idle_hart = (0..self.harts()).find(|&x| {
                    self.is_online(x)
                        && aff.is_none_or(|a| a as usize == x)
                        && self.view_cur(x) == 0
                        // (a hart in the middle of a switch (a thread in transit on it) is on its way to look at its queue, not an idle
                        // hart nobody told; with thousands of threads queued behind two busy harts and the big lock contended it is
                        // routinely so. A hart that is only *marked* is the lost-poke case and is still reported)
                        && self.transit.get(x).is_none_or(|t| t.load(Ordering::Acquire) == 0)
                });
                if let Some(x) = idle_hart {
                    return Some((t, g, x));
                }
            }
        }
        None
    }

    /// When hart `hart`'s one-shot timer must fire next (`HartRq::next_event`): `None` is never.
    pub fn next_event_on(&self, hart: usize) -> Option<Time> {
        self.lq(hart).next_event()
    }

    /// Set a thread's timer slack (SCHED-SMP-SPEC 4.11): nanoseconds, clamped to 1 ms, applied to its next sleep.
    pub fn set_timer_slack(&self, tid: Tid, slack: Time) -> Result<(), Error> {
        let home = self.home_of(tid).ok_or(Error::NoSuchThread)?;
        self.lq(home).set_timer_slack(tid, slack)
    }

    pub fn queued_on(&self, hart: usize) -> usize {
        self.lq(hart).queued_len()
    }
    /// The threads the scheduler tracks, not counting the idle threads of harts that are not online.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: offline harts' idle threads are in the table
    pub fn thread_count(&self) -> usize {
        let offline = (0..self.harts()).filter(|&h| !self.is_online(h)).count();
        self.rqs.table().len() - offline
    }
    pub fn tids(&self) -> Vec<Tid> {
        self.rqs.table().keys().copied().collect()
    }

    #[allow(clippy::indexing_slicing)] // INVARIANT: h is a hart index below the hart count
    fn view_cur(&self, h: usize) -> Prio {
        self.hint[h].cur_eff.load(Ordering::Acquire)
    }

    /// 4.4 `select_hart` (rules 1, 3, 4 and 5; the synchronous-wake hint of rule 2 arrives with direct hand-off). Reads only
    /// hints.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: harts() >= 1
    fn select_hart(&self, rec: &ThreadSched) -> usize {
        if let Some(a) = rec.affinity {
            return a as usize;
        }
        let mut last = (rec.last_hart as usize).min(self.harts() - 1);
        if !self.is_online(last) {
            last = (0..self.harts()).find(|&x| self.is_online(x)).unwrap_or(0);
        }
        if self.view_cur(last) < rec.eff {
            return last;
        }
        let best = (0..self.harts())
            .filter(|&h| self.is_online(h))
            .min_by_key(|&h| (self.view_cur(h), h))
            .unwrap_or(last);
        if self.view_cur(best) < rec.eff {
            best
        } else {
            last
        }
    }

    pub fn is_in_transit(&self, t: Tid) -> bool {
        self.transit.iter().any(|x| x.load(Ordering::Acquire) == t)
    }

    /// The thread hart `h` has switched away from but whose context is not yet saved, if any.
    #[allow(clippy::indexing_slicing)] // INVARIANT: hart is a hart index below the hart count
    pub fn in_transit(&self, hart: usize) -> Option<Tid> {
        match self.transit[hart].load(Ordering::Acquire) {
            0 => None,
            t => Some(t),
        }
    }

    /// Hart `h`'s context switch is complete (called by the thread that resumes on `h`, or by a new thread starting there,
    /// or by `h` when it decided not to switch after all): the thread it left is now available to every hart. Marks owed
    /// for it (it may belong on a lower hart) are set; the caller sends the pokes.
    #[allow(clippy::indexing_slicing)] // INVARIANT: h is a hart index below the hart count
    pub fn switched(&self, h: usize) {
        let t = self.transit[h].swap(0, Ordering::AcqRel);
        if t != 0 {
            if let Some(g) = self.home_of(t) {
                self.push_marks(t, g, h);
                // a thread woken while it was still executing here (G4) waits in this hart's own queue: nobody else was marked for
                // it, so this hart looks again (a preempted thread requeued here is not better than what runs here: no mark)
                if g == h && self.lq(h).wants_resched() {
                    self.mark(h);
                }
            }
        }
    }

    /// Called **with hart `h`'s lock held**, in the same critical section as the change of designation (a tick's rotation, a yield,
    /// a preemption): if the thread `before` that was executing there is now queued, record that it is in transit, so no other hart
    /// can pull it (it is still executing) until `switched(h)`. Doing this after the lock was released would leave a window in
    /// which another hart's pull found it queued and ran it a second time.
    #[allow(clippy::indexing_slicing)] // INVARIANT: h is a hart index below the hart count
    fn displaced(&self, h: usize, q: &HartRq<N>, before: Tid) {
        // The first thread displaced is the one still executing; a later change of designation (a pull after a tick's
        // rotation, say) displaces only a thread that was never run, so the first one stays protected until `switched`
        // (review of task 12: overwriting it left the executing thread free to be pulled and run on two harts).
        if self.transit[h].load(Ordering::Acquire) != 0 {
            return;
        }
        // (any state: a thread that blocked or exited is still executing its last instructions on this hart, and with the lock released
        // at the switch (G4) a wake or a reap on another hart could otherwise reach it before its context is saved)
        if Some(before) != q.current() && q.idle_tid() != Some(before) && q.state(before).is_some()
        {
            self.transit[h].store(before, Ordering::Release);
        }
    }

    /// Mark `x` for a reschedule (one poke owed to whoever turns the mark on).
    #[allow(clippy::indexing_slicing)] // INVARIANT: x is a hart index below the hart count
    fn mark(&self, x: usize) {
        // (a hart that is not in the scheduler is never marked: it looks at its queues when it joins, 3.9)
        if self.is_online(x)
            && self.pending[x]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.marks.fetch_add(1, Ordering::Relaxed);
            self.pokes.fetch_or(1 << x, Ordering::AcqRel);
        }
    }

    /// Push: `tid`, whose record is on hart `g`, is queued (not running). One thread needs one hart: the online hart (other than
    /// the caller, who looks after itself in `settle`) that runs the lowest priority and would actually run it instead is marked,
    /// if the caller is not itself the better choice. (The spec's "owes exactly one IPI", 3.9.) The caller holds no run-queue
    /// lock. Returns whether the caller is itself the best hart for it (and is not where it is queued): then the caller must
    /// look at its own queues now (`settle`), as no one else will.
    fn push_marks(&self, tid: Tid, g: usize, caller: usize) -> bool {
        self.push_marks_with(tid, g, caller, false)
    }

    /// `push_marks`; `executing` says `tid` is the thread `caller` has just displaced (it is in transit, and the mark for it is
    /// owed now all the same: what the single-value scheduler did before the transit was recorded in the same critical section).
    fn push_marks_with(&self, tid: Tid, g: usize, caller: usize, executing: bool) -> bool {
        let (state, eff, aff) = {
            let q = self.lq(g);
            let Some(rec) = q.record(tid) else {
                return false;
            };
            (rec.state, rec.eff, rec.affinity)
        };
        if state != State::Runnable || (!executing && self.is_in_transit(tid)) {
            return false;
        }
        let mut best: Option<(Prio, usize)> = None;
        for x in 0..self.harts() {
            if !self.is_online(x) {
                continue;
            }
            let ok = aff.is_none_or(|a| a as usize == x);
            if ok && self.would_run_hint(x, eff) {
                let key = (self.view_cur(x), x);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
        }
        match best {
            Some((_, x)) if x != caller => {
                self.mark(x);
                false
            }
            Some(_) => g != caller,
            None => false,
        }
    }

    /// Lock two harts' run queues in ascending order and give them back as (`a`'s, `b`'s). `a != b`.
    fn lq2(&self, a: usize, b: usize) -> (Hq<'_, N, R>, Hq<'_, N, R>) {
        debug_assert!(a != b);
        if a < b {
            let qa = self.lq(a);
            let qb = self.lq(b);
            (qa, qb)
        } else {
            let qb = self.lq(b);
            let qa = self.lq(a);
            (qa, qb)
        }
    }

    /// Pull into `h` the best queued thread of each other hart that outranks everything `h` has. Returns how many.
    fn pull(&self, h: usize) -> usize {
        #[cfg(feature = "fault-skip-pull")]
        {
            let _ = h;
            return 0; // negative control (L11d): no hart ever pulls
        }
        #[cfg(not(feature = "fault-skip-pull"))]
        self.pull_work(h)
    }

    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the thread was found queued under the same two locks a few lines above
    fn pull_work(&self, h: usize) -> usize {
        let mut n = 0;
        for g in 0..self.harts() {
            if g == h {
                continue;
            }
            // look at `g` alone, then take both locks in order and look again before moving anything
            let found = {
                let src = self.lq(g);
                src.queued_top_where(&|t| {
                    src.record(t).is_some_and(|r| allowed(r, h)) && !self.is_in_transit(t)
                })
            };
            let Some((tid, p0)) = found else { continue };
            let moved = {
                let (mut dst, mut src) = self.lq2(h, g);
                let mine = dst.cur_eff().max(dst.queued_top().unwrap_or(0));
                // the record may have moved, been taken, or changed while no lock was held
                let again = src.queued_top_where(&|t| {
                    src.record(t).is_some_and(|r| allowed(r, h)) && !self.is_in_transit(t)
                });
                match again {
                    Some((t2, p)) if t2 == tid && p == p0 && p > mine && dst.would_run(p) => {
                        let rec = src.detach_queued(tid).expect("it was queued");
                        dst.attach(rec, Enq::Migrated, h as u8);
                        true
                    }
                    _ => false,
                }
            };
            if moved {
                self.pulls.fetch_add(1, Ordering::Relaxed);
                n += 1;
            } else {
                // not for this hart (it has something at least as good, or would only queue it): hand it on, or the one
                // mark that was owed for it, which this hart consumed, would leave it waiting while a lower hart idles
                self.push_marks(tid, g, h);
            }
        }
        n
    }

    /// Hart `h` makes its own designation right: pull what outranks it, preempt for what waits, and push what it leaves.
    #[allow(clippy::indexing_slicing)] // INVARIANT: h is a hart index below the hart count
    fn settle(&self, h: usize) {
        self.settles.fetch_add(1, Ordering::Relaxed);
        self.charge_now(h);
        self.pending[h].store(false, Ordering::Release);
        let entry_cur = self.lq(h).current();
        // (a mark is owed for the executing thread this settle displaces, unless a tick or yield already put it in transit)
        let owed_for_displaced = self.transit[h].load(Ordering::Acquire) == 0;
        loop {
            let pulled = self.pull(h);
            let before = {
                let mut q = self.lq(h);
                if !q.wants_resched() {
                    None
                } else {
                    let before = q.current().filter(|&c| Some(c) != q.idle_tid());
                    q.resched();
                    if let Some(b) = entry_cur {
                        self.displaced(h, &q, b);
                    }
                    Some(before)
                }
            };
            let Some(before) = before else {
                if pulled == 0 {
                    break;
                }
                continue;
            };
            if let Some(o) = before {
                if self.lq(h).state(o) == Some(State::Runnable) {
                    self.push_marks_with(o, h, h, owed_for_displaced);
                }
            }
        }
        // whatever is still queued here may belong on a hart running something lower: for each other hart, the best queued
        // thread here that may run there (O(harts), not O(threads))
        self.push_queue(h);
    }

    /// A thread is left queued on `g`: look at the best one (and the best one that is not pinned, which any hart may take)
    /// and push each (one mark apiece, `push_marks`).
    fn push_queue(&self, g: usize) {
        let (top, free) = {
            let src = self.lq(g);
            (
                src.queued_top_where(&|t| !self.is_in_transit(t))
                    .map(|(t, _)| t),
                src.queued_top_where(&|t| {
                    !self.is_in_transit(t) && src.record(t).is_some_and(|r| r.affinity.is_none())
                })
                .map(|(t, _)| t),
            )
        };
        for t in [top, free].into_iter().flatten() {
            self.push_marks(t, g, g);
        }
    }

    /// Hart `h` services a mark (or any reason to look at the queues): the only way `cur[h]` changes other than by `h`'s
    /// own block, yield and exit.
    pub fn reschedule_on(&self, h: usize) {
        self.settle(h);
    }

    // ---- creating threads

    /// Adopt the code running on `hart` (the boot thread) as a thread.
    #[allow(clippy::expect_used)] // INVARIANT: the record was just created on this hart
    pub fn adopt_current_on(
        &self,
        hart: usize,
        attr: SchedAttr,
        affinity: Option<usize>,
    ) -> Result<Tid, Error> {
        let tid = self.alloc_tid();
        let home = {
            let mut q = self.lq(hart);
            q.adopt_current_as(tid, attr, affinity.map(|a| a as u8), hart as u8)?;
            q.record(tid).expect("just made").home.clone()
        };
        self.rqs.table().insert(tid, home);
        Ok(tid)
    }

    /// A new runnable thread created by code running on `caller`.
    #[allow(clippy::expect_used)] // INVARIANT: the record was just created on the target hart
    pub fn spawn_on(
        &self,
        caller: usize,
        attr: SchedAttr,
        affinity: Option<usize>,
    ) -> Result<Tid, Error> {
        if attr.prio > PRIO_MAX || attr.prio == 0 {
            return Err(Error::BadPriority); // 0 is the idle priority: idle threads are never queued
        }
        if affinity.is_some_and(|a| a >= self.harts()) {
            return Err(Error::NoSuchThread);
        }
        let tid = self.alloc_tid();
        let probe = ThreadSched {
            home: Arc::new(Home::new(0)),
            tid,
            base: attr,
            eff: attr.prio,
            demoted_window: 0,
            state: State::Runnable,
            affinity: affinity.map(|a| a as u8),
            last_hart: caller as u8,
            slice_left: 0,
            deadline: None,
            timer_hard: 0,
            timer_slack: crate::sched2::DEFAULT_SLACK,
            runnable_since: 0,
            started: 0,
            cpu: 0,
            group: 0,
            job: 0,
            class_words: [0; 4],
            park: crate::park::ParkState::Idle,
        };
        let target = self.select_hart(&probe);
        let home = {
            let mut q = self.lq(target);
            q.fresh_as(
                tid,
                attr,
                State::Runnable,
                affinity.map(|a| a as u8),
                target as u8,
            )?;
            q.enqueue_runnable(tid, Enq::New);
            q.record(tid).expect("just made").home.clone()
        };
        // (the table comes after the run-queue lock: `TT` is ahead of `RQ` in the order)
        self.rqs.table().insert(tid, home);
        self.after_enqueue(tid, target, caller);
        Ok(tid)
    }

    /// The caller's reschedule or marks after `tid` was queued on `home`.
    fn after_enqueue(&self, tid: Tid, home: usize, caller: usize) {
        let me_first = self.push_marks(tid, home, caller);
        if home == caller || me_first {
            self.settle_or_defer(caller);
        }
        // A thread queued behind an equal one on another hart that is running something makes that hart's slice end an event (no
        // periodic tick arms it any more): the hart is marked, looks at its queue and programs its timer (L13). Better threads
        // already mark their hart through `push_marks`.
        if home != caller {
            let eff = self.lq(home).record(tid).map(|r| r.eff);
            if eff.is_some_and(|e| self.view_cur(home) != 0 && e >= self.view_cur(home)) {
                self.mark(home);
            }
        }
    }

    /// Give the scheduler a time source (nanoseconds), before it is shared: every settle then charges the running thread the time since
    /// the hart's last look, which a kernel with no periodic tick needs (the tick used to do it).
    #[must_use]
    pub fn with_clock(mut self, f: fn() -> Time) -> Self {
        self.clock = Some(f);
        self
    }

    /// Charge hart `h`'s running thread up to the clock source's now (no-op without a source).
    fn charge_now(&self, h: usize) {
        if let Some(f) = self.clock {
            self.lq(h).charge(f());
        }
    }

    /// Deferred mode on or off (see the field).
    pub fn set_deferred(&self, on: bool) {
        self.deferred.store(on, Ordering::Relaxed);
    }

    /// The caller's hart must look at its queues now (a wake or a change made something here worth running): at once, or, in
    /// deferred mode, at its next defined point: it is marked, and the caller's `resched_pending` tells the kernel to switch there. No
    /// poke is owed (the hart is the one that set the mark and it acts on it itself).
    #[allow(clippy::indexing_slicing)] // INVARIANT: caller is a hart index below the hart count
    fn settle_or_defer(&self, caller: usize) {
        if self.deferred.load(Ordering::Relaxed) {
            let _ = self.pending[caller].compare_exchange(
                false,
                true,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        } else {
            self.settle(caller);
        }
    }

    // ---- blocking and waking

    /// The current thread of `hart` is about to wait: a wake from now on counts.
    pub fn prepare_block_on(&self, hart: usize) -> Result<(), Error> {
        self.lq(hart).prepare_block()
    }

    /// Block the current thread of `hart` unless a notification is waiting. Returns whether it blocked.
    pub fn block_on(&self, hart: usize) -> Result<bool, Error> {
        self.charge_now(hart); // (the outgoing thread's run is charged to it, before the designation changes: PR 273 review)
        let blocked = {
            let mut q = self.lq(hart);
            let before = q.current();
            let blocked = q.block_current()?;
            if let (true, Some(b)) = (blocked, before) {
                self.displaced(hart, &q, b); // (still executing until the switch completes)
            }
            blocked
        };
        if blocked {
            self.settle(hart);
        }
        Ok(blocked)
    }

    /// Sleep until `deadline` (nanoseconds on this hart's clock).
    pub fn sleep_until_on(&self, hart: usize, deadline: Time) -> Result<bool, Error> {
        self.charge_now(hart);
        let blocked = {
            let mut q = self.lq(hart);
            let before = q.current();
            let blocked = q.sleep_until(deadline)?;
            if let (true, Some(b)) = (blocked, before) {
                self.displaced(hart, &q, b);
            }
            blocked
        };
        if blocked {
            self.settle(hart);
        }
        Ok(blocked)
    }

    /// Wake `tid` (a blocked thread is placed and queued; a prepared one is notified; anything else is left alone).
    pub fn wake_on(&self, caller: usize, tid: Tid) -> Result<(), Error> {
        self.wake_inner(caller, tid).map(|_| ())
    }

    /// `wake_on`, saying whether the wake counted: the thread was blocked (now runnable) or had prepared to wait (now
    /// notified, so it will not block). Anything else is left alone and does not count.
    pub fn wake_counted_on(&self, caller: usize, tid: Tid) -> Result<bool, Error> {
        self.wake_inner(caller, tid)
    }

    fn wake_inner(&self, caller: usize, tid: Tid) -> Result<bool, Error> {
        loop {
            let home = self.home_of(tid).ok_or(Error::NoSuchThread)?;
            self.charge_now(home); // (a tickless hart's clock is as old as its last look: the wake is stamped with the time it happens)
                                   // where would it go? (a blocked thread is placed; the placement reads hints and needs the record)
            let Some(rec) = ({
                let q = self.lq(home);
                q.record(tid).filter(|r| r.home.get() == home).cloned()
            }) else {
                self.home_of(tid).ok_or(Error::NoSuchThread)?; // gone (reaped)
                continue; // it moved (or is moving): look again
            };
            // (a blocked thread still in transit is queued on the hart that is still executing it: that hart's own queue is the only one
            // that cannot be served before its switch is done, and no other hart may pull it until then, G4)
            let target = if rec.state == State::Blocked && !self.is_in_transit(tid) {
                self.select_hart(&rec)
            } else {
                home
            };
            let placed = if target == home {
                let mut q = self.lq(home);
                self.wake_under(&mut q, None, tid, home, target)
            } else {
                let (mut src, mut dst) = self.lq2(home, target);
                self.wake_under(&mut src, Some(&mut dst), tid, home, target)
            };
            match placed {
                Placed::Retry => continue,
                Placed::Missing => {
                    self.home_of(tid).ok_or(Error::NoSuchThread)?; // gone (reaped), or moved: look again
                    continue;
                }
                Placed::Done(counts) => return Ok(counts),
                Placed::Queued(counts) => {
                    self.after_enqueue(tid, target, caller);
                    return Ok(counts);
                }
                Placed::Err(e) => return Err(e),
            }
        }
    }

    /// The wake, under the locks of the thread's hart (`src`) and, if it goes elsewhere, of the target (`dst`).
    fn wake_under(
        &self,
        src: &mut Hq<'_, N, R>,
        dst: Option<&mut Hq<'_, N, R>>,
        tid: Tid,
        home: usize,
        target: usize,
    ) -> Placed {
        let Some(rec) = src.record(tid) else {
            return Placed::Missing; // (the table is looked at with no run-queue lock held)
        };
        if rec.home.get() != home {
            return Placed::Retry;
        }
        let counts = match rec.state {
            State::Blocked => true,
            State::Running | State::Runnable => rec.park == crate::park::ParkState::Prepared,
            State::Exited => false,
        };
        if rec.state != State::Blocked {
            return match src.wake_quiet(tid) {
                Ok(_) => Placed::Done(counts),
                Err(e) => Placed::Err(e),
            };
        }
        match dst {
            None => match src.wake_quiet(tid) {
                Ok(_) => Placed::Queued(counts),
                Err(e) => Placed::Err(e),
            },
            Some(dst) => match src.detach_woken(tid) {
                Ok((rec, slept)) => {
                    dst.attach(rec, Enq::Wake { slept }, target as u8);
                    Placed::Queued(counts)
                }
                Err(e) => Placed::Err(e),
            },
        }
    }

    pub fn yield_on(&self, hart: usize) -> Result<(), Error> {
        self.charge_now(hart);
        {
            let mut q = self.lq(hart);
            let cur = q.current();
            q.yield_now()?;
            if let Some(c) = cur {
                self.displaced(hart, &q, c);
            }
        }
        self.settle(hart);
        Ok(())
    }

    pub fn exit_on(&self, hart: usize) -> Result<(), Error> {
        self.charge_now(hart);
        {
            let mut q = self.lq(hart);
            let before = q.current();
            q.exit_current()?;
            if let Some(b) = before {
                self.displaced(hart, &q, b);
            }
        }
        self.settle(hart);
        Ok(())
    }

    pub fn reap(&self, tid: Tid) -> Result<(), Error> {
        let home = self.home_of(tid).ok_or(Error::NoSuchThread)?;
        self.lq(home).reap(tid)?;
        self.rqs.table().remove(&tid);
        Ok(())
    }

    /// Change a thread's base priority; its hart is marked (or, if it is the caller's, looks at once).
    pub fn set_priority_on(&self, caller: usize, tid: Tid, prio: Prio) -> Result<(), Error> {
        if prio == 0 {
            return Err(Error::BadPriority);
        }
        let home = self.home_of(tid).ok_or(Error::NoSuchThread)?;
        let changed = self.lq(home).set_priority_quiet(tid, prio)?;
        if !changed {
            return Ok(());
        }
        let me_first = self.push_marks(tid, home, caller);
        if home == caller || me_first {
            self.settle_or_defer(caller);
        } else {
            self.mark(home);
        }
        Ok(())
    }

    /// Hart `hart`'s tick: the clock moves to `now`, its due timers fire, the running thread is accounted.
    pub fn tick_on(&self, hart: usize, now: Time) {
        {
            let mut q = self.lq(hart);
            let before = q.current();
            q.tick_to(now);
            if let Some(b) = before {
                self.displaced(hart, &q, b);
            }
        }
        self.settle(hart);
        // (a thread the balance moved here may outrank what this hart runs, and nothing else will mark a hart for it: it looks
        // again at once, or the thread would wait for this hart's next tick, and for ever on a hart that takes none: T162)
        if self.balance(hart) {
            self.settle(hart);
        }
    }

    /// Equal-priority work does not move by priority (no pull of an equal, 4.4), so queues of equals would stay as lopsided as
    /// they were made: a thread on a long queue waits a whole rotation of it. At a tick, a hart whose queue is at least two
    /// shorter than another's takes one thread from it that it may run and that is not worse than what it runs. One per
    /// tick; with queues within one of each other nothing moves, so equals do not churn between harts.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: hart indices are below the hart count; the thread was found queued under the same two locks
    fn balance(&self, h: usize) -> bool {
        #[cfg(feature = "fault-skip-pull")]
        {
            let _ = h;
            return false; // negative control (L11d): nothing moves between harts but by push
        }
        let len = |x: usize| self.hint[x].queued_len.load(Ordering::Acquire);
        let mine = len(h);
        let busiest = (0..self.harts())
            .filter(|&g| g != h && self.is_online(g))
            .max_by_key(|&g| (len(g), core::cmp::Reverse(g)));
        let Some(g) = busiest else { return false };
        if len(g) < mine + 2 {
            return false;
        }
        let cur = self.view_cur(h);
        let find = |src: &Hq<'_, N, R>| {
            src.queued_top_where(&|t| {
                src.record(t).is_some_and(|r| allowed(r, h)) && !self.is_in_transit(t)
            })
        };
        let found = find(&self.lq(g));
        let Some((tid, p)) = found else { return false };
        if p < cur {
            return false;
        }
        let (mut dst, mut src) = self.lq2(h, g);
        // everything that chose this move is looked at again under both locks (the lengths and the running priority were hints)
        if find(&src) != Some((tid, p))
            || src.queued_len() < dst.queued_len() + 2
            || p < dst.cur_eff()
        {
            return false; // it moved, or the picture changed, while no lock was held
        }
        let rec = src.detach_queued(tid).expect("it was queued");
        dst.attach(rec, Enq::Migrated, h as u8);
        self.pulls.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn bad_picks(&self) -> u32 {
        (0..self.harts()).map(|h| self.lq(h).bad_picks).sum()
    }

    // ---- the invariants

    /// **GP2** (always) and, when `quiescent`, **GP1**. Takes every run-queue lock, in ascending order, and holds them all (a
    /// test and debug facility: it must not be called with any run-queue lock held).
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: a count of the records seen
    pub fn audit(&self, quiescent: bool) -> Result<(), String> {
        let n = self.harts();
        let table = self.rqs.table(); // (`TT` is ahead of `RQ` in the order)
        let qs: Vec<Hq<'_, N, R>> = (0..n).map(|x| self.lq(x)).collect();
        for (h, rq) in qs.iter().enumerate() {
            rq.audit().map_err(|e| alloc::format!("hart {h}: {e}"))?;
        }
        let mut seen = 0usize;
        for (h, rq) in qs.iter().enumerate() {
            for t in rq.tids() {
                seen += 1;
                if table.get(&t).map(|x| x.get()) != Some(h) {
                    return Err(alloc::format!(
                        "thread {t} is on hart {h} but located elsewhere"
                    ));
                }
                if let Some(rec) = rq.record(t) {
                    if let Some(a) = rec.affinity {
                        if rec.state != State::Blocked
                            && rec.state != State::Exited
                            && a as usize != h
                        {
                            return Err(alloc::format!(
                                "pinned thread {t} (hart {a}) is on hart {h}"
                            ));
                        }
                    }
                }
            }
        }
        if seen != table.len() {
            return Err(alloc::format!("{seen} records, {} located", table.len()));
        }
        for (h, t) in self.transit.iter().enumerate() {
            let t = t.load(Ordering::Acquire);
            if t != 0
                && (table.get(&t).map(|x| x.get()) != Some(h)
                    || qs[h].state(t).is_none()
                    || (qs[h].state(t) == Some(State::Runnable) && !qs[h].is_queued(t)))
            {
                return Err(alloc::format!(
                    "thread {t} in transit on hart {h} has no record there"
                ));
            }
        }
        if !quiescent {
            return Ok(());
        }
        // GP1, per class: no online hart runs something a waiting thread would displace there (priority order for the
        // fixed-priority and real-time classes; for round robin, work conservation: no hart idles while a thread it may
        // run waits)
        let queued: Vec<(usize, Tid, Prio, Option<u8>)> = qs
            .iter()
            .enumerate()
            .flat_map(|(g, rq)| {
                rq.tids().filter(|&t| rq.is_queued(t)).map(move |t| {
                    let r = rq.record(t).expect("queued");
                    (g, t, r.eff, r.affinity)
                })
            })
            .collect();
        for (x, qx) in qs.iter().enumerate() {
            if !self.is_online(x) {
                continue;
            }
            for &(g, t, eff, aff) in &queued {
                // (a thread in transit is deliberately unavailable until its hart's switch completes)
                if self.is_in_transit(t) {
                    continue;
                }
                if aff.is_none_or(|a| a as usize == x) && qx.would_run(eff) {
                    let state: Vec<String> = qs
                        .iter()
                        .enumerate()
                        .map(|(h, q)| {
                            alloc::format!(
                                "hart {h}: runs {:?} (priority {}), queued {:?}",
                                q.current(),
                                q.cur_eff(),
                                q.tids()
                                    .filter(|&u| q.is_queued(u))
                                    .map(|u| (u, q.record(u).map(|r| r.eff)))
                                    .collect::<Vec<_>>()
                            )
                        })
                        .collect();
                    return Err(alloc::format!(
                        "GP1: hart {x} runs priority {} while thread {t} (priority {eff}) waits on hart {g}; {}",
                        qx.cur_eff(),
                        state.join("; ")
                    ));
                }
            }
        }
        Ok(())
    }
}

/// What a wake did under the locks.
enum Placed {
    /// The record was not where it was looked for: look again.
    Retry,
    /// There is no record on that hart: gone, or moved (the caller looks at the table).
    Missing,
    /// Nothing to queue (already runnable or running); whether the wake counted.
    Done(bool),
    /// Queued on the target; whether the wake counted.
    Queued(bool),
    Err(Error),
}

/// A FIFO of threads waiting for something, on the park protocol (the multi-hart counterpart of `sched::Waiters`): a thread
/// prepares, queues itself and blocks; a waker takes the longest-waiting thread whose wake **counts**. Nothing is
/// remembered for a thread that is not waiting. Used for kernel-thread waits (join, the reaper, the self-tests).
#[derive(Default)]
pub struct Waiters2 {
    waiters: alloc::collections::VecDeque<Tid>,
}

impl Waiters2 {
    pub fn new() -> Waiters2 {
        Waiters2::default()
    }
    pub fn len(&self) -> usize {
        self.waiters.len()
    }
    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    /// Queue the thread running on `hart` and block it. False if a wake that arrived first let it through.
    pub fn wait_on<N: Class, R: Rqs<N>>(
        &mut self,
        sched: &Sched2<N, R>,
        hart: usize,
    ) -> Result<bool, Error> {
        let me = sched.current_on(hart);
        sched.prepare_block_on(hart)?;
        // a thread is queued at most once: a stale entry must not swallow a later wake
        self.waiters.retain(|&t| t != me);
        self.waiters.push_back(me);
        let blocked = sched.block_on(hart)?;
        if !blocked {
            self.waiters.retain(|&t| t != me);
        }
        Ok(blocked)
    }

    /// Wake the longest-waiting thread whose wake counts; stale entries are dropped.
    pub fn wake_one_on<N: Class, R: Rqs<N>>(
        &mut self,
        sched: &Sched2<N, R>,
        caller: usize,
    ) -> Option<Tid> {
        while let Some(tid) = self.waiters.pop_front() {
            if sched.wake_counted_on(caller, tid) == Ok(true) {
                return Some(tid);
            }
        }
        None
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: a count of woken waiters, bounded by the thread limit
    pub fn wake_all_on<N: Class, R: Rqs<N>>(
        &mut self,
        sched: &Sched2<N, R>,
        caller: usize,
    ) -> usize {
        let mut n = 0;
        while self.wake_one_on(sched, caller).is_some() {
            n += 1;
        }
        n
    }
}
