//! The state of pager-backed memory (MM9, KERNEL-V1-SPEC 4.4): which page ranges of a pager's VMOs have been asked for, which threads wait for
//! them, and what ends a wait. Pure: the kernel blocks and wakes the threads and writes the request messages; this decides what is requested
//! (once per page, not once per waiter), who is woken by a supply, a failure or the pager's death, and that every waiter is woken exactly once.
//!
//! * A fault (or a kernel copy) that needs an absent page `wait`s: the thread is registered as a waiter of (VMO, page), and a request is returned
//!   unless one for that page is pending already.
//! * `supply` and `fail` end the waits of the pages in a range (each waiter gets the result once) and forget the requests.
//! * A pager that dies fails every wait of its VMOs; a thread whose process is killed `cancel`s its wait.

use crate::sched::Tid;
use abi::status::Status;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
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

#[derive(Default)]
struct PagerState {
    /// Pages asked for and not yet supplied or failed.
    requested: BTreeSet<(u64, u64)>,
    /// Requests not yet written to the pager's channel (it was full).
    unsent: VecDeque<Request>,
}

/// Every pager and every thread waiting for one.
#[derive(Default)]
pub struct Pagers {
    pagers: BTreeMap<u64, PagerState>,
    waits: BTreeMap<Tid, Wait>,
}

impl Pagers {
    pub fn new() -> Pagers {
        Pagers::default()
    }

    /// A new pager (its id is the caller's).
    pub fn create(&mut self, pager: u64) {
        self.pagers.entry(pager).or_default();
    }

    /// Does the pager exist (it has not been destroyed)?
    pub fn exists(&self, pager: u64) -> bool {
        self.pagers.contains_key(&pager)
    }

    /// Thread `tid` needs `page` of `vmo`, a VMO of `pager`. Returns the request to send (none if one for the page is pending already), or `Err`
    /// when the pager is gone: the thread is not registered and the caller fails the access. A thread waits for one page at a time.
    pub fn wait(
        &mut self,
        tid: Tid,
        pager: u64,
        vmo: u64,
        page: u64,
    ) -> Result<Option<Request>, Status> {
        let st = self.pagers.get_mut(&pager).ok_or(Status::PeerClosed)?;
        self.waits.insert(
            tid,
            Wait {
                pager,
                vmo,
                page,
                result: None,
            },
        );
        if st.requested.insert((vmo, page)) {
            let r = Request { pager, vmo, page };
            st.unsent.push_back(r);
            return Ok(Some(r));
        }
        Ok(None)
    }

    /// The requests not yet written to a pager's channel, oldest first (the caller removes each with `sent` once it is written).
    pub fn unsent(&self, pager: u64) -> Option<Request> {
        self.pagers
            .get(&pager)
            .and_then(|p| p.unsent.front().copied())
    }

    /// The oldest unsent request of the pager was written.
    pub fn sent(&mut self, pager: u64) {
        if let Some(p) = self.pagers.get_mut(&pager) {
            p.unsent.pop_front();
        }
    }

    /// Are there requests waiting for room in some pager's channel?
    pub fn any_unsent(&self) -> bool {
        self.pagers.values().any(|p| !p.unsent.is_empty())
    }

    /// The pagers with unsent requests.
    pub fn with_unsent(&self) -> Vec<u64> {
        self.pagers
            .iter()
            .filter(|(_, p)| !p.unsent.is_empty())
            .map(|(id, _)| *id)
            .collect()
    }

    fn finish(&mut self, pager: u64, vmo: u64, first: u64, end: u64, outcome: Outcome) -> Vec<Tid> {
        let mut woken = Vec::new();
        for (&tid, w) in self.waits.iter_mut() {
            if w.pager == pager
                && w.vmo == vmo
                && w.result.is_none()
                && w.page >= first
                && w.page < end
            {
                w.result = Some(outcome);
                woken.push(tid);
            }
        }
        if let Some(p) = self.pagers.get_mut(&pager) {
            p.requested
                .retain(|&(v, pg)| !(v == vmo && pg >= first && pg < end));
            p.unsent
                .retain(|r| !(r.vmo == vmo && r.page >= first && r.page < end));
        }
        woken
    }

    /// Pages `first .. first + count` of `vmo` were supplied: the waiters for them are woken (returned) with success.
    pub fn supply(&mut self, pager: u64, vmo: u64, first: u64, count: u64) -> Vec<Tid> {
        self.finish(pager, vmo, first, first.saturating_add(count), Ok(()))
    }

    /// The pager fails pages `first .. first + count` of `vmo` with `status`.
    pub fn fail(
        &mut self,
        pager: u64,
        vmo: u64,
        first: u64,
        count: u64,
        status: Status,
    ) -> Vec<Tid> {
        self.finish(pager, vmo, first, first.saturating_add(count), Err(status))
    }

    /// The pager was destroyed: every wait of its VMOs fails (`PeerClosed`) and it is gone.
    pub fn destroy(&mut self, pager: u64) -> Vec<Tid> {
        self.pagers.remove(&pager);
        let mut woken = Vec::new();
        for (&tid, w) in self.waits.iter_mut() {
            if w.pager == pager && w.result.is_none() {
                w.result = Some(Err(Status::PeerClosed));
                woken.push(tid);
            }
        }
        woken
    }

    /// How the thread's wait ended, if it has.
    pub fn result(&self, tid: Tid) -> Option<Outcome> {
        self.waits.get(&tid).and_then(|w| w.result)
    }

    /// The thread's wait is over (it took the result, or it was killed or interrupted): forget it. A request nobody waits for any more stays
    /// pending until the pager answers it.
    pub fn cancel(&mut self, tid: Tid) -> Option<Outcome> {
        let w = self.waits.remove(&tid)?;
        if w.result.is_none() {
            // abandoned (killed or interrupted): when nobody else waits for the page, its request goes too, so that nothing a dead waiter
            // asked for stays in the kernel's heap until the pager answers (a pager that never does would hold it for ever)
            let others = self.waits.values().any(|o| {
                o.result.is_none() && (o.pager, o.vmo, o.page) == (w.pager, w.vmo, w.page)
            });
            if !others {
                if let Some(p) = self.pagers.get_mut(&w.pager) {
                    p.requested.remove(&(w.vmo, w.page));
                    p.unsent.retain(|r| !(r.vmo == w.vmo && r.page == w.page));
                }
            }
        }
        w.result
    }

    /// How many requests are pending in all (a leak check).
    pub fn pending_requests(&self) -> usize {
        self.pagers.values().map(|p| p.requested.len()).sum()
    }

    /// Is the pager's channel backed up (it has requests not written yet)?
    pub fn has_unsent(&self, pager: u64) -> bool {
        self.pagers
            .get(&pager)
            .is_some_and(|p| !p.unsent.is_empty())
    }

    /// Is the thread waiting (registered, with no result yet)?
    pub fn waiting(&self, tid: Tid) -> bool {
        self.waits.get(&tid).is_some_and(|w| w.result.is_none())
    }

    /// How many threads have a wait registered (a leak check).
    pub fn waits(&self) -> usize {
        self.waits.len()
    }
}
