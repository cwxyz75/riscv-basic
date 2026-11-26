//! The register sequences of the kernel's IOMMU driver, as pure code over [`Regs`] (IOMMU5; IOMMU-SPEC 2.4, 2.5, 6.4, 7.1, 7.2):
//! the bring-up from any hand-over state, the submission of commands and the poll of a fence, and the fault-queue drain with its
//! console rate limit. The kernel's `arch::riscv64::iommu` owns the frames, the lock, the clock and the console; everything it
//! writes to the IOMMU's registers is decided here, where `sim::Sim` (the simulated IOMMU) tests it on the host in every
//! hand-over state.
//!
//! Waits are bounded by a caller's clock (`now`, in any unit) and a bound in the same unit: the kernel passes the `time` counter
//! and `IOMMU_FENCE_BOUND` (100 ms of it, IOMMU-SPEC 6.4); the host tests pass a counter of reads.

use core::fmt;

use crate::cmdq::{CmdQueue, Command, QueueError};
use crate::ddt::Ddt;
use crate::faultq::{FaultQueue, FaultRecord};
use crate::regs::{self, decode_ddtp, Caps, Cqcsr, DdtpMode, DdtpRead, Fqcsr, Missing, Required};
use crate::{PhysMem64, Regs};

/// Records drained per fault interrupt at most (IOMMU-SPEC 7.1): a device that retries for ever cannot hold a hart.
pub const DRAIN_PER_INTERRUPT: u32 = 64;
/// Console lines per second at most, for the whole system (IOMMU-SPEC 7.2).
pub const LINES_PER_SECOND: u32 = 16;
/// The command queue's entries and the fault queue's records (IOMMU-SPEC 2.3: one frame each).
pub const CQ_ENTRIES: u32 = 256;
pub const FQ_RECORDS: u32 = 128;

/// A boot refusal (IOMMU-SPEC 2.5). Its `Display` is the fixed line the panic prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The hand-over state was not Off and the IOMMU did not read back Off within the bound after Off was written.
    WillNotGoOff,
    /// A capability of 2.3 is missing.
    Missing(Missing),
    /// A register did not read back what was written.
    Refused(&'static str),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::WillNotGoOff => f.write_str("IOMMU: will not go off"),
            Refusal::Missing(m) => fmt::Display::fmt(m, f),
            Refusal::Refused(reg) => write!(f, "IOMMU: refused {reg}"),
        }
    }
}

/// What the bring-up is given: the empty directory, the two queues (their frames zeroed), what the capabilities must cover, and
/// the wired line of each interrupt cause (`icvec`).
pub struct Plan<'a> {
    pub ddt: &'a Ddt,
    pub cq: &'a CmdQueue,
    pub fq: &'a FaultQueue,
    pub required: Required,
    pub icvec: u64,
}

/// A brought-up IOMMU: the state it was handed over in, its capabilities and the mode read back after step 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Up {
    pub handover: DdtpMode,
    pub caps: Caps,
    pub mode: DdtpMode,
}

/// The name of a `ddtp` mode on the boot line (IOMMU-SPEC 2.5): `Off`, `Bare`, `1LVL`, `2LVL`, `3LVL` or `reserved`.
pub const fn mode_name(m: DdtpMode) -> &'static str {
    match m {
        DdtpMode::Off => "Off",
        DdtpMode::Bare => "Bare",
        DdtpMode::Directory(crate::ddt::Levels::One) => "1LVL",
        DdtpMode::Directory(crate::ddt::Levels::Two) => "2LVL",
        DdtpMode::Directory(crate::ddt::Levels::Three) => "3LVL",
        DdtpMode::Reserved(_) => "reserved",
    }
}

/// Poll `ddtp` until it is not busy, within `bound` of `now`.
fn settled<R: Regs>(r: &mut R, now: &mut dyn FnMut() -> u64, bound: u64) -> Option<DdtpRead> {
    let t0 = now();
    loop {
        let v = decode_ddtp(r.read64(regs::DDTP));
        if !v.busy {
            return Some(v);
        }
        if now().wrapping_sub(t0) > bound {
            return None;
        }
    }
}

/// Disable the queue whose control register is `csr` if it is on, and wait for it to read back off.
fn queue_off<R: Regs>(
    r: &mut R,
    csr: usize,
    now: &mut dyn FnMut() -> u64,
    bound: u64,
) -> Result<(), Refusal> {
    let name = if csr == regs::CQCSR { "cqcsr" } else { "fqcsr" };
    if r.read32(csr) & (regs::QCSR_EN | regs::QCSR_ON | regs::QCSR_BUSY) == 0 {
        return Ok(());
    }
    r.write32(csr, 0);
    let t0 = now();
    while r.read32(csr) & (regs::QCSR_ON | regs::QCSR_BUSY) != 0 {
        if now().wrapping_sub(t0) > bound {
            return Err(Refusal::Refused(name));
        }
    }
    Ok(())
}

/// Enable the queue whose control register is `csr` (with its interrupt), and wait for it to read back on.
fn queue_on<R: Regs>(
    r: &mut R,
    csr: usize,
    now: &mut dyn FnMut() -> u64,
    bound: u64,
) -> Result<(), Refusal> {
    let name = if csr == regs::CQCSR { "cqcsr" } else { "fqcsr" };
    r.write32(csr, regs::QCSR_EN | regs::QCSR_IE);
    let t0 = now();
    loop {
        let v = r.read32(csr);
        if v & regs::QCSR_ON != 0 && v & regs::QCSR_BUSY == 0 {
            return if v & (regs::QCSR_EN | regs::QCSR_IE) == regs::QCSR_EN | regs::QCSR_IE {
                Ok(())
            } else {
                Err(Refusal::Refused(name))
            };
        }
        if now().wrapping_sub(t0) > bound {
            return Err(Refusal::Refused(name));
        }
    }
}

/// IOMMU-SPEC 2.4 steps 1, 2, 4 and 5 (step 3, the allocations, is the caller's; so is the registration of the IOMMU's sources
/// with the interrupt controller, which this sequence does not touch). The only values written to `ddtp` are Off (only when the
/// hand-over state was not Off) and the directory, once; Bare is never written (IO2: there is no constructor for it).
pub fn bring_up<R: Regs>(
    r: &mut R,
    plan: &Plan<'_>,
    now: &mut dyn FnMut() -> u64,
    bound: u64,
) -> Result<Up, Refusal> {
    // step 1: the hand-over state
    let found = settled(r, now, bound).ok_or(Refusal::WillNotGoOff)?;
    if found.mode != DdtpMode::Off {
        regs::write_ddtp(r, regs::Ddtp::off());
        match settled(r, now, bound) {
            Some(v) if v.mode == DdtpMode::Off => {}
            _ => return Err(Refusal::WillNotGoOff),
        }
    }
    queue_off(r, regs::CQCSR, now, bound)?;
    queue_off(r, regs::FQCSR, now, bound)?;
    // step 2: the capabilities, before anything else is written
    let caps = Caps(r.read64(regs::CAPABILITIES));
    caps.check(&plan.required).map_err(Refusal::Missing)?;
    // step 4: wired interrupts, the vectors, the queues
    r.write32(regs::FCTL, regs::fctl_wired());
    if r.read32(regs::FCTL) != regs::fctl_wired() {
        return Err(Refusal::Refused("fctl"));
    }
    r.write64(regs::ICVEC, plan.icvec);
    if r.read64(regs::ICVEC) & 0xffff != plan.icvec & 0xffff {
        return Err(Refusal::Refused("icvec"));
    }
    r.write64(regs::CQB, plan.cq.cqb());
    if r.read64(regs::CQB) != plan.cq.cqb() {
        return Err(Refusal::Refused("cqb"));
    }
    r.write64(regs::FQB, plan.fq.fqb());
    if r.read64(regs::FQB) != plan.fq.fqb() {
        return Err(Refusal::Refused("fqb"));
    }
    r.write32(regs::CQT, 0);
    r.write32(regs::FQH, 0);
    queue_on(r, regs::CQCSR, now, bound)?;
    queue_on(r, regs::FQCSR, now, bound)?;
    // whatever a previous owner left pending is acknowledged (write 1 to clear; switching the queues does not reset it): the
    // kernel's sources are edge-triggered, and a bit left set would mean no edge, and no fault reported, ever again
    r.write32(
        regs::IPSR,
        regs::IPSR_CIP | regs::IPSR_FIP | regs::IPSR_PMIP | regs::IPSR_PIP,
    );
    if r.read32(regs::IPSR) != 0 {
        return Err(Refusal::Refused("ipsr"));
    }
    // step 5: the empty directory, once
    regs::write_ddtp(r, plan.ddt.ddtp());
    let v = settled(r, now, bound).ok_or(Refusal::Refused("ddtp"))?;
    if v.mode != DdtpMode::Directory(plan.ddt.levels()) || v.root_pa != plan.ddt.root() {
        return Err(Refusal::Refused("ddtp"));
    }
    Ok(Up {
        handover: found.mode,
        caps,
        mode: v.mode,
    })
}

/// The negative-control kernel's step 5 (`fault-iommu-bare`): `ddtp` written Bare, the forbidden pass-through state, so that the
/// default-deny test can be seen to fail without isolation. No real build has it.
#[cfg(feature = "fault-iommu-bare")]
pub fn write_bare<R: Regs>(r: &mut R) {
    regs::write_ddtp(r, regs::Ddtp::bare());
}

/// The invalidations that make an empty directory the whole truth after the bring-up (or after any hand-over): every cached
/// device context, every second-stage and every first-stage translation; then a fence (with PR and PW).
pub fn invalidate_everything() -> [Command; 4] {
    [
        Command::iodir_inval_ddt(None),
        Command::iotinval_gvma_all(),
        Command::iotinval_vma(None, None),
        Command::fence(),
    ]
}

/// Push `cmds` and write the new tail to `cqt`; the ticket of the last of them ([`fence_poll`]). Refused, with nothing written
/// to `cqt`, if the ring has no room for all of them.
pub fn submit<R: Regs, M: PhysMem64>(
    r: &mut R,
    mem: &M,
    cq: &mut CmdQueue,
    cmds: &[Command],
) -> Result<u64, QueueError> {
    let free = u64::from(cq.entries())
        .saturating_sub(1)
        .saturating_sub(cq.outstanding());
    if cmds.len() as u64 > free {
        return Err(QueueError::Full);
    }
    let mut tail = cq.tail();
    for c in cmds {
        tail = cq.push(mem, *c)?;
    }
    r.write32(regs::CQT, tail);
    Ok(cq.ticket())
}

/// Where a fence is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fence {
    Done,
    Pending,
    /// The command queue reports an error (an illegal command, a memory fault, a timeout) or a head outside what was submitted:
    /// the wait ends as a failure (IOMMU-SPEC 6.4).
    Failed,
}

/// One poll of the command queue for `ticket`.
pub fn fence_poll<R: Regs>(r: &mut R, cq: &mut CmdQueue, ticket: u64) -> Fence {
    if Cqcsr(r.read32(regs::CQCSR)).failed() {
        return Fence::Failed;
    }
    if cq.completed(r.read32(regs::CQH)).is_err() {
        return Fence::Failed;
    }
    if cq.is_done(ticket) {
        Fence::Done
    } else {
        Fence::Pending
    }
}

/// What one drain of the fault queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drained {
    /// Records taken.
    pub taken: u32,
    /// The queue had overflowed (records were lost; the bit is now cleared).
    pub overflow: bool,
    /// The queue had a memory fault (cleared).
    pub mem_fault: bool,
    /// Records are still waiting: the caller takes the interrupt again (the IOMMU raises its line only when `ipsr.fip` goes from
    /// 0 to 1, so records left behind would otherwise wait for the next fault).
    pub more: bool,
}

/// Drain at most `max` records (IOMMU-SPEC 7.1): acknowledge `ipsr.fip` first (a record that arrives after it raises the line
/// again), take the records, hand each to `each`, write the head, and clear an overflow or memory fault.
pub fn drain_faults<R: Regs, M: PhysMem64>(
    r: &mut R,
    mem: &M,
    fq: &mut FaultQueue,
    max: u32,
    mut each: impl FnMut(FaultRecord),
) -> Drained {
    r.write32(regs::IPSR, regs::IPSR_FIP);
    let csr = Fqcsr(r.read32(regs::FQCSR));
    let tail = r.read32(regs::FQT);
    let mut d = Drained::default();
    while d.taken < max {
        match fq.pop(mem, tail) {
            Some(rec) => {
                each(rec);
                d.taken = d.taken.saturating_add(1);
            }
            None => break,
        }
    }
    if d.taken > 0 {
        r.write32(regs::FQH, fq.head());
    }
    if csr.overflow() || csr.mem_fault() {
        d.overflow = csr.overflow();
        d.mem_fault = csr.mem_fault();
        r.write32(regs::FQCSR, csr.acknowledge());
        // `fip` could not be cleared while the error stood (the write above ended it): clear it now, so that the next record is
        // a new edge
        r.write32(regs::IPSR, regs::IPSR_FIP);
    }
    let tail = r.read32(regs::FQT);
    d.more = tail < fq.entries() && tail != fq.head();
    d
}

/// The console's rate limit (IOMMU-SPEC 7.2): at most [`LINES_PER_SECOND`] lines in each second of the caller's clock; the rest
/// are counted as dropped by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateLimit {
    window: u64,
    printed: u32,
}

impl RateLimit {
    pub const fn new() -> RateLimit {
        RateLimit {
            window: 0,
            printed: 0,
        }
    }
    /// May a line be printed at `now`, with `second` ticks to a second?
    pub fn admit(&mut self, now: u64, second: u64) -> bool {
        let w = now.checked_div(second).unwrap_or(0);
        if w != self.window {
            self.window = w;
            self.printed = 0;
        }
        if self.printed < LINES_PER_SECOND {
            self.printed = self.printed.saturating_add(1);
            true
        } else {
            false
        }
    }
}

/// A fault record's console line (IOMMU-SPEC 7.2), without the newline.
pub struct FaultLine<'a>(pub &'a FaultRecord);

impl fmt::Display for FaultLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (b, d, func) = self.0.pci_address();
        write!(
            f,
            "## iommu: fault cause={} dev={b:02x}:{d:02x}.{func} iova={:#x} {}",
            self.0.cause,
            self.0.iotval,
            self.0.ttype.direction()
        )
    }
}

/// The console line of a fence that did not complete (IOMMU-SPEC 6.4), for `device` or for none.
pub struct FenceTimeoutLine(pub Option<u32>);

impl fmt::Display for FenceTimeoutLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(did) => write!(
                f,
                "## iommu: fence timeout dev={:02x}:{:02x}.{}",
                (did >> 8) & 0xff,
                (did >> 3) & 0x1f,
                did & 7
            ),
            None => f.write_str("## iommu: fence timeout dev=none"),
        }
    }
}
