//! The fault queue (IOMMU-SPEC 7.1; the IOMMU specification's "Fault/Event queue"): the 32-byte record and the consumer side of
//! the ring the IOMMU fills at `fqt` and the kernel drains at `fqh`. The overflow and memory-fault bits of `fqcsr` are decoded by
//! [`crate::regs::Fqcsr`].

use crate::{PhysMem64, PAGE_SIZE};

/// The transaction type of a fault record (`TTYP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionType {
    /// Not related to a transaction (or a reserved value).
    None,
    UntranslatedFetch,
    UntranslatedRead,
    UntranslatedWrite,
    TranslatedFetch,
    TranslatedRead,
    TranslatedWrite,
    AtsRequest,
    Message,
    Reserved(u8),
}

impl TransactionType {
    pub const fn from_code(code: u8) -> TransactionType {
        match code {
            0 => TransactionType::None,
            1 => TransactionType::UntranslatedFetch,
            2 => TransactionType::UntranslatedRead,
            3 => TransactionType::UntranslatedWrite,
            5 => TransactionType::TranslatedFetch,
            6 => TransactionType::TranslatedRead,
            7 => TransactionType::TranslatedWrite,
            8 => TransactionType::AtsRequest,
            9 => TransactionType::Message,
            c => TransactionType::Reserved(c),
        }
    }
    pub const fn code(self) -> u8 {
        match self {
            TransactionType::None => 0,
            TransactionType::UntranslatedFetch => 1,
            TransactionType::UntranslatedRead => 2,
            TransactionType::UntranslatedWrite => 3,
            TransactionType::TranslatedFetch => 5,
            TransactionType::TranslatedRead => 6,
            TransactionType::TranslatedWrite => 7,
            TransactionType::AtsRequest => 8,
            TransactionType::Message => 9,
            TransactionType::Reserved(c) => c,
        }
    }
    /// The last word of the console line (IOMMU-SPEC 7.2): `read`, `write`, `exec` or `none`. A message is a write.
    pub const fn direction(self) -> &'static str {
        match self {
            TransactionType::UntranslatedRead | TransactionType::TranslatedRead => "read",
            TransactionType::UntranslatedWrite
            | TransactionType::TranslatedWrite
            | TransactionType::Message => "write",
            TransactionType::UntranslatedFetch | TransactionType::TranslatedFetch => "exec",
            _ => "none",
        }
    }
}

// Fault causes the system names (the IOMMU specification's table; QEMU `riscv-iommu-bits.h` lines 386 to 417).
pub const CAUSE_READ_ACCESS_FAULT: u16 = 5;
pub const CAUSE_WRITE_ACCESS_FAULT: u16 = 7;
pub const CAUSE_READ_PAGE_FAULT: u16 = 13;
pub const CAUSE_WRITE_PAGE_FAULT: u16 = 15;
pub const CAUSE_READ_GUEST_PAGE_FAULT: u16 = 21;
pub const CAUSE_WRITE_GUEST_PAGE_FAULT: u16 = 23;
pub const CAUSE_DMA_DISABLED: u16 = 256;
pub const CAUSE_DDT_LOAD_FAULT: u16 = 257;
pub const CAUSE_DDT_INVALID: u16 = 258;
pub const CAUSE_DDT_MISCONFIGURED: u16 = 259;
pub const CAUSE_TTYPE_BLOCKED: u16 = 260;
pub const CAUSE_MSI_LOAD_FAULT: u16 = 261;
pub const CAUSE_MSI_INVALID: u16 = 262;
pub const CAUSE_MSI_MISCONFIGURED: u16 = 263;
pub const CAUSE_MRIF_FAULT: u16 = 264;
pub const CAUSE_MSI_WRITE_FAULT: u16 = 273;

/// One fault record, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultRecord {
    pub cause: u16,
    pub ttype: TransactionType,
    pub device_id: u32,
    /// The process id, when `pv` is set (never, in this system).
    pub pv: bool,
    pub pid: u32,
    pub privileged: bool,
    /// The address the device issued (for most causes).
    pub iotval: u64,
    pub iotval2: u64,
}

const HDR_CAUSE_MASK: u64 = 0xfff; // bits 11:0
const HDR_PID_SHIFT: u32 = 12; // bits 31:12
const HDR_PV: u64 = 1 << 32;
const HDR_PRIV: u64 = 1 << 33;
const HDR_TTYPE_SHIFT: u32 = 34; // bits 39:34
const HDR_DID_SHIFT: u32 = 40; // bits 63:40

impl FaultRecord {
    /// Decode the record's four doublewords: header, reserved, `iotval`, `iotval2`.
    pub const fn decode(words: [u64; 4]) -> FaultRecord {
        let hdr = words[0];
        FaultRecord {
            cause: (hdr & HDR_CAUSE_MASK) as u16,
            pid: ((hdr >> HDR_PID_SHIFT) & 0xf_ffff) as u32,
            pv: hdr & HDR_PV != 0,
            privileged: hdr & HDR_PRIV != 0,
            ttype: TransactionType::from_code(((hdr >> HDR_TTYPE_SHIFT) & 0x3f) as u8),
            device_id: (hdr >> HDR_DID_SHIFT) as u32,
            iotval: words[2],
            iotval2: words[3],
        }
    }

    /// The record's four doublewords (what an IOMMU writes; the simulator's side).
    pub const fn encode(&self) -> [u64; 4] {
        let hdr = (self.cause as u64 & HDR_CAUSE_MASK)
            | ((self.pid as u64 & 0xf_ffff) << HDR_PID_SHIFT)
            | if self.pv { HDR_PV } else { 0 }
            | if self.privileged { HDR_PRIV } else { 0 }
            | ((self.ttype.code() as u64 & 0x3f) << HDR_TTYPE_SHIFT)
            | ((self.device_id as u64 & 0xff_ffff) << HDR_DID_SHIFT);
        [hdr, 0, self.iotval, self.iotval2]
    }

    /// The device id as a PCI address `bb:dd.f` (bus, device, function), for the console line.
    pub const fn pci_address(&self) -> (u8, u8, u8) {
        (
            ((self.device_id >> 8) & 0xff) as u8,
            ((self.device_id >> 3) & 0x1f) as u8,
            (self.device_id & 7) as u8,
        )
    }
}

/// The size of one fault record.
pub const RECORD_BYTES: u64 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultQueueError {
    /// The base is not page-aligned or past the PPN field, or the size is not a power of two from 2 to the records of one page.
    BadShape,
}

/// The kernel's side of the fault queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultQueue {
    base: u64,
    /// The `fqb` value.
    reg: u64,
    entries: u32,
    head: u32,
}

impl FaultQueue {
    pub fn new(base_pa: u64, entries: u32) -> Result<FaultQueue, FaultQueueError> {
        let reg = match crate::regs::queue_base(base_pa, entries) {
            Some(reg) if u64::from(entries).saturating_mul(RECORD_BYTES) <= PAGE_SIZE => reg,
            _ => return Err(FaultQueueError::BadShape),
        };
        Ok(FaultQueue {
            base: base_pa,
            reg,
            entries,
            head: 0,
        })
    }

    /// The value for the `fqb` register.
    pub const fn fqb(&self) -> u64 {
        self.reg
    }
    pub const fn entries(&self) -> u32 {
        self.entries
    }
    /// The head, to be written to `fqh` after records were taken.
    pub const fn head(&self) -> u32 {
        self.head
    }

    /// Take the oldest record, given the tail read from `fqt`: `None` when the queue is empty or the tail is out of range (an
    /// IOMMU that misbehaves). The caller writes [`head`](Self::head) to `fqh` when it has taken what it wants.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: head < entries <= 128, so the offset is below 4 KiB
    pub fn pop<M: PhysMem64>(&mut self, mem: &M, tail: u32) -> Option<FaultRecord> {
        if tail >= self.entries || tail == self.head {
            return None;
        }
        let at = self.base + u64::from(self.head) * RECORD_BYTES;
        let words = [
            mem.read(at),
            mem.read(at + 8),
            mem.read(at + 16),
            mem.read(at + 24),
        ];
        self.head = self.head.wrapping_add(1) & self.entries.wrapping_sub(1);
        Some(FaultRecord::decode(words))
    }
}
