//! The priority donation graph (SCHED-SMP-SPEC 4.8, task 7): pure, no locks. It allocates only when a thread is added (`reserve_thread`, then
//! `add_thread`: the spawner's reportable step, T248); donating, revoking, retargeting and removing allocate nothing (the in-edges are an intrusive list).
//!
//! A thread has a *base* priority (its own attribute) and an *effective* priority: the maximum of its base and the
//! effective priority of every thread **donating** to it. A thread that blocks waiting for another (a `channel_call`
//! waiting for its server, a lock waiter waiting for the owner) lends its effective priority to that thread until the
//! wait ends. A thread has at most one out-edge (it blocks on one thing) and any number of in-edges.
//!
//! Every mutation returns the **change list**: the threads whose effective priority changed, in the order the change
//! propagates, at most `2 * (MAX_DONATION_DEPTH + 1)` of them (a retarget touches two chains). The kernel applies them to the run queues. To keep the
//! effective priority exact with bounded work, a donation that would close a cycle or make a chain longer than
//! `MAX_DONATION_DEPTH` is **refused** (no edge is made), so a propagation is never cut short:
//!
//! * **DN1** `eff(t) = max(base(t), eff(d) for every donor d of t)`, exactly (`audit` checks it from scratch);
//! * **DN2** the graph is acyclic;
//! * **DN3** an edge exists only while its donor is in the wait that made it (the kernel's side: every wait exit
//!   revokes; `remove_thread` drops a dying thread's edges).
//!
//! Unused until tasks 22 and 23 produce edges (the stub stage).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)] // INVARIANT: the donation graph is not wired into the kernel yet (task 22): every lookup is of a thread that `add_thread` put in `nodes` and `audit` checks that; the item arrays are sized for the longest chain

use crate::fmap::{FMap, NoMem};
use crate::sched::{Prio, Tid};
use alloc::string::String;

/// The longest chain of donations: a donor, then up to this many hops to the thread that finally runs.
pub const MAX_DONATION_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub tid: Tid,
    pub eff: Prio,
}

/// A bounded change list: no allocation.
#[derive(Clone, Copy, Debug)]
pub struct Changes {
    items: [Change; 2 * (MAX_DONATION_DEPTH + 1)],
    len: usize,
}

impl Default for Changes {
    fn default() -> Changes {
        Changes::new()
    }
}

impl Changes {
    fn new() -> Changes {
        Changes {
            items: [Change { tid: 0, eff: 0 }; 2 * (MAX_DONATION_DEPTH + 1)],
            len: 0,
        }
    }
    fn push(&mut self, c: Change) {
        // (a chain has at most MAX_DONATION_DEPTH + 1 threads and a retarget touches two chains, so the list cannot
        // overflow)
        self.items[self.len] = c;
        self.len += 1;
    }
    pub fn as_slice(&self) -> &[Change] {
        &self.items[..self.len]
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DonateError {
    NoThread,
    /// The donation would close a cycle (a deadlock of blocked calls: their deadlines break it).
    Cycle,
    /// The chain would be longer than `MAX_DONATION_DEPTH`.
    TooDeep,
    /// `from` already lends to someone (`retarget` moves an edge).
    AlreadyDonating,
}

struct Node {
    base: Prio,
    eff: Prio,
    out: Option<Tid>,
    /// The donors to this thread: the head of an intrusive list through the donors' own `next_in`/`prev_in` (a thread lends to at most one, so it
    /// is on at most one list).
    first_in: Option<Tid>,
    next_in: Option<Tid>,
    prev_in: Option<Tid>,
}

#[derive(Default)]
pub struct Donations {
    nodes: FMap<Tid, Node>,
}

impl Donations {
    pub const fn new() -> Donations {
        Donations { nodes: FMap::new() }
    }

    /// Room for one more thread: the spawner's reportable step. The `add_thread` that follows, in the same critical section, allocates nothing.
    pub fn reserve_thread(&mut self) -> Result<(), NoMem> {
        self.nodes.reserve(1)
    }

    /// Add a thread at `base` (allocates only if `reserve_thread` was not called first).
    pub fn add_thread(&mut self, tid: Tid, base: Prio) -> Result<(), NoMem> {
        self.nodes
            .try_insert(
                tid,
                Node {
                    base,
                    eff: base,
                    out: None,
                    first_in: None,
                    next_in: None,
                    prev_in: None,
                },
            )
            .map(|_| ())
    }

    /// The donors to `tid`, in list order.
    fn ins(&self, tid: Tid) -> Ins<'_> {
        Ins {
            g: self,
            next: self.nodes.get(&tid).and_then(|n| n.first_in),
        }
    }

    /// Put `from` at the head of `to`'s list of donors.
    fn link(&mut self, from: Tid, to: Tid) {
        let head = self.nodes[&to].first_in;
        if let Some(h) = head {
            self.nodes.get_mut(&h).expect("donor exists").prev_in = Some(from);
        }
        let f = self.nodes.get_mut(&from).expect("donor exists");
        f.out = Some(to);
        f.next_in = head;
        f.prev_in = None;
        self.nodes.get_mut(&to).expect("target exists").first_in = Some(from);
    }

    /// Take `from` off its target's list (its `out` is cleared); the target, if there was one.
    fn unlink(&mut self, from: Tid) -> Option<Tid> {
        let n = self.nodes.get_mut(&from)?;
        let to = n.out.take()?;
        let (next, prev) = (n.next_in.take(), n.prev_in.take());
        match prev {
            Some(p) => self.nodes.get_mut(&p).expect("donor exists").next_in = next,
            None => self.nodes.get_mut(&to).expect("target exists").first_in = next,
        }
        if let Some(nx) = next {
            self.nodes.get_mut(&nx).expect("donor exists").prev_in = prev;
        }
        Some(to)
    }

    pub fn eff(&self, tid: Tid) -> Option<Prio> {
        self.nodes.get(&tid).map(|n| n.eff)
    }

    pub fn base(&self, tid: Tid) -> Option<Prio> {
        self.nodes.get(&tid).map(|n| n.base)
    }

    /// The thread `from` lends to, if any.
    pub fn target(&self, from: Tid) -> Option<Tid> {
        self.nodes.get(&from)?.out
    }

    pub fn thread_count(&self) -> usize {
        self.nodes.len()
    }

    fn recompute(&self, tid: Tid) -> Prio {
        self.ins(tid)
            .map(|d| self.nodes[&d].eff)
            .fold(self.nodes[&tid].base, Prio::max)
    }

    /// Re-derive `tid`'s effective priority and carry any change up the out-chain, recording each change.
    fn propagate(&mut self, mut tid: Tid, out: &mut Changes) {
        loop {
            let new = self.recompute(tid);
            let node = self.nodes.get_mut(&tid).expect("thread exists");
            if node.eff == new {
                return;
            }
            node.eff = new;
            out.push(Change { tid, eff: new });
            match node.out {
                Some(next) => tid = next,
                None => return,
            }
        }
    }

    /// How many hops `from` is below the end of its chain (0 for a thread that lends to nobody).
    fn up(&self, mut tid: Tid) -> usize {
        let mut n = 0;
        while let Some(next) = self.nodes[&tid].out {
            n += 1;
            tid = next;
        }
        n
    }

    /// The longest chain of donors below `tid` (0 for a thread nobody lends to).
    fn height(&self, tid: Tid) -> usize {
        self.ins(tid).map(|d| 1 + self.height(d)).max().unwrap_or(0)
    }

    pub fn set_base(&mut self, tid: Tid, base: Prio) -> Changes {
        let mut out = Changes::new();
        if let Some(n) = self.nodes.get_mut(&tid) {
            n.base = base;
            self.propagate(tid, &mut out);
        }
        out
    }

    /// `from` (which is about to block on `to`) lends its effective priority to `to` until `revoke(from)`.
    pub fn donate(&mut self, from: Tid, to: Tid) -> Result<Changes, DonateError> {
        let mut out = Changes::new();
        self.donate_into(from, to, &mut out)?;
        Ok(out)
    }

    /// `donate`, its changes added to `out` (a change list is a few hundred bytes: one per frame, KT13).
    pub fn donate_into(
        &mut self,
        from: Tid,
        to: Tid,
        out: &mut Changes,
    ) -> Result<(), DonateError> {
        if !self.nodes.contains_key(&from) || !self.nodes.contains_key(&to) {
            return Err(DonateError::NoThread);
        }
        if self.nodes[&from].out.is_some() {
            return Err(DonateError::AlreadyDonating);
        }
        // a cycle: `to`'s chain reaches `from` (or `from` is `to`)
        let mut t = to;
        loop {
            if t == from {
                return Err(DonateError::Cycle);
            }
            match self.nodes[&t].out {
                Some(next) => t = next,
                None => break,
            }
        }
        if self.height(from) + 1 + self.up(to) > MAX_DONATION_DEPTH {
            return Err(DonateError::TooDeep);
        }
        self.link(from, to);
        self.propagate(to, out);
        Ok(())
    }

    /// End `from`'s donation (its wait ended): whatever it lifted drops back.
    pub fn revoke(&mut self, from: Tid) -> Changes {
        let mut out = Changes::new();
        self.revoke_into(from, &mut out);
        out
    }

    /// `revoke`, its changes added to `out`.
    pub fn revoke_into(&mut self, from: Tid, out: &mut Changes) {
        if let Some(to) = self.unlink(from) {
            self.propagate(to, out);
        }
    }

    /// Move `from`'s edge to `to` (revoke, then donate), as one change list. On a refusal the old edge is restored
    /// (nothing changes).
    pub fn retarget(&mut self, from: Tid, to: Tid) -> Result<Changes, DonateError> {
        let mut all = Changes::new();
        self.retarget_into(from, to, &mut all)?;
        Ok(all)
    }

    /// `retarget`, its net changes added to `all` (each step a frame of its own: a change list and the priorities before are a few hundred
    /// bytes each, KT13).
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: nb counts distinct threads of two chains of at most MAX_DONATION_DEPTH + 1, the array's length; nodes on a chain exist (DN1)
    pub fn retarget_into(
        &mut self,
        from: Tid,
        to: Tid,
        all: &mut Changes,
    ) -> Result<(), DonateError> {
        let old = self.nodes.get(&from).ok_or(DonateError::NoThread)?.out;
        if !self.nodes.contains_key(&to) {
            return Err(DonateError::NoThread);
        }
        // the effective priorities of everything either chain can touch, before anything moves
        // (two chains of at most MAX_DONATION_DEPTH + 1 threads each: an array, nothing allocated)
        let mut before = [(0 as Tid, 0 as Prio); 2 * (MAX_DONATION_DEPTH + 1)];
        let mut nb = 0;
        if let Some(o) = old {
            nb = self.chain_prios(o, &mut before, nb);
        }
        nb = self.chain_prios(to, &mut before, nb);
        self.retarget_move(from, to, old, &before[..nb], all)
    }

    /// Add the threads of `start`'s chain that are not in `before[..nb]` yet, with their effective priorities; the new count.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: as `retarget_into`
    fn chain_prios(&self, start: Tid, before: &mut [(Tid, Prio)], mut nb: usize) -> usize {
        let mut t = Some(start);
        while let Some(x) = t {
            if !before[..nb].iter().any(|&(y, _)| y == x) {
                before[nb] = (x, self.nodes[&x].eff);
                nb += 1;
            }
            t = self.nodes[&x].out;
        }
        nb
    }

    /// The move of `retarget_into`: revoke, donate, and the net list (a thread lowered by the revoke and raised back by the donation did not
    /// change), in the order the changes propagated; on a refusal the old edge is restored.
    #[allow(clippy::indexing_slicing)] // INVARIANT: a changed thread is a node (DN1)
    fn retarget_move(
        &mut self,
        from: Tid,
        to: Tid,
        old: Option<Tid>,
        before: &[(Tid, Prio)],
        all: &mut Changes,
    ) -> Result<(), DonateError> {
        let mut first = Changes::new();
        self.revoke_into(from, &mut first);
        let mut second = Changes::new();
        if let Err(e) = self.donate_into(from, to, &mut second) {
            if let Some(o) = old {
                let _ = self.donate_into(from, o, &mut second); // (restores the edge; cannot fail: it existed a moment ago; its changes undo the revoke's and are dropped)
            }
            return Err(e);
        }
        self.net_changes(first.as_slice(), before, all);
        self.net_changes(second.as_slice(), before, all);
        Ok(())
    }

    /// Add to `all` the threads of `changes` whose effective priority is not what it was `before` and that it does not list yet.
    #[allow(clippy::indexing_slicing)] // INVARIANT: a changed thread is a node (DN1)
    fn net_changes(&self, changes: &[Change], before: &[(Tid, Prio)], all: &mut Changes) {
        for c in changes {
            let net = self.nodes[&c.tid].eff;
            let was = before.iter().find(|&&(t, _)| t == c.tid).map(|&(_, e)| e);
            let listed = all.as_slice().iter().any(|x| x.tid == c.tid);
            if !listed && was != Some(net) {
                all.push(Change {
                    tid: c.tid,
                    eff: net,
                });
            }
        }
    }

    /// Forget a thread: its out-edge and every in-edge go with it.
    pub fn remove_thread(&mut self, tid: Tid) -> Changes {
        let mut out = Changes::new();
        if !self.nodes.contains_key(&tid) {
            return out;
        }
        // (the donors are taken off one at a time: no list of them is made)
        while let Some(d) = self.nodes[&tid].first_in {
            self.unlink(d);
        }
        let target = self.unlink(tid);
        self.nodes.remove(&tid);
        if let Some(to) = target {
            self.propagate(to, &mut out);
        }
        out
    }

    /// DN1 to DN3 from scratch: the edges are consistent, the graph is acyclic and no chain is too long, and every
    /// effective priority equals the recomputed maximum.
    #[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (an invariant check of the tests)
    pub fn audit(&self) -> Result<(), String> {
        for (&t, n) in self.nodes.iter() {
            if let Some(o) = n.out {
                if !self.nodes.contains_key(&o) || !self.ins(o).any(|d| d == t) {
                    return Err(alloc::format!("edge {t} -> {o} has no mirror"));
                }
            }
            let mut prev = None;
            let mut seen = 0usize;
            for d in self.ins(t) {
                seen += 1;
                let dn = self.nodes.get(&d);
                if dn.and_then(|x| x.out) != Some(t) || dn.and_then(|x| x.prev_in) != prev {
                    return Err(alloc::format!("in-edge {d} -> {t} has no mirror"));
                }
                if seen > self.nodes.len() {
                    return Err(alloc::format!("thread {t}: its donor list has a cycle"));
                }
                prev = Some(d);
            }
            // acyclic, bounded
            let mut hops = 0;
            let mut cur = t;
            while let Some(next) = self.nodes[&cur].out {
                hops += 1;
                if next == t || hops > MAX_DONATION_DEPTH {
                    return Err(alloc::format!(
                        "thread {t}: cycle or chain longer than the bound"
                    ));
                }
                cur = next;
            }
            let want = self.recompute(t);
            if n.eff != want {
                return Err(alloc::format!(
                    "thread {t}: eff {} but the donors say {want} (DN1)",
                    n.eff
                ));
            }
        }
        // every out-edge is on exactly one list (counted from the lists)
        let edges = self.nodes.iter().filter(|(_, n)| n.out.is_some()).count();
        let listed: usize = self.nodes.iter().map(|(&t, _)| self.ins(t).count()).sum();
        if edges != listed {
            return Err(alloc::format!("{edges} edges, {listed} on the donor lists"));
        }
        Ok(())
    }
}

/// The donors of a thread, by the intrusive list.
struct Ins<'a> {
    g: &'a Donations,
    next: Option<Tid>,
}

impl Iterator for Ins<'_> {
    type Item = Tid;
    fn next(&mut self) -> Option<Tid> {
        let cur = self.next?;
        self.next = self.g.nodes.get(&cur).and_then(|n| n.next_in);
        Some(cur)
    }
}
