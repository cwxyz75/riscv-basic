//! The data path between the network driver and the stack (NET-SPEC 5): two pools of 128 packet slots, four rings of
//! (slot, length) entries over a shared page, a control header, and the slot-ownership state machines each side keeps.
//! Pure: the shared memory is reached through the [`Mem`] trait (raw volatile accesses in the processes, atomics in
//! the tests), so the ring logic, the validation of everything read from shared memory and the ownership rules are
//! host-tested, with loom and shuttle for the doorbell handshake. `no_std`, no `unsafe`.
//!
//! **Nothing read from shared memory is trusted**: a slot number must be below [`SLOTS`], a length at most
//! [`FRAME_MAX`] (and at least 14 for a received frame), and the slot must be in the state the reader's table says is
//! legal for that ring; anything else is a [`Violation`], counted by the caller and answered by a restart (5.4).
#![no_std]
#![forbid(unsafe_code)]

pub const SLOTS: usize = 128;
pub const SLOT_SIZE: usize = 2048;
/// The virtio-net header in front of every frame.
pub const HDR_LEN: usize = 12;
pub const FRAME_MAX: usize = 1514;
/// Receive slots are numbered `0..SLOTS`; so are transmit slots (separate pools).
pub const CAPACITY: usize = SLOTS;
pub const ENTRY_BYTES: usize = 8;

/// Control header field offsets (page 0 of the ring VMO).
pub mod ctl {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const GENERATION: usize = 8; // u64: low word at 8, high at 12
    pub const STATE: usize = 16;
    pub const LINK_UP: usize = 20;
    pub const SLOTS: usize = 24;
    pub const SLOT_SIZE: usize = 28;
    pub const HDR_LEN: usize = 32;
    pub const FRAME_MAX: usize = 36;
    pub const MAC: usize = 40; // 6 bytes at 40, padded to 48
    pub const NET_WAITING: usize = 48;
    pub const DRV_WAITING: usize = 52;
    /// The 64-bit counters start here, 8 bytes each.
    pub const STATS: usize = 64;
    pub const STATS_END: usize = 256;
    /// The ring indices: ring `r`'s producer at `INDICES + r * 128`, its consumer 64 bytes later.
    pub const INDICES: usize = 256;
    pub const MAGIC_VALUE: u32 = 0x5254_454e; // "NETR"
    pub const VERSION_VALUE: u32 = 1;
}

/// Page 1 of the ring VMO holds the four rings, 1024 bytes each.
pub const RINGS_BASE: usize = 4096;

/// `state` in the control header.
pub mod state {
    pub const NONE: u32 = 0;
    pub const DRIVER_READY: u32 = 1;
    pub const STACK_ATTACHED: u32 = 2;
}

/// The counters in the control header (index = position; each 8 bytes from `ctl::STATS`).
pub mod stat {
    pub const RX_FRAMES: usize = 0;
    pub const RX_DROPPED_RING_FULL: usize = 1;
    pub const RX_BAD_HEADER: usize = 2;
    pub const RX_BAD_LEN: usize = 3;
    pub const TX_FRAMES: usize = 4;
    pub const TX_DROPPED_NO_SLOT: usize = 5;
    pub const TX_DROPPED_DEVICE: usize = 6;
    pub const DOORBELLS_SENT: usize = 7;
    pub const DOORBELLS_SUPPRESSED: usize = 8;
    pub const RING_VIOLATIONS: usize = 9;
    pub const COUNT: usize = 10;
}

/// Shared memory as the ring code sees it: aligned 32-bit words with acquire/release ordering, and byte access for
/// entry payloads. Offsets are bytes from the start of the ring VMO.
pub trait Mem {
    /// An acquire load of the word at `off` (a multiple of 4).
    fn load(&self, off: usize) -> u32;
    /// A release store.
    fn store(&self, off: usize, v: u32);
    /// A full (sequentially consistent) fence: the doorbell handshake is a store-then-load pattern on both sides
    /// (publish, then read the peer's waiting flag; set my waiting flag, then re-read the ring) and is lost without it.
    fn fence(&self);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RingId {
    RxUsed = 0,
    RxFree = 1,
    TxReady = 2,
    TxDone = 3,
}

impl RingId {
    pub const ALL: [RingId; 4] = [
        RingId::RxUsed,
        RingId::RxFree,
        RingId::TxReady,
        RingId::TxDone,
    ];
    fn prod(self) -> usize {
        ctl::INDICES + self as usize * 128
    }
    fn cons(self) -> usize {
        self.prod() + 64
    }
    fn entries(self) -> usize {
        RINGS_BASE + self as usize * CAPACITY * ENTRY_BYTES
    }
}

/// One ring entry: a slot and the frame length in it (without the virtio header).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub slot: u32,
    pub len: u16,
}

/// Why a read from shared memory was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Violation {
    SlotOutOfRange,
    LenTooBig,
    LenTooSmall,
    /// The slot is not in a state that allows this ring to carry it.
    WrongState,
    /// A producer found its ring full: impossible with 128 slots and honest peers.
    RingOverflow,
    /// An index claims more entries than the capacity.
    BadIndices,
}

/// A view of one ring over shared memory.
pub struct Ring<'a, M: Mem> {
    mem: &'a M,
    id: RingId,
}

impl<'a, M: Mem> Ring<'a, M> {
    pub fn new(mem: &'a M, id: RingId) -> Self {
        Ring { mem, id }
    }

    /// Entries waiting (producer less consumer), or `BadIndices` if the peer's index is nonsense.
    pub fn len(&self) -> Result<usize, Violation> {
        let n = self
            .mem
            .load(self.id.prod())
            .wrapping_sub(self.mem.load(self.id.cons()));
        if n as usize > CAPACITY {
            Err(Violation::BadIndices)
        } else {
            Ok(n as usize)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Ok(0)
    }

    /// Publish an entry (the producer). The entry words are written before the index (release).
    pub fn push(&self, e: Entry) -> Result<(), Violation> {
        let prod = self.mem.load(self.id.prod());
        if prod.wrapping_sub(self.mem.load(self.id.cons())) as usize >= CAPACITY {
            return Err(Violation::RingOverflow);
        }
        let at = self.id.entries() + (prod as usize % CAPACITY) * ENTRY_BYTES;
        self.mem.store(at, e.slot);
        self.mem.store(at + 4, u32::from(e.len));
        self.mem.store(self.id.prod(), prod.wrapping_add(1));
        Ok(())
    }

    /// Take the next entry (the consumer), without validating it: the caller does (`validate`).
    pub fn pop(&self) -> Result<Option<Entry>, Violation> {
        let cons = self.mem.load(self.id.cons());
        let n = self.mem.load(self.id.prod()).wrapping_sub(cons);
        if n == 0 {
            return Ok(None);
        }
        if n as usize > CAPACITY {
            return Err(Violation::BadIndices);
        }
        let at = self.id.entries() + (cons as usize % CAPACITY) * ENTRY_BYTES;
        let slot = self.mem.load(at);
        let len = self.mem.load(at + 4);
        self.mem.store(self.id.cons(), cons.wrapping_add(1));
        Ok(Some(Entry {
            slot,
            len: len.min(u32::from(u16::MAX)) as u16,
        }))
    }

    /// Reset both indices to zero (attach: the rings start empty). Only legal when the peer is not running.
    pub fn reset(&self) {
        self.mem.store(self.id.prod(), 0);
        self.mem.store(self.id.cons(), 0);
    }
}

/// The producer's half of the doorbell (5.5), **after** `Ring::push`: must the consumer be signalled? Only if the ring
/// was empty before the publish and the consumer's waiting flag (at `waiting_off`) is set. The fence orders the publish
/// before the flag read; with the consumer's fence in [`announce_waiting`] one of the two sees the other.
pub fn must_signal<M: Mem>(mem: &M, waiting_off: usize, was_empty: bool) -> bool {
    mem.fence();
    was_empty && mem.load(waiting_off) == 1
}

/// The consumer's half (5.5), after draining: set the waiting flag, fence, and re-check the ring. True: the ring is
/// not empty after all (the flag was cleared again; drain and repeat). False: it is safe to sleep.
pub fn announce_waiting<M: Mem>(mem: &M, waiting_off: usize, ring: RingId) -> bool {
    mem.store(waiting_off, 1);
    mem.fence();
    if Ring::new(mem, ring).is_empty() {
        false
    } else {
        mem.store(waiting_off, 0);
        true
    }
}

/// Validate an entry read from `ring` (5.4): the slot in range, the length plausible.
pub fn validate(ring: RingId, e: Entry) -> Result<Entry, Violation> {
    if e.slot as usize >= SLOTS {
        return Err(Violation::SlotOutOfRange);
    }
    if usize::from(e.len) > FRAME_MAX {
        return Err(Violation::LenTooBig);
    }
    if ring == RingId::RxUsed && usize::from(e.len) < 14 {
        return Err(Violation::LenTooSmall);
    }
    Ok(e)
}

/// Where a slot is (5.4). Each side tracks the states it can know and treats the rest as "the peer's".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// The device has it (receive: posted; transmit: being sent).
    DeviceHas,
    /// The driver holds it (between device and ring).
    DriverHas,
    /// In a ring, waiting for the reader.
    InRing(RingId),
    /// The stack holds it (a received frame being processed, or a transmit frame being filled).
    StackHas,
    /// A transmit slot in the stack's free list.
    StackFree,
}

/// The slot table of one pool (receive or transmit): every slot's state, with the transitions of 5.4 as methods that
/// refuse an illegal one. Both sides keep one per pool and apply only the transitions they perform or observe.
pub struct Pool {
    state: [Slot; SLOTS],
}

impl Pool {
    /// A receive pool at attach: every slot is the device's (the driver posted them all).
    pub fn rx_posted() -> Pool {
        Pool {
            state: [Slot::DeviceHas; SLOTS],
        }
    }

    /// A transmit pool at attach: every slot is in the stack's free list.
    pub fn tx_free() -> Pool {
        Pool {
            state: [Slot::StackFree; SLOTS],
        }
    }

    pub fn get(&self, slot: u32) -> Option<Slot> {
        self.state.get(slot as usize).copied()
    }

    pub fn count(&self, s: Slot) -> usize {
        self.state.iter().filter(|&&x| x == s).count()
    }

    /// Move `slot` from `from` to `to`; `WrongState` unless it is in `from` (and `SlotOutOfRange` for a bad number).
    pub fn mv(&mut self, slot: u32, from: Slot, to: Slot) -> Result<(), Violation> {
        let s = self
            .state
            .get_mut(slot as usize)
            .ok_or(Violation::SlotOutOfRange)?;
        if *s != from {
            return Err(Violation::WrongState);
        }
        *s = to;
        Ok(())
    }
}
