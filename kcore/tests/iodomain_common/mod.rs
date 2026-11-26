//! The machine the device-domain tests run on (IOMMU3): one frame database for every frame (DMA VMOs, page tables, roots, the
//! directory and the queues), the IOMMU's view of memory (`IoMem`, which the simulated device reads and writes), the simulated IOMMU of
//! `iommucore::sim` brought up the way the kernel does it (command queue, directory, `ddtp`), and the store. The simulator is the
//! independent half of the oracle: it walks the real tables with its own walker and keeps its own translation and context caches, which
//! only the invalidation commands drop.
#![allow(dead_code)]

use crate::common::FakeMem;
use iommucore::cmdq::CmdQueue;
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::faultq::FaultQueue;
use iommucore::iopt::Access;
use iommucore::iopt::{Iopt, PtMode};
use iommucore::regs::{self, Ddtp};
use iommucore::sim::{Sim, SimConfig, POISON};
use iommucore::{PhysMem64, Regs, TableFrames};
use kcore::aspace::Vmos;
use kcore::frame::FrameDb;
use kcore::iodomain::*;
use kcore::job::{JobId, Jobs, Resource};
use kcore::Mm;
use std::collections::HashMap;
use std::sync::Mutex;

/// Physical memory as the IOMMU sees it, by physical address. Thread-safe (the concurrency tests share it), and allocation-free once
/// a frame exists (`prefill`), so the allocation-counting tests see only the store's own allocations.
#[derive(Default)]
pub struct IoMem {
    frames: Mutex<HashMap<u64, Box<[u64; 512]>>>,
}

impl IoMem {
    pub fn new() -> IoMem {
        IoMem::default()
    }
    /// Create the frames `first ..= first + count - 1` (frame numbers) now.
    pub fn prefill(&self, first: u64, count: u64) {
        let mut f = self.frames.lock().unwrap();
        for p in first..first + count {
            f.entry(p).or_insert_with(|| Box::new([0; 512]));
        }
    }
    /// Forget every frame (a fresh machine in the same memory: the concurrency tests reuse one for every schedule).
    pub fn clear(&self) {
        self.frames.lock().unwrap().clear();
    }
}

impl PhysMem64 for IoMem {
    fn read(&self, pa: u64) -> u64 {
        assert!(pa & 7 == 0);
        self.frames
            .lock()
            .unwrap()
            .get(&(pa >> 12))
            .map_or(0, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write(&self, pa: u64, value: u64) {
        assert!(pa & 7 == 0);
        self.frames
            .lock()
            .unwrap()
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

/// How many table frames and roots the store holds from the frame database.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameCounts {
    pub tables: u64,
    pub roots: u64,
}

/// The store's frames: from the frame database, zeroed when given, poisoned when taken back (a walk through a freed table faults).
pub struct DbFrames<'a> {
    pub db: &'a mut FrameDb,
    pub mem: &'a IoMem,
    pub counts: &'a mut FrameCounts,
    /// Fault injection: when `Some(n)`, the table allocation after `n` more succeeds fails (and every one after it).
    pub fail: Option<u64>,
}

impl TableFrames for DbFrames<'_> {
    fn alloc(&mut self) -> Option<u64> {
        match &mut self.fail {
            Some(0) => return None,
            Some(n) => *n -= 1,
            None => {}
        }
        let pfn = self.db.alloc()?;
        let pa = pfn << 12;
        for i in 0..512 {
            self.mem.write(pa + i * 8, 0);
        }
        self.counts.tables += 1;
        Some(pa)
    }
    fn free(&mut self, pa: u64) {
        for i in 0..512 {
            self.mem.write(pa + i * 8, POISON);
        }
        assert_eq!(
            self.db.dec(pa >> 12),
            Ok(true),
            "a table frame is freed once, by its only holder"
        );
        self.counts.tables -= 1;
    }
}

impl IoFrames for DbFrames<'_> {
    fn alloc_root(&mut self) -> Option<u64> {
        match &mut self.fail {
            Some(0) => return None,
            Some(n) => *n -= 1,
            None => {}
        }
        let pfn = self.db.alloc_contiguous(4)?;
        assert_eq!(pfn % 4, 0, "an order-2 block is aligned to 16 KiB");
        for i in 0..4 * 512 {
            self.mem.write((pfn << 12) + i * 8, 0);
        }
        self.counts.roots += 1;
        Some(pfn << 12)
    }
    fn free_root(&mut self, pa: u64) {
        for f in 0..4 {
            for i in 0..512 {
                self.mem.write(pa + f * 4096 + i * 8, POISON);
            }
            assert_eq!(self.db.dec((pa >> 12) + f), Ok(true));
        }
        self.counts.roots -= 1;
    }
}

/// The first frame number of the database (aligned, so order-2 blocks are 16 KiB aligned), and where the fault queue's page is (one
/// page outside the database).
pub const BASE_PFN: u64 = 0x8_0000;
pub const FQ_PA: u64 = 0x7_f000 << 12;

/// What the bring-up took from the database for good: the directory page, the command-queue page and the quarantine table's root.
pub struct Boot {
    pub ddt: Ddt,
    pub cmdq: CmdQueue,
    pub quarantine: Iopt,
    pub frames_taken: usize,
}

/// Bring the simulated IOMMU up as the kernel does (IOMMU-SPEC 2.4 steps 3 to 5): command queue (256 entries), optionally the fault
/// queue (128 records), a one-level extended directory (device ids 0 to 63), `ddtp` written once; and the quarantine table.
pub fn boot(db: &mut FrameDb, mem: &IoMem, sim: &mut Sim<'_, IoMem>, fault_queue: bool) -> Boot {
    boot_with(db, mem, sim, fault_queue, Levels::One)
}

/// `boot` with a directory of `levels` levels (more than one: the directory pages below the root are allocated as devices are
/// claimed, and a failed claim can unlink one again).
pub fn boot_with(
    db: &mut FrameDb,
    mem: &IoMem,
    sim: &mut Sim<'_, IoMem>,
    fault_queue: bool,
    levels: Levels,
) -> Boot {
    let before = db.free_count();
    let dir = db.alloc().unwrap() << 12;
    let cq = db.alloc().unwrap() << 12;
    let q = db.alloc_contiguous(4).unwrap() << 12;
    for pa in [dir, cq] {
        for i in 0..512 {
            mem.write(pa + i * 8, 0);
        }
    }
    let ddt = Ddt::new(mem, dir, levels, Format::Extended).unwrap();
    let cmdq = CmdQueue::new(cq, 256).unwrap();
    let quarantine = Iopt::new(mem, q, PtMode::Sv39x4).unwrap();
    sim.write64(regs::CQB, cmdq.cqb());
    sim.write32(regs::CQCSR, regs::QCSR_EN);
    if fault_queue {
        mem.prefill(FQ_PA >> 12, 1);
        sim.write64(regs::FQB, FaultQueue::new(FQ_PA, 128).unwrap().fqb());
        sim.write32(regs::FQCSR, regs::QCSR_EN);
    }
    sim.write64(regs::DDTP, Ddtp::directory(levels, dir).bits());
    Boot {
        ddt,
        cmdq,
        quarantine,
        frames_taken: before - db.free_count(),
    }
}

/// The simulator as the tests drive it: commands run only when the test says (the device is slow, or stalled).
pub fn sim_config() -> SimConfig {
    SimConfig {
        auto_process: false,
        ..SimConfig::qemu()
    }
}

/// The frames of the leaf table and the table above it that a live page needs (the treap and the page table agree on this: the
/// store's table count is exactly this over the live pages, because tables are freed as soon as they are empty).
pub fn tables_for(pages: impl Iterator<Item = u64>) -> u64 {
    let mut leaf = std::collections::BTreeSet::new();
    let mut mid = std::collections::BTreeSet::new();
    for p in pages {
        leaf.insert(p >> 9);
        mid.insert(p >> 18);
    }
    (leaf.len() + mid.len()) as u64
}

// ---------------------------------------------------------------------------------------------------------------------------
// the kernel's sequences, single-threaded

pub const FRAMES: usize = 4096;
pub const PAGE: u64 = 4096;

pub struct World<'m> {
    pub mem: &'m IoMem,
    pub sim: Sim<'m, IoMem>,
    pub store: IoDomains,
    pub mm: Mm<FakeMem>,
    pub vmos: Vmos,
    pub jobs: Jobs,
    pub counts: FrameCounts,
    pub fixed: usize,
}

/// How a kernel map ended.
pub enum MapStart {
    Wait(Mapped),
    RolledBack(Pending),
    Refused(IoError),
}

impl<'m> World<'m> {
    pub fn new(mem: &'m IoMem, fault_queue: bool) -> World<'m> {
        World::with_frames(mem, fault_queue, FRAMES, true)
    }

    /// A machine of `frames` frames, created in `mem` now (`prefill`) or as they are touched.
    pub fn with_frames(
        mem: &'m IoMem,
        fault_queue: bool,
        frames: usize,
        prefill: bool,
    ) -> World<'m> {
        World::with_levels(mem, fault_queue, frames, prefill, Levels::One)
    }

    /// `with_frames`, with a directory of `levels` levels.
    pub fn with_levels(
        mem: &'m IoMem,
        fault_queue: bool,
        frames: usize,
        prefill: bool,
        levels: Levels,
    ) -> World<'m> {
        let mut db = FrameDb::new(BASE_PFN, frames);
        if prefill {
            mem.prefill(BASE_PFN, frames as u64);
        }
        let mut sim = Sim::new(mem, sim_config());
        let b = boot_with(&mut db, mem, &mut sim, fault_queue, levels);
        World {
            mem,
            sim,
            store: IoDomains::new(b.ddt, b.cmdq, &b.quarantine, 0x5eed_1234_abcd_0042).unwrap(),
            mm: Mm {
                db,
                mem: FakeMem::default(),
            },
            vmos: Vmos::new(),
            jobs: Jobs::new(),
            counts: FrameCounts::default(),
            fixed: b.frames_taken,
        }
    }

    /// The kernel's `iodomain_create`: charge, create, (the caller waits).
    pub fn create(&mut self, did: u32, job: JobId) -> Result<(DomainId, Ticket), IoError> {
        charge_create(&mut self.jobs, job).map_err(|_| IoError::NoMemory)?;
        let World {
            mem,
            sim,
            store,
            mm,
            counts,
            jobs,
            ..
        } = self;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail: None,
        };
        let r = store.create(*mem, sim, &mut f, did, job);
        if r.is_err() {
            uncharge_create(jobs, job);
        }
        r
    }

    /// The kernel's `iodomain_map` up to the wait: the hold and frame list (VMO), the charge (JOBS), the record (no lock), the store.
    pub fn map(
        &mut self,
        d: DomainId,
        vmo: u64,
        first: u64,
        pages: u64,
        access: Access,
        fail: Option<u64>,
    ) -> MapStart {
        let Some(job) = self.store.job_of(d) else {
            return MapStart::Refused(IoError::BadState);
        };
        let mut pfns = Vec::new();
        self.vmos
            .retain_for_io(vmo, first, pages, &mut pfns)
            .unwrap();
        let charge = match charge_map(&mut self.jobs, job, pages) {
            Ok(c) => c,
            Err(_) => {
                self.vmos
                    .release(&mut self.mm, &mut self.jobs, vmo)
                    .unwrap();
                return MapStart::Refused(IoError::NoMemory);
            }
        };
        let rec = Record::new(vmo, first, pages, access).unwrap();
        assert_eq!((rec.vmo(), rec.pages()), (vmo, pages));
        let World {
            mem,
            sim,
            store,
            mm,
            counts,
            ..
        } = self;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail,
        };
        match store.map(*mem, sim, &mut f, d, rec, &pfns) {
            Ok(m) => {
                charge.settle(&mut self.jobs, m.tables);
                MapStart::Wait(m)
            }
            Err(MapFailed::Refused(e, rec)) => {
                charge.refund(&mut self.jobs);
                self.vmos
                    .release(&mut self.mm, &mut self.jobs, rec.vmo())
                    .unwrap();
                MapStart::Refused(e)
            }
            Err(MapFailed::RolledBack {
                pending, tables, ..
            }) => {
                charge.settle(&mut self.jobs, tables);
                MapStart::RolledBack(pending)
            }
            Err(e) => panic!("a map in one call meets no destruction: {e:?}"),
        }
    }

    pub fn unmap(&mut self, d: DomainId, iova: u64, pages: u64) -> Result<Pending, IoError> {
        self.store.unmap(self.mem, &mut self.sim, d, iova, pages)
    }

    pub fn destroy(&mut self, d: DomainId) -> Option<Destroying> {
        self.store.destroy(self.mem, &mut self.sim, d)
    }

    /// 6.1 step 4: drop the hold (VMO), free the tables and credit (FRAME, JOBS).
    pub fn settle(&mut self, r: Released) {
        let vmo = r.vmo();
        self.vmos
            .release(&mut self.mm, &mut self.jobs, vmo)
            .unwrap();
        self.vmos.drain_all(&mut self.mm, &mut self.jobs);
        let World {
            mem,
            mm,
            counts,
            jobs,
            ..
        } = self;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail: None,
        };
        r.settle(*mem, &mut f, jobs);
    }

    pub fn finish(&mut self, mut det: Detached) {
        while let Some(r) = det.next_record() {
            self.settle(r);
        }
        let World {
            mem,
            mm,
            counts,
            jobs,
            ..
        } = self;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail: None,
        };
        det.finish(*mem, &mut f, jobs);
    }

    pub fn close_vmo(&mut self, vmo: u64) {
        self.vmos
            .release(&mut self.mm, &mut self.jobs, vmo)
            .unwrap();
        self.vmos.drain_all(&mut self.mm, &mut self.jobs);
    }
}

/// A domain, its create's fence waited for.
pub fn ready(w: &mut World, did: u32, job: JobId) -> DomainId {
    let (d, t) = w.create(did, job).unwrap();
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, t), Fence::Done);
    d
}

/// A map, its fence waited for: the I/O address.
pub fn map_ok(w: &mut World, d: DomainId, vmo: u64, first: u64, pages: u64, access: Access) -> u64 {
    match w.map(d, vmo, first, pages, access, None) {
        MapStart::Wait(m) => {
            w.sim.process_commands();
            assert_eq!(w.store.poll(&mut w.sim, m.ticket), Fence::Done);
            m.iova
        }
        MapStart::RolledBack(_) => panic!("map rolled back"),
        MapStart::Refused(e) => panic!("map refused: {e:?}"),
    }
}

/// An unmap, waited for, completed and settled: the record that came back.
pub fn unmap_ok(w: &mut World, d: DomainId, iova: u64, pages: u64) -> (u64, u64) {
    let p = w.unmap(d, iova, pages).unwrap();
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Done);
    let rel = w
        .store
        .complete(&p)
        .unwrap()
        .expect("the record comes back");
    let got = (rel.vmo(), rel.pages());
    w.settle(rel);
    got
}

/// A destruction, waited for and finished: the records it returned.
pub fn destroy_ok(w: &mut World, d: DomainId) -> Vec<(u64, u64)> {
    let x = w.destroy(d).unwrap();
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, x.ticket()), Fence::Done);
    let mut det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    let mut got = vec![];
    while let Some(rel) = det.next_record() {
        got.push((rel.vmo(), rel.pages()));
        w.settle(rel);
    }
    w.finish(det);
    got
}

pub fn kernel_bytes(w: &World, job: JobId) -> u64 {
    w.jobs.usage(job).unwrap().get(Resource::KernelBytes)
}

/// Drain the simulated fault queue into the store, as the kernel's fault interrupt does: the outcomes.
pub fn drain_faults(w: &mut World, fq: &mut FaultQueue, now: u64) -> Vec<FaultOutcome> {
    use iommucore::Regs;
    let mut out = vec![];
    loop {
        let tail = w.sim.read32(iommucore::regs::FQT);
        let Some(rec) = fq.pop(w.mem, tail) else {
            break;
        };
        out.push(w.store.fault(rec, now, 1000));
    }
    let head = fq.head();
    w.sim.write32(iommucore::regs::FQH, head);
    out
}
