//! A process's handle table (OS-ARCHITECTURE.md, 5.1; SYSCALLS.md, "Handles").
//!
//! A handle value is `generation << 16 | (slot + 1)`: the slot plus one so that 0 is
//! never valid, and a generation that changes each time the slot is freed so a stale
//! value fails instead of naming whatever reuses the slot (INVARIANTS H2). The table
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

struct Slot {
    gen: u16,
    entry: Option<Entry>,
}

pub struct HandleTable {
    slots: Vec<Slot>,
    free: Vec<u16>,
    limit: usize,
    live: usize,
}

impl HandleTable {
    /// A table that holds at most `limit` handles (capped at 65535 by the encoding).
    pub fn new(limit: usize) -> HandleTable {
        HandleTable {
            slots: Vec::new(),
            free: Vec::new(),
            limit: limit.min(0xffff),
            live: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// How many more handles fit.
    pub fn room(&self) -> usize {
        self.limit - self.live
    }

    fn decode(&self, h: Handle) -> Option<usize> {
        let value = h.0;
        let slot = (value & 0xffff).checked_sub(1)? as usize;
        let gen = (value >> 16) as u16;
        let s = self.slots.get(slot)?;
        (s.gen == gen && s.entry.is_some()).then_some(slot)
    }

    fn encode(slot: usize, gen: u16) -> Handle {
        Handle(((gen as u32) << 16) | (slot as u32 + 1))
    }

    /// Look up a handle.
    pub fn get(&self, h: Handle) -> Result<Entry, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        Ok(self.slots[slot].entry.expect("decoded slot is occupied"))
    }

    /// Add a handle that takes over the caller's reference to `obj`.
    pub fn insert(&mut self, obj: ObjId, rights: Rights) -> Result<Handle, Status> {
        if self.live >= self.limit {
            return Err(Status::NoMemory);
        }
        let entry = Some(Entry { obj, rights });
        let slot = match self.free.pop() {
            Some(s) => {
                self.slots[s as usize].entry = entry;
                s as usize
            }
            None => {
                self.slots.push(Slot { gen: 0, entry });
                self.slots.len() - 1
            }
        };
        self.live += 1;
        Ok(Self::encode(slot, self.slots[slot].gen))
    }

    /// Free a slot, returning its entry (the reference moves to the caller).
    fn remove(&mut self, slot: usize) -> Entry {
        let s = &mut self.slots[slot];
        let entry = s.entry.take().expect("occupied");
        s.gen = s.gen.wrapping_add(1);
        self.free.push(slot as u16);
        self.live -= 1;
        entry
    }

    /// Close a handle and release its reference.
    pub fn close<T: Object>(&mut self, reg: &mut Registry<T>, h: Handle) -> Result<(), Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        let entry = self.remove(slot);
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
        if self.live >= self.limit {
            return Err(Status::NoMemory);
        }
        if !reg.retain(entry.obj) {
            return Err(Status::BadHandle); // cannot happen: a live handle owns a reference
        }
        self.insert(entry.obj, rights)
    }

    /// Swap a handle for one with fewer rights; the old value stops working. On an
    /// error the handle is untouched.
    pub fn replace(&mut self, h: Handle, rights: Rights) -> Result<Handle, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        let old = self.slots[slot].entry.expect("occupied");
        let rights = Self::resolve_rights(old.rights, rights)?;
        let entry = self.remove(slot);
        Ok(self
            .insert(entry.obj, rights)
            .expect("a slot was just freed"))
    }

    /// Take a handle out of the table to send it somewhere (needs TRANSFER). The
    /// reference goes with the returned entry.
    pub fn remove_for_transfer(&mut self, h: Handle) -> Result<Entry, Status> {
        let slot = self.decode(h).ok_or(Status::BadHandle)?;
        if !self.slots[slot]
            .entry
            .expect("occupied")
            .rights
            .contains(Rights::TRANSFER)
        {
            return Err(Status::AccessDenied);
        }
        Ok(self.remove(slot))
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

    /// Close everything (process death).
    pub fn drain<T: Object>(&mut self, reg: &mut Registry<T>) {
        for slot in 0..self.slots.len() {
            if self.slots[slot].entry.is_some() {
                let entry = self.remove(slot);
                reg.release(entry.obj);
            }
        }
    }
}
