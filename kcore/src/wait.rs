//! The wait table (slice W step 3, W-SPEC section 3): which threads wait on which object, and which of
//! them a batch of touched objects releases. Pure bookkeeping over the registry's dirty log: the kernel
//! registers a thread (checking its own predicate first, in one step under the big kernel lock), and
//! `commit` returns exactly the threads whose wait a touch can have ended. Signals stay derived: the
//! table only evaluates them for touched objects.
//!
//! (KT9, T177: the table allocates only when a thread is **admitted** (`admit`, an R point of the blocking calls); registering, forgetting and
//! committing allocate nothing: a registration is a record of the thread's own, linked into a chain of its object's bucket.)
//!
//! A registration is removed when it fires; a thread that is woken any other way (a timer, a kill)
//! calls `forget`. A port's waiters are looked at when an object it binds is touched (L21a); `channel_call`'s exact reply registration comes
//! later (L21d).

use crate::fmap::{FMap, NoMem};
use crate::object::{Dirty, ObjId};
use alloc::vec::Vec;

/// A thread id as the kernel names it.
pub type Waiter = u64;

/// What a registration waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Any of these signals on the object.
    Mask(u32),
    /// A `port_wait`: a thread that will **poll** the port when it wakes, which is what lets a port wake one such waiter and rely on the poll to
    /// pass the wake on (L21b). A plain wait on a port's READABLE (`object_wait`) does not poll, so it never takes the baton.
    Poller(u32),
    /// The reply with this transaction id on a channel end (a `channel_call`, L21d), or the peer going: not READABLE, which any queued message
    /// would also raise.
    Call(u32),
}

const NIL: u32 = u32::MAX;

/// One admitted thread's record: where it waits, if it does, and its place in the chain of that object's bucket.
#[derive(Clone, Copy)]
struct Rec {
    thread: Waiter,
    on: Option<(ObjId, Kind)>,
    next: u32,
    prev: u32,
}

#[derive(Default)]
pub struct WaitTable {
    /// The records of the admitted threads (a slot is reused through `free`; the vectors never grow outside `admit`).
    recs: Vec<Rec>,
    free: Vec<u32>,
    /// The record of each admitted thread.
    idx: FMap<Waiter, u32>,
    /// Head and tail of the chain of registrations whose object hashes to the bucket, in arrival order (a power of two long, at least the admitted
    /// count). A chain may hold several objects' registrations; each is filtered by its object.
    buckets: Vec<(u32, u32)>,
    /// How many threads are registered.
    registered: usize,
    /// The scratch `commit` returns (capacity for every admitted thread) and gets back through `recycle`.
    woken: Vec<Waiter>,
    fired: Vec<ObjId>,
}

impl WaitTable {
    pub fn new() -> WaitTable {
        WaitTable::default()
    }

    /// Make `thread` known to the table, with room for everything it can ever register or be woken (an R point: no memory is `NoMem`, and nothing
    /// has changed). Idempotent. A thread must be admitted before it registers.
    pub fn admit(&mut self, thread: Waiter) -> Result<(), NoMem> {
        if self.idx.contains_key(&thread) {
            return Ok(());
        }
        let admitted = self.idx.len().saturating_add(1);
        self.idx.reserve(1)?;
        let need = self.recs.len().saturating_add(1);
        self.recs.try_reserve(1).map_err(|_| NoMem)?;
        self.free
            .try_reserve(need.saturating_sub(self.free.len()))
            .map_err(|_| NoMem)?;
        self.woken
            .try_reserve(admitted.saturating_sub(self.woken.len()))
            .map_err(|_| NoMem)?;
        self.fired
            .try_reserve(admitted.saturating_sub(self.fired.len()))
            .map_err(|_| NoMem)?;
        if admitted > self.buckets.len() {
            self.grow_buckets(admitted.next_power_of_two().max(8))?;
        }
        let slot = match self.free.pop() {
            Some(s) => {
                if let Some(r) = self.recs.get_mut(s as usize) {
                    *r = Rec {
                        thread,
                        on: None,
                        next: NIL,
                        prev: NIL,
                    };
                }
                s
            }
            None => {
                self.recs.push(Rec {
                    thread,
                    on: None,
                    next: NIL,
                    prev: NIL,
                });
                u32::try_from(self.recs.len().saturating_sub(1)).unwrap_or(NIL)
            }
        };
        self.idx.insert_reserved(thread, slot);
        Ok(())
    }

    /// The thread ended: its record goes (and any registration it still had).
    pub fn retire(&mut self, thread: Waiter) {
        self.forget(thread);
        if let Some(slot) = self.idx.remove(&thread) {
            if let Some(r) = self.recs.get_mut(slot as usize) {
                r.thread = u64::MAX;
            }
            if self.free.len() < self.free.capacity() {
                self.free.push(slot);
            }
        }
    }

    /// A bigger bucket array, the chains moved over in order: each object's registrations stay in arrival order, which the baton of L21b depends on.
    fn grow_buckets(&mut self, n: usize) -> Result<(), NoMem> {
        let mut nb: Vec<(u32, u32)> = Vec::new();
        nb.try_reserve_exact(n).map_err(|_| NoMem)?;
        nb.resize(n, (NIL, NIL));
        let old = core::mem::replace(&mut self.buckets, nb);
        for (head, _) in old {
            let mut cur = head;
            while cur != NIL {
                let (next, obj) = match self.recs.get(cur as usize) {
                    Some(r) => (r.next, r.on.map(|(o, _)| o)),
                    None => break,
                };
                if let Some(o) = obj {
                    self.link(cur, o);
                }
                cur = next;
            }
        }
        Ok(())
    }

    fn bucket_of(&self, obj: ObjId) -> usize {
        let h = obj.hash_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h >> 32) as usize & self.buckets.len().saturating_sub(1)
    }

    /// Append record `slot` at the tail of the chain of `obj`'s bucket.
    fn link(&mut self, slot: u32, obj: ObjId) {
        if self.buckets.is_empty() {
            return;
        }
        let b = self.bucket_of(obj);
        let tail = self.buckets.get(b).map_or(NIL, |x| x.1);
        if let Some(r) = self.recs.get_mut(slot as usize) {
            r.prev = tail;
            r.next = NIL;
        }
        if tail == NIL {
            if let Some(x) = self.buckets.get_mut(b) {
                x.0 = slot;
            }
        } else if let Some(t) = self.recs.get_mut(tail as usize) {
            t.next = slot;
        }
        if let Some(x) = self.buckets.get_mut(b) {
            x.1 = slot;
        }
    }

    /// Take record `slot` out of its chain.
    fn unlink(&mut self, slot: u32, obj: ObjId) {
        let b = self.bucket_of(obj);
        let (prev, next) = match self.recs.get(slot as usize) {
            Some(r) => (r.prev, r.next),
            None => return,
        };
        if prev == NIL {
            if let Some(x) = self.buckets.get_mut(b) {
                x.0 = next;
            }
        } else if let Some(p) = self.recs.get_mut(prev as usize) {
            p.next = next;
        }
        if next == NIL {
            if let Some(x) = self.buckets.get_mut(b) {
                x.1 = prev;
            }
        } else if let Some(n) = self.recs.get_mut(next as usize) {
            n.prev = prev;
        }
        if let Some(r) = self.recs.get_mut(slot as usize) {
            r.next = NIL;
            r.prev = NIL;
        }
    }

    fn put(&mut self, thread: Waiter, obj: ObjId, kind: Kind) {
        self.forget(thread);
        let Some(&slot) = self.idx.get(&thread) else {
            debug_assert!(false, "a thread registers only after it is admitted");
            return;
        };
        if let Some(r) = self.recs.get_mut(slot as usize) {
            r.on = Some((obj, kind));
        }
        self.link(slot, obj);
        self.registered = self.registered.saturating_add(1);
    }

    /// `thread` starts waiting for any of `mask` on `obj`. Any earlier registration of the thread is replaced.
    pub fn register(&mut self, thread: Waiter, obj: ObjId, mask: u32) {
        self.put(thread, obj, Kind::Mask(mask));
    }

    /// Remove `thread`'s registration (it was woken by a timer or a kill, or finished). Returns whether it had one.
    pub fn forget(&mut self, thread: Waiter) -> bool {
        let Some(&slot) = self.idx.get(&thread) else {
            return false;
        };
        let Some(obj) = self
            .recs
            .get(slot as usize)
            .and_then(|r| r.on.map(|(o, _)| o))
        else {
            return false;
        };
        self.unlink(slot, obj);
        if let Some(r) = self.recs.get_mut(slot as usize) {
            r.on = None;
        }
        self.registered = self.registered.saturating_sub(1);
        true
    }

    /// `thread` waits for the reply with transaction id `txid` on channel end `ch` (or its peer closing): a `channel_call` (L21d). An unrelated
    /// message that arrives on the end does not release it.
    pub fn register_call(&mut self, thread: Waiter, ch: ObjId, txid: u32) {
        self.put(thread, ch, Kind::Call(txid));
    }

    /// `register` on a port: `thread` waits for any of `mask` on `port`, whose READABLE follows the objects bound to it (`Kobjs::for_each_binder`
    /// says which touches can release it). It is released with every other such waiter when the port turns ready (an `object_wait`);
    /// `register_poller` is the `port_wait` kind that takes the baton.
    pub fn register_port(&mut self, thread: Waiter, port: ObjId, mask: u32) {
        self.put(thread, port, Kind::Mask(mask));
    }

    /// `thread` is in `port_wait` (or `port_wait_many`) on `port`: it will poll the port when it wakes, so the port wakes one such waiter at a time
    /// (L21b).
    pub fn register_poller(&mut self, thread: Waiter, port: ObjId, mask: u32) {
        self.put(thread, port, Kind::Poller(mask));
    }

    pub fn is_registered(&self, thread: Waiter) -> bool {
        self.idx
            .get(&thread)
            .and_then(|&s| self.recs.get(s as usize))
            .is_some_and(|r| r.on.is_some())
    }

    /// How many threads are registered.
    pub fn len(&self) -> usize {
        self.registered
    }

    pub fn is_empty(&self) -> bool {
        self.registered == 0
    }

    /// How many threads are admitted.
    pub fn admitted(&self) -> usize {
        self.idx.len()
    }

    /// Give the vector `commit` returned back, emptied, so that the next commit allocates nothing.
    pub fn recycle(&mut self, mut v: Vec<Waiter>) {
        v.clear();
        if v.capacity() >= self.woken.capacity() {
            self.woken = v;
        }
    }

    /// The threads released by `dirty`: for every touched object, each registration whose object is gone (`signals` returns `None`), whose touch was
    /// forced (a handle to the object was removed: the waiter must re-validate), or whose mask matches the object's signals now. Their registrations
    /// are removed. An overflowed log releases every registration (the lost entries may have been forced touches); each waiter re-checks its own
    /// condition.
    ///
    /// A port's READABLE follows the objects bound to it, which are touched, not the port (L21a): a touch of an object also evaluates the ports
    /// that bind it, which `binders` names (`Kobjs::for_each_binder`: kept from bind to unbind, so nothing is remembered here).
    pub fn commit(
        &mut self,
        signals: impl Fn(ObjId) -> Option<u32>,
        binders: impl Fn(ObjId, &mut dyn FnMut(ObjId)),
        call_ready: impl Fn(ObjId, u32) -> bool,
        dirty: impl core::borrow::Borrow<Dirty>,
    ) -> Vec<Waiter> {
        let dirty: &Dirty = dirty.borrow();
        let mut woken = core::mem::take(&mut self.woken);
        woken.clear();
        // (no registration: nothing to look at; the room for the answer was made at admission)
        if self.registered == 0 {
            return woken;
        }
        let mut fired = core::mem::take(&mut self.fired);
        fired.clear();
        let mut overflow = dirty.overflow;
        if overflow {
            // the log lost entries (forced touches among them): release everyone, they re-check
            self.release_all(&mut woken);
        }
        // (the log names an object once for every `get_mut` of it: each object is looked at once per commit, forced if any of its entries was, and a port
        // that a bound object's touch would evaluate again is evaluated once. The log is short (a syscall touches a handful of objects), so this is a scan
        // of what was already seen, not a map: every commit pays for it)
        let items = &dirty.items;
        if !overflow {
            for (i, &(obj, forced)) in items.iter().enumerate() {
                if items.iter().take(i).any(|&(o, _)| o == obj) {
                    continue; // (looked at when it first appeared, with every entry of it)
                }
                let forced = forced
                    || items
                        .iter()
                        .skip(i.saturating_add(1))
                        .any(|&(o, f)| o == obj && f);
                // (only an object somebody waits on is looked at, and remembered: at most one per registered thread, which `admit` made room for)
                if self.has_waiter(obj) {
                    if forced || !fired.contains(&obj) {
                        self.fire(obj, forced, &signals, &call_ready, &mut woken);
                    }
                    if !fired.contains(&obj) {
                        if fired.len() == fired.capacity() {
                            overflow = true;
                            break;
                        }
                        fired.push(obj);
                    }
                }
                // the ports that bind the touched object
                let mut stop = false;
                binders(obj, &mut |port| {
                    if stop || fired.contains(&port) || !self.has_waiter(port) {
                        return;
                    }
                    if fired.len() == fired.capacity() {
                        stop = true;
                        return;
                    }
                    fired.push(port);
                    self.fire(port, false, &signals, &call_ready, &mut woken);
                });
                if stop {
                    overflow = true;
                    break;
                }
            }
            if overflow {
                self.release_all(&mut woken);
            }
        }
        self.fired = fired;
        woken
    }

    /// Does anyone wait on `obj`?
    fn has_waiter(&self, obj: ObjId) -> bool {
        if self.buckets.is_empty() {
            return false;
        }
        let mut cur = self.buckets.get(self.bucket_of(obj)).map_or(NIL, |x| x.0);
        while cur != NIL {
            let Some(r) = self.recs.get(cur as usize) else {
                return false;
            };
            if r.on.is_some_and(|(o, _)| o == obj) {
                return true;
            }
            cur = r.next;
        }
        false
    }

    /// Release every registration (the log lost entries, or there was no room to look at them one by one).
    fn release_all(&mut self, woken: &mut Vec<Waiter>) {
        for slot in 0..self.recs.len() {
            let Some(r) = self.recs.get(slot) else {
                continue;
            };
            let Some((obj, _)) = r.on else {
                continue;
            };
            let thread = r.thread;
            let slot = slot as u32;
            self.unlink(slot, obj);
            if let Some(r) = self.recs.get_mut(slot as usize) {
                r.on = None;
            }
            self.registered = self.registered.saturating_sub(1);
            woken.push(thread);
        }
    }

    fn fire(
        &mut self,
        obj: ObjId,
        forced: bool,
        signals: &impl Fn(ObjId) -> Option<u32>,
        call_ready: &impl Fn(ObjId, u32) -> bool,
        woken: &mut Vec<Waiter>,
    ) {
        if self.buckets.is_empty() {
            return;
        }
        let mut cur = self.buckets.get(self.bucket_of(obj)).map_or(NIL, |x| x.0);
        if cur == NIL {
            return;
        }
        let mut seen: Option<Option<u32>> = None; // (looked at once, and only if somebody waits on this object)
                                                  // (a port wakes **one** waiter, the first in line, when it turns readable: the others stay asleep. The waiter that was woken polls, which logs
                                                  // the port, so the commit that follows looks at it again and wakes the next one if a binding is still ready: the baton, L21b)
        let mut baton_out = false;
        while cur != NIL {
            let Some(r) = self.recs.get(cur as usize).copied() else {
                break;
            };
            let slot = cur;
            cur = r.next;
            let Some((o, kind)) = r.on else {
                continue;
            };
            if o != obj {
                continue;
            }
            let now = *seen.get_or_insert_with(|| signals(obj));
            let by_signals = match kind {
                Kind::Mask(mask) | Kind::Poller(mask) => now.is_some_and(|s| s & mask != 0),
                Kind::Call(txid) => {
                    now.is_some_and(|s| s & abi::signals::PEER_CLOSED != 0) || call_ready(obj, txid)
                }
            };
            let release = if forced || now.is_none() {
                true // (a removed handle, or the object gone: everyone re-validates)
            } else if by_signals && matches!(kind, Kind::Poller(_)) {
                !core::mem::replace(&mut baton_out, true)
            } else {
                by_signals
            };
            if release {
                self.unlink(slot, obj);
                if let Some(rr) = self.recs.get_mut(slot as usize) {
                    rr.on = None;
                }
                self.registered = self.registered.saturating_sub(1);
                woken.push(r.thread);
            }
        }
    }

    /// Audit (W3): every registration names a live object, the chains hold exactly the registered records, each in the bucket of its object.
    pub fn audit(&self, alive: impl Fn(ObjId) -> bool) -> Result<(), alloc::string::String> {
        let mut chained = 0usize;
        for (b, &(head, tail)) in self.buckets.iter().enumerate() {
            let (mut cur, mut prev) = (head, NIL);
            while cur != NIL {
                let Some(r) = self.recs.get(cur as usize) else {
                    return Err("a chain names a record that does not exist".into());
                };
                let Some((obj, _)) = r.on else {
                    return Err(alloc::format!(
                        "thread {} is chained but not registered",
                        r.thread
                    ));
                };
                if !alive(obj) {
                    return Err(alloc::format!("a registration names a dead object {obj:?}"));
                }
                if self.bucket_of(obj) != b {
                    return Err(alloc::format!("thread {} is in the wrong bucket", r.thread));
                }
                if r.prev != prev {
                    return Err(alloc::format!("thread {} has a broken back link", r.thread));
                }
                if self.idx.get(&r.thread) != Some(&cur) {
                    return Err(alloc::format!(
                        "thread {} is registered inconsistently",
                        r.thread
                    ));
                }
                chained = chained.saturating_add(1);
                prev = cur;
                cur = r.next;
                if chained > self.recs.len() {
                    return Err("a chain loops".into());
                }
            }
            if prev != tail {
                return Err(alloc::format!("bucket {b} has the wrong tail"));
            }
        }
        if chained != self.registered {
            return Err("the chains and the count disagree".into());
        }
        let on = self.recs.iter().filter(|r| r.on.is_some()).count();
        if on != self.registered {
            return Err("the records and the count disagree".into());
        }
        Ok(())
    }
}
