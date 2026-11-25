//! Register offsets and field codecs (the IOMMU specification's "Register layout"; QEMU's `riscv-iommu-bits.h` lines 62 to 276),
//! and the capability check that decides the boot (IOMMU-SPEC 2.3, 2.5).

use crate::ddt::Levels;
use crate::{ppn_addr, ppn_field, PPN_MASK, PPN_SHIFT};
use core::fmt;

// Offsets in the register page (4 KiB).
pub const CAPABILITIES: usize = 0x000; // 64-bit
pub const FCTL: usize = 0x008; // 32-bit
pub const DDTP: usize = 0x010; // 64-bit
pub const CQB: usize = 0x018; // 64-bit
pub const CQH: usize = 0x020; // 32-bit
pub const CQT: usize = 0x024; // 32-bit
pub const FQB: usize = 0x028; // 64-bit
pub const FQH: usize = 0x030; // 32-bit
pub const FQT: usize = 0x034; // 32-bit
pub const PQB: usize = 0x038; // 64-bit
pub const PQH: usize = 0x040; // 32-bit
pub const PQT: usize = 0x044; // 32-bit
pub const CQCSR: usize = 0x048; // 32-bit
pub const FQCSR: usize = 0x04c; // 32-bit
pub const PQCSR: usize = 0x050; // 32-bit
pub const IPSR: usize = 0x054; // 32-bit
pub const TR_REQ_IOVA: usize = 0x258; // 64-bit
pub const TR_REQ_CTL: usize = 0x260; // 64-bit
pub const TR_RESPONSE: usize = 0x268; // 64-bit
pub const ICVEC: usize = 0x2f8; // 64-bit
pub const MSI_CFG_TBL: usize = 0x300; // 16 entries of 16 bytes
pub const REG_SIZE: usize = 0x1000;

// ---------------------------------------------------------------------------------------------------------------------------
// capabilities

/// The interrupt generation support field (`IGS`, bits 29:28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Igs {
    /// Message-signalled only.
    Msi,
    /// Wired only.
    Wsi,
    /// Either, chosen by `fctl.WSI`.
    Both,
    Reserved,
}

/// The `capabilities` register, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps(pub u64);

impl Caps {
    const fn bit(self, n: u32) -> bool {
        (self.0 >> n) & 1 != 0
    }
    /// The specification version: (major, minor). 1.0 reads `0x10`.
    pub const fn version(self) -> (u8, u8) {
        (((self.0 >> 4) & 0xf) as u8, (self.0 & 0xf) as u8)
    }
    pub const fn sv32(self) -> bool {
        self.bit(8)
    }
    pub const fn sv39(self) -> bool {
        self.bit(9)
    }
    pub const fn sv48(self) -> bool {
        self.bit(10)
    }
    pub const fn sv57(self) -> bool {
        self.bit(11)
    }
    /// Bits 60:59 of a page-table entry are for software (`Svrsw60t59b`).
    pub const fn svrsw60t59b(self) -> bool {
        self.bit(14)
    }
    pub const fn sv32x4(self) -> bool {
        self.bit(16)
    }
    pub const fn sv39x4(self) -> bool {
        self.bit(17)
    }
    pub const fn sv48x4(self) -> bool {
        self.bit(18)
    }
    pub const fn sv57x4(self) -> bool {
        self.bit(19)
    }
    pub const fn msi_flat(self) -> bool {
        self.bit(22)
    }
    pub const fn msi_mrif(self) -> bool {
        self.bit(23)
    }
    pub const fn amo_hwad(self) -> bool {
        self.bit(24)
    }
    pub const fn ats(self) -> bool {
        self.bit(25)
    }
    pub const fn t2gpa(self) -> bool {
        self.bit(26)
    }
    pub const fn igs(self) -> Igs {
        match (self.0 >> 28) & 3 {
            0 => Igs::Msi,
            1 => Igs::Wsi,
            2 => Igs::Both,
            _ => Igs::Reserved,
        }
    }
    pub const fn hpm(self) -> bool {
        self.bit(30)
    }
    pub const fn dbg(self) -> bool {
        self.bit(31)
    }
    /// The physical address width, in bits.
    pub const fn pas(self) -> u8 {
        ((self.0 >> 32) & 0x3f) as u8
    }
    pub const fn pd8(self) -> bool {
        self.bit(38)
    }
    pub const fn pd17(self) -> bool {
        self.bit(39)
    }
    pub const fn pd20(self) -> bool {
        self.bit(40)
    }

    /// The boot's capability check (IOMMU-SPEC 2.3): the first requirement this IOMMU does not meet, in the table's order.
    pub fn check(self, required: &Required) -> Result<(), Missing> {
        if self.version() != (1, 0) {
            return Err(Missing::Version);
        }
        if !self.sv39x4() {
            return Err(Missing::Sv39x4);
        }
        if !self.msi_flat() {
            return Err(Missing::MsiFlat);
        }
        if !self.msi_mrif() {
            return Err(Missing::MsiMrif);
        }
        if !matches!(self.igs(), Igs::Wsi | Igs::Both) {
            return Err(Missing::WiredInterrupts);
        }
        if self.pas() < required.pas_bits {
            return Err(Missing::PhysAddrWidth);
        }
        Ok(())
    }
}

/// What the boot requires beyond the fixed rows of IOMMU-SPEC 2.3: a physical address width that covers every RAM address of the
/// device tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Required {
    pub pas_bits: u8,
}

impl Required {
    /// The width needed to address everything below `top` (one past the highest physical address of the tree).
    pub const fn covering(top: u64) -> Required {
        Required {
            pas_bits: u64::BITS.wrapping_sub(top.saturating_sub(1).leading_zeros()) as u8,
        }
    }
}

/// The first capability of IOMMU-SPEC 2.3 the IOMMU lacks. Its `Display` is the boot refusal's line (2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Version,
    Sv39x4,
    MsiFlat,
    MsiMrif,
    WiredInterrupts,
    PhysAddrWidth,
}

impl Missing {
    /// The capability's name in the refusal line.
    pub const fn name(self) -> &'static str {
        match self {
            Missing::Version => "version 1.0",
            Missing::Sv39x4 => "Sv39x4",
            Missing::MsiFlat => "MSI_FLAT",
            Missing::MsiMrif => "MSI_MRIF",
            Missing::WiredInterrupts => "wired interrupts",
            Missing::PhysAddrWidth => "PAS",
        }
    }
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::MsiMrif => f.write_str("IOMMU: no MRIF"),
            other => write!(f, "IOMMU: missing {}", other.name()),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// fctl

/// `fctl.BE`: big-endian accesses (never set by this system).
pub const FCTL_BE: u32 = 1 << 0;
/// `fctl.WSI`: wired interrupts.
pub const FCTL_WSI: u32 = 1 << 1;
/// `fctl.GXL`: 32-bit guest stage (never set).
pub const FCTL_GXL: u32 = 1 << 2;

/// The value the kernel writes to `fctl`: little-endian, 64-bit, wired interrupts (IOMMU-SPEC 2.4 step 4).
pub const fn fctl_wired() -> u32 {
    FCTL_WSI
}

// ---------------------------------------------------------------------------------------------------------------------------
// ddtp

/// `ddtp.iommu_mode` as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdtpMode {
    Off,
    Bare,
    Directory(Levels),
    Reserved(u8),
}

impl DdtpMode {
    const fn code(self) -> u64 {
        match self {
            DdtpMode::Off => 0,
            DdtpMode::Bare => 1,
            DdtpMode::Directory(Levels::One) => 2,
            DdtpMode::Directory(Levels::Two) => 3,
            DdtpMode::Directory(Levels::Three) => 4,
            DdtpMode::Reserved(c) => c as u64,
        }
    }
}

const DDTP_BUSY: u64 = 1 << 4;

/// A value to write to `ddtp`. The only constructors are the two values the kernel ever writes (IOMMU-SPEC 2.4): Off and a
/// directory mode. There is no Bare (IO2), except behind the negative-control feature `fault-iommu-bare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ddtp(u64);

impl Ddtp {
    /// Off: every transaction is refused.
    pub const fn off() -> Ddtp {
        Ddtp(0)
    }
    /// A directory of `levels` levels whose root is the page at `root_pa`.
    pub const fn directory(levels: Levels, root_pa: u64) -> Ddtp {
        Ddtp(ppn_field(root_pa) | DdtpMode::Directory(levels).code())
    }
    /// The forbidden pass-through state, for the negative-control kernel `fault-iommu-bare` only.
    #[cfg(feature = "fault-iommu-bare")]
    pub const fn bare() -> Ddtp {
        Ddtp(1)
    }
    pub const fn bits(self) -> u64 {
        self.0
    }
    pub const fn mode(self) -> DdtpMode {
        decode_ddtp(self.0).mode
    }
}

/// `ddtp` as read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdtpRead {
    pub mode: DdtpMode,
    /// A mode change is still in progress: the register must not be written and the mode not relied on.
    pub busy: bool,
    pub root_pa: u64,
}

pub const fn decode_ddtp(raw: u64) -> DdtpRead {
    let mode = match raw & 0xf {
        0 => DdtpMode::Off,
        1 => DdtpMode::Bare,
        2 => DdtpMode::Directory(Levels::One),
        3 => DdtpMode::Directory(Levels::Two),
        4 => DdtpMode::Directory(Levels::Three),
        c => DdtpMode::Reserved(c as u8),
    };
    DdtpRead {
        mode,
        busy: raw & DDTP_BUSY != 0,
        root_pa: ppn_addr(raw),
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// queues

/// A queue base register (`cqb`, `fqb`): the queue's first page and `LOG2SZ-1`, so a queue of `2^k` entries has `k - 1` in bits
/// 4:0. `None` for an unaligned base or a size that is not a power of two of at least 2.
pub fn queue_base(base_pa: u64, entries: u32) -> Option<u64> {
    if !entries.is_power_of_two()
        || entries < 2
        || base_pa & (crate::PAGE_SIZE - 1) != 0
        || (base_pa >> 12) > PPN_MASK
    {
        return None;
    }
    let log2sz_minus_1 = u64::from(entries.trailing_zeros()).checked_sub(1)?;
    Some(((base_pa >> 12) << PPN_SHIFT) | log2sz_minus_1)
}

/// Decode a queue base register: (first page's address, number of entries).
pub fn decode_queue_base(raw: u64) -> (u64, u64) {
    (ppn_addr(raw), 2u64 << (raw & 0x1f))
}

/// Queue control and status bits common to `cqcsr` and `fqcsr` (and `pqcsr`).
pub const QCSR_EN: u32 = 1 << 0;
pub const QCSR_IE: u32 = 1 << 1;
/// The queue hit a memory fault (write 1 to clear).
pub const QCSR_MF: u32 = 1 << 8;
/// `cqcsr.cmd_to`: a command timed out (write 1 to clear).
pub const CQCSR_CMD_TO: u32 = 1 << 9;
/// `fqcsr.fqof`: the fault queue overflowed (write 1 to clear).
pub const FQCSR_OF: u32 = 1 << 9;
/// `cqcsr.cmd_ill`: an illegal command (write 1 to clear).
pub const CQCSR_CMD_ILL: u32 = 1 << 10;
/// `cqcsr.fence_w_ip`: an `IOFENCE.C` with WSI completed (write 1 to clear).
pub const CQCSR_FENCE_W_IP: u32 = 1 << 11;
/// The queue is active.
pub const QCSR_ON: u32 = 1 << 16;
/// A change of the enable bit is in progress.
pub const QCSR_BUSY: u32 = 1 << 17;

/// `cqcsr`, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cqcsr(pub u32);

impl Cqcsr {
    pub const fn enabled(self) -> bool {
        self.0 & QCSR_EN != 0
    }
    pub const fn on(self) -> bool {
        self.0 & QCSR_ON != 0
    }
    pub const fn busy(self) -> bool {
        self.0 & QCSR_BUSY != 0
    }
    pub const fn mem_fault(self) -> bool {
        self.0 & QCSR_MF != 0
    }
    pub const fn timeout(self) -> bool {
        self.0 & CQCSR_CMD_TO != 0
    }
    pub const fn illegal(self) -> bool {
        self.0 & CQCSR_CMD_ILL != 0
    }
    /// The queue has stopped on an error (IOMMU-SPEC 6.4: a fence wait ends as a failure).
    pub const fn failed(self) -> bool {
        self.mem_fault() || self.timeout() || self.illegal()
    }
}

/// `fqcsr`, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fqcsr(pub u32);

impl Fqcsr {
    pub const fn enabled(self) -> bool {
        self.0 & QCSR_EN != 0
    }
    pub const fn on(self) -> bool {
        self.0 & QCSR_ON != 0
    }
    pub const fn busy(self) -> bool {
        self.0 & QCSR_BUSY != 0
    }
    pub const fn mem_fault(self) -> bool {
        self.0 & QCSR_MF != 0
    }
    pub const fn overflow(self) -> bool {
        self.0 & FQCSR_OF != 0
    }
    /// The value that keeps the queue as it is (enable and interrupt enable) and clears the overflow and memory-fault bits.
    pub const fn acknowledge(self) -> u32 {
        (self.0 & (QCSR_EN | QCSR_IE)) | (self.0 & (FQCSR_OF | QCSR_MF))
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// ipsr, icvec

/// `ipsr` bits (write 1 to clear): command queue, fault queue, performance monitor, page-request queue.
pub const IPSR_CIP: u32 = 1 << 0;
pub const IPSR_FIP: u32 = 1 << 1;
pub const IPSR_PMIP: u32 = 1 << 2;
pub const IPSR_PIP: u32 = 1 << 3;

/// `icvec`: the interrupt vector (wired line index) of each cause, 4 bits each.
pub const fn icvec(civ: u8, fiv: u8, pmiv: u8, piv: u8) -> u64 {
    (civ as u64 & 0xf)
        | ((fiv as u64 & 0xf) << 4)
        | ((pmiv as u64 & 0xf) << 8)
        | ((piv as u64 & 0xf) << 12)
}

// ---------------------------------------------------------------------------------------------------------------------------
// the debug translation interface

const TR_GO_BUSY: u64 = 1 << 0;
const TR_NW: u64 = 1 << 3;

/// `tr_req_ctl` for a translation request by `device_id` (no process id): `GO` set, `NW` (no write) for a read.
pub const fn tr_req_ctl(device_id: u32, write: bool) -> u64 {
    TR_GO_BUSY | if write { 0 } else { TR_NW } | ((device_id as u64 & 0xff_ffff) << 40)
}

/// The request is still being processed.
pub const fn tr_busy(ctl: u64) -> bool {
    ctl & TR_GO_BUSY != 0
}

/// `tr_response`, decoded: `None` for a fault, else the translated page's address.
pub const fn decode_tr_response(raw: u64) -> Option<u64> {
    if raw & 1 != 0 {
        None
    } else {
        Some(ppn_addr(raw))
    }
}

/// A `tr_response` value (the simulator's side): `None` is a fault.
pub const fn tr_response(result: Option<u64>) -> u64 {
    match result {
        Some(pa) => ppn_field(pa),
        None => 1,
    }
}
