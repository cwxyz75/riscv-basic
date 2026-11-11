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

use alloc::vec::Vec;

/// Slots reserved when a registry is created (`abi::limits::MAX_OBJECTS`).
const PREALLOC_SLOTS: usize = 16384;

/// The object count above which `needs_sweep` is true until the first sweep.
const SWEEP_MIN: usize = 256;

/// An object id: a slot and the generation of that slot, so an id to a destroyed
/// object never resolves to whatever reuses the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjId {
    index: u32,
    gen: u32,
}

/// What the registry needs from the objects it stores.
pub trait Object {
    /// The ids this object holds references to (queued handles, for a channel).
    fn held(&self, out: &mut Vec<ObjId>);
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
}

struct Slot<T> {
    gen: u32,
    refs: u32,
    obj: Option<T>,
}

pub struct Registry<T: Object> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    live: usize,
    /// `needs_sweep` once `live` passes this.
    threshold: usize,
    /// Destroyed objects that `needs_cleanup`, waiting for the owner.
    graveyard: Vec<T>,
    /// Sum of `meter()` over live objects (kept by the owner through `meter_add` /
    /// `meter_sub` as data is queued and dequeued, and by destruction).
    meter: usize,
}

impl<T: Object> Default for Registry<T> {
    fn default() -> Self {
        Registry::new()
    }
}

impl<T: Object> Registry<T> {
    pub fn new() -> Registry<T> {
        Registry {
            // Reserved once: growing a table this size by doubling needs a contiguous block
            // twice as large as the old one, and an allocation failure is a kernel panic
            // (found by the trust-boundary audit). Owners cap `live` at `MAX_OBJECTS`, and
            // slots are reused, so the table never needs to grow.
            slots: Vec::with_capacity(PREALLOC_SLOTS),
            free: Vec::new(),
            live: 0,
            threshold: SWEEP_MIN,
            graveyard: Vec::new(),
            meter: 0,
        }
    }

    /// Store an object with one reference (the caller's, to be moved into a handle).
    pub fn create(&mut self, obj: T) -> ObjId {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.refs = 1;
            slot.obj = Some(obj);
            ObjId {
                index,
                gen: slot.gen,
            }
        } else {
            self.slots.push(Slot {
                gen: 0,
                refs: 1,
                obj: Some(obj),
            });
            ObjId {
                index: self.slots.len() as u32 - 1,
                gen: 0,
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

    pub fn get_mut(&mut self, id: ObjId) -> Option<&mut T> {
        self.slot_mut(id)?.obj.as_mut()
    }

    pub fn is_alive(&self, id: ObjId) -> bool {
        self.slot(id).is_some()
    }

    /// Bytes currently counted by the meter.
    pub fn meter(&self) -> usize {
        self.meter
    }

    pub fn meter_add(&mut self, n: usize) {
        self.meter += n;
    }

    pub fn meter_sub(&mut self, n: usize) {
        self.meter = self.meter.saturating_sub(n);
    }

    /// The number of live objects: the leak counter of the tests.
    pub fn live(&self) -> usize {
        self.live
    }

    pub fn refs(&self, id: ObjId) -> Option<u32> {
        self.slot(id).map(|s| s.refs)
    }

    /// Take another reference. `false` if the id is not live.
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
    pub fn release(&mut self, id: ObjId) {
        let mut work = alloc::vec![id];
        while let Some(id) = work.pop() {
            let Some(slot) = self.slot_mut(id) else {
                continue;
            };
            slot.refs -= 1;
            if slot.refs > 0 {
                continue;
            }
            let obj = slot.obj.take().expect("slot was live");
            slot.gen = slot.gen.wrapping_add(1);
            self.free.push(id.index);
            self.live -= 1;
            self.meter = self.meter.saturating_sub(obj.meter());
            obj.held(&mut work);
            if let Some(peer) = obj.peer() {
                if let Some(p) = self.get_mut(peer) {
                    p.peer_destroyed();
                }
            }
            if obj.needs_cleanup() {
                self.graveyard.push(obj);
            }
        }
    }

    /// The destroyed objects whose destruction needs work from the owner (since the last
    /// call), oldest first. Call after any operation that can destroy objects: `release`,
    /// `sweep`, closing handles, draining a table.
    pub fn drain_dead(&mut self) -> Vec<T> {
        core::mem::take(&mut self.graveyard)
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
            let obj = slot.obj.take().expect("live");
            slot.refs = 0;
            slot.gen = slot.gen.wrapping_add(1);
            self.free.push(index);
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
                self.graveyard.push(obj);
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
