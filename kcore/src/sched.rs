//! The scheduler core: thread states, the single block/wake path, sleeping, time
//! slices, and the scheduling-class interface (OS-ARCHITECTURE.md, section 8).
//!
//! Pure logic with a simulated clock (`tick`); the kernel binary drives it from
//! the timer interrupt and performs the actual context switches. The core keeps
//! mechanism (who is in which state, the wake token, timers, slices, validation of
//! the policy's choices); a [`SchedClass`] supplies policy (the run queues and which
//! thread is best).
//!
//! The core maintains one invariant after every operation: [`Sched::current`] is
//! *the thread that should be running now*. The kernel compares it with the thread
//! that is actually running and switches if they differ. A thread is in exactly one
//! place: running, in the class's run queue, blocked (possibly with a timer), or
//! exited. [`Sched::audit`] checks that.
//!
//! Wake/block semantics (INVARIANTS.md C3): [`Sched::wake`] of a thread that is not
//! blocked leaves a *wake token*; the thread's next [`Sched::block_current`] consumes
//! the token and does not block. So a wake that races ahead of the block is never
//! lost, and several wakes before one block count once (wakeups do not queue).
//! The one exception is [`Sched::sleep_until`], which discards a token left from
//! before the call: a sleep is for *time*, and a stale wake must not cut it short.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

pub type Tid = u32;
pub type Prio = u8;

/// The idle thread's priority; nothing else uses it.
pub const PRIO_IDLE: Prio = 0;
/// Normal band: 1 to 31. Real-time band: 32 to 63, always above normal.
pub const PRIO_NORMAL_MAX: Prio = 31;
pub const PRIO_RT_MIN: Prio = 32;
pub const PRIO_MAX: Prio = 63;
/// Ticks a thread runs before yielding to an equal-priority peer.
pub const TIMESLICE_TICKS: u32 = 10;

/// Scheduling policy: the run queues. Selected at boot (OD5); the core owns every
/// piece of mechanism, so a class can only choose.
pub trait SchedClass {
    fn name(&self) -> &'static str;
    /// A thread became runnable and goes to the back of its level.
    fn enqueue_tail(&mut self, tid: Tid, prio: Prio);
    /// A thread was preempted and goes to the front of its level.
    fn enqueue_head(&mut self, tid: Tid, prio: Prio);
    /// Take the best runnable thread out of the queues.
    fn pick_next(&mut self) -> Option<Tid> {
        self.pick_next_where(&|_| true)
    }
    /// Take the best runnable thread for which `ok` holds (the threads that may run on one
    /// hart: affinity). Threads for which it does not hold stay queued, in order.
    fn pick_next_where(&mut self, ok: &dyn Fn(Tid) -> bool) -> Option<Tid>;
    /// Remove a specific thread (it is in the queues, or not).
    fn remove(&mut self, tid: Tid);
    fn contains(&self, tid: Tid) -> bool;
    fn len(&self) -> usize;
    /// Is a thread waiting that should run *instead of* a running thread of `running`?
    fn would_preempt(&self, running: Prio) -> bool {
        self.would_preempt_where(running, &|_| true)
    }
    /// Is a thread waiting that should share the CPU with a running thread of
    /// `running` when its time slice ends?
    fn would_rotate(&self, running: Prio) -> bool {
        self.would_rotate_where(running, &|_| true)
    }
    /// `would_preempt`, considering only the threads for which `ok` holds.
    fn would_preempt_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool;
    /// `would_rotate`, considering only the threads for which `ok` holds.
    fn would_rotate_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Fixed priority, round robin within a level (the default policy).
pub struct FixedPriority {
    levels: Vec<VecDeque<Tid>>,
}

impl FixedPriority {
    pub fn new() -> FixedPriority {
        FixedPriority {
            levels: (0..=PRIO_MAX as usize).map(|_| VecDeque::new()).collect(),
        }
    }

    fn best_level_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<Prio> {
        (0..=PRIO_MAX)
            .rev()
            .find(|&p| self.levels[p as usize].iter().any(|&t| ok(t)))
    }
}

impl Default for FixedPriority {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedClass for FixedPriority {
    fn name(&self) -> &'static str {
        "fixed-priority"
    }
    fn enqueue_tail(&mut self, tid: Tid, prio: Prio) {
        self.levels[prio as usize].push_back(tid);
    }
    fn enqueue_head(&mut self, tid: Tid, prio: Prio) {
        self.levels[prio as usize].push_front(tid);
    }
    fn pick_next_where(&mut self, ok: &dyn Fn(Tid) -> bool) -> Option<Tid> {
        for level in self.levels.iter_mut().rev() {
            if let Some(i) = level.iter().position(|&t| ok(t)) {
                return level.remove(i);
            }
        }
        None
    }
    fn remove(&mut self, tid: Tid) {
        for level in &mut self.levels {
            level.retain(|&t| t != tid);
        }
    }
    fn contains(&self, tid: Tid) -> bool {
        self.levels.iter().any(|l| l.contains(&tid))
    }
    fn len(&self) -> usize {
        self.levels.iter().map(VecDeque::len).sum()
    }
    fn would_preempt_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool {
        self.best_level_where(ok).is_some_and(|p| p > running)
    }
    fn would_rotate_where(&self, running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool {
        self.best_level_where(ok).is_some_and(|p| p >= running)
    }
}

/// Plain round robin that ignores priorities: a second policy, to prove classes
/// are pluggable and that the conformance suite is about *every* policy.
#[derive(Default)]
pub struct RoundRobin {
    queue: VecDeque<Tid>,
}

impl RoundRobin {
    pub fn new() -> RoundRobin {
        RoundRobin::default()
    }
}

impl SchedClass for RoundRobin {
    fn name(&self) -> &'static str {
        "round-robin"
    }
    fn enqueue_tail(&mut self, tid: Tid, _prio: Prio) {
        self.queue.push_back(tid);
    }
    fn enqueue_head(&mut self, tid: Tid, _prio: Prio) {
        self.queue.push_front(tid);
    }
    fn pick_next_where(&mut self, ok: &dyn Fn(Tid) -> bool) -> Option<Tid> {
        let i = self.queue.iter().position(|&t| ok(t))?;
        self.queue.remove(i)
    }
    fn remove(&mut self, tid: Tid) {
        self.queue.retain(|&t| t != tid);
    }
    fn contains(&self, tid: Tid) -> bool {
        self.queue.contains(&tid)
    }
    fn len(&self) -> usize {
        self.queue.len()
    }
    fn would_preempt_where(&self, _running: Prio, _ok: &dyn Fn(Tid) -> bool) -> bool {
        false
    }
    fn would_rotate_where(&self, _running: Prio, ok: &dyn Fn(Tid) -> bool) -> bool {
        self.queue.iter().any(|&t| ok(t))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Designated to run now (see the module comment).
    Running,
    /// In the class's run queue.
    Runnable,
    /// Waiting for a wake (and perhaps a timer).
    Blocked,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedError {
    NoSuchThread,
    BadPriority,
    /// The operation needs a current thread and there is none.
    NoCurrent,
    /// `adopt_current` while a thread is already running.
    HasCurrent,
    /// `reap` of a thread that has not exited.
    NotExited,
    /// A hart index outside the scheduler's harts.
    BadHart,
}

struct Thread {
    prio: Prio,
    state: State,
    token: bool,
    timer: Option<u64>,
    /// Ticks left of its time slice when it next runs: a full slice, except for a
    /// thread that was preempted, which keeps what it had left (so a periodic
    /// higher-priority thread cannot keep resetting its peers' slices and starve
    /// the thread next in line).
    slice: u32,
    /// The only hart this thread may run on (`None`: any).
    affinity: Option<usize>,
    /// The hart it is designated to run on while `Running`.
    hart: Option<usize>,
}

/// The most harts the core handles (the pokes are a bitmask).
pub const MAX_SCHED_HARTS: usize = 32;

pub struct Sched<C: SchedClass> {
    class: C,
    threads: BTreeMap<Tid, Thread>,
    /// `cur[h]`: the thread designated to run on hart `h`. **Only hart `h` ever changes it**
    /// (S5, spec section 4): a wake or tick on another hart never re-designates it, it marks
    /// `resched_pending[h]` and pokes the hart, which calls [`Sched::reschedule_on`] itself.
    cur: Vec<Option<Tid>>,
    next_tid: Tid,
    now: u64,
    timers: BTreeSet<(u64, Tid)>,
    slice_left: Vec<u32>,
    resched_pending: Vec<bool>,
    /// Harts that were marked since the last [`Sched::take_pokes`].
    pokes: u32,
    /// How many times the class returned something the core had to reject.
    pub bad_picks: u32,
}

impl<C: SchedClass> Sched<C> {
    /// A scheduler for one hart (the original interface; hart 0).
    pub fn new(class: C) -> Sched<C> {
        Sched::with_harts(class, 1)
    }

    /// A scheduler for `harts` harts (1 to [`MAX_SCHED_HARTS`]).
    pub fn with_harts(class: C, harts: usize) -> Sched<C> {
        assert!((1..=MAX_SCHED_HARTS).contains(&harts), "1 to 32 harts");
        Sched {
            class,
            threads: BTreeMap::new(),
            cur: alloc::vec![None; harts],
            next_tid: 1,
            now: 0,
            timers: BTreeSet::new(),
            slice_left: alloc::vec![TIMESLICE_TICKS; harts],
            resched_pending: alloc::vec![false; harts],
            pokes: 0,
            bad_picks: 0,
        }
    }

    pub fn harts(&self) -> usize {
        self.cur.len()
    }
    pub fn class(&self) -> &C {
        &self.class
    }
    pub fn now(&self) -> u64 {
        self.now
    }
    /// The thread that should be running on hart 0 now (the single-hart interface).
    pub fn current(&self) -> Option<Tid> {
        self.cur[0]
    }
    /// The thread designated to run on `hart` now.
    pub fn current_on(&self, hart: usize) -> Option<Tid> {
        self.cur[hart]
    }
    /// Is a reschedule pending for `hart` (another hart saw something better to run there)?
    pub fn resched_pending(&self, hart: usize) -> bool {
        self.resched_pending[hart]
    }
    /// The harts to send a reschedule IPI to (bitmask by hart index), clearing the set.
    pub fn take_pokes(&mut self) -> u32 {
        core::mem::take(&mut self.pokes)
    }
    pub fn state(&self, tid: Tid) -> Option<State> {
        self.threads.get(&tid).map(|t| t.state)
    }
    pub fn priority(&self, tid: Tid) -> Option<Prio> {
        self.threads.get(&tid).map(|t| t.prio)
    }
    pub fn affinity(&self, tid: Tid) -> Option<usize> {
        self.threads.get(&tid)?.affinity
    }
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }
    /// The tick at which a blocked thread's timer fires.
    pub fn timer_of(&self, tid: Tid) -> Option<u64> {
        self.threads.get(&tid)?.timer
    }

    fn fresh(
        &mut self,
        prio: Prio,
        state: State,
        affinity: Option<usize>,
    ) -> Result<Tid, SchedError> {
        if prio > PRIO_MAX {
            return Err(SchedError::BadPriority);
        }
        if affinity.is_some_and(|h| h >= self.cur.len()) {
            return Err(SchedError::BadHart);
        }
        let tid = self.next_tid;
        self.next_tid += 1;
        self.threads.insert(
            tid,
            Thread {
                prio,
                state,
                token: false,
                timer: None,
                slice: TIMESLICE_TICKS,
                affinity,
                hart: None,
            },
        );
        Ok(tid)
    }

    /// May `tid` run on `hart`?
    fn allowed(&self, tid: Tid, hart: usize) -> bool {
        self.threads
            .get(&tid)
            .is_some_and(|t| t.affinity.is_none_or(|a| a == hart))
    }

    /// Adopt the code that is already running (the boot thread) as a thread (hart 0).
    pub fn adopt_current(&mut self, prio: Prio) -> Result<Tid, SchedError> {
        self.adopt_current_on(0, prio, None)
    }

    /// Adopt the code running on `hart` as a thread, optionally pinned.
    pub fn adopt_current_on(
        &mut self,
        hart: usize,
        prio: Prio,
        affinity: Option<usize>,
    ) -> Result<Tid, SchedError> {
        if self.cur[hart].is_some() {
            return Err(SchedError::HasCurrent);
        }
        let tid = self.fresh(prio, State::Running, affinity)?;
        self.threads.get_mut(&tid).expect("fresh").hart = Some(hart);
        self.cur[hart] = Some(tid);
        self.slice_left[hart] = TIMESLICE_TICKS;
        Ok(tid)
    }

    /// A new runnable thread. It may preempt the current one at once (hart 0 is the caller).
    pub fn spawn(&mut self, prio: Prio) -> Result<Tid, SchedError> {
        self.spawn_on(0, prio, None)
    }

    /// A new runnable thread created by code running on `caller`, allowed to run only on
    /// `affinity` if given.
    pub fn spawn_on(
        &mut self,
        caller: usize,
        prio: Prio,
        affinity: Option<usize>,
    ) -> Result<Tid, SchedError> {
        let tid = self.fresh(prio, State::Runnable, affinity)?;
        self.class.enqueue_tail(tid, prio);
        self.resched_all(caller);
        Ok(tid)
    }

    /// After something became runnable (or a priority changed): for every hart whose current
    /// thread something better is waiting for, preempt now if it is the caller's own hart, else
    /// mark it pending and poke it.
    fn resched_all(&mut self, caller: usize) {
        for h in 0..self.cur.len() {
            let better = match self.cur[h] {
                None => true,
                Some(c) => {
                    let prio = self.threads[&c].prio;
                    self.class
                        .would_preempt_where(prio, &|t| self.allowed_ro(t, h))
                }
            };
            if !better {
                continue;
            }
            if h == caller {
                self.reschedule_on(h);
            } else if !self.resched_pending[h] {
                self.resched_pending[h] = true;
                self.pokes |= 1 << h;
            }
        }
    }

    /// `allowed` for use inside closures that already borrow `self.class`.
    fn allowed_ro(&self, tid: Tid, hart: usize) -> bool {
        self.allowed(tid, hart)
    }

    /// Hart `hart` (the caller) makes its own designation right: choose a thread if there is
    /// none, and preempt if a better one is waiting. Clears this hart's pending mark.
    pub fn reschedule_on(&mut self, hart: usize) {
        self.resched_pending[hart] = false;
        match self.cur[hart] {
            None => self.dispatch(hart),
            Some(c) => {
                let prio = self.threads[&c].prio;
                let better = {
                    let this: &Sched<C> = self;
                    this.class
                        .would_preempt_where(prio, &|t| this.allowed_ro(t, hart))
                };
                if better {
                    let t = self.threads.get_mut(&c).expect("current exists");
                    t.state = State::Runnable;
                    t.hart = None;
                    t.slice = self.slice_left[hart].max(1);
                    self.class.enqueue_head(c, prio);
                    self.cur[hart] = None;
                    self.dispatch(hart);
                    // the preempted thread is queued again: another hart may be the place for it
                    self.resched_others(hart);
                }
            }
        }
    }

    /// Pick the next thread for `hart` from the class, rejecting anything that is not a
    /// runnable thread (the guard against a faulty policy).
    fn dispatch(&mut self, hart: usize) {
        debug_assert!(self.cur[hart].is_none());
        loop {
            let picked = {
                let threads = &self.threads;
                self.class.pick_next_where(&|t| {
                    threads
                        .get(&t)
                        .is_none_or(|th| th.affinity.is_none_or(|a| a == hart))
                })
            };
            let Some(tid) = picked else { return };
            match self.threads.get_mut(&tid) {
                Some(t) if t.state == State::Runnable => {
                    t.state = State::Running;
                    t.hart = Some(hart);
                    self.slice_left[hart] = core::mem::replace(&mut t.slice, TIMESLICE_TICKS);
                    self.cur[hart] = Some(tid);
                    return;
                }
                _ => self.bad_picks += 1,
            }
        }
    }

    /// Block the current thread until woken, unless a wake token is waiting (then
    /// it is consumed and the thread keeps running). Returns whether it blocked.
    pub fn block_current(&mut self) -> Result<bool, SchedError> {
        self.block_current_on(0)
    }

    pub fn block_current_on(&mut self, hart: usize) -> Result<bool, SchedError> {
        let c = self.cur[hart].ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        if core::mem::take(&mut t.token) {
            return Ok(false);
        }
        t.state = State::Blocked;
        t.hart = None;
        self.cur[hart] = None;
        self.dispatch(hart);
        Ok(true)
    }

    /// Sleep until tick `deadline` or an earlier wake. A deadline that has passed
    /// does not block. Any wake token left from before is discarded.
    pub fn sleep_until(&mut self, deadline: u64) -> Result<bool, SchedError> {
        self.sleep_until_on(0, deadline)
    }

    pub fn sleep_until_on(&mut self, hart: usize, deadline: u64) -> Result<bool, SchedError> {
        let c = self.cur[hart].ok_or(SchedError::NoCurrent)?;
        self.threads.get_mut(&c).expect("current exists").token = false;
        if deadline <= self.now {
            return Ok(false);
        }
        self.threads.get_mut(&c).expect("current exists").timer = Some(deadline);
        self.timers.insert((deadline, c));
        self.block_current_on(hart)
    }

    /// Wake a thread (see the module comment for the token rule); hart 0 is the caller.
    pub fn wake(&mut self, tid: Tid) -> Result<(), SchedError> {
        self.wake_on(0, tid)
    }

    pub fn wake_on(&mut self, caller: usize, tid: Tid) -> Result<(), SchedError> {
        let t = self.threads.get_mut(&tid).ok_or(SchedError::NoSuchThread)?;
        match t.state {
            State::Blocked => {
                if let Some(deadline) = t.timer.take() {
                    self.timers.remove(&(deadline, tid));
                }
                t.state = State::Runnable;
                let prio = t.prio;
                self.class.enqueue_tail(tid, prio);
                self.resched_all(caller);
            }
            State::Running | State::Runnable => t.token = true,
            State::Exited => {}
        }
        Ok(())
    }

    /// The current thread gives up the rest of its slice.
    pub fn yield_current(&mut self) -> Result<(), SchedError> {
        self.yield_current_on(0)
    }

    pub fn yield_current_on(&mut self, hart: usize) -> Result<(), SchedError> {
        let c = self.cur[hart].ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        t.state = State::Runnable;
        t.hart = None;
        let prio = t.prio;
        self.class.enqueue_tail(c, prio);
        self.cur[hart] = None;
        self.dispatch(hart);
        // (a thread queued for another hart may now have something to run on)
        self.resched_others(hart);
        Ok(())
    }

    /// The current thread is finished for good.
    pub fn exit_current(&mut self) -> Result<(), SchedError> {
        self.exit_current_on(0)
    }

    pub fn exit_current_on(&mut self, hart: usize) -> Result<(), SchedError> {
        let c = self.cur[hart].ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        t.state = State::Exited;
        t.hart = None;
        t.token = false;
        self.cur[hart] = None;
        self.dispatch(hart);
        Ok(())
    }

    /// Re-check every hart but `caller` (used after the caller put a thread back in the queue).
    fn resched_others(&mut self, caller: usize) {
        for h in 0..self.cur.len() {
            if h == caller {
                continue;
            }
            let better = match self.cur[h] {
                None => true,
                Some(c) => {
                    let prio = self.threads[&c].prio;
                    self.class
                        .would_preempt_where(prio, &|t| self.allowed_ro(t, h))
                }
            };
            if better && !self.resched_pending[h] {
                self.resched_pending[h] = true;
                self.pokes |= 1 << h;
            }
        }
    }

    /// Forget an exited thread's record (its stack is the caller's to free).
    pub fn reap(&mut self, tid: Tid) -> Result<(), SchedError> {
        match self.threads.get(&tid) {
            Some(t) if t.state == State::Exited => {
                self.threads.remove(&tid);
                Ok(())
            }
            Some(_) => Err(SchedError::NotExited),
            None => Err(SchedError::NoSuchThread),
        }
    }

    pub fn set_priority(&mut self, tid: Tid, prio: Prio) -> Result<(), SchedError> {
        self.set_priority_on(0, tid, prio)
    }

    pub fn set_priority_on(
        &mut self,
        caller: usize,
        tid: Tid,
        prio: Prio,
    ) -> Result<(), SchedError> {
        if prio > PRIO_MAX {
            return Err(SchedError::BadPriority);
        }
        let t = self.threads.get_mut(&tid).ok_or(SchedError::NoSuchThread)?;
        let old = core::mem::replace(&mut t.prio, prio);
        match t.state {
            State::Runnable if old != prio => {
                self.class.remove(tid);
                self.class.enqueue_tail(tid, prio);
                self.resched_all(caller);
            }
            State::Running => self.resched_all(caller),
            _ => {}
        }
        Ok(())
    }

    /// One timer tick on hart 0 of a one-hart scheduler: advance the clock by one, fire timers,
    /// account the time slice.
    pub fn tick(&mut self) {
        self.advance_to(self.now + 1, 0);
        self.tick_slice(0);
    }

    /// Advance the clock to `now` (never backwards) and fire the timers that are due; `caller`
    /// is the hart doing it (it reschedules itself at once; others are marked and poked).
    pub fn advance_to(&mut self, now: u64, caller: usize) {
        if now > self.now {
            self.now = now;
        }
        let due: Vec<(u64, Tid)> = self.timers.range(..(self.now + 1, 0)).copied().collect();
        for (deadline, tid) in due {
            self.timers.remove(&(deadline, tid));
            if let Some(t) = self.threads.get_mut(&tid) {
                t.timer = None;
                if t.state == State::Blocked {
                    t.state = State::Runnable;
                    let prio = t.prio;
                    self.class.enqueue_tail(tid, prio);
                }
            }
        }
        self.resched_all(caller);
    }

    /// Hart `hart`'s own timer tick: account the time slice of its current thread, rotating it
    /// to the back of its level when the slice is over and an equal or better thread waits.
    pub fn tick_slice(&mut self, hart: usize) {
        if let Some(c) = self.cur[hart] {
            self.slice_left[hart] = self.slice_left[hart].saturating_sub(1);
            if self.slice_left[hart] == 0 {
                let prio = self.threads[&c].prio;
                let rotate = {
                    let this: &Sched<C> = self;
                    this.class
                        .would_rotate_where(prio, &|t| this.allowed_ro(t, hart))
                };
                if rotate {
                    let t = self.threads.get_mut(&c).expect("current exists");
                    t.state = State::Runnable;
                    t.hart = None;
                    self.class.enqueue_tail(c, prio);
                    self.cur[hart] = None;
                    self.dispatch(hart);
                    self.resched_others(hart);
                } else {
                    self.slice_left[hart] = TIMESLICE_TICKS;
                }
            }
        }
        self.reschedule_on(hart);
    }

    /// Check the "exactly one place" rule, the per-hart rules and the timer bookkeeping.
    pub fn audit(&self) -> Result<(), String> {
        let mut running = 0;
        for (&tid, t) in &self.threads {
            let queued = self.class.contains(tid);
            let timed = self.timers.iter().filter(|&&(_, x)| x == tid).count();
            match t.state {
                State::Running => {
                    running += 1;
                    let Some(h) = t.hart else {
                        return Err(alloc::format!("thread {tid} is Running on no hart"));
                    };
                    if self.cur[h] != Some(tid) {
                        return Err(alloc::format!(
                            "thread {tid} is Running on hart {h} but that hart's current is {:?}",
                            self.cur[h]
                        ));
                    }
                    if queued {
                        return Err(alloc::format!("running thread {tid} is also queued"));
                    }
                    if t.affinity.is_some_and(|a| a != h) {
                        return Err(alloc::format!(
                            "thread {tid} is pinned to {:?} but runs on hart {h}",
                            t.affinity
                        ));
                    }
                }
                State::Runnable if !queued => {
                    return Err(alloc::format!("runnable thread {tid} is in no queue"))
                }
                State::Blocked | State::Exited if queued => {
                    return Err(alloc::format!(
                        "{:?} thread {tid} is in a run queue",
                        t.state
                    ))
                }
                _ => {}
            }
            if t.state != State::Running && t.hart.is_some() {
                return Err(alloc::format!("thread {tid} ({:?}) has a hart", t.state));
            }
            if t.state != State::Blocked && (timed != 0 || t.timer.is_some()) {
                return Err(alloc::format!("thread {tid} ({:?}) has a timer", t.state));
            }
            if t.state == State::Blocked && t.timer.is_some() != (timed == 1) {
                return Err(alloc::format!(
                    "thread {tid}: timer record disagrees with the timer set"
                ));
            }
            if t.state == State::Exited && t.token {
                return Err(alloc::format!("exited thread {tid} holds a wake token"));
            }
        }
        let designated: Vec<Tid> = self.cur.iter().flatten().copied().collect();
        if designated.len() != running {
            return Err(alloc::format!(
                "{running} running threads, {} designated",
                designated.len()
            ));
        }
        for (i, a) in designated.iter().enumerate() {
            if designated[..i].contains(a) {
                return Err(alloc::format!("thread {a} is designated on two harts"));
            }
        }
        let queued_total: usize = self
            .threads
            .values()
            .filter(|t| t.state == State::Runnable)
            .count();
        if queued_total != self.class.len() {
            return Err(alloc::format!(
                "{queued_total} runnable threads but {} queued entries",
                self.class.len()
            ));
        }
        // Every hart runs the best thread that may run there, or a reschedule is pending for it.
        for h in 0..self.cur.len() {
            if let Some(c) = self.cur[h] {
                let prio = self.threads[&c].prio;
                let better = self
                    .class
                    .would_preempt_where(prio, &|t| self.allowed(t, h));
                if better && !self.resched_pending[h] {
                    return Err(alloc::format!(
                        "hart {h} runs {c} (priority {prio}) while a better thread waits and no reschedule is pending"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A FIFO of threads waiting for something, built on `block_current`/`wake`.
/// `wait` and `wake_one` are called with the scheduler's lock held (interrupts off
/// in the kernel), so adding oneself and blocking is atomic with respect to wakers:
/// there is no window for a lost wakeup.
#[derive(Default)]
pub struct WaitQueue {
    waiters: VecDeque<Tid>,
}

impl WaitQueue {
    pub fn new() -> WaitQueue {
        WaitQueue::default()
    }

    pub fn len(&self) -> usize {
        self.waiters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    /// Queue the current thread and block it. Returns whether it blocked (false if a
    /// wake token let it through; it is then no longer queued).
    pub fn wait<C: SchedClass>(&mut self, sched: &mut Sched<C>) -> Result<bool, SchedError> {
        self.wait_on(sched, 0)
    }

    /// `wait` for the thread running on `hart`.
    pub fn wait_on<C: SchedClass>(
        &mut self,
        sched: &mut Sched<C>,
        hart: usize,
    ) -> Result<bool, SchedError> {
        let me = sched.current_on(hart).ok_or(SchedError::NoCurrent)?;
        // A thread is queued at most once: a stale entry (it was woken some other
        // way) must not stay behind to swallow a later wake_one.
        self.waiters.retain(|&t| t != me);
        self.waiters.push_back(me);
        let blocked = sched.block_current_on(hart)?;
        if !blocked {
            self.waiters.retain(|&t| t != me);
        }
        Ok(blocked)
    }

    /// Like `wait`, but also wakes at tick `deadline` (a timeout). Returns whether it
    /// blocked; after waking the caller tells a wake from a timeout by re-checking its
    /// condition and the clock. A thread woken by the deadline leaves a stale entry here,
    /// which `wake_one` skips (it is not blocked) and the thread's next wait replaces.
    pub fn wait_until<C: SchedClass>(
        &mut self,
        sched: &mut Sched<C>,
        deadline: u64,
    ) -> Result<bool, SchedError> {
        self.wait_until_on(sched, 0, deadline)
    }

    pub fn wait_until_on<C: SchedClass>(
        &mut self,
        sched: &mut Sched<C>,
        hart: usize,
        deadline: u64,
    ) -> Result<bool, SchedError> {
        let me = sched.current_on(hart).ok_or(SchedError::NoCurrent)?;
        self.waiters.retain(|&t| t != me);
        self.waiters.push_back(me);
        let blocked = sched.sleep_until_on(hart, deadline)?;
        if !blocked {
            self.waiters.retain(|&t| t != me);
        }
        Ok(blocked)
    }

    /// Wake the longest-waiting thread, if any.
    pub fn wake_one<C: SchedClass>(&mut self, sched: &mut Sched<C>) -> Option<Tid> {
        self.wake_one_on(sched, 0)
    }

    /// `wake_one`, done by code running on `caller`.
    pub fn wake_one_on<C: SchedClass>(
        &mut self,
        sched: &mut Sched<C>,
        caller: usize,
    ) -> Option<Tid> {
        while let Some(tid) = self.waiters.pop_front() {
            // Only a thread that is actually blocked counts as woken. Anything else is
            // stale (woken some other way, or gone) and is dropped: a waiter that is
            // merely running re-checks its condition before it waits again.
            if sched.state(tid) == Some(State::Blocked) && sched.wake_on(caller, tid).is_ok() {
                return Some(tid);
            }
        }
        None
    }

    pub fn wake_all<C: SchedClass>(&mut self, sched: &mut Sched<C>) -> usize {
        self.wake_all_on(sched, 0)
    }

    pub fn wake_all_on<C: SchedClass>(&mut self, sched: &mut Sched<C>, caller: usize) -> usize {
        let mut n = 0;
        while self.wake_one_on(sched, caller).is_some() {
            n += 1;
        }
        n
    }
}
