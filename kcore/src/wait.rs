//! The wait table (slice W step 3, W-SPEC section 3): which threads wait on which object, and which of
//! them a batch of touched objects releases. Pure bookkeeping over the registry's dirty log: the kernel
//! registers a thread (checking its own predicate first, in one step under the big kernel lock), and
//! `commit` returns exactly the threads whose wait a touch can have ended. Signals stay derived: the
//! table only evaluates them for touched objects.
//!
//! A registration is removed when it fires; a thread that is woken any other way (a timer, a kill)
//! calls `forget`. A port's waiters are looked at when an object it binds is touched (L21a); `channel_call`'s exact reply registration comes
//! later (L21d).

use crate::object::{Dirty, ObjId};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// A thread id as the kernel names it.
pub type Waiter = u64;

/// What a registration waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Any of these signals on the object.
    Mask(u32),
    /// The reply with this transaction id on a channel end (a `channel_call`, L21d), or the peer going: not READABLE, which any queued message
    /// would also raise.
    Call(u32),
}

#[derive(Default)]
pub struct WaitTable {
    /// The registrations on each object: (thread, the signals it waits for).
    regs: BTreeMap<ObjId, Vec<(Waiter, Kind)>>,
    /// The object each registered thread waits on (a thread waits on one object at a time).
    by_thread: BTreeMap<Waiter, ObjId>,
    /// The ports with waiters that bind each object (L21a): a port's READABLE follows the objects bound to it, so a touch of one of them is
    /// what can release a port's waiters; a commit evaluates only those ports (before, every port with a waiter on every commit).
    watchers: BTreeMap<ObjId, BTreeSet<ObjId>>,
    /// What each such port binds, as of the last time it was looked at (so the entries above can be taken back).
    watching: BTreeMap<ObjId, Vec<ObjId>>,
}

impl WaitTable {
    pub fn new() -> WaitTable {
        WaitTable::default()
    }

    /// `thread` starts waiting for any of `mask` on `obj`. Any earlier registration of the thread is
    /// replaced.
    pub fn register(&mut self, thread: Waiter, obj: ObjId, mask: u32) {
        self.forget(thread);
        self.regs
            .entry(obj)
            .or_default()
            .push((thread, Kind::Mask(mask)));
        self.by_thread.insert(thread, obj);
    }

    /// Remove `thread`'s registration (it was woken by a timer or a kill, or finished). Returns whether
    /// it had one.
    pub fn forget(&mut self, thread: Waiter) -> bool {
        let Some(obj) = self.by_thread.remove(&thread) else {
            return false;
        };
        if let Some(v) = self.regs.get_mut(&obj) {
            v.retain(|(t, _)| *t != thread);
            if v.is_empty() {
                self.regs.remove(&obj);
                self.unwatch_port(obj);
            }
        }
        true
    }

    /// `thread` waits for the reply with transaction id `txid` on channel end `ch` (or its peer closing): a `channel_call` (L21d). An unrelated
    /// message that arrives on the end does not release it.
    pub fn register_call(&mut self, thread: Waiter, ch: ObjId, txid: u32) {
        self.forget(thread);
        self.regs
            .entry(ch)
            .or_default()
            .push((thread, Kind::Call(txid)));
        self.by_thread.insert(thread, ch);
    }

    /// `register` on a port: `thread` waits for any of `mask` on `port`, which binds `bound` now (L21a).
    pub fn register_port(&mut self, thread: Waiter, port: ObjId, mask: u32, bound: Vec<ObjId>) {
        self.register(thread, port, mask);
        self.watch_port(port, bound);
    }

    /// `port` has waiters and binds `bound` now: remember which touches can release them (L21a). Replaces what was remembered for the port. The kernel
    /// calls it when a thread registers on a port and when the port is touched (a bind, unbind or rearm), so the memory is never older than the port's
    /// last change.
    pub fn watch_port(&mut self, port: ObjId, bound: Vec<ObjId>) {
        if self.watching.get(&port) == Some(&bound) {
            return; // (nothing changed: the log names a port once for every `get_mut` of it, which a poll or a rearm makes without binding anything new)
        }
        self.unwatch_port(port);
        for &o in &bound {
            self.watchers.entry(o).or_default().insert(port);
        }
        self.watching.insert(port, bound);
    }

    fn unwatch_port(&mut self, port: ObjId) {
        let Some(old) = self.watching.remove(&port) else {
            return;
        };
        for o in old {
            if let Some(set) = self.watchers.get_mut(&o) {
                set.remove(&port);
                if set.is_empty() {
                    self.watchers.remove(&o);
                }
            }
        }
    }

    /// The ports with waiters that watch `obj` (a test hook).
    pub fn watchers_of(&self, obj: ObjId) -> Vec<ObjId> {
        self.watchers
            .get(&obj)
            .map_or_else(Vec::new, |s| s.iter().copied().collect())
    }

    pub fn is_registered(&self, thread: Waiter) -> bool {
        self.by_thread.contains_key(&thread)
    }

    /// How many threads are registered.
    pub fn len(&self) -> usize {
        self.by_thread.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_thread.is_empty()
    }

    /// The threads released by `dirty`: for every touched object, each registration whose object is
    /// gone (`signals` returns `None`), whose touch was forced (a handle to the object was removed: the
    /// waiter must re-validate), or whose mask matches the object's signals now. Their registrations
    /// are removed. An overflowed log releases every registration (the lost entries may have been forced
    /// touches); each waiter re-checks its own condition.
    ///
    /// A port's READABLE follows the objects bound to it, which are touched, not the port (L21a): the ports with waiters are remembered
    /// by what they bind (`watch_port`), and a touch of a bound object evaluates exactly those ports. A touch of a port itself (a bind, unbind
    /// or rearm: the kernel touches it) first renews what is remembered for it, with `bound` (the objects it binds now), then evaluates it.
    pub fn commit(
        &mut self,
        signals: impl Fn(ObjId) -> Option<u32>,
        bound: impl Fn(ObjId) -> Vec<ObjId>,
        call_ready: impl Fn(ObjId, u32) -> bool,
        dirty: impl core::borrow::Borrow<Dirty>,
    ) -> Vec<Waiter> {
        let dirty: &Dirty = dirty.borrow();
        let mut woken = Vec::new();
        if dirty.overflow {
            // the log lost entries (forced touches among them): release everyone, they re-check
            let all: Vec<ObjId> = self.regs.keys().copied().collect();
            for obj in all {
                self.fire(obj, true, &signals, &call_ready, &mut woken);
            }
            self.watchers.clear();
            self.watching.clear();
        }
        // (the log names an object once for every `get_mut` of it: an object and a port are looked at once per commit)
        let mut done: BTreeSet<(ObjId, bool)> = BTreeSet::new();
        let mut ports_done: BTreeSet<ObjId> = BTreeSet::new();
        for &(obj, forced) in &dirty.items {
            if !done.insert((obj, forced)) {
                continue;
            }
            // (a port touched itself: what it binds may have changed)
            if self.watching.contains_key(&obj) {
                self.watch_port(obj, bound(obj));
            }
            self.fire(obj, forced, &signals, &call_ready, &mut woken);
            // the ports that bind the touched object
            let ports: Vec<ObjId> = self
                .watchers
                .get(&obj)
                .map_or_else(Vec::new, |s| s.iter().copied().collect());
            for port in ports {
                if ports_done.insert(port) {
                    self.fire(port, false, &signals, &call_ready, &mut woken);
                }
            }
        }
        woken
    }

    fn fire(
        &mut self,
        obj: ObjId,
        forced: bool,
        signals: &impl Fn(ObjId) -> Option<u32>,
        call_ready: &impl Fn(ObjId, u32) -> bool,
        woken: &mut Vec<Waiter>,
    ) {
        let Some(list) = self.regs.get_mut(&obj) else {
            return;
        };
        let now = signals(obj);
        // (a port wakes **one** waiter, the first in line, when it turns readable: the others stay asleep. The waiter that was woken polls, which logs
        // the port, so the commit that follows looks at it again and wakes the next one if a binding is still ready: the baton, L21b)
        let is_port = self.watching.contains_key(&obj);
        let mut baton_out = false;
        let mut keep = Vec::with_capacity(list.len());
        for (thread, kind) in list.drain(..) {
            let by_signals = match kind {
                Kind::Mask(mask) => now.is_some_and(|s| s & mask != 0),
                Kind::Call(txid) => {
                    now.is_some_and(|s| s & abi::signals::PEER_CLOSED != 0) || call_ready(obj, txid)
                }
            };
            let release = if forced || now.is_none() {
                true // (a removed handle, or the object gone: everyone re-validates)
            } else if by_signals && is_port {
                !core::mem::replace(&mut baton_out, true)
            } else {
                by_signals
            };
            if release {
                self.by_thread.remove(&thread);
                woken.push(thread);
            } else {
                keep.push((thread, kind));
            }
        }
        if keep.is_empty() {
            self.regs.remove(&obj);
            self.unwatch_port(obj);
        } else {
            *list = keep;
        }
    }

    /// Audit (W3): every registration names a live object. `alive` says which are.
    pub fn audit(&self, alive: impl Fn(ObjId) -> bool) -> Result<(), alloc::string::String> {
        for (obj, v) in &self.regs {
            if !alive(*obj) {
                return Err(alloc::format!("a registration names a dead object {obj:?}"));
            }
            for (t, _) in v {
                if self.by_thread.get(t) != Some(obj) {
                    return Err(alloc::format!("thread {t} is registered inconsistently"));
                }
            }
        }
        if self.by_thread.len() != self.regs.values().map(Vec::len).sum::<usize>() {
            return Err("the two indexes disagree".into());
        }
        // the port watch (L21a): only ports with a waiter are watched, and the two maps are each other's inverse
        for (port, bound) in &self.watching {
            if !self.regs.contains_key(port) {
                return Err(alloc::format!("port {port:?} is watched with no waiter"));
            }
            for o in bound {
                if !self.watchers.get(o).is_some_and(|s| s.contains(port)) {
                    return Err(alloc::format!(
                        "port {port:?} watches {o:?} but is not in its watchers"
                    ));
                }
            }
        }
        for (o, ports) in &self.watchers {
            for p in ports {
                if !self.watching.get(p).is_some_and(|b| b.contains(o)) {
                    return Err(alloc::format!(
                        "{o:?} lists port {p:?}, which does not watch it"
                    ));
                }
            }
        }
        Ok(())
    }
}
