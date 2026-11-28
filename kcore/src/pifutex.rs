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
use crate::fmap::FMap;
use crate::sched::Tid;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Where the priority changes of an operation go as it makes them (the kernel applies each list under the graph's lock): nothing is collected.
pub type Sink<'a> = &'a mut dyn FnMut(&[Change]);

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
    /// `lock` could not reserve the room it needs to queue the caller: NO_MEMORY, nothing changed (the word included). Everything after a
    /// successful reservation (hand-over, cancel, death) allocates nothing (T248).
    NoMemory,
}

/// What `lock` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Lock {
    /// The caller owns the lock.
    Acquired,
    /// A poll (`wait` false) found the lock held: nothing was queued and nothing donated.
    WouldBlock,
    /// The caller is queued and donating to the owner: block until the lock is handed over, a kill, an interrupt or the deadline. The effective
    /// priorities the donation moved went to the sink.
    Blocked,
}

/// What `unlock` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Unlock {
    /// Nobody waited: the word is 0.
    Free,
    /// The lock went to `to`, who is to be woken; the effective priorities the hand-over moved went to the sink.
    Handed { to: Tid },
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

/// A thread that queued: where, and how its wait ended if the owner ended it (a hand-over or a death: left for the woken thread to take, so
/// that it never reads the word again). The entry stays until the thread takes the outcome or cancels, so the one reservation `lock` made for it
/// pays for everything after.
#[derive(Debug)]
struct Waiting {
    key: Key,
    outcome: Option<Outcome>,
}

/// The PI futexes of one bucket (the kernel keeps one per futex bucket, under the bucket's lock).
///
/// Allocation (T248): only `lock` allocates, in one reservation at the point where it decides to queue (`NoMemory`, nothing changed). A queued
/// thread has one `Waiting`, is in one record's queue, and a record exists only while somebody is queued, so every table is bounded by the
/// number of waiters and the reservation made for the newest waiter covers hand-over, cancel, the owner's death and the end of the process.
#[derive(Debug, Default)]
pub struct PiFutexes {
    recs: FMap<Key, Rec>,
    waiting: FMap<Tid, Waiting>,
    /// Waiters a call ended without being asked to (a record the user's own stores made stale): the caller wakes them (`drain_wake`). Its
    /// capacity is at least the number of entries in `waiting`, kept by `lock`.
    to_wake: Vec<Tid>,
}

/// Does the id fit in the lock word?
pub fn fits(tid: Tid) -> bool {
    tid != 0 && tid <= u64::from(OWNER_MASK)
}

impl PiFutexes {
    pub const fn new() -> PiFutexes {
        PiFutexes {
            recs: FMap::new(),
            waiting: FMap::new(),
            to_wake: Vec::new(),
        }
    }

    /// The waiters that a call ended on its own, handed to `f` (nothing allocated; the room stays).
    pub fn drain_wake_into(&mut self, f: impl FnOnce(&[Tid])) {
        if !self.to_wake.is_empty() {
            f(&self.to_wake);
            self.to_wake.clear();
        }
    }

    /// Make room for one more queued thread (the reservation `lock` takes at the point of no return): the tables, the record's queue (a spare
    /// queue for a record that is to be made, which `lock` takes if it needs it), the wake list. Nothing is changed by a failure.
    fn reserve_waiter(&mut self, key: Key, owner: Tid) -> Result<VecDeque<Tid>, PiError> {
        self.recs.reserve(1).map_err(|_| PiError::NoMemory)?;
        self.waiting.reserve(1).map_err(|_| PiError::NoMemory)?;
        let want = self.waiting.len().saturating_add(1);
        if self.to_wake.capacity() < want {
            self.to_wake
                .try_reserve(want.saturating_sub(self.to_wake.len()))
                .map_err(|_| PiError::NoMemory)?;
        }
        let mut spare = VecDeque::new();
        match self.recs.get_mut(&key) {
            Some(rec) if rec.owner == owner => {
                rec.waiters.try_reserve(1).map_err(|_| PiError::NoMemory)?;
            }
            // (no record, or a stale one that `lock` dissolves first: a new one is made)
            _ => spare.try_reserve(1).map_err(|_| PiError::NoMemory)?,
        }
        Ok(spare)
    }

    /// The record of `key` no longer matches the word (user code stored to it outside the protocol: a plain store, a fast-path lock after an
    /// unlock that was not ours): its waiters are ended with `Died`, their donations revoked, and the record dissolved. Nothing is allocated: the
    /// waiters stay in `waiting` with their outcome, and `to_wake` has room for all of them.
    fn dissolve(&mut self, d: &mut Donations, key: Key, sink: Sink<'_>) {
        let Some(rec) = self.recs.remove(&key) else {
            return;
        };
        for t in rec.waiters {
            if let Some(w) = self.waiting.get_mut(&t) {
                w.outcome = Some(Outcome::Died);
            }
            self.to_wake.push(t);
            sink(d.revoke(t).as_slice());
        }
    }

    /// `futex_lock_pi` for thread `me` on `key`. `live(t)` says whether `t` is a live thread of the caller's process. Needs `me` registered in
    /// `d`. With `wait` false a lock that is held is not waited for (`WouldBlock`, nothing queued). On `Blocked` the caller sleeps and then asks
    /// `still_waiting`/`cancel`; the priority changes of a `Blocked` and of a stale record's dissolution go to `sink` (and `drain_wake` names the
    /// waiters that were ended). `NoMemory` is reported before the word or anything else is changed.
    #[allow(clippy::too_many_arguments)] // the word, the graph, the key, the caller, its process's threads, whether to wait, and where the changes go
    pub fn lock<W: Word>(
        &mut self,
        d: &mut Donations,
        w: &W,
        key: Key,
        me: Tid,
        live: impl Fn(Tid) -> bool,
        wait: bool,
        sink: Sink<'_>,
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
            if !wait {
                return Ok(Lock::WouldBlock);
            }
            // the room the queueing needs, before the word or anything is changed (the one reportable step: after it nothing here allocates)
            let spare = self.reserve_waiter(key, owner)?;
            if v & WAITERS == 0 && w.cas(v, v | WAITERS).is_err() {
                continue; // the word changed: look again
            }
            // (the word names another owner than the record does: user code stored to it outside the protocol; the old waiters are ended)
            return self.block_on(d, key, me, owner, spare, sink);
        }
    }

    /// `lock`'s wait: `me` joins the queue of `key` (owned by `owner`; `spare` is the queue made for it if it is the first) and lends its
    /// priority to the owner (a frame of its own: KT13).
    fn block_on(
        &mut self,
        d: &mut Donations,
        key: Key,
        me: Tid,
        owner: Tid,
        spare: alloc::collections::VecDeque<Tid>,
        sink: Sink<'_>,
    ) -> Result<Lock, PiError> {
        self.enqueue_waiter(d, key, me, owner, spare, sink);
        let mut ch = crate::donate::Changes::default();
        match d.donate_into(me, owner, &mut ch) {
            Ok(()) => {
                sink(ch.as_slice());
                Ok(Lock::Blocked)
            }
            // (a cycle or a chain too long, a thread the graph does not know, or one already lending: the waiter leaves the queue)
            Err(DonateError::Cycle | DonateError::TooDeep | DonateError::NoThread)
            | Err(DonateError::AlreadyDonating) => {
                self.leave_queue(me);
                Err(PiError::BadState)
            }
        }
    }

    /// `block_on`'s queueing of `me` on `key`.
    fn enqueue_waiter(
        &mut self,
        d: &mut Donations,
        key: Key,
        me: Tid,
        owner: Tid,
        mut spare: alloc::collections::VecDeque<Tid>,
        sink: Sink<'_>,
    ) {
        if self.recs.get(&key).is_some_and(|r| r.owner != owner) {
            self.dissolve(d, key, sink);
        }
        // queued in arrival order: the unlock picks the best effective priority at that time, first come among equals
        match self.recs.get_mut(&key) {
            Some(rec) => rec.waiters.push_back(me),
            None => {
                spare.push_back(me);
                self.recs.insert_reserved(
                    key,
                    Rec {
                        owner,
                        waiters: spare,
                    },
                );
            }
        }
        self.waiting
            .insert_reserved(me, Waiting { key, outcome: None });
    }

    /// `futex_unlock_pi` by `me`: the lock goes to the head of the queue, or the word becomes 0. Allocates nothing; the changes go to `sink`.
    pub fn unlock<W: Word>(
        &mut self,
        d: &mut Donations,
        w: &W,
        key: Key,
        me: Tid,
        sink: Sink<'_>,
    ) -> Result<Unlock, PiError> {
        if !fits(me) {
            return Err(PiError::NoId);
        }
        loop {
            let v = w.load();
            if v & OWNER_DIED != 0 || Tid::from(v & OWNER_MASK) != me {
                return Err(PiError::NotOwner);
            }
            // (a record of another owner than the word's: user stores made it stale; its waiters are ended, and this unlock frees the word)
            if self.recs.get(&key).is_some_and(|r| r.owner != me) {
                self.dissolve(d, key, sink);
            }
            let Some(rec) = self.recs.get_mut(&key).filter(|r| !r.waiters.is_empty()) else {
                // nobody queued (a `WAITERS` bit left by a waiter that gave up): free
                if w.cas(v, 0).is_err() {
                    continue;
                }
                self.recs.remove(&key);
                return Ok(Unlock::Free);
            };
            // the waiter with the best effective priority now (donations that reached a waiter while it was queued count), first come among equals
            let mut best: Option<(usize, Tid, u8)> = None;
            for (i, &t) in rec.waiters.iter().enumerate() {
                let e = d.eff(t).unwrap_or(0);
                if best.is_none_or(|(_, _, be)| e > be) {
                    best = Some((i, t, e));
                }
            }
            let Some((at, first, _)) = best else {
                continue; // (filtered above: the queue is not empty)
            };
            let more = rec.waiters.len() > 1;
            let new = first as u32 | if more { WAITERS } else { 0 };
            if w.cas(v, new).is_err() {
                continue; // a waiter set `WAITERS` meanwhile: again
            }
            self.hand_off(d, key, at, first, more, sink);
            return Ok(Unlock::Handed { to: first });
        }
    }

    /// `unlock`'s hand-off of `key` to `first` (at `at` in its queue; `more`: others still wait): it is granted, its donation goes, and every other
    /// waiter's donation moves to it (a frame of its own: KT13).
    fn hand_off(
        &mut self,
        d: &mut Donations,
        key: Key,
        at: usize,
        first: Tid,
        more: bool,
        sink: Sink<'_>,
    ) {
        let Some(rec) = self.recs.get_mut(&key) else {
            return;
        };
        rec.waiters.remove(at);
        if let Some(wt) = self.waiting.get_mut(&first) {
            wt.outcome = Some(Outcome::Granted);
        }
        let mut ch = crate::donate::Changes::default();
        d.revoke_into(first, &mut ch);
        sink(ch.as_slice());
        for &t in &rec.waiters {
            // (each waiter's lend moves from the old owner to the new one)
            retarget_to(d, t, first, sink);
        }
        if more {
            rec.owner = first;
        } else {
            self.recs.remove(&key);
        }
    }

    /// How the wait of `who` ended, if the owner ended it (and forget that).
    pub fn take_outcome(&mut self, who: Tid) -> Option<Outcome> {
        self.waiting.get(&who)?.outcome?;
        self.waiting.remove(&who)?.outcome
    }

    /// Is `who` still queued (not handed the lock, not woken by its owner's death)?
    pub fn still_waiting(&self, who: Tid) -> bool {
        self.waiting.get(&who).is_some_and(|w| w.outcome.is_none())
    }

    /// `who`, queued, gives up (a timeout, a kill, an interrupt): it leaves the queue and its donation is revoked (the priorities that moved go to
    /// `sink`); nothing if it was not queued (it was handed the lock or woken meanwhile).
    pub fn cancel(&mut self, d: &mut Donations, who: Tid, sink: Sink<'_>) {
        if self.still_waiting(who) && self.leave_queue(who) {
            sink(d.revoke(who).as_slice());
        }
    }

    /// The keys of the PI futexes with waiters that `owner` owns (the kernel reads their words, outside the bucket lock, before `owner_died`).
    pub fn owned_keys(&self, owner: Tid) -> Vec<Key> {
        self.recs
            .iter()
            .filter(|(_, r)| r.owner == owner)
            .map(|(&k, _)| k)
            .collect()
    }

    /// The key of one PI futex with waiters that `owner` owns (none when there is none): the owner's death takes them one at a time, allocating
    /// nothing.
    pub fn first_owned_key(&self, owner: Tid) -> Option<Key> {
        self.recs
            .iter()
            .find(|(_, r)| r.owner == owner)
            .map(|(&k, _)| k)
    }

    /// Thread `dead` has gone: the record of `key` (which it owns) ends. `word` is the word's value now (`None` if its page is gone). The waiters
    /// end with `Died`, their donations are revoked (to `sink`) and they are in `drain_wake`. Returns whether the word is to be poisoned (`OWNER_DIED`:
    /// the word still names the dead thread or is gone); a word that names someone else is left alone (the record is dissolved). Allocates nothing.
    pub fn owner_died_one(
        &mut self,
        d: &mut Donations,
        dead: Tid,
        key: Key,
        word: Option<u32>,
        sink: Sink<'_>,
    ) -> bool {
        if self.recs.get(&key).is_none_or(|r| r.owner != dead) {
            return false;
        }
        if word.is_some_and(|v| Tid::from(v & OWNER_MASK) != dead) {
            self.dissolve(d, key, sink);
            return false;
        }
        let Some(rec) = self.recs.remove(&key) else {
            return false;
        };
        for t in rec.waiters {
            if let Some(w) = self.waiting.get_mut(&t) {
                w.outcome = Some(Outcome::Died);
            }
            self.to_wake.push(t);
            sink(d.revoke(t).as_slice());
        }
        true
    }

    /// Thread `dead` has gone: every PI futex it owned that has waiters is poisoned. `word_of(key)` is the word's current value (or `None` if its
    /// page is gone). Returns, for each, its key and the waiters to wake (they leave the queue, their donations are revoked), and the priorities
    /// that moved. The caller sets `OWNER_DIED` in the words (`poison`).
    pub fn owner_died(
        &mut self,
        d: &mut Donations,
        dead: Tid,
        word_of: impl Fn(Key) -> Option<u32>,
    ) -> (Vec<(Key, Vec<Tid>)>, Vec<Change>) {
        let mut out = Vec::new();
        let mut changes = Vec::new();
        for key in self.owned_keys(dead) {
            // a word that no longer names the dead thread (user stores handed it on outside the protocol) is not its lock any more: the record
            // is dissolved (its waiters end with `Died` and are in `drain_wake`) and the word is left alone; a word that is gone (its page) or
            // still names the dead thread is poisoned
            if word_of(key).is_some_and(|v| Tid::from(v & OWNER_MASK) != dead) {
                self.dissolve(d, key, &mut |c| changes.extend_from_slice(c));
                continue;
            }
            let Some(rec) = self.recs.remove(&key) else {
                continue;
            };
            let woken: Vec<Tid> = rec.waiters.into_iter().collect();
            for &t in &woken {
                self.waiter_died(d, t, &mut changes);
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
                for t in rec.waiters {
                    self.waiter_died(d, t, &mut changes);
                    gone.push(t);
                }
            }
        }
        (gone, changes)
    }

    /// Waiter `t`'s futex went with its owner: it wakes with `Died`, and its donation goes (the changes are added to `changes`; a frame of its own
    /// for its change list: KT13).
    fn waiter_died(&mut self, d: &mut Donations, t: Tid, changes: &mut Vec<Change>) {
        if let Some(w) = self.waiting.get_mut(&t) {
            w.outcome = Some(Outcome::Died);
        }
        let mut ch = crate::donate::Changes::default();
        d.revoke_into(t, &mut ch);
        changes.extend_from_slice(ch.as_slice());
    }

    /// How many threads are queued (a leak counter for tests).
    pub fn len(&self) -> usize {
        self.waiting
            .values()
            .filter(|w| w.outcome.is_none())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty() && self.recs.is_empty()
    }

    /// The structure's own consistency (PF1 and the indexes), for the model tests: every queued thread is in `waiting` and in one record, no
    /// record is empty, every waiting entry without an outcome is in its record's queue, and the room `lock` keeps is there.
    #[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (an invariant check of the tests)
    pub fn audit(&self, d: &Donations) -> Result<(), alloc::string::String> {
        use alloc::format;
        let mut seen: usize = 0;
        for (key, rec) in self.recs.iter() {
            if rec.waiters.is_empty() {
                return Err(format!("record {key:?} has no waiters"));
            }
            for &t in &rec.waiters {
                seen = seen.saturating_add(1);
                match self.waiting.get(&t) {
                    Some(w) if w.key == *key && w.outcome.is_none() => {}
                    _ => return Err(format!("waiter {t} of {key:?} not in the index")),
                }
                if d.target(t) != Some(rec.owner) {
                    return Err(format!(
                        "PF1: waiter {t} of {key:?} does not donate to owner {}",
                        rec.owner
                    ));
                }
            }
        }
        if seen != self.len() {
            return Err(format!("{} queued, {} indexed", seen, self.len()));
        }
        if self.to_wake.capacity() < self.waiting.len() {
            return Err(format!(
                "the wake list has room for {}, {} are queued",
                self.to_wake.capacity(),
                self.waiting.len()
            ));
        }
        Ok(())
    }

    /// Take `who` out of its queue and forget it. True if it was queued.
    fn leave_queue(&mut self, who: Tid) -> bool {
        let Some(w) = self.waiting.remove(&who) else {
            return false;
        };
        if let Some(rec) = self.recs.get_mut(&w.key) {
            rec.waiters.retain(|&t| t != who);
            if rec.waiters.is_empty() {
                self.recs.remove(&w.key);
            }
        }
        true
    }
}

/// Move `t`'s donation to `to` and tell the sink what changed (a refused move changes nothing). A frame of its own for its change list (KT13).
fn retarget_to(d: &mut Donations, t: Tid, to: Tid, sink: Sink<'_>) {
    let mut ch = crate::donate::Changes::default();
    if d.retarget_into(t, to, &mut ch).is_ok() {
        sink(ch.as_slice());
    }
}
