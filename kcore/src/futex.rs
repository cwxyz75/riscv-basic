//! The futex wait table (S7b, S7-SPEC section 4): who sleeps on which `(process, address)`.
//!
//! Pure bookkeeping. The kernel checks the user word and enqueues in one step under the key's bucket lock
//! (so a wake between a user store and the wait is never lost); wakes dequeue in FIFO order under the same
//! lock; a sleeper leaves the table when it is woken, times out, is killed or its process ends. A thread
//! waits on at most one address at a time.
//!
//! L14: the table is 64 buckets by hash of the key (`FutexTable`), each its own lock in the kernel (class
//! `FUTEX`, SCHED-SMP-SPEC 3.3, never two at once), so `futex_wake` needs no big kernel lock and wakes on
//! different words do not meet. `Futexes` is one bucket and is the unit the model tests check.

use crate::fmap::{FMap, NoMem};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// A futex: a process and an address in its address space.
pub type Key = (u64, u64);
/// A sleeper (a kernel thread id).
pub type Waiter = u64;

#[derive(Default)]
pub struct Futexes {
    queues: FMap<Key, VecDeque<Waiter>>,
    sleeping_on: FMap<Waiter, Key>,
    /// The priority-inheritance futexes of this bucket (L23): a different protocol on the same keys, under the same lock.
    pub pi: crate::pifutex::PiFutexes,
}

impl Futexes {
    pub const fn new() -> Futexes {
        Futexes {
            queues: FMap::new(),
            sleeping_on: FMap::new(),
            pi: crate::pifutex::PiFutexes::new(),
        }
    }

    /// Put `who` to sleep on `key`. A thread already sleeping is moved (it can only wait once); `Ok(false)` in that case, which the kernel treats as a
    /// bug (and asserts in debug builds). Every allocation is made first and the table changes only when all of them have worked (KT9, T177: running
    /// out is `Err`, with the thread not queued).
    pub fn wait(&mut self, key: Key, who: Waiter) -> Result<bool, NoMem> {
        let fresh = !self.forget(who);
        self.sleeping_on.reserve(1)?;
        match self.queues.get_mut(&key) {
            Some(q) => q.try_reserve(1).map_err(|_| NoMem)?,
            None => {
                self.queues.reserve(1)?;
                let mut q = VecDeque::new();
                q.try_reserve(1).map_err(|_| NoMem)?;
                self.queues.insert_reserved(key, q);
            }
        }
        if let Some(q) = self.queues.get_mut(&key) {
            q.push_back(who);
        }
        self.sleeping_on.insert_reserved(who, key);
        Ok(fresh)
    }

    /// Wake up to `count` sleepers on `key`, oldest first; they leave the table. `Err` (nothing woken) if the list of them cannot be made.
    pub fn wake(&mut self, key: Key, count: u64) -> Result<Vec<Waiter>, NoMem> {
        let mut woken = Vec::new();
        let Some(q) = self.queues.get_mut(&key) else {
            return Ok(woken);
        };
        let n = usize::try_from(count).unwrap_or(usize::MAX).min(q.len());
        woken.try_reserve_exact(n).map_err(|_| NoMem)?;
        while (woken.len() as u64) < count {
            let Some(w) = q.pop_front() else { break };
            self.sleeping_on.remove(&w);
            woken.push(w);
        }
        if q.is_empty() {
            self.queues.remove(&key);
        }
        Ok(woken)
    }

    /// `who` stops sleeping (timed out, killed): true if it was in the table.
    pub fn forget(&mut self, who: Waiter) -> bool {
        let Some(key) = self.sleeping_on.remove(&who) else {
            return false;
        };
        if let Some(q) = self.queues.get_mut(&key) {
            q.retain(|&w| w != who);
            if q.is_empty() {
                self.queues.remove(&key);
            }
        }
        true
    }

    /// Everyone sleeping in process `pid` leaves (the process is gone); returns them.
    pub fn forget_process(&mut self, pid: u64) -> Vec<Waiter> {
        let gone: Vec<Waiter> = self
            .sleeping_on
            .iter()
            .filter(|(_, k)| k.0 == pid)
            .map(|(&w, _)| w)
            .collect();
        for &w in &gone {
            self.forget(w);
        }
        gone
    }

    /// `forget_process` without the list: one sleeper at a time, nothing allocated (a teardown's backstop must not fail for want of memory).
    pub fn forget_process_quiet(&mut self, pid: u64) {
        while let Some(w) = self
            .sleeping_on
            .iter()
            .find(|(_, k)| k.0 == pid)
            .map(|(&w, _)| w)
        {
            self.forget(w);
        }
    }

    /// Is `who` still asleep?
    pub fn contains(&self, who: Waiter) -> bool {
        self.sleeping_on.contains_key(&who)
    }

    /// How many sleepers there are (a leak counter for tests).
    pub fn len(&self) -> usize {
        self.sleeping_on.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sleeping_on.is_empty()
    }
}

/// How many buckets the table has.
pub const BUCKETS: usize = 64;

/// The bucket of a key: a hash of the process and the address, so that the words of one process spread (a word is 4-aligned: its low two
/// bits carry nothing).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: wrapping multiplication and a shift of a u64
pub fn bucket_of(key: Key) -> usize {
    let h = (key.1 >> 2)
        .wrapping_add(key.0.wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_mul(0xff51_afd7_ed55_8ccd);
    ((h >> 32) as usize) % BUCKETS
}

/// What the table needs from its locks: run `f` on bucket `b` (`b < BUCKETS`) with that bucket's lock held. The kernel implements it with one
/// spinlock per bucket; the host tests with plain cells.
pub trait Buckets {
    fn with<R>(&self, b: usize, f: impl FnOnce(&mut Futexes) -> R) -> R;
}

/// The futex table: 64 buckets behind `Buckets`. Every operation takes the one bucket its key hashes to (never two at once), except the
/// two that look at everything (`forget_process`, `len`), which take them one after another.
pub struct FutexTable<B: Buckets> {
    buckets: B,
}

impl<B: Buckets> FutexTable<B> {
    pub const fn new(buckets: B) -> Self {
        FutexTable { buckets }
    }

    /// The futex wait's one step: under the key's lock, `check` (the kernel reads the user word and compares it) and, if it says yes, `who`
    /// is queued. Returns whether it was queued. A wake that follows a user store takes the same lock, so it finds the sleeper or the sleeper
    /// finds the store.
    pub fn wait_if(
        &self,
        key: Key,
        who: Waiter,
        check: impl FnOnce() -> bool,
    ) -> Result<bool, NoMem> {
        self.buckets.with(bucket_of(key), |f| {
            if !check() {
                return Ok(false);
            }
            let fresh = f.wait(key, who)?;
            debug_assert!(fresh, "a thread sleeps on one futex at a time");
            Ok(true)
        })
    }

    /// Run `f` on the bucket of `key` with its lock held (the PI futexes and anything else that works on a bucket as a whole).
    pub fn with_key<R>(&self, key: Key, f: impl FnOnce(&mut Futexes) -> R) -> R {
        self.buckets.with(bucket_of(key), f)
    }

    /// Run `f` on every bucket, one after another (never two at once).
    pub fn for_each_bucket(&self, mut f: impl FnMut(&mut Futexes)) {
        for b in 0..BUCKETS {
            self.buckets.with(b, &mut f);
        }
    }

    /// Queue `who` on `key` unconditionally (a test hook and the models).
    pub fn wait(&self, key: Key, who: Waiter) -> Result<bool, NoMem> {
        self.buckets.with(bucket_of(key), |f| f.wait(key, who))
    }

    /// Wake up to `count` sleepers on `key`, oldest first, and run `then` on them **with the lock still held**, so that the wake of a
    /// sleeper that has left the queue is issued before the lock is let go (SCHED-SMP-SPEC 3.8: `FUTEX` is before the run-queue locks).
    pub fn wake_with(
        &self,
        key: Key,
        count: u64,
        then: impl FnOnce(&[Waiter]),
    ) -> Result<usize, NoMem> {
        self.buckets.with(bucket_of(key), |f| {
            let woken = f.wake(key, count)?;
            if !woken.is_empty() {
                then(&woken);
            }
            Ok(woken.len())
        })
    }

    /// Wake up to `count` sleepers on `key`, oldest first; they leave the table.
    pub fn wake(&self, key: Key, count: u64) -> Result<Vec<Waiter>, NoMem> {
        self.buckets.with(bucket_of(key), |f| f.wake(key, count))
    }

    /// `who`, which sleeps on `key`, stops sleeping (timed out, killed): true if it was in the table.
    pub fn forget(&self, key: Key, who: Waiter) -> bool {
        self.buckets.with(bucket_of(key), |f| f.forget(who))
    }

    /// Is `who` still asleep on `key`?
    pub fn contains(&self, key: Key, who: Waiter) -> bool {
        self.buckets.with(bucket_of(key), |f| f.contains(who))
    }

    /// Everyone sleeping in process `pid` leaves (the process is gone); returns them. One bucket at a time.
    pub fn forget_process(&self, pid: u64) -> Vec<Waiter> {
        let mut gone = Vec::new();
        for b in 0..BUCKETS {
            gone.extend(self.buckets.with(b, |f| f.forget_process(pid)));
        }
        gone
    }

    /// `forget_process` for a caller that does not need the list: allocates nothing.
    pub fn forget_process_quiet(&self, pid: u64) {
        for b in 0..BUCKETS {
            self.buckets.with(b, |f| f.forget_process_quiet(pid));
        }
    }

    /// How many sleepers there are (a leak counter for tests). Not one instant: the buckets are counted one after another.
    pub fn len(&self) -> usize {
        (0..BUCKETS)
            .map(|b| self.buckets.with(b, |f| f.len()))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Buckets for a single thread of control (the host tests): plain cells, no locks.
pub struct LocalBuckets(Vec<core::cell::RefCell<Futexes>>);

impl LocalBuckets {
    pub fn new() -> Self {
        LocalBuckets((0..BUCKETS).map(|_| Default::default()).collect())
    }
}

impl Default for LocalBuckets {
    fn default() -> Self {
        Self::new()
    }
}

impl Buckets for LocalBuckets {
    #[allow(clippy::indexing_slicing)] // INVARIANT: b < BUCKETS, the length of the vector
    fn with<R>(&self, b: usize, f: impl FnOnce(&mut Futexes) -> R) -> R {
        f(&mut self.0[b].borrow_mut())
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn wake_is_fifo_counted_and_keyed() {
        let mut f = Futexes::new();
        for w in 1..=3 {
            assert!(f.wait((7, 0x1000), w).unwrap());
        }
        assert!(
            f.wait((8, 0x1000), 9).unwrap(),
            "another process, same address"
        );
        assert_eq!(
            f.wake((7, 0x2000), 5).unwrap(),
            Vec::<u64>::new(),
            "another address"
        );
        assert_eq!(
            f.wake((9, 0x1000), 5).unwrap(),
            Vec::<u64>::new(),
            "another process"
        );
        assert_eq!(
            f.wake((7, 0x1000), 2).unwrap(),
            [1, 2],
            "oldest first, only as many as asked"
        );
        assert!(f.contains(3) && !f.contains(1));
        assert_eq!(f.wake((7, 0x1000), 0).unwrap(), Vec::<u64>::new());
        assert_eq!(f.wake((7, 0x1000), 9).unwrap(), [3]);
        assert_eq!(f.len(), 1);
        assert_eq!(f.wake((8, 0x1000), 1).unwrap(), [9]);
        assert!(f.is_empty());
    }

    #[test]
    fn forget_removes_exactly_one_sleeper() {
        let mut f = Futexes::new();
        f.wait((1, 4), 10).unwrap();
        f.wait((1, 4), 11).unwrap();
        f.wait((1, 4), 12).unwrap();
        assert!(f.forget(11));
        assert!(!f.forget(11), "already gone");
        assert_eq!(f.wake((1, 4), 9).unwrap(), [10, 12]);
    }

    #[test]
    fn a_process_ending_takes_its_sleepers_with_it() {
        let mut f = Futexes::new();
        f.wait((1, 4), 10).unwrap();
        f.wait((2, 4), 20).unwrap();
        f.wait((1, 8), 11).unwrap();
        let mut gone = f.forget_process(1);
        gone.sort();
        assert_eq!(gone, [10, 11]);
        assert_eq!(f.len(), 1);
        assert!(f.contains(20));
    }

    #[test]
    fn a_thread_waits_on_one_address_at_a_time() {
        let mut f = Futexes::new();
        assert!(f.wait((1, 4), 5).unwrap());
        assert!(
            !f.wait((1, 8), 5).unwrap(),
            "a second wait by the same thread is reported"
        );
        assert_eq!(f.wake((1, 4), 9).unwrap(), Vec::<u64>::new(), "it moved");
        assert_eq!(f.wake((1, 8), 9).unwrap(), [5]);
    }

    #[test]
    fn the_table_keys_every_operation_to_its_bucket() {
        let t = FutexTable::new(LocalBuckets::new());
        // words that hash apart and words that collide behave the same: a key is a (process, address), never a bucket
        let keys: Vec<Key> = (0..300u64).map(|i| (1 + i % 3, 0x1000 + 4 * i)).collect();
        for (i, &k) in keys.iter().enumerate() {
            assert!(t.wait_if(k, i as u64, || true).unwrap());
        }
        assert_eq!(t.len(), keys.len());
        let used: alloc::collections::BTreeSet<usize> =
            keys.iter().map(|&k| bucket_of(k)).collect();
        assert!(used.len() > BUCKETS / 2, "the hash spreads: {}", used.len());
        assert!(
            !t.wait_if((1, 8), 999, || false).unwrap(),
            "a refused check queues nothing"
        );
        for (i, &k) in keys.iter().enumerate() {
            assert!(t.contains(k, i as u64));
            assert_eq!(
                t.wake(k, 5).unwrap(),
                [i as u64],
                "only its own word's sleeper"
            );
        }
        assert!(t.is_empty());
    }

    #[test]
    fn a_process_ending_leaves_no_sleeper_in_any_bucket() {
        let t = FutexTable::new(LocalBuckets::new());
        for i in 0..100u64 {
            t.wait((1 + i % 2, 0x1000 + 4 * i), i).unwrap();
        }
        let mut gone = t.forget_process(1);
        gone.sort();
        assert_eq!(gone, (0..100u64).filter(|i| i % 2 == 0).collect::<Vec<_>>());
        assert_eq!(t.len(), 50);
    }

    #[test]
    fn a_wake_runs_its_action_with_the_sleepers_it_took() {
        let t = FutexTable::new(LocalBuckets::new());
        t.wait((1, 4), 7).unwrap();
        t.wait((1, 4), 8).unwrap();
        let mut seen: Vec<u64> = Vec::new();
        assert_eq!(
            t.wake_with((1, 4), 1, |w| seen.extend_from_slice(w))
                .unwrap(),
            1
        );
        assert_eq!(seen, [7]);
        assert_eq!(
            t.wake_with((1, 4), 5, |w| seen.extend_from_slice(w))
                .unwrap(),
            1
        );
        assert_eq!(seen, [7, 8]);
        assert_eq!(
            t.wake_with((1, 4), 5, |_| panic!("nobody left")).unwrap(),
            0
        );
    }
}
