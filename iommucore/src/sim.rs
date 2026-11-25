//! A simulated RISC-V IOMMU for host tests (this crate's, and `kcore`'s and `ksys`'s through the `sim` feature): registers over
//! [`Regs`], memory over [`PhysMem64`], and device accesses that walk the real structures the way the hardware does: `ddtp`, the
//! directory, the device context, the page table or the MSI page table and the interrupt file.
//!
//! It is written from the IOMMU specification's processes as QEMU 11.1.3 implements them (`hw/riscv/riscv-iommu.c`:
//! `riscv_iommu_ctx_fetch`, `riscv_iommu_spa_fetch`, `riscv_iommu_msi_write`, `riscv_iommu_process_cq_tail`,
//! `riscv_iommu_process_ddtp`), **not** from this crate's encoders: its directory and page-table walkers are separate code, so the
//! tests can compare the two (the two-walker test). Where it is stricter than QEMU it is so to catch a kernel bug:
//!
//! * a **translation cache** and a **device-context cache** that only the invalidation commands drop (QEMU has both);
//! * an **MSI-entry cache**, dropped only by `IOTINVAL.GVMA` for the entry's soft context and address (QEMU has none; hardware
//!   may: IOMMU0 item 12, A6), so a missing invalidation after an MSI entry changes is visible;
//! * a page-table entry with any of bits 63:54 set is a fault (this system never writes them);
//! * `ddtp` can be handed over in any mode, with its queues on, and can model the busy bit and an IOMMU that will not go Off
//!   (IOMMU-SPEC 2.4 step 1, A17); every value written to `ddtp` is recorded.
//!
//! Not modelled: process directories, nested (two-stage) contexts, ATS and page requests, 32-bit formats, wired or message
//! interrupt delivery (only `ipsr`), memory access faults on table reads (the fake memory has none).
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)] // INVARIANT: test-only simulator (feature `sim`), never in a kernel build

use crate::cmdq::Command;
use crate::faultq::{self, FaultRecord, TransactionType};
use crate::msi::pext;
use crate::regs::{self, Caps};
use crate::{PhysMem64, Regs, TableFrames};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cell::RefCell;

// ---------------------------------------------------------------------------------------------------------------------------
// fake memory and frames

/// Physical memory as a map of 4 KiB frames, created (zeroed) on first write; a word never written reads as 0.
#[derive(Default)]
pub struct FakeMem {
    frames: RefCell<BTreeMap<u64, Box<[u64; 512]>>>,
}

impl FakeMem {
    pub fn new() -> FakeMem {
        FakeMem::default()
    }
    /// The frame containing `pa` has been written.
    pub fn has_frame(&self, pa: u64) -> bool {
        self.frames.borrow().contains_key(&(pa >> 12))
    }
    /// Write a 32-bit value at a 4-byte-aligned address (a device's write).
    pub fn write32(&self, pa: u64, value: u32) {
        let shift = (pa & 4) * 8;
        let old = self.read(pa & !7);
        self.write(
            pa & !7,
            (old & !(0xffff_ffffu64 << shift)) | (u64::from(value) << shift),
        );
    }
    pub fn read32(&self, pa: u64) -> u32 {
        (self.read(pa & !7) >> ((pa & 4) * 8)) as u32
    }
}

impl PhysMem64 for FakeMem {
    fn read(&self, pa: u64) -> u64 {
        assert!(pa & 7 == 0, "unaligned read at {pa:#x}");
        self.frames
            .borrow()
            .get(&(pa >> 12))
            .map_or(0, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write(&self, pa: u64, value: u64) {
        assert!(pa & 7 == 0, "unaligned write at {pa:#x}");
        self.frames
            .borrow_mut()
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
    }
    fn fetch_or(&self, pa: u64, bits: u64) -> u64 {
        let old = self.read(pa);
        self.write(pa, old | bits);
        old
    }
    fn fetch_and(&self, pa: u64, bits: u64) -> u64 {
        let old = self.read(pa);
        self.write(pa, old & bits);
        old
    }
}

/// What a freed table frame is filled with: valid, with reserved bits set, so a walk through a freed table faults (or reads as
/// misconfigured) instead of finding stale entries.
pub const POISON: u64 = 0xa5a5_a5a5_a5a5_a5a5;

/// A frame allocator for tables: frames from a fixed range, zeroed when given, poisoned when taken back; it counts, refuses on
/// request, and panics on a double free or a free of a frame it never gave.
pub struct FakeFrames<'m, M: PhysMem64> {
    mem: &'m M,
    next: u64,
    end: u64,
    free: Vec<u64>,
    live: BTreeSet<u64>,
    /// Allocations still allowed before `alloc` returns `None` (`None`: unlimited).
    pub budget: Option<u64>,
    pub allocs: u64,
    pub frees: u64,
}

impl<'m, M: PhysMem64> FakeFrames<'m, M> {
    /// Frames from `first_pa` to `first_pa + count` pages.
    pub fn new(mem: &'m M, first_pa: u64, count: u64) -> Self {
        FakeFrames {
            mem,
            next: first_pa,
            end: first_pa + count * 4096,
            free: Vec::new(),
            live: BTreeSet::new(),
            budget: None,
            allocs: 0,
            frees: 0,
        }
    }
    /// Frames given and not taken back.
    pub fn live(&self) -> usize {
        self.live.len()
    }
    pub fn is_live(&self, pa: u64) -> bool {
        self.live.contains(&pa)
    }
}

impl<M: PhysMem64> TableFrames for FakeFrames<'_, M> {
    fn alloc(&mut self) -> Option<u64> {
        if let Some(b) = self.budget.as_mut() {
            if *b == 0 {
                return None;
            }
            *b -= 1;
        }
        let pa = if let Some(pa) = self.free.pop() {
            pa
        } else if self.next < self.end {
            self.next += 4096;
            self.next - 4096
        } else {
            return None;
        };
        for i in 0..512 {
            self.mem.write(pa + i * 8, 0);
        }
        self.live.insert(pa);
        self.allocs += 1;
        Some(pa)
    }
    fn free(&mut self, pa: u64) {
        assert!(
            self.live.remove(&pa),
            "free of a frame that is not live: {pa:#x}"
        );
        for i in 0..512 {
            self.mem.write(pa + i * 8, POISON);
        }
        self.free.push(pa);
        self.frees += 1;
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// configuration

/// `capabilities` of QEMU 11.1.3's platform IOMMU, as observed (IOMMU0, "Corrections"): version 1.0, Sv32 to Sv57 and their x4
/// forms, `Svrsw60t59b`, MSI_FLAT, MSI_MRIF, ATS, IGS both, HPM, DBG, PAS 56, PD8.
pub const QEMU_CAPS: u64 = 0x0000_0078_e2cf_4f10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    pub caps: u64,
    /// Reads of `ddtp` that still show the busy bit after an accepted write (0: the change is immediate, as in QEMU).
    pub busy_reads: u32,
    /// A write of Off is ignored: the IOMMU that will not go Off (IOMMU-SPEC 2.5).
    pub never_off: bool,
    /// Commands run when `cqt` is written (QEMU), or only when the test calls [`Sim::process_commands`].
    pub auto_process: bool,
}

impl SimConfig {
    pub const fn qemu() -> SimConfig {
        SimConfig {
            caps: QEMU_CAPS,
            busy_reads: 0,
            never_off: false,
            auto_process: true,
        }
    }
}

/// The state a previous owner left the IOMMU in (IOMMU-SPEC 2.4 step 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Handover {
    /// `ddtp` as left (mode and root).
    pub ddtp: u64,
    /// The command queue left enabled, with this `cqb`.
    pub cq: Option<u64>,
    /// The fault queue left enabled, with this `fqb`.
    pub fq: Option<u64>,
}

/// How a message reached a simulated interrupt file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// A basic-translate entry forwarded the device's own data word.
    Basic,
    /// An MRIF entry's notice: the page and identity the entry names.
    Notice,
}

/// A message that reached a (simulated) hart's interrupt-file page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub page: u64,
    pub identity: u32,
    pub via: Via,
}

/// What a device's write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Written to memory at this physical address.
    Memory(u64),
    /// Taken as an MSI: forwarded through a basic entry to (page, data).
    Basic { page: u64, data: u32 },
    /// Taken as an MSI through an MRIF entry: the pending bit was set; `notice` is (page, identity) if the identity was enabled.
    Mrif { notice: Option<(u64, u16)> },
}

// ---------------------------------------------------------------------------------------------------------------------------
// the simulator

// Device-context fields (QEMU riscv-iommu-bits.h lines 293 to 313, 419 to 423).
const TC_V: u64 = 1 << 0;
const TC_EN_ATS: u64 = 1 << 1;
const TC_EN_PRI: u64 = 1 << 2;
const TC_T2GPA: u64 = 1 << 3;
const TC_DTF: u64 = 1 << 4;
const TC_PDTV: u64 = 1 << 5;
const TC_PRPR: u64 = 1 << 6;
const TC_GADE: u64 = 1 << 7;
const TC_SADE: u64 = 1 << 8;
const TC_DPE: u64 = 1 << 9;
const TC_SBE: u64 = 1 << 10;
const TC_SXL: u64 = 1 << 11;
const TC_RESERVED: u64 = 0xffff_ffff_00ff_f000; // bits 23:12 and 63:32

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ctx {
    did: u32,
    tc: u64,
    iohgatp: u64,
    ta: u64,
    fsc: u64,
    msiptp: u64,
    mask: u64,
    pattern: u64,
}

impl Ctx {
    fn g_mode(&self) -> u64 {
        self.iohgatp >> 60
    }
    fn s_mode(&self) -> u64 {
        self.fsc >> 60
    }
    fn gscid(&self) -> u16 {
        ((self.iohgatp >> 44) & 0xffff) as u16
    }
    fn pscid(&self) -> u32 {
        ((self.ta >> 12) & 0xf_ffff) as u32
    }
    /// The soft context that tags its cached translations.
    fn tag(&self) -> Tag {
        if self.g_mode() != 0 {
            Tag::G(self.gscid())
        } else {
            Tag::S(self.pscid())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tag {
    G(u16),
    S(u32),
}

/// A cached translation of one page: the frame, and what the leaf allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tlbe {
    pa: u64,
    write: bool,
}

/// Where an access goes after translation.
enum Target {
    Memory(u64),
    Msi(Ctx, u64),
}

/// The simulated IOMMU.
pub struct Sim<'m, M: PhysMem64> {
    mem: &'m M,
    cfg: SimConfig,
    fctl: u32,
    ddtp: u64,
    ddtp_busy: u32,
    ddtp_next: u64,
    cqb: u64,
    cqh: u32,
    cqt: u32,
    cqcsr: u32,
    cq_mask: u32,
    cq_addr: u64,
    fqb: u64,
    fqh: u32,
    fqt: u32,
    fqcsr: u32,
    fq_mask: u32,
    fq_addr: u64,
    ipsr: u32,
    icvec: u64,
    tr_iova: u64,
    tr_ctl: u64,
    tr_resp: u64,
    ddtp_writes: Vec<u64>,
    ctx_cache: BTreeMap<u32, Ctx>,
    tlb: BTreeMap<(Tag, u64), Tlbe>,
    msi_cache: BTreeMap<(Tag, u64), [u64; 2]>,
    deliveries: Vec<Delivery>,
    executed: Vec<Command>,
    faults_reported: u64,
    faults_suppressed: u64,
}

const QCSR_RW1C_CQ: u32 =
    regs::QCSR_MF | regs::CQCSR_CMD_TO | regs::CQCSR_CMD_ILL | regs::CQCSR_FENCE_W_IP;
const QCSR_RW1C_FQ: u32 = regs::QCSR_MF | regs::FQCSR_OF;

impl<'m, M: PhysMem64> Sim<'m, M> {
    /// An IOMMU at reset: `ddtp` Off, queues off, `fctl` 0.
    pub fn new(mem: &'m M, cfg: SimConfig) -> Self {
        let wsi_fixed = matches!(Caps(cfg.caps).igs(), regs::Igs::Wsi);
        Sim {
            mem,
            cfg,
            fctl: if wsi_fixed { regs::FCTL_WSI } else { 0 },
            ddtp: 0,
            ddtp_busy: 0,
            ddtp_next: 0,
            cqb: 0,
            cqh: 0,
            cqt: 0,
            cqcsr: 0,
            cq_mask: 0,
            cq_addr: 0,
            fqb: 0,
            fqh: 0,
            fqt: 0,
            fqcsr: 0,
            fq_mask: 0,
            fq_addr: 0,
            ipsr: 0,
            icvec: 0,
            tr_iova: 0,
            tr_ctl: 0,
            tr_resp: 0,
            ddtp_writes: Vec::new(),
            ctx_cache: BTreeMap::new(),
            tlb: BTreeMap::new(),
            msi_cache: BTreeMap::new(),
            deliveries: Vec::new(),
            executed: Vec::new(),
            faults_reported: 0,
            faults_suppressed: 0,
        }
    }

    /// An IOMMU in the state `h` (written by an earlier owner; nothing is recorded as written).
    pub fn handed_over(mem: &'m M, cfg: SimConfig, h: Handover) -> Self {
        let mut s = Sim::new(mem, cfg);
        s.ddtp = h.ddtp & (crate::PPN_MASK << crate::PPN_SHIFT | 0xf);
        if let Some(cqb) = h.cq {
            s.cqb = cqb;
            s.enable_cq();
            s.cqcsr |= regs::QCSR_EN;
        }
        if let Some(fqb) = h.fq {
            s.fqb = fqb;
            s.enable_fq();
            s.fqcsr |= regs::QCSR_EN;
        }
        s
    }

    /// Every value written to `ddtp`, in order (including writes the IOMMU ignored).
    pub fn ddtp_writes(&self) -> &[u64] {
        &self.ddtp_writes
    }
    /// Every message that reached a simulated interrupt file, in order.
    pub fn deliveries(&self) -> &[Delivery] {
        &self.deliveries
    }
    /// Every command executed, in order.
    pub fn executed(&self) -> &[Command] {
        &self.executed
    }
    /// Faults written to the fault queue (or dropped on overflow), and faults a context's `DTF` suppressed.
    pub fn faults_reported(&self) -> u64 {
        self.faults_reported
    }
    pub fn faults_suppressed(&self) -> u64 {
        self.faults_suppressed
    }
    /// Cached translations, device contexts and MSI entries (for tests that check a flush happened).
    pub fn cached(&self) -> (usize, usize, usize) {
        (self.tlb.len(), self.ctx_cache.len(), self.msi_cache.len())
    }

    fn caps(&self) -> Caps {
        Caps(self.cfg.caps)
    }

    // ---- registers ----------------------------------------------------------------------------------------------------

    fn write_ddtp(&mut self, v: u64) {
        self.ddtp_writes.push(v);
        if self.ddtp_busy > 0 {
            return; // a write while busy is ignored
        }
        let old = self.ddtp & 0xf;
        let new = v & 0xf;
        let ok = match new {
            _ if new == old => true,
            0 | 1 => true,
            2..=4 => old <= 1,
            _ => false,
        };
        if !ok || (new == 0 && old != 0 && self.cfg.never_off) {
            return;
        }
        let value = (v & (crate::PPN_MASK << crate::PPN_SHIFT)) | new;
        if self.cfg.busy_reads > 0 {
            self.ddtp_busy = self.cfg.busy_reads;
            self.ddtp_next = value;
        } else {
            self.ddtp = value;
        }
    }

    fn read_ddtp(&mut self) -> u64 {
        if self.ddtp_busy > 0 {
            let v = self.ddtp | (1 << 4);
            self.ddtp_busy -= 1;
            if self.ddtp_busy == 0 {
                self.ddtp = self.ddtp_next;
            }
            return v;
        }
        self.ddtp
    }

    fn enable_cq(&mut self) {
        let (addr, n) = regs::decode_queue_base(self.cqb);
        self.cq_addr = addr;
        self.cq_mask = (n - 1) as u32;
        self.cqh = 0;
        self.cqt = 0;
        self.cqcsr = (self.cqcsr | regs::QCSR_ON) & !(QCSR_RW1C_CQ | regs::QCSR_BUSY);
    }

    fn enable_fq(&mut self) {
        let (addr, n) = regs::decode_queue_base(self.fqb);
        self.fq_addr = addr;
        self.fq_mask = (n - 1) as u32;
        self.fqh = 0;
        self.fqt = 0;
        self.fqcsr = (self.fqcsr | regs::QCSR_ON) & !(QCSR_RW1C_FQ | regs::QCSR_BUSY);
    }

    fn write_cqcsr(&mut self, v: u32) {
        let keep = self.cqcsr & !(v & QCSR_RW1C_CQ) & (QCSR_RW1C_CQ | regs::QCSR_ON);
        self.cqcsr = keep | (v & (regs::QCSR_EN | regs::QCSR_IE));
        let on = self.cqcsr & regs::QCSR_ON != 0;
        if v & regs::QCSR_EN != 0 && !on {
            self.enable_cq();
        } else if v & regs::QCSR_EN == 0 && on {
            self.cqcsr &= !regs::QCSR_ON;
        }
        if self.cfg.auto_process {
            self.process_commands();
        }
    }

    fn write_fqcsr(&mut self, v: u32) {
        let keep = self.fqcsr & !(v & QCSR_RW1C_FQ) & (QCSR_RW1C_FQ | regs::QCSR_ON);
        self.fqcsr = keep | (v & (regs::QCSR_EN | regs::QCSR_IE));
        let on = self.fqcsr & regs::QCSR_ON != 0;
        if v & regs::QCSR_EN != 0 && !on {
            self.enable_fq();
        } else if v & regs::QCSR_EN == 0 && on {
            self.fqcsr &= !regs::QCSR_ON;
        }
    }

    fn write_fctl(&mut self, v: u32) {
        match self.caps().igs() {
            regs::Igs::Both => self.fctl = v & regs::FCTL_WSI,
            regs::Igs::Wsi => self.fctl = regs::FCTL_WSI,
            _ => self.fctl = 0,
        }
    }

    fn debug_translate(&mut self) {
        if !self.caps().dbg() || !regs::tr_busy(self.tr_ctl) {
            return;
        }
        let did = (self.tr_ctl >> 40) as u32;
        let write = self.tr_ctl & (1 << 3) == 0;
        let r = match self.translate(did, self.tr_iova, write, false) {
            Ok(Target::Memory(pa)) => Some(pa & !0xfff),
            Ok(Target::Msi(_, gpa)) => Some(gpa & !0xfff),
            Err(_) => None,
        };
        self.tr_resp = regs::tr_response(r);
        self.tr_ctl &= !1;
    }

    // ---- commands -----------------------------------------------------------------------------------------------------

    /// Run the commands between `cqh` and `cqt` (as QEMU does on every `cqt` write), stopping at an illegal one.
    pub fn process_commands(&mut self) {
        if self.cqcsr & regs::QCSR_ON == 0
            || self.cqcsr & (regs::CQCSR_CMD_ILL | regs::QCSR_MF) != 0
        {
            return;
        }
        while self.cqh != self.cqt {
            let at = self.cq_addr + u64::from(self.cqh) * 16;
            let cmd = Command {
                dword0: self.mem.read(at),
                dword1: self.mem.read(at + 8),
            };
            if !self.execute(cmd) {
                self.cqcsr |= regs::CQCSR_CMD_ILL;
                if self.cqcsr & regs::QCSR_IE != 0 {
                    self.ipsr |= regs::IPSR_CIP;
                }
                return;
            }
            self.executed.push(cmd);
            self.cqh = (self.cqh + 1) & self.cq_mask;
        }
    }

    /// Execute one command; `false` if it is illegal.
    fn execute(&mut self, cmd: Command) -> bool {
        let d0 = cmd.dword0;
        let opcode = d0 & 0x7f;
        let func = (d0 >> 7) & 7;
        let av = d0 & (1 << 10) != 0;
        let addr_page = ((cmd.dword1 << 2) & !0xfff) >> 12;
        match (opcode, func) {
            (2, 0) => {
                // IOFENCE.C: reserved bits 31:14 must be zero.
                if d0 & 0xffff_c000 != 0 {
                    return false;
                }
                if av {
                    let addr = cmd.dword1 << 2;
                    let data = (d0 >> 32) as u32;
                    let shift = (addr & 4) * 8;
                    let old = self.mem.read(addr & !7);
                    self.mem.write(
                        addr & !7,
                        (old & !(0xffff_ffffu64 << shift)) | (u64::from(data) << shift),
                    );
                }
                true
            }
            (1, 1) => {
                // IOTINVAL.GVMA
                if d0 & (1 << 32) != 0 {
                    return false; // PSCV is illegal here
                }
                let gv = d0 & (1 << 33) != 0;
                let gscid = ((d0 >> 44) & 0xffff) as u16;
                let hit = |tag: &Tag, page: u64| match tag {
                    Tag::G(g) => (!gv || *g == gscid) && (!av || page == addr_page),
                    Tag::S(_) => false,
                };
                self.tlb.retain(|(t, p), _| !hit(t, *p));
                self.msi_cache.retain(|(t, p), _| !hit(t, *p));
                true
            }
            (1, 0) => {
                // IOTINVAL.VMA: with GV it names a nested (VS-stage) context, which is not modelled, so it drops nothing; without
                // GV it drops first-stage-only translations (QEMU: it does not flush a second-stage cache: IOMMU0 item 8).
                let gv = d0 & (1 << 33) != 0;
                let pscv = d0 & (1 << 32) != 0;
                let pscid = ((d0 >> 12) & 0xf_ffff) as u32;
                if !gv {
                    let hit = |tag: &Tag, page: u64| match tag {
                        Tag::S(p) => (!pscv || *p == pscid) && (!av || page == addr_page),
                        Tag::G(_) => false,
                    };
                    self.tlb.retain(|(t, p), _| !hit(t, *p));
                    self.msi_cache.retain(|(t, p), _| !hit(t, *p));
                }
                true
            }
            (3, 0) => {
                // IODIR.INVAL_DDT
                if d0 & (1 << 33) != 0 {
                    self.ctx_cache.remove(&((d0 >> 40) as u32));
                } else {
                    self.ctx_cache.clear();
                }
                true
            }
            (3, 1) => d0 & (1 << 33) != 0, // IODIR.INVAL_PDT: no process contexts are modelled
            _ => false,
        }
    }

    // ---- faults -------------------------------------------------------------------------------------------------------

    fn report(
        &mut self,
        ctx: Option<&Ctx>,
        did: u32,
        write: bool,
        cause: u16,
        iotval: u64,
        iotval2: u64,
    ) {
        if let Some(c) = ctx {
            let always = matches!(
                cause,
                faultq::CAUSE_DMA_DISABLED
                    | faultq::CAUSE_DDT_LOAD_FAULT
                    | faultq::CAUSE_DDT_INVALID
                    | faultq::CAUSE_DDT_MISCONFIGURED
                    | 268
                    | 272
                    | faultq::CAUSE_MSI_WRITE_FAULT
            );
            if c.tc & TC_DTF != 0 && !always {
                self.faults_suppressed += 1;
                return;
            }
        }
        self.faults_reported += 1;
        if self.fqcsr & regs::QCSR_ON == 0 || self.fqcsr & (regs::FQCSR_OF | regs::QCSR_MF) != 0 {
            return;
        }
        let next = (self.fqt + 1) & self.fq_mask;
        if next == self.fqh {
            self.fqcsr |= regs::FQCSR_OF;
        } else {
            let rec = FaultRecord {
                cause,
                ttype: if write {
                    TransactionType::UntranslatedWrite
                } else {
                    TransactionType::UntranslatedRead
                },
                device_id: did,
                pv: false,
                pid: 0,
                privileged: false,
                iotval,
                iotval2,
            };
            let at = self.fq_addr + u64::from(self.fqt) * 32;
            for (i, w) in rec.encode().iter().enumerate() {
                self.mem.write(at + i as u64 * 8, *w);
            }
            self.fqt = next;
        }
        if self.fqcsr & regs::QCSR_IE != 0 {
            self.ipsr |= regs::IPSR_FIP;
        }
    }

    // ---- device contexts ----------------------------------------------------------------------------------------------

    /// Locate and load `did`'s device context (the specification's "Process to locate the Device-context"), from the cache if
    /// it is there. `Ok(None)` is `ddtp` Bare.
    fn context(&mut self, did: u32) -> Result<Option<Ctx>, u16> {
        let mode = self.ddtp & 0xf;
        match mode {
            0 => return Err(faultq::CAUSE_DMA_DISABLED),
            1 => return Ok(None),
            2..=4 => {}
            _ => return Err(faultq::CAUSE_DDT_MISCONFIGURED),
        }
        if let Some(c) = self.ctx_cache.get(&did) {
            return Ok(Some(*c));
        }
        let extended = self.caps().msi_flat();
        let levels = mode - 1; // 1LVL is 2, so 1 to 3
                               // The device-id index of each level, low to high: DDI[0] is the leaf page's.
        let ddi: [(u32, u32); 3] = if extended {
            [(0, 6), (6, 9), (15, 9)]
        } else {
            [(0, 7), (7, 9), (16, 8)]
        };
        let width = match (levels, extended) {
            (1, true) => 6,
            (1, false) => 7,
            (2, true) => 15,
            (2, false) => 16,
            _ => 24,
        };
        if u64::from(did) >> width != 0 {
            return Err(faultq::CAUSE_TTYPE_BLOCKED);
        }
        let field = |i: usize| (u64::from(did) >> ddi[i].0) & ((1 << ddi[i].1) - 1);
        let mut a = ((self.ddtp >> 10) & crate::PPN_MASK) << 12;
        let mut i = levels as usize - 1;
        while i > 0 {
            let ddte = self.mem.read(a + field(i) * 8);
            if ddte & 1 == 0 {
                return Err(faultq::CAUSE_DDT_INVALID);
            }
            if ddte & !((crate::PPN_MASK << 10) | 1) != 0 {
                return Err(faultq::CAUSE_DDT_MISCONFIGURED);
            }
            a = ((ddte >> 10) & crate::PPN_MASK) << 12;
            i -= 1;
        }
        let size = if extended { 64 } else { 32 };
        let at = a + field(0) * size;
        let w = |k: u64| {
            if k * 8 < size {
                self.mem.read(at + k * 8)
            } else {
                0
            }
        };
        let c = Ctx {
            did,
            tc: w(0),
            iohgatp: w(1),
            ta: w(2),
            fsc: w(3),
            msiptp: w(4),
            mask: w(5),
            pattern: w(6),
        };
        if c.tc & TC_V == 0 {
            return Err(faultq::CAUSE_DDT_INVALID);
        }
        if !self.context_ok(&c) {
            return Err(faultq::CAUSE_DDT_MISCONFIGURED);
        }
        self.ctx_cache.insert(did, c);
        Ok(Some(c))
    }

    /// The specification's "Device-context configuration checks", for what the simulator models.
    fn context_ok(&self, c: &Ctx) -> bool {
        let caps = self.caps();
        let tc = c.tc;
        if tc & TC_RESERVED != 0 || tc & (TC_SBE | TC_SXL | TC_PDTV | TC_DPE) != 0 {
            return false; // big-endian, 32-bit and process contexts are not modelled; DPE needs PDTV
        }
        if tc & (TC_EN_ATS | TC_EN_PRI | TC_PRPR | TC_T2GPA) != 0 {
            return false; // ATS is not modelled (and needs the capability)
        }
        if tc & (TC_GADE | TC_SADE) != 0 && !caps.amo_hwad() {
            return false;
        }
        let stage_ok = |mode: u64, x4: bool| match mode {
            0 => true,
            8 => {
                if x4 {
                    caps.sv39x4()
                } else {
                    caps.sv39()
                }
            }
            9 => {
                if x4 {
                    caps.sv48x4()
                } else {
                    caps.sv48()
                }
            }
            10 => {
                if x4 {
                    caps.sv57x4()
                } else {
                    caps.sv57()
                }
            }
            _ => false,
        };
        if !stage_ok(c.g_mode(), true) || !stage_ok(c.s_mode(), false) {
            return false;
        }
        if c.g_mode() != 0 && ((c.iohgatp & crate::PPN_MASK) << 12) & 0x3fff != 0 {
            return false; // the second-stage root must be 16 KiB aligned
        }
        if c.g_mode() != 0 && c.s_mode() != 0 {
            return false; // nested translation is not modelled
        }
        if caps.msi_flat() && !matches!(c.msiptp >> 60, 0 | 1) {
            return false;
        }
        true
    }

    fn msi_active(&self, c: &Ctx) -> bool {
        self.caps().msi_flat() && c.msiptp >> 60 == 1
    }

    /// The MSI address check (QEMU `riscv_iommu_msi_check`).
    fn msi_match(&self, c: &Ctx, gpa: u64) -> bool {
        self.msi_active(c) && ((gpa >> 12) ^ c.pattern) & !c.mask == 0
    }

    // ---- page tables --------------------------------------------------------------------------------------------------

    /// The specification's two-stage translation process for one stage, top down: `Ok((pa, writable))` or a fault.
    fn walk(&self, c: &Ctx, iova: u64, write: bool) -> Result<(u64, bool), ()> {
        let g = c.g_mode() != 0;
        let (atp, ade) = if g {
            (c.iohgatp, c.tc & TC_GADE != 0)
        } else {
            (c.fsc, c.tc & TC_SADE != 0)
        };
        let levels: u32 = match atp >> 60 {
            8 => 3,
            9 => 4,
            10 => 5,
            _ => return Err(()),
        };
        let va_bits = 12 + 9 * levels + if g { 2 } else { 0 };
        if g {
            if iova >> va_bits != 0 {
                return Err(());
            }
        } else {
            let top = (iova as i64) >> (va_bits - 1);
            if top != 0 && top != -1 {
                return Err(());
            }
        }
        let mut a = (atp & crate::PPN_MASK) << 12;
        let mut i = levels - 1;
        let pte_addr;
        let pte = loop {
            let width = if g && i == levels - 1 { 11 } else { 9 };
            let vpn = (iova >> (12 + 9 * i)) & ((1 << width) - 1);
            let at = a + vpn * 8;
            let pte = self.mem.read(at);
            let (v, r, w, x) = (pte & 1 != 0, pte & 2 != 0, pte & 4 != 0, pte & 8 != 0);
            if !v || (!r && w) || pte >> 54 != 0 {
                return Err(());
            }
            if r || x {
                pte_addr = at;
                break pte;
            }
            if i == 0 {
                return Err(());
            }
            i -= 1;
            a = ((pte >> 10) & crate::PPN_MASK) << 12;
        };
        let ppn = (pte >> 10) & crate::PPN_MASK;
        let (r, w, u, acc, dirty) = (
            pte & 2 != 0,
            pte & 4 != 0,
            pte & 16 != 0,
            pte & 64 != 0,
            pte & 128 != 0,
        );
        if !u {
            return Err(()); // no process id: a user access (first stage); the second stage always checks as user
        }
        if (write && !w) || (!write && !r) {
            return Err(());
        }
        if i > 0 && ppn & ((1 << (9 * i)) - 1) != 0 {
            return Err(()); // a misaligned superpage
        }
        if !acc || (write && !dirty) {
            if !ade {
                return Err(());
            }
            self.mem
                .fetch_or(pte_addr, 64 | if write { 128 } else { 0 });
        }
        let span = (1u64 << (12 + 9 * i)) - 1;
        Ok(((ppn << 12) & !span | (iova & span), w && (dirty || ade)))
    }

    /// Translate one access of `did` at `iova`: to memory, or to the MSI path. Reports a fault when `report` is set.
    fn translate(&mut self, did: u32, iova: u64, write: bool, report: bool) -> Result<Target, u16> {
        let ctx = match self.context(did) {
            Ok(Some(c)) => c,
            Ok(None) => return Ok(Target::Memory(iova)), // ddtp Bare
            Err(cause) => {
                if report {
                    self.report(None, did, write, cause, iova, 0);
                }
                return Err(cause);
            }
        };
        let s_bare = ctx.s_mode() == 0;
        if s_bare && write && self.msi_match(&ctx, iova) {
            return Ok(Target::Msi(ctx, iova));
        }
        if ctx.g_mode() == 0 && s_bare {
            return Ok(Target::Memory(iova)); // a context with both stages Bare: pass-through
        }
        let key = (ctx.tag(), iova >> 12);
        if let Some(e) = self.tlb.get(&key) {
            if !write || e.write {
                return Ok(Target::Memory(e.pa | (iova & 0xfff)));
            }
        }
        match self.walk(&ctx, iova, write) {
            Ok((pa, writable)) => {
                if !s_bare && write && self.msi_match(&ctx, pa) {
                    return Ok(Target::Msi(ctx, pa));
                }
                if report {
                    self.tlb.insert(
                        key,
                        Tlbe {
                            pa: pa & !0xfff,
                            write: writable,
                        },
                    );
                }
                Ok(Target::Memory(pa))
            }
            Err(()) => {
                let g = ctx.g_mode() != 0;
                let cause = match (g, write) {
                    (true, false) => faultq::CAUSE_READ_GUEST_PAGE_FAULT,
                    (true, true) => faultq::CAUSE_WRITE_GUEST_PAGE_FAULT,
                    (false, false) => faultq::CAUSE_READ_PAGE_FAULT,
                    (false, true) => faultq::CAUSE_WRITE_PAGE_FAULT,
                };
                if report {
                    self.report(
                        Some(&ctx),
                        did,
                        write,
                        cause,
                        iova,
                        if g { iova } else { 0 },
                    );
                }
                Err(cause)
            }
        }
    }

    /// A device's MSI write, after the address check (QEMU `riscv_iommu_msi_write`).
    fn msi_write(&mut self, c: &Ctx, gpa: u64, data: u32) -> Result<WriteOutcome, u16> {
        let r = self.msi_path(c, gpa, data);
        if let Err(cause) = r {
            self.report(Some(c), c.did, true, cause, 0, 0);
        }
        r
    }

    fn msi_path(&mut self, c: &Ctx, gpa: u64, data: u32) -> Result<WriteOutcome, u16> {
        let file = pext(gpa >> 12, c.mask);
        let offset = file * 16;
        let root = (c.msiptp & crate::PPN_MASK) << 12;
        if root & offset != 0 {
            return Err(faultq::CAUSE_MSI_LOAD_FAULT); // the file number is outside the table
        }
        let key = (c.tag(), gpa >> 12);
        let pte = match self.msi_cache.get(&key) {
            Some(p) => *p,
            None => {
                let p = [
                    self.mem.read(root | offset),
                    self.mem.read((root | offset) + 8),
                ];
                if p[0] & 1 == 0 || p[0] >> 63 != 0 {
                    return Err(faultq::CAUSE_MSI_INVALID); // invalid, or custom (C): not cached
                }
                self.msi_cache.insert(key, p);
                p
            }
        };
        match (pte[0] >> 1) & 3 {
            3 => {
                // Basic translate: forward the device's own data word to the entry's page.
                if pte[0] & ((0x1ff << 54) | (0x7f << 3)) != 0 {
                    return Err(faultq::CAUSE_MSI_MISCONFIGURED);
                }
                let page = ((pte[0] >> 10) & crate::PPN_MASK) << 12;
                self.deliveries.push(Delivery {
                    page,
                    identity: data,
                    via: Via::Basic,
                });
                Ok(WriteOutcome::Basic { page, data })
            }
            1 => {
                // MRIF: set the pending bit; if enabled, send the entry's notice.
                if data > 2047 || gpa & 3 != 0 {
                    return Err(faultq::CAUSE_MSI_MISCONFIGURED);
                }
                let file_pa = ((pte[0] >> 7) & ((1 << 47) - 1)) << 9;
                let pending = file_pa | ((u64::from(data) & 0x7c0) >> 2);
                let bit = 1u64 << (data & 0x3f);
                self.mem.fetch_or(pending, bit);
                if self.mem.read(pending + 8) & bit == 0 {
                    return Ok(WriteOutcome::Mrif { notice: None });
                }
                let page = ((pte[1] >> 10) & crate::PPN_MASK) << 12;
                let id = ((pte[1] & 0x3ff) | (((pte[1] >> 60) & 1) << 10)) as u16;
                self.deliveries.push(Delivery {
                    page,
                    identity: u32::from(id),
                    via: Via::Notice,
                });
                Ok(WriteOutcome::Mrif {
                    notice: Some((page, id)),
                })
            }
            _ => Err(faultq::CAUSE_MSI_MISCONFIGURED),
        }
    }

    // ---- device accesses ----------------------------------------------------------------------------------------------

    /// Device `did` reads at `iova`: the physical address it reaches, or the fault cause (also written to the fault queue
    /// unless the context suppresses it).
    pub fn read(&mut self, did: u32, iova: u64) -> Result<u64, u16> {
        match self.translate(did, iova, false, true)? {
            Target::Memory(pa) => Ok(pa),
            Target::Msi(..) => unreachable!("a read never takes the MSI path"),
        }
    }

    /// Device `did` writes the 32-bit `data` at the 4-byte-aligned `iova`: to memory (which is written), or as an MSI.
    pub fn write(&mut self, did: u32, iova: u64, data: u32) -> Result<WriteOutcome, u16> {
        match self.translate(did, iova, true, true)? {
            Target::Memory(pa) => {
                let shift = (pa & 4) * 8;
                let old = self.mem.read(pa & !7);
                self.mem.write(
                    pa & !7,
                    (old & !(0xffff_ffffu64 << shift)) | (u64::from(data) << shift),
                );
                Ok(WriteOutcome::Memory(pa))
            }
            Target::Msi(ctx, gpa) => self.msi_write(&ctx, gpa, data),
        }
    }
}

impl<M: PhysMem64> Regs for Sim<'_, M> {
    fn read32(&mut self, offset: usize) -> u32 {
        match offset {
            regs::FCTL => self.fctl,
            regs::CQH => self.cqh,
            regs::CQT => self.cqt,
            regs::FQH => self.fqh,
            regs::FQT => self.fqt,
            regs::CQCSR => self.cqcsr,
            regs::FQCSR => self.fqcsr,
            regs::PQCSR | regs::PQH | regs::PQT => 0,
            regs::IPSR => self.ipsr,
            o if o % 8 == 4 => (self.read64(o - 4) >> 32) as u32,
            o => self.read64(o) as u32,
        }
    }
    fn write32(&mut self, offset: usize, value: u32) {
        match offset {
            regs::FCTL => self.write_fctl(value),
            regs::CQT => {
                self.cqt = value & self.cq_mask;
                if self.cfg.auto_process {
                    self.process_commands();
                }
            }
            regs::FQH => self.fqh = value & self.fq_mask,
            regs::CQCSR => self.write_cqcsr(value),
            regs::FQCSR => self.write_fqcsr(value),
            regs::IPSR => self.ipsr &= !value,
            _ => {} // read-only or not modelled
        }
    }
    fn read64(&mut self, offset: usize) -> u64 {
        match offset {
            regs::CAPABILITIES => self.cfg.caps,
            regs::DDTP => self.read_ddtp(),
            regs::CQB => self.cqb,
            regs::FQB => self.fqb,
            regs::ICVEC => self.icvec,
            regs::TR_REQ_IOVA => self.tr_iova,
            regs::TR_REQ_CTL => self.tr_ctl,
            regs::TR_RESPONSE => self.tr_resp,
            o if matches!(
                o,
                regs::FCTL | regs::CQH | regs::FQH | regs::CQCSR | regs::IPSR
            ) =>
            {
                u64::from(self.read32(o)) | (u64::from(self.read32(o + 4)) << 32)
            }
            _ => 0,
        }
    }
    fn write64(&mut self, offset: usize, value: u64) {
        match offset {
            regs::DDTP => self.write_ddtp(value),
            regs::CQB => {
                if self.cqcsr & regs::QCSR_ON == 0 {
                    self.cqb = value & ((crate::PPN_MASK << 10) | 0x1f);
                }
            }
            regs::FQB => {
                if self.fqcsr & regs::QCSR_ON == 0 {
                    self.fqb = value & ((crate::PPN_MASK << 10) | 0x1f);
                }
            }
            regs::ICVEC => self.icvec = value & 0xffff,
            regs::TR_REQ_IOVA => self.tr_iova = value & !0xfff,
            regs::TR_REQ_CTL => {
                self.tr_ctl = value;
                self.debug_translate();
            }
            _ => {}
        }
    }
}
