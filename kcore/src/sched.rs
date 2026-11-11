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
    fn pick_next(&mut self) -> Option<Tid>;
    /// Remove a specific thread (it is in the queues, or not).
    fn remove(&mut self, tid: Tid);
    fn contains(&self, tid: Tid) -> bool;
    fn len(&self) -> usize;
    /// Is a thread waiting that should run *instead of* a running thread of `running`?
    fn would_preempt(&self, running: Prio) -> bool;
    /// Is a thread waiting that should share the CPU with a running thread of
    /// `running` when its time slice ends?
    fn would_rotate(&self, running: Prio) -> bool;
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

    fn best_level(&self) -> Option<Prio> {
        (0..=PRIO_MAX)
            .rev()
            .find(|&p| !self.levels[p as usize].is_empty())
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
    fn pick_next(&mut self) -> Option<Tid> {
        let level = self.best_level()?;
        self.levels[level as usize].pop_front()
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
    fn would_preempt(&self, running: Prio) -> bool {
        self.best_level().is_some_and(|p| p > running)
    }
    fn would_rotate(&self, running: Prio) -> bool {
        self.best_level().is_some_and(|p| p >= running)
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
    fn pick_next(&mut self) -> Option<Tid> {
        self.queue.pop_front()
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
    fn would_preempt(&self, _running: Prio) -> bool {
        false
    }
    fn would_rotate(&self, _running: Prio) -> bool {
        !self.queue.is_empty()
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
}

pub struct Sched<C: SchedClass> {
    class: C,
    threads: BTreeMap<Tid, Thread>,
    current: Option<Tid>,
    next_tid: Tid,
    now: u64,
    timers: BTreeSet<(u64, Tid)>,
    slice_left: u32,
    /// How many times the class returned something the core had to reject.
    pub bad_picks: u32,
}

impl<C: SchedClass> Sched<C> {
    pub fn new(class: C) -> Sched<C> {
        Sched {
            class,
            threads: BTreeMap::new(),
            current: None,
            next_tid: 1,
            now: 0,
            timers: BTreeSet::new(),
            slice_left: TIMESLICE_TICKS,
            bad_picks: 0,
        }
    }

    pub fn class(&self) -> &C {
        &self.class
    }
    pub fn now(&self) -> u64 {
        self.now
    }
    /// The thread that should be running now.
    pub fn current(&self) -> Option<Tid> {
        self.current
    }
    pub fn state(&self, tid: Tid) -> Option<State> {
        self.threads.get(&tid).map(|t| t.state)
    }
    pub fn priority(&self, tid: Tid) -> Option<Prio> {
        self.threads.get(&tid).map(|t| t.prio)
    }
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }
    /// The tick at which a blocked thread's timer fires.
    pub fn timer_of(&self, tid: Tid) -> Option<u64> {
        self.threads.get(&tid)?.timer
    }

    fn fresh(&mut self, prio: Prio, state: State) -> Result<Tid, SchedError> {
        if prio > PRIO_MAX {
            return Err(SchedError::BadPriority);
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
            },
        );
        Ok(tid)
    }

    /// Adopt the code that is already running (the boot thread) as a thread.
    pub fn adopt_current(&mut self, prio: Prio) -> Result<Tid, SchedError> {
        if self.current.is_some() {
            return Err(SchedError::HasCurrent);
        }
        let tid = self.fresh(prio, State::Running)?;
        self.current = Some(tid);
        self.slice_left = TIMESLICE_TICKS;
        Ok(tid)
    }

    /// A new runnable thread. It may preempt the current one at once.
    pub fn spawn(&mut self, prio: Prio) -> Result<Tid, SchedError> {
        let tid = self.fresh(prio, State::Runnable)?;
        self.class.enqueue_tail(tid, prio);
        self.resched();
        Ok(tid)
    }

    /// Make sure `current` is the right thread: choose one if there is none, and
    /// preempt if a better one is waiting.
    fn resched(&mut self) {
        match self.current {
            None => self.dispatch(),
            Some(c) => {
                let prio = self.threads[&c].prio;
                if self.class.would_preempt(prio) {
                    let t = self.threads.get_mut(&c).expect("current exists");
                    t.state = State::Runnable;
                    t.slice = self.slice_left.max(1);
                    self.class.enqueue_head(c, prio);
                    self.current = None;
                    self.dispatch();
                }
            }
        }
    }

    /// Pick the next thread from the class, rejecting anything that is not a
    /// runnable thread (the guard against a faulty policy).
    fn dispatch(&mut self) {
        debug_assert!(self.current.is_none());
        while let Some(tid) = self.class.pick_next() {
            match self.threads.get_mut(&tid) {
                Some(t) if t.state == State::Runnable => {
                    t.state = State::Running;
                    self.slice_left = core::mem::replace(&mut t.slice, TIMESLICE_TICKS);
                    self.current = Some(tid);
                    return;
                }
                _ => self.bad_picks += 1,
            }
        }
    }

    /// Block the current thread until woken, unless a wake token is waiting (then
    /// it is consumed and the thread keeps running). Returns whether it blocked.
    pub fn block_current(&mut self) -> Result<bool, SchedError> {
        let c = self.current.ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        if core::mem::take(&mut t.token) {
            return Ok(false);
        }
        t.state = State::Blocked;
        self.current = None;
        self.dispatch();
        Ok(true)
    }

    /// Sleep until tick `deadline` or an earlier wake. A deadline that has passed
    /// does not block. Any wake token left from before is discarded.
    pub fn sleep_until(&mut self, deadline: u64) -> Result<bool, SchedError> {
        let c = self.current.ok_or(SchedError::NoCurrent)?;
        self.threads.get_mut(&c).expect("current exists").token = false;
        if deadline <= self.now {
            return Ok(false);
        }
        self.threads.get_mut(&c).expect("current exists").timer = Some(deadline);
        self.timers.insert((deadline, c));
        self.block_current()
    }

    /// Wake a thread (see the module comment for the token rule).
    pub fn wake(&mut self, tid: Tid) -> Result<(), SchedError> {
        let t = self.threads.get_mut(&tid).ok_or(SchedError::NoSuchThread)?;
        match t.state {
            State::Blocked => {
                if let Some(deadline) = t.timer.take() {
                    self.timers.remove(&(deadline, tid));
                }
                t.state = State::Runnable;
                let prio = t.prio;
                self.class.enqueue_tail(tid, prio);
                self.resched();
            }
            State::Running | State::Runnable => t.token = true,
            State::Exited => {}
        }
        Ok(())
    }

    /// The current thread gives up the rest of its slice.
    pub fn yield_current(&mut self) -> Result<(), SchedError> {
        let c = self.current.ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        t.state = State::Runnable;
        let prio = t.prio;
        self.class.enqueue_tail(c, prio);
        self.current = None;
        self.dispatch();
        Ok(())
    }

    /// The current thread is finished for good.
    pub fn exit_current(&mut self) -> Result<(), SchedError> {
        let c = self.current.ok_or(SchedError::NoCurrent)?;
        let t = self.threads.get_mut(&c).expect("current exists");
        t.state = State::Exited;
        t.token = false;
        self.current = None;
        self.dispatch();
        Ok(())
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
        if prio > PRIO_MAX {
            return Err(SchedError::BadPriority);
        }
        let t = self.threads.get_mut(&tid).ok_or(SchedError::NoSuchThread)?;
        let old = core::mem::replace(&mut t.prio, prio);
        match t.state {
            State::Runnable if old != prio => {
                self.class.remove(tid);
                self.class.enqueue_tail(tid, prio);
                self.resched();
            }
            State::Running => self.resched(),
            _ => {}
        }
        Ok(())
    }

    /// One timer tick: advance the clock, fire timers, account the time slice.
    pub fn tick(&mut self) {
        self.now += 1;
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
        if let Some(c) = self.current {
            self.slice_left = self.slice_left.saturating_sub(1);
            if self.slice_left == 0 {
                let prio = self.threads[&c].prio;
                if self.class.would_rotate(prio) {
                    let t = self.threads.get_mut(&c).expect("current exists");
                    t.state = State::Runnable;
                    self.class.enqueue_tail(c, prio);
                    self.current = None;
                    self.dispatch();
                } else {
                    self.slice_left = TIMESLICE_TICKS;
                }
            }
        }
        self.resched();
    }

    /// Check the "exactly one place" rule and the timer bookkeeping.
    pub fn audit(&self) -> Result<(), String> {
        let mut running = 0;
        for (&tid, t) in &self.threads {
            let queued = self.class.contains(tid);
            let timed = self.timers.iter().filter(|&&(_, x)| x == tid).count();
            match t.state {
                State::Running => {
                    running += 1;
                    if self.current != Some(tid) {
                        return Err(alloc::format!(
                            "thread {tid} is Running but current is {:?}",
                            self.current
                        ));
                    }
                    if queued {
                        return Err(alloc::format!("running thread {tid} is also queued"));
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
        if running > 1 || (running == 1) != self.current.is_some() {
            return Err(alloc::format!(
                "{running} running threads, current {:?}",
                self.current
            ));
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
        let me = sched.current().ok_or(SchedError::NoCurrent)?;
        // A thread is queued at most once: a stale entry (it was woken some other
        // way) must not stay behind to swallow a later wake_one.
        self.waiters.retain(|&t| t != me);
        self.waiters.push_back(me);
        let blocked = sched.block_current()?;
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
        let me = sched.current().ok_or(SchedError::NoCurrent)?;
        self.waiters.retain(|&t| t != me);
        self.waiters.push_back(me);
        let blocked = sched.sleep_until(deadline)?;
        if !blocked {
            self.waiters.retain(|&t| t != me);
        }
        Ok(blocked)
    }

    /// Wake the longest-waiting thread, if any.
    pub fn wake_one<C: SchedClass>(&mut self, sched: &mut Sched<C>) -> Option<Tid> {
        while let Some(tid) = self.waiters.pop_front() {
            // Only a thread that is actually blocked counts as woken. Anything else is
            // stale (woken some other way, or gone) and is dropped: a waiter that is
            // merely running re-checks its condition before it waits again.
            if sched.state(tid) == Some(State::Blocked) && sched.wake(tid).is_ok() {
                return Some(tid);
            }
        }
        None
    }

    pub fn wake_all<C: SchedClass>(&mut self, sched: &mut Sched<C>) -> usize {
        let mut n = 0;
        while self.wake_one(sched).is_some() {
            n += 1;
        }
        n
    }
}
