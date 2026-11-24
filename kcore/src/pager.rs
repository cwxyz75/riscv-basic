//! The state of pager-backed memory (MM9, KERNEL-V1-SPEC 4.4): which page ranges of a pager's VMOs have been asked for, which threads wait for
//! them, and what ends a wait. Pure: the kernel blocks and wakes the threads and writes the request messages; this decides what is requested
//! (once per page, not once per waiter), who is woken by a supply, a failure or the pager's death, and that every waiter is woken exactly once.
//!
//! * A fault (or a kernel copy) that needs an absent page `wait`s: the thread is registered as a waiter of (VMO, page), and a request is returned
//!   unless one for that page is pending already.
//! * `supply` and `fail` end the waits of the pages in a range (each waiter gets the result once) and forget the requests.
//! * A pager that dies fails every wait of its VMOs; a thread whose process is killed `cancel`s its wait.

use crate::fmap::{FMap, NoMem};
use crate::sched::Tid;
use abi::status::Status;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// A request for the pager: the pages of `vmo` from `page`, one page (the pager may supply more).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub pager: u64,
    pub vmo: u64,
    pub page: u64,
}

/// How a wait ended.
pub type Outcome = Result<(), Status>;

struct Wait {
    pager: u64,
    vmo: u64,
    page: u64,
    result: Option<Outcome>,
}

/// Every pager and every thread waiting for one.
///
/// Allocation (T248): a thread that can wait gets its room when it is made (`reserve_room`, the spawner's reportable step), for its
/// wait, its request and its wake; after that `wait`, `supply`, `fail`, `destroy` and `cancel` allocate nothing. A page is "requested" exactly
/// while a wait without a result names it (it is derived, not kept). The threads a call ends are collected in one vector with room for them all and handed to the caller's waker
/// (`take_woken`) once it let go of its locks.
#[derive(Default)]
pub struct Pagers {
    pagers: FMap<u64, ()>,
    /// One entry per waiting thread, in no order: a vector with room for every thread (a tree would need nodes at a wait).
    waits: Vec<(Tid, Wait)>,
    /// Requests not yet written to their pager's channel (it was full), oldest first, all pagers: at most one per page waited for.
    unsent: VecDeque<Request>,
    /// The threads whose wait just ended, until the caller wakes them (`drain_woken`): room for every thread, each ending once.
    woken: Vec<Tid>,
}

impl Pagers {
    pub fn new() -> Pagers {
        Pagers::default()
    }

    /// Room for `n` threads to wait at once (the call that makes the `n`th thread asks for it, before it changes anything). Reportable.
    pub fn reserve_room(&mut self, n: usize) -> Result<(), NoMem> {
        self.waits
            .try_reserve(n.saturating_sub(self.waits.len()))
            .map_err(|_| NoMem)?;
        self.unsent
            .try_reserve(n.saturating_sub(self.unsent.len()))
            .map_err(|_| NoMem)?;
        self.woken
            .try_reserve(n.saturating_sub(self.woken.len()))
            .map_err(|_| NoMem)
    }

    /// Move up to `out.len()` of the threads whose wait ended into `out` (the caller wakes them after it let go of its locks); returns how many.
    pub fn take_woken(&mut self, out: &mut [Tid]) -> usize {
        let mut n = 0;
        while let Some(slot) = out.get_mut(n) {
            let Some(t) = self.woken.pop() else { break };
            *slot = t;
            n = n.saturating_add(1);
        }
        n
    }

    /// Hand the threads whose wait ended to `sink`, and forget them (the tests' way to see them).
    pub fn drain_woken(&mut self, sink: &mut dyn FnMut(&[Tid])) {
        if !self.woken.is_empty() {
            sink(&self.woken);
            self.woken.clear();
        }
    }

    /// Make room for one more pager (`create` then cannot fail). Reportable.
    pub fn reserve_pager(&mut self) -> Result<(), NoMem> {
        self.pagers.reserve(1)
    }

    /// A new pager (its id is the caller's); `reserve_pager` came first.
    pub fn create(&mut self, pager: u64) {
        self.pagers.insert_reserved(pager, ());
    }

    /// Does the pager exist (it has not been destroyed)?
    pub fn exists(&self, pager: u64) -> bool {
        self.pagers.contains_key(&pager)
    }

    /// Thread `tid` needs `page` of `vmo`, a VMO of `pager`. Returns the request to send (none if one for the page is pending already), or `Err`
    /// when the pager is gone: the thread is not registered and the caller fails the access (`NoMemory` for a thread that had no room made). A thread waits for one page at a time.
    pub fn wait(
        &mut self,
        tid: Tid,
        pager: u64,
        vmo: u64,
        page: u64,
    ) -> Result<Option<Request>, Status> {
        if !self.pagers.contains_key(&pager) {
            return Err(Status::PeerClosed);
        }
        let pending = self
            .waits
            .iter()
            .any(|(_, w)| w.result.is_none() && (w.pager, w.vmo, w.page) == (pager, vmo, page));
        // (the room first: nothing changes on a refusal)
        if self.waits.len() >= self.waits.capacity() {
            return Err(Status::NoMemory);
        }
        // (a record left by an earlier wait of the thread is replaced)
        if let Some(i) = self.waits.iter().position(|(t, _)| *t == tid) {
            self.waits.swap_remove(i);
        }
        if !pending {
            self.unsent.try_reserve(1).map_err(|_| Status::NoMemory)?;
        }
        self.waits.push((
            tid,
            Wait {
                pager,
                vmo,
                page,
                result: None,
            },
        ));
        if !pending {
            let r = Request { pager, vmo, page };
            self.unsent.push_back(r);
            return Ok(Some(r));
        }
        Ok(None)
    }

    /// The requests not yet written to a pager's channel, oldest first (the caller removes each with `sent` once it is written).
    pub fn unsent(&self, pager: u64) -> Option<Request> {
        self.unsent.iter().find(|r| r.pager == pager).copied()
    }

    /// The oldest unsent request of the pager was written.
    pub fn sent(&mut self, pager: u64) {
        if let Some(i) = self.unsent.iter().position(|r| r.pager == pager) {
            self.unsent.remove(i);
        }
    }

    /// Are there requests waiting for room in some pager's channel?
    pub fn any_unsent(&self) -> bool {
        !self.unsent.is_empty()
    }

    /// The pagers with unsent requests.
    pub fn with_unsent(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for r in &self.unsent {
            if !out.contains(&r.pager) {
                out.push(r.pager);
            }
        }
        out
    }

    fn finish(&mut self, pager: u64, vmo: u64, first: u64, end: u64, outcome: Outcome) {
        for (tid, w) in self.waits.iter_mut() {
            if w.pager == pager
                && w.vmo == vmo
                && w.result.is_none()
                && w.page >= first
                && w.page < end
            {
                w.result = Some(outcome);
                self.woken.push(*tid);
            }
        }
        self.unsent
            .retain(|r| !(r.pager == pager && r.vmo == vmo && r.page >= first && r.page < end));
    }

    /// Pages `first .. first + count` of `vmo` were supplied: the waiters for them end with success (`take_woken` has them).
    pub fn supply(&mut self, pager: u64, vmo: u64, first: u64, count: u64) {
        self.finish(pager, vmo, first, first.saturating_add(count), Ok(()))
    }

    /// The pager fails pages `first .. first + count` of `vmo` with `status`.
    pub fn fail(&mut self, pager: u64, vmo: u64, first: u64, count: u64, status: Status) {
        self.finish(pager, vmo, first, first.saturating_add(count), Err(status))
    }

    /// The pager was destroyed: every wait of its VMOs fails (`PeerClosed`) and it is gone.
    pub fn destroy(&mut self, pager: u64) {
        self.pagers.remove(&pager);
        self.unsent.retain(|r| r.pager != pager);
        for (tid, w) in self.waits.iter_mut() {
            if w.pager == pager && w.result.is_none() {
                w.result = Some(Err(Status::PeerClosed));
                self.woken.push(*tid);
            }
        }
    }

    /// How the thread's wait ended, if it has.
    pub fn result(&self, tid: Tid) -> Option<Outcome> {
        self.waits
            .iter()
            .find(|(t, _)| *t == tid)
            .and_then(|(_, w)| w.result)
    }

    /// The thread's wait is over (it took the result, or it was killed or interrupted): forget it. A request nobody waits for any more stays
    /// pending until the pager answers it.
    pub fn cancel(&mut self, tid: Tid) -> Option<Outcome> {
        let i = self.waits.iter().position(|(t, _)| *t == tid)?;
        let (_, w) = self.waits.swap_remove(i);
        if w.result.is_none() {
            // abandoned (killed or interrupted): when nobody else waits for the page, its request goes too, so that nothing a dead waiter
            // asked for stays in the kernel's heap until the pager answers (a pager that never does would hold it for ever)
            let others = self.waits.iter().map(|(_, o)| o).any(|o| {
                o.result.is_none() && (o.pager, o.vmo, o.page) == (w.pager, w.vmo, w.page)
            });
            if !others {
                self.unsent
                    .retain(|r| !(r.pager == w.pager && r.vmo == w.vmo && r.page == w.page));
            }
        }
        w.result
    }

    /// How many requests are pending in all (a leak check).
    pub fn pending_requests(&self) -> usize {
        let mut seen: Vec<(u64, u64, u64)> = Vec::new();
        for (_, w) in &self.waits {
            let k = (w.pager, w.vmo, w.page);
            if w.result.is_none() && !seen.contains(&k) {
                seen.push(k);
            }
        }
        seen.len()
    }

    /// Is the pager's channel backed up (it has requests not written yet)?
    pub fn has_unsent(&self, pager: u64) -> bool {
        self.unsent.iter().any(|r| r.pager == pager)
    }

    /// Is the thread waiting (registered, with no result yet)?
    pub fn waiting(&self, tid: Tid) -> bool {
        self.waits
            .iter()
            .any(|(t, w)| *t == tid && w.result.is_none())
    }

    /// How many threads have a wait registered (a leak check).
    pub fn waits(&self) -> usize {
        self.waits.len()
    }
}
