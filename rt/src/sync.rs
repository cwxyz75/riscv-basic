//! Locks that sleep (S7b): a mutex, condition variable, once-flag and semaphore on the kernel's futex
//! (`futex_wait`/`futex_wake`), so a thread that cannot proceed does not spin. Process-private, like
//! the futexes themselves.
//!
//! Every blocking operation also has a `*_until(deadline)` form (an absolute tick, as everywhere in
//! the system): tests use those so a lost wakeup fails instead of hanging.

use crate::sys;
use abi::status::Status;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

const FOREVER: u64 = u64::MAX;

fn wait(word: &AtomicU32, expected: u32, deadline: u64) -> Status {
    // (OK: woken; SHOULD_WAIT: the word differed; TIMED_OUT: the deadline passed)
    sys::futex_wait(word.as_ptr(), expected, deadline)
        .status()
        .unwrap_or(Status::BadSyscall)
}

fn wake(word: &AtomicU32, count: u32) -> usize {
    sys::futex_wake(word.as_ptr(), count).a1
}

fn expired(deadline: u64) -> bool {
    deadline != FOREVER && sys::clock_get() >= deadline
}

/// A mutual-exclusion lock (the three-state futex mutex: 0 free, 1 locked, 2 locked with sleepers).
pub struct Mutex<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serialises every access to `value`.
unsafe impl<T: Send> Send for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Mutex<T> {}

pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> {
        Mutex {
            state: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard { lock: self })
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.lock_until(FOREVER)
            .expect("a forever lock never times out")
    }

    /// Lock, giving up at `deadline` (an absolute tick).
    pub fn lock_until(&self, deadline: u64) -> Option<MutexGuard<'_, T>> {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(MutexGuard { lock: self });
        }
        loop {
            // announce a sleeper (2), unless the lock just became free (then take it as contended)
            if self.state.swap(2, Ordering::Acquire) == 0 {
                return Some(MutexGuard { lock: self });
            }
            // Give up only on TIMED_OUT: the kernel reports that only for a sleeper that was still in
            // its table, which therefore consumed no wake. An OK (woken) must retry the swap even
            // past the deadline, or a wake meant for the lock's next owner would be lost.
            if wait(&self.state, 2, deadline) == Status::TimedOut {
                return None;
            }
        }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.lock.state.swap(0, Ordering::Release) == 2 {
            wake(&self.lock.state, 1);
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves the lock is held.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves the lock is held, exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

/// A condition variable: waiters sleep on a sequence number that `notify_*` bumps.
pub struct Condvar {
    seq: AtomicU32,
}

impl Condvar {
    pub const fn new() -> Condvar {
        Condvar {
            seq: AtomicU32::new(0),
        }
    }

    /// Release `guard`, sleep until notified (spurious wakeups are possible), lock again.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait_until(guard, FOREVER).0
    }

    /// As `wait`, giving up at `deadline`; the flag says the deadline passed.
    pub fn wait_until<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        deadline: u64,
    ) -> (MutexGuard<'a, T>, bool) {
        let seq = self.seq.load(Ordering::Relaxed);
        let lock = guard.lock;
        drop(guard); // unlock: a notify after this changes `seq`, so the sleep below returns at once
        let timed_out = wait(&self.seq, seq, deadline) == Status::TimedOut;
        (lock.lock(), timed_out)
    }

    pub fn notify_one(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        wake(&self.seq, 1);
    }

    pub fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        wake(&self.seq, u32::MAX);
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Condvar::new()
    }
}

/// Run something once: 0 new, 1 running, 2 done.
pub struct Once {
    state: AtomicU32,
}

impl Once {
    pub const fn new() -> Once {
        Once {
            state: AtomicU32::new(0),
        }
    }

    /// Run `f` if nobody has; every caller returns only after it has finished.
    pub fn call_once(&self, f: impl FnOnce()) {
        loop {
            match self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => {
                    f();
                    self.state.store(2, Ordering::Release);
                    wake(&self.state, u32::MAX);
                    return;
                }
                Err(2) => return,
                Err(_) => {
                    wait(&self.state, 1, FOREVER);
                }
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }
}

impl Default for Once {
    fn default() -> Self {
        Once::new()
    }
}

/// A counting semaphore.
pub struct Semaphore {
    count: AtomicU32,
}

impl Semaphore {
    pub const fn new(count: u32) -> Semaphore {
        Semaphore {
            count: AtomicU32::new(count),
        }
    }

    pub fn post(&self) {
        self.count.fetch_add(1, Ordering::Release);
        wake(&self.count, 1);
    }

    pub fn try_wait(&self) -> bool {
        let mut c = self.count.load(Ordering::Relaxed);
        while c > 0 {
            match self
                .count
                .compare_exchange_weak(c, c - 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(now) => c = now,
            }
        }
        false
    }

    /// Take one, giving up at `deadline`; false if it passed.
    pub fn wait_until(&self, deadline: u64) -> bool {
        loop {
            if self.try_wait() {
                return true;
            }
            if wait(&self.count, 0, deadline) == Status::TimedOut || expired(deadline) {
                return self.try_wait();
            }
        }
    }

    pub fn wait(&self) {
        self.wait_until(FOREVER);
    }
}
