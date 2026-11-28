//! The user library's semaphore protocol over a futex (T287, T222): `rt::sync::Semaphore` runs exactly this code over the kernel's
//! `futex_wait` and `futex_wake`, and the host runs it over a model futex under loom and shuttle (`tests/usync_concurrency.rs`), an
//! oracle for "no wake is lost" in the user half of the wait/wake path (the kernel half is `futex_model.rs` and
//! `futex_concurrency.rs`).
//!
//! The count is one word. `post` adds one and wakes one sleeper; a taker decrements the count with a compare-exchange while it is
//! above zero, and otherwise sleeps on the word while it reads 0. A wake that comes between the taker's look and its sleep is not
//! lost: the futex compares the word with 0 under its table lock and refuses to sleep (SHOULD_WAIT) when a post has changed it.
//!
//! [`wait_until`] also says how the permit was taken ([`Took`]), so a test can tell a late taker from a lost wake: a permit taken
//! only after the futex reported a timeout was there while the taker slept, which is a lost wake unless the post itself came after
//! the deadline.

use crate::sync::{AtomicU32, Ordering};

/// What a futex wait reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Woken by a wake (OK): the sleeper was taken out of the table by `futex_wake`.
    Woken,
    /// The word did not hold the expected value (SHOULD_WAIT): it did not sleep.
    Differed,
    /// The deadline passed while it was still in the table (TIMED_OUT): it consumed no wake.
    TimedOut,
}

/// The futex a protocol sleeps on: the kernel's for `rt`, a model's on the host.
pub trait Futex {
    /// Sleep on `word` while it holds `expected`, until woken or `deadline` (an absolute tick; `u64::MAX` never).
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: u64) -> Wait;
    /// Wake at most `count` sleepers on `word`; how many were woken.
    fn wake(&self, word: &AtomicU32, count: u32) -> usize;
    /// Whether `deadline` has passed (`u64::MAX` never does).
    fn expired(&self, deadline: u64) -> bool;
}

/// How [`wait_until`] took its permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Took {
    /// At the first look, without sleeping.
    AtOnce,
    /// After a wait that a wake ended (the post it was woken for, or a later one).
    AfterWake,
    /// After a wait that did not sleep because the count had changed (a post between the look and the sleep).
    AfterDiffered,
    /// Only after the futex reported a timeout (or the deadline passed): the permit was there while the taker slept or gave up.
    AfterTimeout,
}

/// Add one permit and wake one sleeper.
pub fn post<F: Futex>(count: &AtomicU32, futex: &F) {
    count.fetch_add(1, Ordering::Release);
    futex.wake(count, 1);
}

/// Take a permit if there is one.
pub fn try_wait(count: &AtomicU32) -> bool {
    let mut c = count.load(Ordering::Relaxed);
    while c > 0 {
        match count.compare_exchange_weak(
            c,
            c.wrapping_sub(1),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(now) => c = now,
        }
    }
    false
}

/// Take a permit, giving up at `deadline`: how it was taken, or `None` if the deadline passed with none.
pub fn wait_until<F: Futex>(count: &AtomicU32, deadline: u64, futex: &F) -> Option<Took> {
    let mut how = Took::AtOnce;
    loop {
        if try_wait(count) {
            return Some(how);
        }
        match futex.wait(count, 0, deadline) {
            Wait::TimedOut => return try_wait(count).then_some(Took::AfterTimeout),
            Wait::Woken => how = Took::AfterWake,
            Wait::Differed => how = Took::AfterDiffered,
        }
        // (past the deadline after a wake or a changed word: one last look, and the permit, if any, counts as taken that way)
        if futex.expired(deadline) {
            return try_wait(count).then_some(how);
        }
    }
}
