//! The kernel object registry: reference-counted objects with generation-checked ids.
//!
//! Every handle, and every handle in flight inside a channel message, holds one
//! reference to its object. When the last reference is released the object is
//! destroyed: the references it held are released in turn (iteratively, with a
//! worklist, so a deeply nested chain cannot overflow the kernel stack), and its
//! peer, if any, is told it is gone. Reference counting cannot free a *cycle* of
//! objects that hold each other (two channel endpoints queued in each other's
//! channels, with no handle to either); [`Registry::sweep`] frees those by
//! reachability from a set of roots (decision OD37).

use crate::job::JobId;
use alloc::vec::Vec;

/// Slots reserved when a registry is created (`abi::limits::MAX_OBJECTS`).
const PREALLOC_SLOTS: usize = 16384;

/// The object count above which `needs_sweep` is true until the first sweep.
const SWEEP_MIN: usize = 256;

/// An object id: a slot and the generation of that slot, so an id to a destroyed
/// object never resolves to whatever reuses the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjId {
    index: u32,
    gen: u64,
}

/// What the registry needs from the objects it stores.
pub trait Object {
    /// The ids this object holds references to (queued handles, for a channel).
    fn held(&self, out: &mut Vec<ObjId>);
    /// How many ids `held` would push.
    fn held_count(&self) -> usize {
        0
    }
    /// The object this one is paired with, if any (a channel's other end). A peer
    /// reference is *weak*: it does not keep the peer alive.
    fn peer(&self) -> Option<ObjId> {
        None
    }
    /// Called when the peer has been destroyed.
    fn peer_destroyed(&mut self) {}
    /// Does destroying this object require work the registry cannot do (freeing a VMO's
    /// pages, killing a process)? If so it is kept in the registry's graveyard for the owner
    /// to collect with [`Registry::drain_dead`].
    fn needs_cleanup(&self) -> bool {
        false
    }
    /// Bytes of kernel memory this object holds on behalf of its user (queued data), counted
    /// in the registry's `meter` so a global budget can be enforced.
    fn meter(&self) -> usize {
        0
    }
    /// The part of `meter()` that is charged to the object's job (queued data; not, for example,
    /// a port's bindings, which are bounded by the object quota and counted globally only).
    fn charged_bytes(&self) -> usize {
        0
    }
}

struct Slot<T> {
    gen: u64,
    /// The object's kernel id (`Registry::koid`): a counter from 1 assigned at creation, never reused (CNT1).
    koid: u64,
    refs: u32,
    obj: Option<T>,
    /// The job this object is charged to (S4.5c), if it is charged at all. When the object is
    /// destroyed, one object and the bytes it still held are queued as credits for that job.
    job: Option<JobId>,
}

pub struct Registry<T: Object> {
    slots: Vec<Slot<T>>,
    /// The koid the next object gets (a counter from 1; test builds start it high, `fast_forward_koid`).
    next_koid: u64,
    /// The generation a new slot starts at (0; test builds start it near the limit, `set_first_generation`).
    first_gen: u64,
    free: Vec<u32>,
    live: usize,
    /// `needs_sweep` once `live` passes this.
    threshold: usize,
    /// Destroyed objects that `needs_cleanup`, waiting for the owner.
    graveyard: Vec<T>,
    /// Sum of `meter()` over live objects (kept by the owner through `meter_add` /
    /// `meter_sub` as data is queued and dequeued, and by destruction).
    meter: usize,
    /// Credits for charged objects that were destroyed since the last `take_credits`:
    /// (job, bytes the object still held). One object per entry.
    credits: Vec<(JobId, usize)>,
    /// Objects whose signals or liveness may have changed since the last `take_dirty`, with a
    /// *forced* flag (a handle to the object was removed: waiters must re-validate). Slice W step 1;
    /// nothing consumes it yet.
    dirty: Vec<(ObjId, bool)>,
    /// The log filled up and was dropped: the consumer must treat every object as touched.
    dirty_overflow: bool,
    /// The work list of `release` (objects whose last reference just went), kept between calls so a release never allocates.
    work: Vec<ObjId>,
    /// The dirty log's twin: `take_dirty` swaps it in (`recycle_dirty` gives the emptied log back), so taking the log allocates nothing.
    dirty_spare: Vec<(ObjId, bool)>,
    /// The graveyard's twin: `drain_dead` hands out one and keeps the other, so its capacity survives (`recycle` returns it).
    spare: Vec<T>,
}

/// How many log entries are kept before the log is compacted and, failing that, dropped (the
/// kernel drains it at the end of every syscall, so this is a safety net).
const DIRTY_CAP: usize = 4096;

/// What the dirty log reserves at creation and again after every `take_dirty`. The log never grows beyond what it holds: a log that is full is
/// compacted and, failing that, dropped (`overflow`: every object counts as touched), so `log` cannot allocate (KT9).
const DIRTY_RESERVE: usize = 256;

/// What `take_dirty` returns.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Dirty {
    /// The touched objects (live or since destroyed) and whether the touch was forced.
    pub items: Vec<(ObjId, bool)>,
    /// The log overflowed: assume every object was touched.
    pub overflow: bool,
}

impl<T: Object> Default for Registry<T> {
    fn default() -> Self {
        Registry::new()
    }
}

impl<T: Object> Registry<T> {
    pub fn new() -> Registry<T> {
        Registry {
            next_koid: 1,
            first_gen: 0,
            // Reserved once: growing a table this size by doubling needs a contiguous block
            // twice as large as the old one, and an allocation failure is a kernel panic
            // (found by the trust-boundary audit). Owners cap `live` at `MAX_OBJECTS`, and
            // slots are reused, so the table never needs to grow.
            slots: Vec::with_capacity(PREALLOC_SLOTS),
            // (everything a destruction pushes to is sized for the most objects there can be, once, so that destroying an object, which cannot be
            // allowed to fail, never allocates: KT9)
            free: Vec::with_capacity(PREALLOC_SLOTS),
            live: 0,
            threshold: SWEEP_MIN,
            graveyard: Vec::new(),
            meter: 0,
            credits: Vec::with_capacity(PREALLOC_SLOTS),
            dirty: Vec::with_capacity(DIRTY_RESERVE),
            dirty_overflow: false,
            work: Vec::with_capacity(PREALLOC_SLOTS),
            dirty_spare: Vec::with_capacity(DIRTY_RESERVE),
            spare: Vec::new(),
        }
    }

    fn log(&mut self, id: ObjId, forced: bool) {
        if self.dirty.len() >= DIRTY_CAP.min(self.dirty.capacity()) {
            self.dirty
                .sort_unstable_by_key(|(i, f)| (i.index, i.gen, !*f));
            self.dirty.dedup_by(|b, a| {
                if a.0 == b.0 {
                    a.1 |= b.1;
                    true
                } else {
                    false
                }
            });
            if self.dirty.len() >= DIRTY_CAP.min(self.dirty.capacity()) {
                self.dirty.clear();
                self.dirty_overflow = true;
            }
        }
        if !self.dirty_overflow {
            self.dirty.push((id, forced));
        }
    }

    fn dirty_log_doomed(&mut self, id: ObjId) {
        self.log(id, false);
    }

    /// Note that `id`'s signals may have changed: it and, since a channel or socket end's signals
    /// derive from its peer, its peer (if alive). An id that is already gone is logged too (its
    /// waiters must learn it died).
    pub fn touch(&mut self, id: ObjId) {
        self.log(id, false);
        if let Some(peer) = self
            .slot(id)
            .and_then(|s| s.obj.as_ref())
            .and_then(|o| o.peer())
        {
            self.log(peer, false);
        }
    }

    /// Note that a handle to `id` was removed (closed, replaced, transferred away): waiters on it
    /// wake whatever its signals say, to re-validate their handle.
    pub fn touch_forced(&mut self, id: ObjId) {
        self.log(id, true);
    }

    /// Everything touched since the last call, oldest first (duplicates possible).
    pub fn take_dirty(&mut self) -> Dirty {
        if self.dirty.is_empty() && !self.dirty_overflow {
            return Dirty::default(); // (nothing logged: keep the room, and allocate nothing)
        }
        // the twin takes the log's place and the log goes out; `recycle_dirty` brings its emptied vector back as the next twin
        core::mem::swap(&mut self.dirty, &mut self.dirty_spare);
        let items = core::mem::take(&mut self.dirty_spare);
        if self.dirty.capacity() == 0 {
            // (a caller that did not recycle: make the room here, where a failure is harmless: a log with no room just overflows, which is safe)
            let _ = self.dirty.try_reserve_exact(DIRTY_RESERVE);
        }
        Dirty {
            items,
            overflow: core::mem::take(&mut self.dirty_overflow),
        }
    }

    /// Give back a `Dirty` once it has been used, so that the next `take_dirty` allocates nothing.
    pub fn recycle_dirty(&mut self, d: Dirty) {
        let mut v = d.items;
        v.clear();
        if self.dirty_spare.capacity() < v.capacity() {
            self.dirty_spare = v;
        }
    }

    /// The log without draining it (diagnostics).
    pub fn peek_dirty(&self) -> &[(ObjId, bool)] {
        &self.dirty
    }

    /// The kernel id of a live object: a counter value from 1, assigned at creation, never reused (what `object_info` reports).
    pub fn koid(&self, id: ObjId) -> Option<u64> {
        self.slot(id).map(|s| s.koid)
    }

    /// For test builds (`ff=koid:<n>`): the next object gets koid `n`.
    pub fn fast_forward_koid(&mut self, n: u64) {
        self.next_koid = n.max(1);
    }

    /// For test builds (`ff=objgen:<n>`): slots made from now on start at generation `n`, so the retirement of an exhausted slot is reached in a few
    /// reuses.
    pub fn set_first_generation(&mut self, n: u64) {
        self.first_gen = n;
    }

    /// Is anything waiting in the dirty log?
    pub fn has_dirty(&self) -> bool {
        !self.dirty.is_empty() || self.dirty_overflow
    }

    /// Store an object with one reference (the caller's, to be moved into a handle). It is not
    /// charged to any job.
    pub fn create(&mut self, obj: T) -> ObjId {
        self.create_charged(obj, None)
    }

    /// Like `create`, for an object the caller has charged to `job` (one object): destroying it
    /// queues the credit (`take_credits`).
    pub fn create_in(&mut self, obj: T, job: JobId) -> ObjId {
        self.create_charged(obj, Some(job))
    }

    /// `create` / `create_in` with the job as an option.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: live is bounded by MAX_OBJECTS (checked by every caller's quota test)
    pub fn create_charged(&mut self, obj: T, job: Option<JobId>) -> ObjId {
        self.live += 1;
        // (a u64 counter from 1: 2^64 creations is 584 years at a billion a second; saturating keeps the arithmetic total)
        let koid = self.next_koid;
        self.next_koid = self.next_koid.saturating_add(1);
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.refs = 1;
            slot.koid = koid;
            slot.obj = Some(obj);
            slot.job = job;
            ObjId {
                index,
                gen: slot.gen,
            }
        } else {
            self.slots.push(Slot {
                gen: self.first_gen,
                koid,
                refs: 1,
                obj: Some(obj),
                job,
            });
            ObjId {
                index: self.slots.len() as u32 - 1,
                gen: self.first_gen,
            }
        }
    }

    fn slot(&self, id: ObjId) -> Option<&Slot<T>> {
        self.slots
            .get(id.index as usize)
            .filter(|s| s.gen == id.gen && s.obj.is_some())
    }

    fn slot_mut(&mut self, id: ObjId) -> Option<&mut Slot<T>> {
        self.slots
            .get_mut(id.index as usize)
            .filter(|s| s.gen == id.gen && s.obj.is_some())
    }

    pub fn get(&self, id: ObjId) -> Option<&T> {
        self.slot(id)?.obj.as_ref()
    }

    /// Mutable access. Handing it out counts as a touch (the object and its peer), so a mutation
    /// made through it cannot be missed by the consumer of the dirty log; a caller that only reads
    /// should use `get`.
    pub fn get_mut(&mut self, id: ObjId) -> Option<&mut T> {
        self.slot(id)?;
        self.touch(id);
        self.slot_mut(id)?.obj.as_mut()
    }

    pub fn is_alive(&self, id: ObjId) -> bool {
        self.slot(id).is_some()
    }

    /// The job a live object is charged to (`None`: it is not charged).
    pub fn job_of(&self, id: ObjId) -> Option<JobId> {
        self.slot(id)?.job
    }

    /// The same credits, drained in place: the vector keeps its capacity, so the next destruction of a charged object does not allocate.
    pub fn drain_credits(&mut self) -> alloc::vec::Drain<'_, (JobId, usize)> {
        self.credits.drain(..)
    }

    /// Bytes currently counted by the meter.
    pub fn meter(&self) -> usize {
        self.meter
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the meter is bounded by the global IPC budget
    pub fn meter_add(&mut self, n: usize) {
        self.meter += n;
    }

    pub fn meter_sub(&mut self, n: usize) {
        self.meter = self.meter.saturating_sub(n);
    }

    /// The number of live objects: the leak counter of the tests.
    /// Every live object's id (for tests and audits; O(slots)).
    pub fn live_ids(&self) -> Vec<ObjId> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.obj.is_some())
            .map(|(i, s)| ObjId {
                index: i as u32,
                gen: s.gen,
            })
            .collect()
    }

    pub fn live(&self) -> usize {
        self.live
    }

    pub fn refs(&self, id: ObjId) -> Option<u32> {
        self.slot(id).map(|s| s.refs)
    }

    /// Take another reference. `false` if the id is not live.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: refs is bounded by the handle quota
    pub fn retain(&mut self, id: ObjId) -> bool {
        match self.slot_mut(id) {
            Some(slot) => {
                slot.refs += 1;
                true
            }
            None => false,
        }
    }

    /// Drop a reference; destroy the object, and whatever that frees in turn, when
    /// it was the last.
    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: refs >= 1 for a reference being released
    pub fn release(&mut self, id: ObjId) {
        let mut work = core::mem::take(&mut self.work);
        work.clear();
        work.push(id);
        while let Some(id) = work.pop() {
            let Some(slot) = self.slot_mut(id) else {
                continue;
            };
            slot.refs -= 1;
            if slot.refs > 0 {
                continue;
            }
            let obj = slot.obj.take().expect("slot was live");
            let charged = slot.job.take();
            // (CNT1: a slot whose generation is exhausted is retired, not wrapped)
            let retire = slot.gen == u64::MAX;
            slot.gen = slot.gen.saturating_add(1);
            self.log(id, false);
            if let Some(job) = charged {
                self.credits.push((job, obj.charged_bytes()));
            }
            if !retire {
                self.free.push(id.index);
            }
            self.live -= 1;
            self.meter = self.meter.saturating_sub(obj.meter());
            // (room for the references the object held, before they are queued: if there is none they are leaked, which `sweep` reclaims later,
            // rather than failing a destruction: KT9)
            if work.try_reserve(obj.held_count()).is_ok() {
                obj.held(&mut work);
            }
            if let Some(peer) = obj.peer() {
                if let Some(p) = self.get_mut(peer) {
                    p.peer_destroyed();
                }
            }
            if obj.needs_cleanup() {
                self.bury(obj);
            }
        }
        self.work = work;
    }

    /// Hand a destroyed object that needs cleanup to the graveyard. The room was made by `reserve_one` (the graveyard holds at least one slot
    /// per live object); if it is somehow not there the object's cleanup is lost, a leak, never a failure of the destruction.
    fn bury(&mut self, obj: T) {
        if self.graveyard.try_reserve(1).is_ok() {
            self.graveyard.push(obj);
        }
    }

    /// Make room for one more object and for everything its destruction will need to record (the graveyard slot of an object that needs
    /// cleanup): the caller creates the object only after this returned `Ok`, so creating and then destroying it cannot fail for lack of
    /// memory (KT9).
    pub fn reserve_one(&mut self) -> Result<(), abi::status::Status> {
        // (a slot in the graveyard and in its twin for every live object, so that however many cleanup-needing objects die at once, and
        // whichever of the two vectors is in place, burying them does not allocate)
        let want = self.live.saturating_add(1);
        for v in [&mut self.graveyard, &mut self.spare] {
            let more = want.saturating_sub(v.len());
            if v.capacity() < want {
                v.try_reserve_exact(more)
                    .map_err(|_| abi::status::Status::NoMemory)?;
            }
        }
        Ok(())
    }

    /// The destroyed objects whose destruction needs work from the owner (since the last
    /// call), oldest first. Call after any operation that can destroy objects: `release`,
    /// `sweep`, closing handles, draining a table.
    pub fn drain_dead(&mut self) -> Vec<T> {
        // (the graveyard keeps its capacity: the empty twin takes its place, and `recycle` brings the emptied one back)
        // (the twin was sized like the graveyard by `reserve_one`)
        core::mem::swap(&mut self.graveyard, &mut self.spare);
        core::mem::take(&mut self.spare)
    }

    /// Give a vector from `drain_dead` back once it is empty, so the next destruction does not allocate.
    pub fn recycle(&mut self, mut v: Vec<T>) {
        v.clear();
        if v.capacity() > self.graveyard.capacity() {
            core::mem::swap(&mut self.graveyard, &mut v);
            self.graveyard.append(&mut v);
        }
    }

    /// Is any destroyed object waiting in the graveyard?
    pub fn has_dead(&self) -> bool {
        !self.graveyard.is_empty()
    }

    /// Has the number of live objects grown enough that the owner should call `sweep`
    /// (with every handle table's objects as roots) before creating more? Cycles of
    /// objects that hold each other are garbage reference counting cannot see, so a
    /// process that makes them in a loop must not be allowed to grow memory without
    /// bound until it dies (decision OD37).
    pub fn needs_sweep(&self) -> bool {
        self.live > self.threshold
    }

    /// Destroy every object that is not reachable from `roots` by following held
    /// references, and return how many that was. This is how cycles are reclaimed.
    /// `roots` must list every object that something outside the registry (a handle
    /// table) still refers to: **an omitted root is destroyed even though a handle to it
    /// exists** (the handle then fails with BAD_HANDLE, never with a wrong object). The
    /// caller passes the objects of *all* handle tables.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: ids come from the registry's own slot table and index it
    pub fn sweep(&mut self, roots: &[ObjId]) -> usize {
        let mut reached = alloc::vec![false; self.slots.len()];
        let mut work: Vec<ObjId> = roots.to_vec();
        while let Some(id) = work.pop() {
            if !self.is_alive(id) || core::mem::replace(&mut reached[id.index as usize], true) {
                continue;
            }
            self.slots[id.index as usize]
                .obj
                .as_ref()
                .expect("live")
                .held(&mut work);
        }
        let doomed: Vec<u32> = (0..self.slots.len() as u32)
            .filter(|&i| self.slots[i as usize].obj.is_some() && !reached[i as usize])
            .collect();
        let mut dropped_refs: Vec<ObjId> = Vec::new();
        let mut peers: Vec<ObjId> = Vec::new();
        for &index in &doomed {
            let slot = &mut self.slots[index as usize];
            let id = ObjId {
                index,
                gen: slot.gen,
            };
            let obj = slot.obj.take().expect("live");
            self.dirty_log_doomed(id);
            let slot = &mut self.slots[index as usize];
            if let Some(job) = slot.job.take() {
                self.credits.push((job, obj.charged_bytes()));
            }
            slot.refs = 0;
            let retire = slot.gen == u64::MAX;
            slot.gen = slot.gen.saturating_add(1);
            if !retire {
                self.free.push(index);
            }
            self.live -= 1;
            self.meter = self.meter.saturating_sub(obj.meter());
            // The references a doomed object holds go away with it. Those to other doomed
            // objects are moot; those to survivors must be given back, or the survivor
            // could never be freed.
            obj.held(&mut dropped_refs);
            if let Some(peer) = obj.peer() {
                peers.push(peer);
            }
            if obj.needs_cleanup() {
                self.bury(obj);
            }
        }
        for peer in peers {
            if let Some(p) = self.get_mut(peer) {
                p.peer_destroyed();
            }
        }
        for id in dropped_refs {
            if self.is_alive(id) {
                self.release(id);
            }
        }
        // Never past the cap (garbage cycles must not be able to fill it), and never so close
        // to it that every creation sweeps while the objects are all genuinely reachable.
        self.threshold = SWEEP_MIN.max(self.live * 2).min(PREALLOC_SLOTS - 4);
        doomed.len()
    }
}
