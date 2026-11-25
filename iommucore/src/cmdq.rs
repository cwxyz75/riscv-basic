//! The command queue (IOMMU-SPEC 6; the IOMMU specification's "Command queue"): the encodings of the four commands the kernel
//! issues and the ring arithmetic of the queue as a state machine. Commands are 16 bytes; the queue is a power-of-two ring the
//! kernel fills at `cqt` and the IOMMU consumes at `cqh`. One slot is always left empty, so `head == tail` means empty.
//!
//! The kernel's fence wait is a **ticket**: the sequence number one past the fence's own, done once the IOMMU's head has passed
//! it ([`CmdQueue::is_done`]). Sequence numbers are 64-bit and never wrap in practice, so a ticket survives any number of trips
//! round the ring.

use crate::{PhysMem64, PAGE_SIZE};

const OP_IOTINVAL: u64 = 1;
const OP_IOFENCE: u64 = 2;
const OP_IODIR: u64 = 3;
const FUNC_SHIFT: u32 = 7;
const FUNC_VMA: u64 = 0;
const FUNC_GVMA: u64 = 1;
const FUNC_INVAL_DDT: u64 = 0;
const FUNC_FENCE_C: u64 = 0;
/// `IOTINVAL.AV` and `IOFENCE.C.AV`: the address operand is valid.
const AV: u64 = 1 << 10;
const IOFENCE_PR: u64 = 1 << 12;
const IOFENCE_PW: u64 = 1 << 13;
const IOFENCE_DATA_SHIFT: u32 = 32;
const IOTINVAL_PSCID_SHIFT: u32 = 12;
const IOTINVAL_PSCV: u64 = 1 << 32;
const IOTINVAL_GV: u64 = 1 << 33;
const IOTINVAL_GSCID_SHIFT: u32 = 44;
const IODIR_DV: u64 = 1 << 33;
const IODIR_DID_SHIFT: u32 = 40;

/// One command: two doublewords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub dword0: u64,
    pub dword1: u64,
}

/// The address operand of an invalidation: bits 63:12 of the address in bits 61:10 of the second doubleword.
const fn addr_operand(addr: u64) -> u64 {
    (addr >> 12) << 10
}

impl Command {
    /// `IOTINVAL.GVMA`: drop the second-stage translations (and cached MSI entries) of soft-context `gscid`, all of them or only
    /// those of the page at `addr`. The only form the kernel issues for a domain's address space (IOMMU-SPEC amendment 2).
    pub const fn iotinval_gvma(gscid: u16, addr: Option<u64>) -> Command {
        let d0 = OP_IOTINVAL
            | (FUNC_GVMA << FUNC_SHIFT)
            | IOTINVAL_GV
            | ((gscid as u64) << IOTINVAL_GSCID_SHIFT);
        match addr {
            Some(a) => Command {
                dword0: d0 | AV,
                dword1: addr_operand(a),
            },
            None => Command {
                dword0: d0,
                dword1: 0,
            },
        }
    }

    /// `IOTINVAL.VMA` for a first-stage-only context: drop the translations of process soft-context `pscid` (or of every one),
    /// all of them or only those of the page at `addr`.
    pub const fn iotinval_vma(pscid: Option<u32>, addr: Option<u64>) -> Command {
        let mut d0 = OP_IOTINVAL | (FUNC_VMA << FUNC_SHIFT);
        if let Some(p) = pscid {
            d0 |= IOTINVAL_PSCV | ((p as u64 & 0xf_ffff) << IOTINVAL_PSCID_SHIFT);
        }
        match addr {
            Some(a) => Command {
                dword0: d0 | AV,
                dword1: addr_operand(a),
            },
            None => Command {
                dword0: d0,
                dword1: 0,
            },
        }
    }

    /// `IODIR.INVAL_DDT`: drop the cached device context of `device_id`, or of every device.
    pub const fn iodir_inval_ddt(device_id: Option<u32>) -> Command {
        let d0 = OP_IODIR | (FUNC_INVAL_DDT << FUNC_SHIFT);
        match device_id {
            Some(d) => Command {
                dword0: d0 | IODIR_DV | ((d as u64 & 0xff_ffff) << IODIR_DID_SHIFT),
                dword1: 0,
            },
            None => Command {
                dword0: d0,
                dword1: 0,
            },
        }
    }

    /// `IOFENCE.C`, always with PR and PW (it also orders the device reads and writes the IOMMU has already let through: A5), and
    /// with `completion` = (address, data), a 4-byte write of `data` to the 4-byte-aligned address when it completes.
    pub const fn iofence_c(completion: Option<(u64, u32)>) -> Command {
        let d0 = OP_IOFENCE | (FUNC_FENCE_C << FUNC_SHIFT) | IOFENCE_PR | IOFENCE_PW;
        match completion {
            Some((addr, data)) => Command {
                dword0: d0 | AV | ((data as u64) << IOFENCE_DATA_SHIFT),
                dword1: addr >> 2,
            },
            None => Command {
                dword0: d0,
                dword1: 0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// The ring has no free slot: wait for the IOMMU.
    Full,
    /// A head value outside the commands the kernel submitted (an IOMMU that misbehaves).
    BadHead,
    /// The base is not page-aligned, or the size is not a power of two from 2 to the entries of one page.
    BadShape,
}

/// The kernel's side of the command queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdQueue {
    base: u64,
    entries: u32,
    /// The next slot to fill (the value written to `cqt`).
    tail: u32,
    /// The IOMMU's head as last read.
    head: u32,
    /// Commands ever pushed, and ever consumed.
    pushed: u64,
    retired: u64,
}

/// The size of one command.
pub const COMMAND_BYTES: u64 = 16;

impl CmdQueue {
    /// A queue of `entries` commands in the page at `base_pa` (which the caller zeroed), empty, as the IOMMU resets it when the
    /// queue is enabled.
    pub fn new(base_pa: u64, entries: u32) -> Result<CmdQueue, QueueError> {
        if base_pa & (PAGE_SIZE - 1) != 0
            || !entries.is_power_of_two()
            || entries < 2
            || u64::from(entries).saturating_mul(COMMAND_BYTES) > PAGE_SIZE
        {
            return Err(QueueError::BadShape);
        }
        Ok(CmdQueue {
            base: base_pa,
            entries,
            tail: 0,
            head: 0,
            pushed: 0,
            retired: 0,
        })
    }

    /// The value for the `cqb` register.
    pub fn cqb(&self) -> u64 {
        crate::regs::queue_base(self.base, self.entries).unwrap_or(0)
    }
    pub const fn entries(&self) -> u32 {
        self.entries
    }
    pub const fn tail(&self) -> u32 {
        self.tail
    }

    const fn next(&self, i: u32) -> u32 {
        i.wrapping_add(1) & (self.entries.wrapping_sub(1))
    }

    /// Commands the kernel has pushed that the IOMMU has not yet consumed.
    pub const fn outstanding(&self) -> u64 {
        self.pushed.wrapping_sub(self.retired)
    }
    pub const fn is_empty(&self) -> bool {
        self.head == self.tail
    }
    pub const fn is_full(&self) -> bool {
        self.next(self.tail) == self.head
    }

    /// Write `cmd` into the next slot and advance the tail; returns the new tail, to be written to `cqt`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: tail < entries <= 256, so the offset is below 4 KiB
    pub fn push<M: PhysMem64>(&mut self, mem: &M, cmd: Command) -> Result<u32, QueueError> {
        if self.is_full() {
            return Err(QueueError::Full);
        }
        let at = self.base + u64::from(self.tail) * COMMAND_BYTES;
        mem.write(at, cmd.dword0);
        mem.write(at + 8, cmd.dword1);
        self.tail = self.next(self.tail);
        self.pushed = self.pushed.wrapping_add(1);
        Ok(self.tail)
    }

    /// The ticket of everything pushed so far: [`is_done`](Self::is_done) once the IOMMU has consumed it all. Take it right after
    /// pushing the fence.
    pub const fn ticket(&self) -> u64 {
        self.pushed
    }

    /// Record the head read from `cqh`. A head outside the submitted commands is refused and changes nothing.
    pub fn completed(&mut self, head: u32) -> Result<(), QueueError> {
        if head >= self.entries {
            return Err(QueueError::BadHead);
        }
        let mask = self.entries.wrapping_sub(1);
        let advanced = head.wrapping_sub(self.head) & mask;
        let in_flight = self.tail.wrapping_sub(self.head) & mask;
        if advanced > in_flight {
            return Err(QueueError::BadHead);
        }
        self.head = head;
        self.retired = self.retired.wrapping_add(u64::from(advanced));
        Ok(())
    }

    /// The IOMMU has consumed every command up to `ticket`.
    pub const fn is_done(&self, ticket: u64) -> bool {
        self.retired >= ticket
    }
}
