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
use crate::sched2::{Class, Enq, Error, HartRq, Prio, SchedAttr, ThreadSched, Time};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// The most harts (the poke set is a bitmask).
pub const MAX_HARTS: usize = 32;

pub struct Sched2<N: Class> {
    harts: Vec<HartRq<N>>,
    /// Which hart holds each thread's record.
    loc: BTreeMap<Tid, usize>,
    next_tid: Tid,
    pending: Vec<bool>,
    /// Harts that are part of the scheduler: only they are chosen to run or hold threads (a hart that has not joined
    /// yet is skipped by placement and by push). Every hart starts online; the kernel clears the others until they join.
    online: Vec<bool>,
    pokes: u32,
    /// How many threads were pulled from another hart (a counter for tests and the kernel's statistics).
    pub pulls: u64,
    /// Statistics: calls to `settle`, marks turned on, and records migrated by a wake.
    pub settles: u64,
    pub marks: u64,
}

fn allowed(rec: &ThreadSched, hart: usize) -> bool {
    rec.affinity.is_none_or(|a| a as usize == hart)
}

impl<N: Class> Sched2<N> {
    /// `harts` run queues (1 to [`MAX_HARTS`]), each with a normal class made by `make`. Every hart starts with its idle
    /// thread running (`idle(h)` gives its id).
    pub fn new(harts: usize, make: impl Fn() -> N) -> Sched2<N> {
        assert!((1..=MAX_HARTS).contains(&harts), "1 to 32 harts");
        let mut s = Sched2 {
            harts: (0..harts).map(|_| HartRq::new(make())).collect(),
            loc: BTreeMap::new(),
            next_tid: 1,
            pending: alloc::vec![false; harts],
            online: alloc::vec![true; harts],
            pokes: 0,
            pulls: 0,
            settles: 0,
            marks: 0,
        };
        for h in 0..harts {
            let tid = s.alloc_tid();
            s.harts[h].adopt_idle_as(tid, h as u8);
            s.loc.insert(tid, h);
        }
        s
    }

    fn alloc_tid(&mut self) -> Tid {
        let t = self.next_tid;
        self.next_tid += 1;
        t
    }

    pub fn harts(&self) -> usize {
        self.harts.len()
    }
    /// Take `hart` into the scheduler or out of it (placement and push skip a hart that is not online).
    pub fn set_online(&mut self, hart: usize, online: bool) {
        self.online[hart] = online;
    }
    /// Set `hart`'s clock without running any timers (when it joins).
    pub fn set_clock(&mut self, hart: usize, now: Time) {
        self.harts[hart].set_now(now);
    }
    pub fn now_on(&self, hart: usize) -> Time {
        self.harts[hart].now()
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
    pub fn idle(&self, hart: usize) -> Tid {
        self.harts[hart]
            .idle_tid()
            .expect("every hart has an idle thread")
    }
    /// The thread designated to run on `hart` (its idle thread when nothing else does).
    pub fn current_on(&self, hart: usize) -> Tid {
        self.harts[hart]
            .current()
            .expect("a hart always has a current thread")
    }
    pub fn resched_pending(&self, hart: usize) -> bool {
        self.pending[hart]
    }
    /// The harts to send a reschedule IPI to (bitmask), clearing the set.
    pub fn take_pokes(&mut self) -> u32 {
        core::mem::take(&mut self.pokes)
    }
    pub fn state(&self, tid: Tid) -> Option<State> {
        let h = *self.loc.get(&tid)?;
        self.harts[h].state(tid)
    }
    pub fn record(&self, tid: Tid) -> Option<&ThreadSched> {
        let h = *self.loc.get(&tid)?;
        self.harts[h].record(tid)
    }
    /// The hart that holds `tid`'s record.
    pub fn home(&self, tid: Tid) -> Option<usize> {
        self.loc.get(&tid).copied()
    }
    pub fn queued_on(&self, hart: usize) -> usize {
        self.harts[hart].queued_len()
    }
    /// The threads the scheduler tracks, not counting the idle threads of harts that are not online.
    pub fn thread_count(&self) -> usize {
        let offline = (0..self.harts.len()).filter(|&h| !self.online[h]).count();
        self.loc.len() - offline
    }
    pub fn tids(&self) -> Vec<Tid> {
        self.loc.keys().copied().collect()
    }

    fn view_cur(&self, h: usize) -> Prio {
        self.harts[h].cur_eff()
    }

    /// 4.4 `select_hart` (rules 1, 3, 4 and 5; the synchronous-wake hint of rule 2 arrives with direct hand-off).
    fn select_hart(&self, rec: &ThreadSched) -> usize {
        if let Some(a) = rec.affinity {
            return a as usize;
        }
        let mut last = (rec.last_hart as usize).min(self.harts.len() - 1);
        if !self.online[last] {
            last = self.online.iter().position(|&o| o).unwrap_or(0);
        }
        if self.view_cur(last) < rec.eff {
            return last;
        }
        let best = (0..self.harts.len())
            .filter(|&h| self.online[h])
            .min_by_key(|&h| (self.view_cur(h), h))
            .unwrap_or(last);
        if self.view_cur(best) < rec.eff {
            best
        } else {
            last
        }
    }

    /// Mark `x` for a reschedule (one poke owed to whoever turns the mark on).
    fn mark(&mut self, x: usize) {
        // (a hart that is not in the scheduler is never marked: it looks at its queues when it joins, 3.9)
        if self.online[x] && !self.pending[x] {
            self.marks += 1;
            self.pending[x] = true;
            self.pokes |= 1 << x;
        }
    }

    /// Push: `tid` is queued (not running). One thread needs one hart: the online hart (other than the caller, who looks
    /// after itself in `settle`) that runs the lowest priority and would actually run it instead is marked, if the caller
    /// is not itself the better choice. (The spec's "owes exactly one IPI", 3.9; marking every lower hart sent up to
    /// `harts - 1` IPIs for one thread.)
    /// Returns whether the caller is itself the best hart for it (and is not where it is queued): then the caller must
    /// look at its own queues now (`settle`), as no one else will.
    fn push_marks(&mut self, tid: Tid, caller: usize) -> bool {
        let Some(&g) = self.loc.get(&tid) else {
            return false;
        };
        let Some(rec) = self.harts[g].record(tid) else {
            return false;
        };
        if rec.state != State::Runnable {
            return false;
        }
        let eff = rec.eff;
        let mut best: Option<(Prio, usize)> = None;
        for x in 0..self.harts.len() {
            if !self.online[x] {
                continue;
            }
            let ok = self.harts[g].record(tid).is_some_and(|r| allowed(r, x));
            if ok && self.harts[x].would_run(eff) {
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

    /// Pull into `h` the best queued thread of each other hart that outranks everything `h` has. Returns how many.
    fn pull(&mut self, h: usize) -> usize {
        let mut n = 0;
        for g in 0..self.harts.len() {
            if g == h {
                continue;
            }
            let mine = self
                .view_cur(h)
                .max(self.harts[h].queued_top().unwrap_or(0));
            let found = {
                let src = &self.harts[g];
                src.queued_top_where(&|t| src.record(t).is_some_and(|r| allowed(r, h)))
            };
            let Some((tid, p)) = found else { continue };
            if p <= mine || !self.harts[h].would_run(p) {
                // not for this hart (it has something at least as good, or would only queue it): hand it on, or the one
                // mark that was owed for it, which this hart consumed, would leave it waiting while a lower hart idles
                self.push_marks(tid, h);
                continue;
            }
            let rec = self.harts[g].detach_queued(tid).expect("it was queued");
            self.harts[h].attach(rec, Enq::Migrated, h as u8);
            self.loc.insert(tid, h);
            self.pulls += 1;
            n += 1;
        }
        n
    }

    /// Hart `h` makes its own designation right: pull what outranks it, preempt for what waits, and push what it leaves.
    fn settle(&mut self, h: usize) {
        self.settles += 1;
        self.pending[h] = false;
        loop {
            let pulled = self.pull(h);
            if !self.harts[h].wants_resched() {
                if pulled == 0 {
                    break;
                }
                continue;
            }
            let before = self.harts[h]
                .current()
                .filter(|&c| Some(c) != self.harts[h].idle_tid());
            self.harts[h].resched();
            if let Some(o) = before {
                if self.harts[h].state(o) == Some(State::Runnable) {
                    self.push_marks(o, h);
                }
            }
        }
        // whatever is still queued here may belong on a hart running something lower: for each other hart, the best queued
        // thread here that may run there (O(harts), not O(threads))
        self.push_queue(h);
    }

    /// A thread is left queued on `g`: look at the best one (and the best one that is not pinned, which any hart may take)
    /// and push each (one mark apiece, `push_marks`).
    fn push_queue(&mut self, g: usize) {
        let (top, free) = {
            let src = &self.harts[g];
            (
                src.queued_top_where(&|_| true).map(|(t, _)| t),
                src.queued_top_where(&|t| src.record(t).is_some_and(|r| r.affinity.is_none()))
                    .map(|(t, _)| t),
            )
        };
        for t in [top, free].into_iter().flatten() {
            self.push_marks(t, g);
        }
    }

    /// Hart `h` services a mark (or any reason to look at the queues): the only way `cur[h]` changes other than by `h`'s
    /// own block, yield and exit.
    pub fn reschedule_on(&mut self, h: usize) {
        self.settle(h);
    }

    // ---- creating threads

    /// Adopt the code running on `hart` (the boot thread) as a thread.
    pub fn adopt_current_on(
        &mut self,
        hart: usize,
        attr: SchedAttr,
        affinity: Option<usize>,
    ) -> Result<Tid, Error> {
        let tid = self.alloc_tid();
        self.harts[hart].adopt_current_as(tid, attr, affinity.map(|a| a as u8), hart as u8)?;
        self.loc.insert(tid, hart);
        Ok(tid)
    }

    /// A new runnable thread created by code running on `caller`.
    pub fn spawn_on(
        &mut self,
        caller: usize,
        attr: SchedAttr,
        affinity: Option<usize>,
    ) -> Result<Tid, Error> {
        if attr.prio > PRIO_MAX || attr.prio == 0 {
            return Err(Error::BadPriority); // 0 is the idle priority: idle threads are never queued
        }
        if affinity.is_some_and(|a| a >= self.harts.len()) {
            return Err(Error::NoSuchThread);
        }
        let tid = self.alloc_tid();
        let probe = ThreadSched {
            tid,
            base: attr,
            eff: attr.prio,
            demoted_window: 0,
            state: State::Runnable,
            affinity: affinity.map(|a| a as u8),
            last_hart: caller as u8,
            slice_left: 0,
            deadline: None,
            runnable_since: 0,
            started: 0,
            cpu: 0,
            group: 0,
            job: 0,
            class_words: [0; 4],
            park: crate::park::ParkState::Idle,
        };
        let target = self.select_hart(&probe);
        self.harts[target].fresh_as(
            tid,
            attr,
            State::Runnable,
            affinity.map(|a| a as u8),
            target as u8,
        )?;
        self.harts[target].enqueue_runnable(tid, Enq::New);
        self.loc.insert(tid, target);
        self.after_enqueue(tid, caller);
        Ok(tid)
    }

    /// The caller's reschedule or marks after `tid` was queued somewhere.
    fn after_enqueue(&mut self, tid: Tid, caller: usize) {
        let me_first = self.push_marks(tid, caller);
        let home = self.loc[&tid];
        if home == caller || me_first {
            self.settle(caller);
        }
    }

    // ---- blocking and waking

    /// The current thread of `hart` is about to wait: a wake from now on counts.
    pub fn prepare_block_on(&mut self, hart: usize) -> Result<(), Error> {
        self.harts[hart].prepare_block()
    }

    /// Block the current thread of `hart` unless a notification is waiting. Returns whether it blocked.
    pub fn block_on(&mut self, hart: usize) -> Result<bool, Error> {
        let blocked = self.harts[hart].block_current()?;
        if blocked {
            self.settle(hart);
        }
        Ok(blocked)
    }

    /// Sleep until `deadline` (nanoseconds on this hart's clock).
    pub fn sleep_until_on(&mut self, hart: usize, deadline: Time) -> Result<bool, Error> {
        let blocked = self.harts[hart].sleep_until(deadline)?;
        if blocked {
            self.settle(hart);
        }
        Ok(blocked)
    }

    /// Wake `tid` (a blocked thread is placed and queued; a prepared one is notified; anything else is left alone).
    pub fn wake_on(&mut self, caller: usize, tid: Tid) -> Result<(), Error> {
        let home = *self.loc.get(&tid).ok_or(Error::NoSuchThread)?;
        if self.harts[home].state(tid) != Some(State::Blocked) {
            self.harts[home].wake_quiet(tid)?;
            return Ok(());
        }
        let (rec, slept) = self.harts[home].detach_woken(tid)?;
        let target = self.select_hart(&rec);
        self.harts[target].attach(rec, Enq::Wake { slept }, target as u8);
        self.loc.insert(tid, target);
        self.after_enqueue(tid, caller);
        Ok(())
    }

    /// `wake_on`, saying whether the wake counted: the thread was blocked (now runnable) or had prepared to wait (now
    /// notified, so it will not block). Anything else is left alone and does not count.
    pub fn wake_counted_on(&mut self, caller: usize, tid: Tid) -> Result<bool, Error> {
        let rec = self.record(tid).ok_or(Error::NoSuchThread)?;
        let counts = match rec.state {
            State::Blocked => true,
            State::Running | State::Runnable => rec.park == crate::park::ParkState::Prepared,
            State::Exited => false,
        };
        self.wake_on(caller, tid)?;
        Ok(counts)
    }

    pub fn yield_on(&mut self, hart: usize) -> Result<(), Error> {
        let cur = self.harts[hart].current();
        self.harts[hart].yield_now()?;
        if let Some(c) = cur {
            self.push_marks(c, hart);
        }
        self.settle(hart);
        Ok(())
    }

    pub fn exit_on(&mut self, hart: usize) -> Result<(), Error> {
        self.harts[hart].exit_current()?;
        self.settle(hart);
        Ok(())
    }

    pub fn reap(&mut self, tid: Tid) -> Result<(), Error> {
        let home = *self.loc.get(&tid).ok_or(Error::NoSuchThread)?;
        self.harts[home].reap(tid)?;
        self.loc.remove(&tid);
        Ok(())
    }

    /// Change a thread's base priority; its hart is marked (or, if it is the caller's, looks at once).
    pub fn set_priority_on(&mut self, caller: usize, tid: Tid, prio: Prio) -> Result<(), Error> {
        if prio == 0 {
            return Err(Error::BadPriority);
        }
        let home = *self.loc.get(&tid).ok_or(Error::NoSuchThread)?;
        let changed = self.harts[home].set_priority_quiet(tid, prio)?;
        if !changed {
            return Ok(());
        }
        let me_first = self.push_marks(tid, caller);
        if home == caller || me_first {
            self.settle(caller);
        } else {
            self.mark(home);
        }
        Ok(())
    }

    /// Hart `hart`'s tick: the clock moves to `now`, its due timers fire, the running thread is accounted.
    pub fn tick_on(&mut self, hart: usize, now: Time) {
        self.harts[hart].tick_to(now);
        self.settle(hart);
        self.balance(hart);
    }

    /// Equal-priority work does not move by priority (no pull of an equal, 4.4), so queues of equals would stay as lopsided as
    /// they were made: a thread on a long queue waits a whole rotation of it. At a tick, a hart whose queue is at least two
    /// shorter than another's takes one thread from it that it may run and that is not worse than what it runs. One per
    /// tick; with queues within one of each other nothing moves, so equals do not churn between harts.
    fn balance(&mut self, h: usize) {
        let mine = self.harts[h].queued_len();
        let busiest = (0..self.harts.len())
            .filter(|&g| g != h && self.online[g])
            .max_by_key(|&g| (self.harts[g].queued_len(), core::cmp::Reverse(g)));
        let Some(g) = busiest else { return };
        if self.harts[g].queued_len() < mine + 2 {
            return;
        }
        let cur = self.view_cur(h);
        let found = {
            let src = &self.harts[g];
            src.queued_top_where(&|t| src.record(t).is_some_and(|r| allowed(r, h)))
        };
        let Some((tid, p)) = found else { return };
        if p < cur {
            return;
        }
        let rec = self.harts[g].detach_queued(tid).expect("it was queued");
        self.harts[h].attach(rec, Enq::Migrated, h as u8);
        self.loc.insert(tid, h);
        self.pulls += 1;
    }

    pub fn bad_picks(&self) -> u32 {
        self.harts.iter().map(|h| h.bad_picks).sum()
    }

    // ---- the invariants

    /// **GP2** (always) and, when `quiescent`, **GP1**.
    pub fn audit(&self, quiescent: bool) -> Result<(), String> {
        for (h, rq) in self.harts.iter().enumerate() {
            rq.audit().map_err(|e| alloc::format!("hart {h}: {e}"))?;
        }
        let mut seen = 0usize;
        for (h, rq) in self.harts.iter().enumerate() {
            for t in rq.tids() {
                seen += 1;
                if self.loc.get(&t) != Some(&h) {
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
        if seen != self.loc.len() {
            return Err(alloc::format!("{seen} records, {} located", self.loc.len()));
        }
        if !quiescent {
            return Ok(());
        }
        // GP1, per class: no online hart runs something a waiting thread would displace there (priority order for the
        // fixed-priority and real-time classes; for round robin, work conservation: no hart idles while a thread it may
        // run waits)
        let queued: Vec<(usize, Tid, Prio, Option<u8>)> = self
            .harts
            .iter()
            .enumerate()
            .flat_map(|(g, rq)| {
                rq.tids().filter(|&t| rq.is_queued(t)).map(move |t| {
                    let r = rq.record(t).expect("queued");
                    (g, t, r.eff, r.affinity)
                })
            })
            .collect();
        for x in 0..self.harts.len() {
            if !self.online[x] {
                continue;
            }
            for &(g, t, eff, aff) in &queued {
                if aff.is_none_or(|a| a as usize == x) && self.harts[x].would_run(eff) {
                    return Err(alloc::format!(
                        "GP1: hart {x} runs priority {} while thread {t} (priority {eff}) waits on hart {g}",
                        self.view_cur(x)
                    ));
                }
            }
        }
        Ok(())
    }
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
    pub fn wait_on<N: Class>(&mut self, sched: &mut Sched2<N>, hart: usize) -> Result<bool, Error> {
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
    pub fn wake_one_on<N: Class>(&mut self, sched: &mut Sched2<N>, caller: usize) -> Option<Tid> {
        while let Some(tid) = self.waiters.pop_front() {
            if sched.wake_counted_on(caller, tid) == Ok(true) {
                return Some(tid);
            }
        }
        None
    }

    pub fn wake_all_on<N: Class>(&mut self, sched: &mut Sched2<N>, caller: usize) -> usize {
        let mut n = 0;
        while self.wake_one_on(sched, caller).is_some() {
            n += 1;
        }
        n
    }
}
