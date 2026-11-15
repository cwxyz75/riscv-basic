//! The priority donation graph (SCHED-SMP-SPEC 4.8, task 7): pure, no locks, no allocation per change list.
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

use crate::sched::{Prio, Tid};
use alloc::collections::{BTreeMap, BTreeSet};
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
    ins: BTreeSet<Tid>,
}

#[derive(Default)]
pub struct Donations {
    nodes: BTreeMap<Tid, Node>,
}

impl Donations {
    pub fn new() -> Donations {
        Donations::default()
    }

    pub fn add_thread(&mut self, tid: Tid, base: Prio) {
        self.nodes.insert(
            tid,
            Node {
                base,
                eff: base,
                out: None,
                ins: BTreeSet::new(),
            },
        );
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
        let n = &self.nodes[&tid];
        n.ins
            .iter()
            .map(|d| self.nodes[d].eff)
            .fold(n.base, Prio::max)
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
        self.nodes[&tid]
            .ins
            .iter()
            .map(|&d| 1 + self.height(d))
            .max()
            .unwrap_or(0)
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
        self.nodes.get_mut(&from).expect("checked").out = Some(to);
        self.nodes.get_mut(&to).expect("checked").ins.insert(from);
        let mut out = Changes::new();
        self.propagate(to, &mut out);
        Ok(out)
    }

    /// End `from`'s donation (its wait ended): whatever it lifted drops back.
    pub fn revoke(&mut self, from: Tid) -> Changes {
        let mut out = Changes::new();
        let Some(to) = self.nodes.get_mut(&from).and_then(|n| n.out.take()) else {
            return out;
        };
        self.nodes
            .get_mut(&to)
            .expect("target exists")
            .ins
            .remove(&from);
        self.propagate(to, &mut out);
        out
    }

    /// Move `from`'s edge to `to` (revoke, then donate), as one change list. On a refusal the old edge is restored
    /// (nothing changes).
    pub fn retarget(&mut self, from: Tid, to: Tid) -> Result<Changes, DonateError> {
        let old = self.nodes.get(&from).ok_or(DonateError::NoThread)?.out;
        if !self.nodes.contains_key(&to) {
            return Err(DonateError::NoThread);
        }
        // the effective priorities of everything either chain can touch, before anything moves
        let mut before: alloc::vec::Vec<(Tid, Prio)> = alloc::vec::Vec::new();
        for start in [old, Some(to)].into_iter().flatten() {
            let mut t = Some(start);
            while let Some(x) = t {
                if !before.iter().any(|&(y, _)| y == x) {
                    before.push((x, self.nodes[&x].eff));
                }
                t = self.nodes[&x].out;
            }
        }
        let first = self.revoke(from);
        match self.donate(from, to) {
            Ok(second) => {
                // one list of *net* changes (a thread lowered by the revoke and raised back by the donation
                // did not change), in the order the changes propagated
                let mut all = Changes::new();
                for c in first.as_slice().iter().chain(second.as_slice()) {
                    let net = self.nodes[&c.tid].eff;
                    let was = before.iter().find(|&&(t, _)| t == c.tid).map(|&(_, e)| e);
                    let listed = all.items[..all.len].iter().any(|x| x.tid == c.tid);
                    if !listed && was != Some(net) {
                        all.push(Change {
                            tid: c.tid,
                            eff: net,
                        });
                    }
                }
                Ok(all)
            }
            Err(e) => {
                if let Some(o) = old {
                    let _ = self.donate(from, o); // (restores the edge; cannot fail: it existed a moment ago)
                }
                Err(e)
            }
        }
    }

    /// Forget a thread: its out-edge and every in-edge go with it.
    pub fn remove_thread(&mut self, tid: Tid) -> Changes {
        let mut out = Changes::new();
        if !self.nodes.contains_key(&tid) {
            return out;
        }
        let donors: alloc::vec::Vec<Tid> = self.nodes[&tid].ins.iter().copied().collect();
        for d in donors {
            self.nodes.get_mut(&d).expect("donor exists").out = None;
        }
        let target = self.nodes[&tid].out;
        self.nodes.remove(&tid);
        if let Some(to) = target {
            self.nodes
                .get_mut(&to)
                .expect("target exists")
                .ins
                .remove(&tid);
            self.propagate(to, &mut out);
        }
        out
    }

    /// DN1 to DN3 from scratch: the edges are consistent, the graph is acyclic and no chain is too long, and every
    /// effective priority equals the recomputed maximum.
    pub fn audit(&self) -> Result<(), String> {
        for (&t, n) in &self.nodes {
            if let Some(o) = n.out {
                match self.nodes.get(&o) {
                    Some(on) if on.ins.contains(&t) => {}
                    _ => return Err(alloc::format!("edge {t} -> {o} has no mirror")),
                }
            }
            for d in &n.ins {
                if self.nodes.get(d).and_then(|x| x.out) != Some(t) {
                    return Err(alloc::format!("in-edge {d} -> {t} has no mirror"));
                }
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
        Ok(())
    }
}
