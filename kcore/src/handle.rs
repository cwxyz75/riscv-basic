//! A process's handle table (OS-ARCHITECTURE.md, 5.1; SYSCALLS.md, "Handles").
//!
//! A handle value is 64 bits, `generation << 24 | (slot + 1)` (KERNEL-V1-SPEC 3.2, AB1): the slot plus one (24 bits, so up to 2^24 - 1
//! slots) so that 0 is never valid, and a 40-bit generation that changes each time the slot is
//! freed so a stale value fails instead of naming whatever reuses the slot (INVARIANTS H2). The table grows on demand. A
//! slot whose generation has used its whole range is retired, never reused, so a stale value
//! cannot become valid again by wrap-around. The table
//! maps a handle to an object id and a rights mask; rights only ever shrink (H3).
//!
//! Each live handle owns one reference to its object. Operations that create a handle
//! from a reference the caller already owns (`insert`) move it; `duplicate` takes a new
//! one from the registry; `close` and `drain` give them back.

use crate::object::{ObjId, Object, Registry};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub obj: ObjId,
    pub rights: Rights,
}

/// Bits of a handle value that hold the slot number plus one.
const SLOT_BITS: u32 = 24;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
/// The generations a slot goes through before it is retired (40 bits).
const GENERATIONS: u64 = 1 << (64 - SLOT_BITS);

/// The most handles a table can name.
pub const MAX_SLOTS: usize = SLOT_MASK as usize;

struct Slot {
    gen: u64,
    entry: Option<Entry>,
}

/// How many times any handle table has grown a slot (MM5): a counter the kernel compares with what a hart last saw, so a syscall that
/// grew nothing anywhere costs two loads, not a look at the process.
// (`core`'s atomics, not `crate::sync`'s: the model checkers' atomics cannot be built in a static, and these counters are no part of a modelled protocol)
static GROWTH_EVENTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many tables are clamped by `settle_growth` now (their job could not pay): while any is, every settle looks again.
static CLAMPED_TABLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Slot growths so far, in all tables.
pub fn growth_events() -> u64 {
    GROWTH_EVENTS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Tables that are clamped now.
pub fn clamped_tables() -> u64 {
    CLAMPED_TABLES.load(core::sync::atomic::Ordering::Relaxed)
}

/// The next table's identity (L21c): a binding of a port names the table and handle it was made through.
static NEXT_TABLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

pub struct HandleTable {
    /// This table's identity, never reused.
    id: u64,
    slots: Vec<Slot>,
    free: Vec<u32>,
    limit: usize,
    live: usize,
    /// The generation a new slot starts at (0; a test fast-forwards it to reach retirement without 2^40 reuses).
    first_gen: u64,
    /// The most slots the table may have (`MAX_SLOTS`; a test lowers it to fill the table cheaply).
    slot_cap: usize,
    /// The handle limit the table was made with: `limit` goes down to the slots it has while its job cannot pay for more (MM5).
    full_limit: usize,
    /// Bytes of the table's growth charged to its job's kernel bytes (MM5): what `settle_growth` has charged so far.
    charged: u64,
    /// Whether `settle_growth` has clamped `limit` (counted in `CLAMPED_TABLES`).
    clamped: bool,
}

impl Drop for HandleTable {
    fn drop(&mut self) {
        if self.clamped {
            CLAMPED_TABLES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl HandleTable {
    /// A table that holds at most `limit` handles (capped at 2^24 - 1 by the encoding). It grows on demand: nothing is sized for the limit.
    pub fn new(limit: usize) -> HandleTable {
        HandleTable {
            id: NEXT_TABLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
            slots: Vec::new(),
            free: Vec::new(),
            limit: limit.min(MAX_SLOTS),
            full_limit: limit.min(MAX_SLOTS),
            charged: 0,
            clamped: false,
            live: 0,
            first_gen: 0,
            slot_cap: MAX_SLOTS,
        }
    }

    /// The table's identity (L21c).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// For tests: a table whose new slots start at generation `gen`, so that retirement (the generation reaching 2^40) can be reached in a
    /// few reuses of a slot instead of 2^40.
    pub fn starting_at_generation(limit: usize, gen: u64) -> HandleTable {
        let mut t = HandleTable::new(limit);
        t.first_gen = gen.min(GENERATIONS - 1);
        t
    }

    /// For tests: slots made from now on start at generation `gen` (the boot loader makes the bootstrap handle, value 1, first, then calls this).
    pub fn set_first_generation(&mut self, gen: u64) {
        self.first_gen = gen.min(GENERATIONS - 1);
    }

    /// For tests: lower the most slots the table may have (so a full table of retired slots is cheap to build).
    pub fn with_slot_cap(mut self, cap: usize) -> HandleTable {
        self.slot_cap = cap.min(MAX_SLOTS);
        self
    }

    /// Make room, in memory, for `n` more handles: the slots they may need and the free list that gives them back, so that neither growing nor
    /// closing a handle can fail for lack of memory after this returned `Ok` (KT9: a message must not be lost, nor a handle leaked, because an
    /// allocation failed half-way).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: free.len() <= slots.len(), and both are below the encoding's 8191 slots
    pub fn reserve(&mut self, n: usize) -> Result<(), Status> {
        let fresh = n.saturating_sub(self.free.len());
        self.slots
            .try_reserve(fresh)
            .map_err(|_| Status::NoMemory)?;
        // the free list can hold every slot there will be
        self.free
            .try_reserve(self.slots.len() + fresh - self.free.len())
            .map_err(|_| Status::NoMemory)?;
        Ok(())
    }

    /// The memory the table's two vectors hold, in bytes: their capacity, not their length (they grow by doubling, and the memory is held
    /// whether or not it is used).
    pub fn slot_bytes(&self) -> u64 {
        const SLOT: u64 = core::mem::size_of::<Slot>() as u64;
        const FREE: u64 = core::mem::size_of::<u32>() as u64;
        (self.slots.capacity() as u64)
            .saturating_mul(SLOT)
            .saturating_add((self.free.capacity() as u64).saturating_mul(FREE))
    }

    /// Bytes of growth charged so far.
    pub fn charged(&self) -> u64 {
        self.charged
    }

    /// Settle the table's growth with its job (MM5): `charge(n)` asks the ledger for `n` more bytes and says whether it was granted. A
    /// refusal keeps the table from growing further (its handle limit drops to the slots it has: the free slots stay usable, a new one
    /// is NO_MEMORY) until a later call is granted; the growth that already happened stays pending, so the overshoot is bounded by one
    /// call's insertions (per thread: threads of one process that each have an unsettled call add up, bounded by the thread limit). Called after the calls that can insert, never in the middle of one.
    pub fn settle_growth(&mut self, charge: impl FnOnce(u64) -> bool) {
        let have = self.slot_bytes();
        let pending = have.saturating_sub(self.charged);
        if pending == 0 || charge(pending) {
            if pending > 0 {
                self.charged = have;
            }
            self.limit = self.full_limit;
            if self.clamped {
                self.clamped = false;
                CLAMPED_TABLES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
            }
        } else {
            self.limit = self.slots.len().max(self.live).min(self.full_limit);
            if !self.clamped {
                self.clamped = true;
                CLAMPED_TABLES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// How many more handles fit: the limit, and also the slots that can still be handed out
    /// (a slot that has been through every generation is retired for good, so a table can run out of
    /// slots while far below its limit; `insert` must then fail, and callers that checked `room`
    /// must not find it failing).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: live <= limit and slots_left is computed from the table sizes
    pub fn room(&self) -> usize {
        (self.limit - self.live).min(self.slots_left())
    }

    /// Slots that can still be handed out (free ones and unused ones).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: the free list and the slot table are both at most SLOT_MASK long
    fn slots_left(&self) -> usize {
        self.free.len() + self.slot_cap.saturating_sub(self.slots.len())
    }

    fn decode(&self, h: Handle) -> Option<usize> {
        let value = h.0;
        let slot = (value & SLOT_MASK).checked_sub(1)? as usize;
        let gen = value >> SLOT_BITS;
        let s = self.slots.get(slot)?;
        (s.gen == gen && s.entry.is_some()).then_some(slot)
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: gen < GENERATIONS and slot < SLOT_MASK, so the encoded value fits in 64 bits
    fn encode(slot: usize, gen: u64) -> Handle {
        Handle((gen << SLOT_BITS) | (slot as u64 + 1))
    }

    /// Look up a handle.
    #[allow(clippy::expect_used, clippy::indexing_slicing)] // INVARIANT: the slot was decoded from a handle and bounds-checked against the table just above, and its entry was checked occupied before decoding succeeded
    pub fn get(&self, h: Handle) -> Result<Entry, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        Ok(self.slots[slot].entry.expect("decoded slot is occupied"))
    }

    /// Add a handle that takes over the caller's reference to `obj`. The registry is a parameter so that the sweep's write barrier cannot be
    /// forgotten (MM10): while a sweep runs, a new handle shades its object, or a handle moved out of a queued message could hide the
    /// object from the marker.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: s is a free slot index or the new last slot; live is at most the table's limit
    pub fn insert<T: Object>(
        &mut self,
        reg: &mut Registry<T>,
        obj: ObjId,
        rights: Rights,
    ) -> Result<Handle, Status> {
        if self.live >= self.limit {
            return Err(Status::NoMemory);
        }
        reg.shade(obj);
        let entry = Some(Entry { obj, rights });
        let slot = match self.free.pop() {
            Some(s) => {
                self.slots[s as usize].entry = entry;
                s as usize
            }
            None => {
                // (retired slots stay in the vector, so the slot numbers must still fit)
                if self.slots.len() >= self.slot_cap {
                    return Err(Status::NoMemory);
                }
                self.reserve(1)?;
                GROWTH_EVENTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                self.slots.push(Slot {
                    gen: self.first_gen,
                    entry,
                });
                self.slots.len() - 1
            }
        };
        self.live += 1;
        Ok(Self::encode(slot, self.slots[slot].gen))
    }

    /// Free a slot, returning its entry (the reference moves to the caller).
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: the slot was resolved by `decode` a few lines above (in range and occupied); gen is reset before it can reach GENERATIONS
    fn remove(&mut self, slot: usize) -> Entry {
        let s = &mut self.slots[slot];
        let entry = s.entry.take().expect("occupied");
        s.gen += 1;
        // A slot that has gone through every generation is retired: reusing it would wrap the
        // generation round to values stale handles still hold.
        if s.gen < GENERATIONS {
            self.free.push(slot as u32);
        }
        self.live -= 1;
        entry
    }

    /// Close a handle and release its reference.
    pub fn close<T: Object>(&mut self, reg: &mut Registry<T>, h: Handle) -> Result<(), Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        let entry = self.remove(slot);
        reg.touch_forced(entry.obj);
        reg.handle_gone(entry.obj, self.id, h.0);
        reg.release(entry.obj);
        Ok(())
    }

    fn resolve_rights(old: Rights, requested: Rights) -> Result<Rights, Status> {
        if requested == Rights::SAME {
            Ok(old)
        } else if requested.is_valid() && requested.is_subset_of(old) {
            Ok(requested)
        } else {
            Err(Status::AccessDenied)
        }
    }

    /// A second handle to the same object. Needs DUPLICATE; rights can only shrink.
    pub fn duplicate<T: Object>(
        &mut self,
        reg: &mut Registry<T>,
        h: Handle,
        rights: Rights,
    ) -> Result<Handle, Status> {
        let entry = self.get(h)?;
        if !entry.rights.contains(Rights::DUPLICATE) {
            return Err(Status::AccessDenied);
        }
        let rights = Self::resolve_rights(entry.rights, rights)?;
        if self.room() == 0 {
            return Err(Status::NoMemory);
        }
        // (the room in memory before the reference is taken, so a failure leaves nothing to undo: KT9)
        self.reserve(1)?;
        if !reg.retain(entry.obj) {
            return Err(Status::BadHandle); // cannot happen: a live handle owns a reference
        }
        self.insert(reg, entry.obj, rights)
    }

    /// Swap a handle for one with fewer rights; the old value stops working. On an
    /// error the handle is untouched.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: the slot was resolved by `decode` just above (in range and occupied); the generation test keeps gen + 1 below GENERATIONS
    pub fn replace<T: Object>(
        &mut self,
        reg: &mut Registry<T>,
        h: Handle,
        rights: Rights,
    ) -> Result<Handle, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        let old = self.slots[slot].entry.expect("occupied");
        let rights = Self::resolve_rights(old.rights, rights)?;
        // Removing may retire this slot (its last generation); then the new handle needs another
        // one, and if there is none the replace must fail with the old handle untouched.
        if self.slots[slot].gen + 1 >= GENERATIONS && self.slots_left() == 0 {
            return Err(Status::NoMemory);
        }
        // (the room in memory first: the new handle goes into the slot the old one frees, or into a new slot if that one retires)
        self.reserve(1)?;
        let entry = self.remove(slot);
        // the old value stops working: waiters on it must re-validate (a forced touch), and bindings made through it end (L21c)
        reg.touch_forced(entry.obj);
        reg.handle_gone(entry.obj, self.id, h.0);
        Ok(self
            .insert(reg, entry.obj, rights)
            .expect("a slot was just freed"))
    }

    /// Take a handle out of the table to send it somewhere (needs TRANSFER). The
    /// reference goes with the returned entry.
    #[allow(clippy::expect_used, clippy::indexing_slicing)] // INVARIANT: the slot was resolved by `decode` just above (in range and occupied)
    pub fn remove_for_transfer<T: Object>(
        &mut self,
        reg: &mut Registry<T>,
        h: Handle,
    ) -> Result<Entry, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        if !self.slots[slot]
            .entry
            .expect("occupied")
            .rights
            .contains(Rights::TRANSFER)
        {
            return Err(Status::AccessDenied);
        }
        let entry = self.remove(slot);
        reg.touch_forced(entry.obj);
        reg.handle_gone(entry.obj, self.id, h.0);
        Ok(entry)
    }

    /// Check that `h` is live and transferable without removing it.
    pub fn check_transferable(&self, h: Handle) -> Result<Entry, Status> {
        let entry = self.get(h)?;
        if entry.rights.contains(Rights::TRANSFER) {
            Ok(entry)
        } else {
            Err(Status::AccessDenied)
        }
    }

    /// Every object this table refers to (the roots for `Registry::sweep`).
    pub fn objects(&self) -> impl Iterator<Item = ObjId> + '_ {
        self.slots.iter().filter_map(|s| s.entry.map(|e| e.obj))
    }

    /// Every live handle value in this table (for tests and audits).
    pub fn handle_values(&self) -> impl Iterator<Item = Handle> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.entry.is_some())
            .map(|(i, s)| Self::encode(i, s.gen))
    }

    /// Close everything (process death).
    #[allow(clippy::indexing_slicing)] // INVARIANT: slot < slots.len() (loop bound)
    pub fn drain<T: Object>(&mut self, reg: &mut Registry<T>) {
        for slot in 0..self.slots.len() {
            if self.slots[slot].entry.is_some() {
                let gone = Self::encode(slot, self.slots[slot].gen);
                let entry = self.remove(slot);
                reg.touch_forced(entry.obj);
                reg.handle_gone(entry.obj, self.id, gone.0);
                reg.release(entry.obj);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{event_create, Kobjs};

    /// A table whose slot vector is full of retired slots (the state `handle_replace` can drive a
    /// table to after 2^31 calls, review 2026-11-14): `room` must say so and nothing that checked it
    /// may panic; a replace that would retire the last usable slot fails with the handle untouched.
    #[test]
    fn a_table_out_of_slots_refuses_instead_of_panicking() {
        let mut reg = Kobjs::new();
        let mut t = HandleTable::new(64).with_slot_cap(8);
        // slot 0 is free but on its last generation; every other slot is retired
        t.slots.push(Slot {
            gen: GENERATIONS - 1,
            entry: None,
        });
        t.free.push(0);
        for _ in 1..8 {
            t.slots.push(Slot {
                gen: GENERATIONS,
                entry: None,
            });
        }
        assert_eq!(t.room(), 1, "one usable slot, far below the limit");
        let h = event_create(&mut reg, &mut t).unwrap();
        assert_eq!(t.room(), 0, "no slot left");
        assert_eq!(
            event_create(&mut reg, &mut t),
            Err(Status::NoMemory),
            "create"
        );
        assert_eq!(
            t.duplicate(&mut reg, h, Rights::SAME),
            Err(Status::NoMemory),
            "duplicate"
        );
        assert_eq!(
            t.replace(&mut reg, h, Rights::SAME),
            Err(Status::NoMemory),
            "replace would retire the slot with nowhere to go"
        );
        assert!(t.get(h).is_ok(), "the handle survived the failed replace");
        let live_before = reg.live();
        let _ = t.duplicate(&mut reg, h, Rights::SAME);
        assert_eq!(reg.live(), live_before, "no object leaked");
        t.drain(&mut reg);
    }
}
