//! The priority-inheritance futex (L23, SCHED-SMP-SPEC 4.8, "PI futex"): the lock word's protocol and the queue of waiters, as pure logic over a
//! `Word` (the 32-bit word in user memory) and the donation graph (`kcore::donate`).
//!
//! The word: 0 is free; otherwise bits 0 to 29 hold the owner's thread id, bit 30 `OWNER_DIED`, bit 31 `WAITERS`. User code locks by a
//! compare-and-swap of 0 to its own id and unlocks by a compare-and-swap of its id to 0; either failing enters the kernel, which does the rest
//! here under the key's bucket lock. A thread whose id does not fit in 30 bits cannot use a PI futex (`PiError::NoId`: `NOT_SUPPORTED`).
//!
//! A waiter is queued in effective-priority order (FIFO among equals) and **donates** to the owner; the unlock hands the lock to the head, revokes
//! its donation and retargets the others to it, so the highest-priority waiter always gets the lock (PF2). While a thread is queued the word names
//! a live thread and the graph holds the waiter's edge to it (PF1). A waiter that times out, is killed or interrupted leaves the queue
//! (`cancel`) and is revoked; the word's `WAITERS` bit is left as it is (the next unlock finds no queue and writes 0).
//!
//! The kernel owner's death (`owner_died`) poisons the word (`OWNER_DIED`, owner kept) and wakes every waiter, which reports BAD_STATE.
//! The kernel never guesses a recovery.

use crate::donate::{Change, DonateError, Donations};
use crate::sched::Tid;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

pub const OWNER_MASK: u32 = (1 << 30) - 1;
pub const OWNER_DIED: u32 = 1 << 30;
pub const WAITERS: u32 = 1 << 31;

/// A PI futex: a process and an address in its address space (the same key as the plain futexes).
pub type Key = (u64, u64);

/// The 32-bit lock word in user memory, as the kernel reaches it (a physical address with one atomic load and compare-and-swap).
pub trait Word {
    fn load(&self) -> u32;
    /// Replace `old` by `new`; the current value on failure.
    fn cas(&self, old: u32, new: u32) -> Result<u32, u32>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiError {
    /// The caller owns it already, the owner has died or is not a live thread of the process, or the wait would close a cycle or a chain
    /// too long: BAD_STATE. Nothing was queued.
    BadState,
    /// `unlock` by a thread that is not the owner: ACCESS_DENIED.
    NotOwner,
    /// The caller's thread id does not fit in the word (30 bits): NOT_SUPPORTED.
    NoId,
}

/// What `lock` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Lock {
    /// The caller owns the lock.
    Acquired,
    /// The caller is queued and donating to the owner: block until the lock is handed over, a kill, an interrupt or the deadline. `changes` are
    /// the effective priorities the donation moved, for the kernel to apply.
    Blocked { changes: Vec<Change> },
}

/// What `unlock` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Unlock {
    /// Nobody waited: the word is 0.
    Free,
    /// The lock went to `to`, who is to be woken; `changes` are the effective priorities the hand-over moved.
    Handed { to: Tid, changes: Vec<Change> },
}

/// How a queued thread's wait ended, when it did not give up: left for the woken thread to take (`take_outcome`), so it never reads the word again
/// (its page may be gone by then).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The lock was handed to it: it owns the futex.
    Granted,
    /// The owner died (or the process ended) with it queued: BAD_STATE.
    Died,
}

#[derive(Debug)]
struct Rec {
    owner: Tid,
    /// The queued waiters, best first.
    waiters: VecDeque<Tid>,
}

/// The PI futexes of one bucket (the kernel keeps one per futex bucket, under the bucket's lock).
#[derive(Debug, Default)]
pub struct PiFutexes {
    recs: BTreeMap<Key, Rec>,
    waiting: BTreeMap<Tid, Key>,
    /// The keys a thread owns that have waiters (so its death finds them).
    owned: BTreeMap<Tid, BTreeSet<Key>>,
    /// Waiters taken out of a queue by the owner (hand-over, death) that have not yet noticed.
    outcomes: BTreeMap<Tid, Outcome>,
}

/// Does the id fit in the lock word?
pub fn fits(tid: Tid) -> bool {
    tid != 0 && tid <= u64::from(OWNER_MASK)
}

impl PiFutexes {
    pub const fn new() -> PiFutexes {
        PiFutexes {
            recs: BTreeMap::new(),
            waiting: BTreeMap::new(),
            owned: BTreeMap::new(),
            outcomes: BTreeMap::new(),
        }
    }

    /// `futex_lock_pi` for thread `me` on `key`. `live(t)` says whether `t` is a live thread of the caller's process. Needs `me` registered in
    /// `d`. On `Blocked` the caller sleeps and then asks `still_waiting`/`cancel`.
    pub fn lock<W: Word>(
        &mut self,
        d: &mut Donations,
        w: &W,
        key: Key,
        me: Tid,
        live: impl Fn(Tid) -> bool,
    ) -> Result<Lock, PiError> {
        if !fits(me) {
            return Err(PiError::NoId);
        }
        let me32 = me as u32;
        loop {
            let v = w.load();
            if v == 0 {
                if w.cas(0, me32).is_ok() {
                    return Ok(Lock::Acquired);
                }
                continue;
            }
            if v & OWNER_DIED != 0 {
                return Err(PiError::BadState);
            }
            let owner = Tid::from(v & OWNER_MASK);
            if owner == me {
                return Err(PiError::BadState); // a relock would wait for itself
            }
            if !live(owner) {
                // the owner is gone and no one handed the lock over: poison it, as an owner that died with waiters would have
                let _ = w.cas(v, (v | OWNER_DIED) & !WAITERS);
                return Err(PiError::BadState);
            }
            if v & WAITERS == 0 && w.cas(v, v | WAITERS).is_err() {
                continue; // the word changed: look again
            }
            // queue in effective-priority order, FIFO among equals
            let mine = d.eff(me).unwrap_or(0);
            let rec = self.recs.entry(key).or_insert_with(|| Rec {
                owner,
                waiters: VecDeque::new(),
            });
            if rec.owner != owner {
                // (the user fast path handed the lock on with no waiters: the record is stale and has none)
                debug_assert!(rec.waiters.is_empty());
                if let Some(keys) = self.owned.get_mut(&rec.owner) {
                    keys.remove(&key);
                }
                rec.owner = owner;
            }
            let at = rec
                .waiters
                .iter()
                .position(|&t| d.eff(t).unwrap_or(0) < mine)
                .unwrap_or(rec.waiters.len());
            rec.waiters.insert(at, me);
            self.waiting.insert(me, key);
            self.owned.entry(owner).or_default().insert(key);
            return match d.donate(me, owner) {
                Ok(ch) => Ok(Lock::Blocked {
                    changes: ch.as_slice().to_vec(),
                }),
                Err(DonateError::Cycle | DonateError::TooDeep | DonateError::NoThread) => {
                    // (the classic deadlock, or a chain too long: refused, and nothing stays queued)
                    self.leave_queue(me);
                    Err(PiError::BadState)
                }
                Err(DonateError::AlreadyDonating) => {
                    self.leave_queue(me);
                    Err(PiError::BadState)
                }
            };
        }
    }

    /// `futex_unlock_pi` by `me`: the lock goes to the head of the queue, or the word becomes 0.
    pub fn unlock<W: Word>(
        &mut self,
        d: &mut Donations,
        w: &W,
        key: Key,
        me: Tid,
    ) -> Result<Unlock, PiError> {
        if !fits(me) {
            return Err(PiError::NoId);
        }
        loop {
            let v = w.load();
            if v & OWNER_DIED != 0 || Tid::from(v & OWNER_MASK) != me {
                return Err(PiError::NotOwner);
            }
            let Some(rec) = self.recs.get_mut(&key).filter(|r| !r.waiters.is_empty()) else {
                // nobody queued (a `WAITERS` bit left by a waiter that gave up): free
                if w.cas(v, 0).is_err() {
                    continue;
                }
                self.drop_rec(key);
                return Ok(Unlock::Free);
            };
            let Some(&first) = rec.waiters.front() else {
                continue; // (filtered above: the queue is not empty)
            };
            let more = rec.waiters.len() > 1;
            let new = first as u32 | if more { WAITERS } else { 0 };
            if w.cas(v, new).is_err() {
                continue; // a waiter set `WAITERS` meanwhile: again
            }
            rec.waiters.pop_front();
            let rest: Vec<Tid> = rec.waiters.iter().copied().collect();
            let mut changes: Vec<Change> = Vec::new();
            self.waiting.remove(&first);
            self.outcomes.insert(first, Outcome::Granted);
            changes.extend_from_slice(d.revoke(first).as_slice());
            for t in rest {
                // (retargeting to the new owner cannot close a cycle: `first` is not donating to anyone)
                if let Ok(ch) = d.retarget(t, first) {
                    changes.extend_from_slice(ch.as_slice());
                }
            }
            // the record follows the lock
            if let Some(keys) = self.owned.get_mut(&me) {
                keys.remove(&key);
                if keys.is_empty() {
                    self.owned.remove(&me);
                }
            }
            if more {
                if let Some(rec) = self.recs.get_mut(&key) {
                    rec.owner = first;
                }
                self.owned.entry(first).or_default().insert(key);
            } else {
                self.recs.remove(&key);
            }
            return Ok(Unlock::Handed { to: first, changes });
        }
    }

    /// How the wait of `who` ended, if the owner ended it (and forget that).
    pub fn take_outcome(&mut self, who: Tid) -> Option<Outcome> {
        self.outcomes.remove(&who)
    }

    /// Is `who` still queued (not handed the lock, not woken by its owner's death)?
    pub fn still_waiting(&self, who: Tid) -> bool {
        self.waiting.contains_key(&who)
    }

    /// `who`, queued, gives up (a timeout, a kill, an interrupt): it leaves the queue and its donation is revoked. Returns the priorities that
    /// moved; empty if it was not queued (it was handed the lock or woken meanwhile).
    pub fn cancel(&mut self, d: &mut Donations, who: Tid) -> Vec<Change> {
        if !self.leave_queue(who) {
            return Vec::new();
        }
        d.revoke(who).as_slice().to_vec()
    }

    /// Thread `dead` has gone: every PI futex it owned that has waiters is poisoned. Returns, for each, its key and the waiters to wake (they
    /// leave the queue, their donations are revoked), and the priorities that moved. The caller sets `OWNER_DIED` in the words (`poison`).
    pub fn owner_died(
        &mut self,
        d: &mut Donations,
        dead: Tid,
    ) -> (Vec<(Key, Vec<Tid>)>, Vec<Change>) {
        let mut out = Vec::new();
        let mut changes = Vec::new();
        let Some(keys) = self.owned.remove(&dead) else {
            return (out, changes);
        };
        for key in keys {
            let Some(rec) = self.recs.remove(&key) else {
                continue;
            };
            let woken: Vec<Tid> = rec.waiters.into_iter().collect();
            for &t in &woken {
                self.waiting.remove(&t);
                self.outcomes.insert(t, Outcome::Died);
                changes.extend_from_slice(d.revoke(t).as_slice());
            }
            out.push((key, woken));
        }
        (out, changes)
    }

    /// Mark the word of a poisoned futex: `OWNER_DIED`, owner kept, no `WAITERS`.
    pub fn poison<W: Word>(w: &W) {
        loop {
            let v = w.load();
            if w.cas(v, (v | OWNER_DIED) & !WAITERS).is_ok() {
                return;
            }
        }
    }

    /// Everything of process `pid` leaves (the process is gone); returns the waiters (their donations are revoked).
    pub fn forget_process(&mut self, d: &mut Donations, pid: u64) -> (Vec<Tid>, Vec<Change>) {
        let keys: Vec<Key> = self.recs.keys().copied().filter(|k| k.0 == pid).collect();
        let mut gone = Vec::new();
        let mut changes = Vec::new();
        for key in keys {
            if let Some(rec) = self.recs.remove(&key) {
                if let Some(o) = self.owned.get_mut(&rec.owner) {
                    o.remove(&key);
                }
                for t in rec.waiters {
                    self.waiting.remove(&t);
                    self.outcomes.insert(t, Outcome::Died);
                    changes.extend_from_slice(d.revoke(t).as_slice());
                    gone.push(t);
                }
            }
        }
        self.owned.retain(|_, k| !k.is_empty());
        (gone, changes)
    }

    /// How many threads are queued (a leak counter for tests).
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
            && self.recs.is_empty()
            && self.owned.is_empty()
            && self.outcomes.is_empty()
    }

    /// The structure's own consistency (PF1 and the indexes), for the model tests: every queued thread is in `waiting` and in one record, every
    /// record's owner has it in `owned`, no record is empty.
    pub fn audit(&self, d: &Donations) -> Result<(), alloc::string::String> {
        use alloc::format;
        let mut seen: usize = 0;
        for (key, rec) in &self.recs {
            if rec.waiters.is_empty() {
                return Err(format!("record {key:?} has no waiters"));
            }
            if !self.owned.get(&rec.owner).is_some_and(|k| k.contains(key)) {
                return Err(format!("record {key:?} not in its owner's list"));
            }
            for &t in &rec.waiters {
                seen = seen.saturating_add(1);
                if self.waiting.get(&t) != Some(key) {
                    return Err(format!("waiter {t} of {key:?} not in the index"));
                }
                if d.target(t) != Some(rec.owner) {
                    return Err(format!(
                        "PF1: waiter {t} of {key:?} does not donate to owner {}",
                        rec.owner
                    ));
                }
            }
        }
        if seen != self.waiting.len() {
            return Err(format!("{} queued, {} indexed", seen, self.waiting.len()));
        }
        for (o, keys) in &self.owned {
            for k in keys {
                if self.recs.get(k).map(|r| r.owner) != Some(*o) {
                    return Err(format!(
                        "owner list of {o} names {k:?}, which it does not own"
                    ));
                }
            }
        }
        Ok(())
    }

    fn drop_rec(&mut self, key: Key) {
        if let Some(rec) = self.recs.remove(&key) {
            if let Some(keys) = self.owned.get_mut(&rec.owner) {
                keys.remove(&key);
                if keys.is_empty() {
                    self.owned.remove(&rec.owner);
                }
            }
        }
    }

    /// Take `who` out of its queue. True if it was queued.
    fn leave_queue(&mut self, who: Tid) -> bool {
        let Some(key) = self.waiting.remove(&who) else {
            return false;
        };
        if let Some(rec) = self.recs.get_mut(&key) {
            rec.waiters.retain(|&t| t != who);
            if rec.waiters.is_empty() {
                self.drop_rec(key);
            }
        }
        true
    }
}
