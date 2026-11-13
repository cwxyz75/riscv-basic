//! The futex wait table (S7b, S7-SPEC section 4): who sleeps on which `(process, address)`.
//!
//! Pure bookkeeping. The kernel checks the user word and enqueues in one step under the big kernel
//! lock (so a wake between a user store and the wait is never lost); wakes dequeue in FIFO order;
//! a sleeper leaves the table when it is woken, times out, is killed or its process ends. A thread
//! waits on at most one address at a time.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

/// A futex: a process and an address in its address space.
pub type Key = (u32, u64);
/// A sleeper (a kernel thread id).
pub type Waiter = u64;

#[derive(Default)]
pub struct Futexes {
    queues: BTreeMap<Key, VecDeque<Waiter>>,
    sleeping_on: BTreeMap<Waiter, Key>,
}

impl Futexes {
    pub fn new() -> Futexes {
        Futexes::default()
    }

    /// Put `who` to sleep on `key`. A thread already sleeping is moved (it can only wait once);
    /// returns false in that case, which the kernel treats as a bug (and asserts in debug builds).
    pub fn wait(&mut self, key: Key, who: Waiter) -> bool {
        let fresh = !self.forget(who);
        self.queues.entry(key).or_default().push_back(who);
        self.sleeping_on.insert(who, key);
        fresh
    }

    /// Wake up to `count` sleepers on `key`, oldest first; they leave the table.
    pub fn wake(&mut self, key: Key, count: u64) -> Vec<Waiter> {
        let mut woken = Vec::new();
        let Some(q) = self.queues.get_mut(&key) else {
            return woken;
        };
        while (woken.len() as u64) < count {
            let Some(w) = q.pop_front() else { break };
            self.sleeping_on.remove(&w);
            woken.push(w);
        }
        if q.is_empty() {
            self.queues.remove(&key);
        }
        woken
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
    pub fn forget_process(&mut self, pid: u32) -> Vec<Waiter> {
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

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn wake_is_fifo_counted_and_keyed() {
        let mut f = Futexes::new();
        for w in 1..=3 {
            assert!(f.wait((7, 0x1000), w));
        }
        assert!(f.wait((8, 0x1000), 9), "another process, same address");
        assert_eq!(f.wake((7, 0x2000), 5), Vec::<u64>::new(), "another address");
        assert_eq!(f.wake((9, 0x1000), 5), Vec::<u64>::new(), "another process");
        assert_eq!(
            f.wake((7, 0x1000), 2),
            [1, 2],
            "oldest first, only as many as asked"
        );
        assert!(f.contains(3) && !f.contains(1));
        assert_eq!(f.wake((7, 0x1000), 0), Vec::<u64>::new());
        assert_eq!(f.wake((7, 0x1000), 9), [3]);
        assert_eq!(f.len(), 1);
        assert_eq!(f.wake((8, 0x1000), 1), [9]);
        assert!(f.is_empty());
    }

    #[test]
    fn forget_removes_exactly_one_sleeper() {
        let mut f = Futexes::new();
        f.wait((1, 4), 10);
        f.wait((1, 4), 11);
        f.wait((1, 4), 12);
        assert!(f.forget(11));
        assert!(!f.forget(11), "already gone");
        assert_eq!(f.wake((1, 4), 9), [10, 12]);
    }

    #[test]
    fn a_process_ending_takes_its_sleepers_with_it() {
        let mut f = Futexes::new();
        f.wait((1, 4), 10);
        f.wait((2, 4), 20);
        f.wait((1, 8), 11);
        let mut gone = f.forget_process(1);
        gone.sort();
        assert_eq!(gone, [10, 11]);
        assert_eq!(f.len(), 1);
        assert!(f.contains(20));
    }

    #[test]
    fn a_thread_waits_on_one_address_at_a_time() {
        let mut f = Futexes::new();
        assert!(f.wait((1, 4), 5));
        assert!(
            !f.wait((1, 8), 5),
            "a second wait by the same thread is reported"
        );
        assert_eq!(f.wake((1, 4), 9), Vec::<u64>::new(), "it moved");
        assert_eq!(f.wake((1, 8), 9), [5]);
    }
}
