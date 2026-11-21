//! The suspend count of one thread (EX1, KERNEL-V1-SPEC 3.6): `thread_suspend` raises it, `thread_resume` lowers it, and the thread runs only
//! while it is 0. The machine around it decides *where* the thread is (blocked in a call, returning to user mode) and calls the matching
//! event; this type says what each event means: whether the SUSPENDED signal rises or falls, and whether a parked thread must be woken.
//!
//! * A thread stops at its next return to user mode (`return_to_user`): it parks and SUSPENDED rises.
//! * A thread blocked in a call counts as stopped where it is: `suspend(true)` raises SUSPENDED at once. When its wait ends it reaches the
//!   return to user mode, finds the count above 0 and parks (SUSPENDED is already up).
//! * `resume` lowering the count to 0 drops SUSPENDED; a thread that is parked is woken, one that is still inside its call just goes on.
//! * A resume of a thread that is not suspended is an error, and never loses a wake: whatever the order of the events, a thread whose count
//!   is 0 is not parked once the machine has acted on the answers.

/// How many suspends one thread may hold.
pub const MAX_SUSPEND: u32 = 1 << 20;

/// What `resume` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumed {
    /// The count fell but is still above 0.
    Still,
    /// The count fell to 0: SUSPENDED falls if it was up, and `wake` says a parked thread is to be woken.
    Running { clear: bool, wake: bool },
}

/// What `return_to_user` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtReturn {
    /// Count 0: go on to user mode.
    Run,
    /// Park here. `raise`: SUSPENDED rises now (it was not up).
    Park { raise: bool },
}

/// The suspend state of a thread.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Suspend {
    count: u32,
    /// Stopped for an exception (EX2), apart from the count: `thread_resume` does not release it, only the exception's resolution does.
    exc: bool,
    /// SUSPENDED is up: the thread has stopped (parked, or blocked in a call).
    stopped: bool,
    /// The thread is parked at the return to user mode (and must be woken to leave it).
    parked: bool,
}

impl Suspend {
    pub const fn new() -> Suspend {
        Suspend {
            count: 0,
            exc: false,
            stopped: false,
            parked: false,
        }
    }

    /// Is the thread to stay stopped: its count is above 0 or it has an exception pending?
    pub fn active(&self) -> bool {
        self.count > 0 || self.exc
    }

    /// The suspend count.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Is SUSPENDED up?
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Is the thread parked at the return to user mode?
    pub fn parked(&self) -> bool {
        self.parked
    }

    /// `thread_suspend`: `blocked` says the thread is blocked in a call (it has stopped where it is). Returns whether SUSPENDED rises now,
    /// or `None` when the count is at its limit (nothing changed).
    pub fn suspend(&mut self, blocked: bool) -> Option<bool> {
        if self.count >= MAX_SUSPEND {
            return None;
        }
        self.count = self.count.saturating_add(1);
        let raise = blocked && !self.stopped;
        if raise {
            self.stopped = true;
        }
        Some(raise)
    }

    /// `thread_resume`: `None` when the count is 0 (BAD_STATE).
    pub fn resume(&mut self) -> Option<Resumed> {
        if self.count == 0 {
            return None;
        }
        self.count = self.count.saturating_sub(1);
        if self.active() {
            return Some(Resumed::Still);
        }
        let (clear, wake) = (self.stopped, self.parked);
        self.stopped = false;
        self.parked = false;
        Some(Resumed::Running { clear, wake })
    }

    /// The thread is about to block in a call while its count is above 0 and SUSPENDED is not up yet (it entered the call after the suspend):
    /// it counts as stopped where it is. Returns whether SUSPENDED rises now.
    pub fn blocks(&mut self) -> bool {
        if self.active() && !self.stopped {
            self.stopped = true;
            return true;
        }
        false
    }

    /// The thread reached the return to user mode.
    pub fn return_to_user(&mut self) -> AtReturn {
        if !self.active() {
            return AtReturn::Run;
        }
        let raise = !self.stopped;
        self.stopped = true;
        self.parked = true;
        AtReturn::Park { raise }
    }

    /// The thread took an exception (EX2): it stays stopped until `release_exception`, whatever `thread_suspend` and `thread_resume` do. Returns
    /// whether SUSPENDED rises now (a thread blocked in a call is stopped where it is; the faulting thread parks and raises it itself).
    pub fn stop_for_exception(&mut self, blocked: bool) -> bool {
        self.exc = true;
        let raise = blocked && !self.stopped;
        if raise {
            self.stopped = true;
        }
        raise
    }

    /// The exception was resolved with RETRY. `None` when there was none; otherwise as `resume`.
    pub fn release_exception(&mut self) -> Option<Resumed> {
        if !self.exc {
            return None;
        }
        self.exc = false;
        if self.active() {
            return Some(Resumed::Still);
        }
        let (clear, wake) = (self.stopped, self.parked);
        self.stopped = false;
        self.parked = false;
        Some(Resumed::Running { clear, wake })
    }

    /// Has the thread an exception pending?
    pub fn has_exception(&self) -> bool {
        self.exc
    }

    /// A parked thread was woken and found the count 0 (or was killed): it leaves the return to user mode.
    pub fn unparked(&mut self) {
        self.parked = false;
    }
}
