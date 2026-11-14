//! The wait table (slice W step 3, W-SPEC section 3): which threads wait on which object, and which of
//! them a batch of touched objects releases. Pure bookkeeping over the registry's dirty log: the kernel
//! registers a thread (checking its own predicate first, in one step under the big kernel lock), and
//! `commit` returns exactly the threads whose wait a touch can have ended. Signals stay derived: the
//! table only evaluates them for touched objects.
//!
//! A registration is removed when it fires; a thread that is woken any other way (a timer, a kill)
//! calls `forget`. Ports and `channel_call`'s exact reply registration come in later steps.

use crate::object::{Dirty, ObjId};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// A thread id as the kernel names it.
pub type Waiter = u32;

#[derive(Default)]
pub struct WaitTable {
    /// The registrations on each object: (thread, the signals it waits for).
    regs: BTreeMap<ObjId, Vec<(Waiter, u32)>>,
    /// The object each registered thread waits on (a thread waits on one object at a time).
    by_thread: BTreeMap<Waiter, ObjId>,
}

impl WaitTable {
    pub fn new() -> WaitTable {
        WaitTable::default()
    }

    /// `thread` starts waiting for any of `mask` on `obj`. Any earlier registration of the thread is
    /// replaced.
    pub fn register(&mut self, thread: Waiter, obj: ObjId, mask: u32) {
        self.forget(thread);
        self.regs.entry(obj).or_default().push((thread, mask));
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
            }
        }
        true
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
    /// are removed. An overflowed log releases every registration whose predicate holds (or whose
    /// object is gone), forced or not.
    ///
    /// `derived` says which objects' signals follow *other* objects' (a port's READABLE follows the
    /// objects bound to it, which are touched, not the port): their registrations are evaluated on
    /// every commit that touched anything. (Until ports are registrations themselves, step 4.)
    pub fn commit(
        &mut self,
        signals: impl Fn(ObjId) -> Option<u32>,
        derived: impl Fn(ObjId) -> bool,
        dirty: Dirty,
    ) -> Vec<Waiter> {
        let mut woken = Vec::new();
        let touched = dirty.overflow || !dirty.items.is_empty();
        if dirty.overflow {
            let all: Vec<ObjId> = self.regs.keys().copied().collect();
            for obj in all {
                self.fire(obj, false, &signals, &mut woken);
            }
        }
        for (obj, forced) in dirty.items {
            self.fire(obj, forced, &signals, &mut woken);
        }
        if touched {
            let follow: Vec<ObjId> = self.regs.keys().copied().filter(|o| derived(*o)).collect();
            for obj in follow {
                self.fire(obj, false, &signals, &mut woken);
            }
        }
        woken
    }

    fn fire(
        &mut self,
        obj: ObjId,
        forced: bool,
        signals: &impl Fn(ObjId) -> Option<u32>,
        woken: &mut Vec<Waiter>,
    ) {
        let Some(list) = self.regs.get_mut(&obj) else {
            return;
        };
        let now = signals(obj);
        let mut keep = Vec::with_capacity(list.len());
        for (thread, mask) in list.drain(..) {
            let release = forced || now.is_none_or(|s| s & mask != 0);
            if release {
                self.by_thread.remove(&thread);
                woken.push(thread);
            } else {
                keep.push((thread, mask));
            }
        }
        if keep.is_empty() {
            self.regs.remove(&obj);
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
        Ok(())
    }
}
