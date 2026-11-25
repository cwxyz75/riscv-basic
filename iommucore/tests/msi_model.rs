//! The MSI path (IOMMU2; IOMMU-SPEC 8, 13): random MSI page tables, interrupt files and device writes. The simulated IOMMU's
//! result (memory, a forwarded message, an MRIF update with or without a notice, or a fault with its cause in the fault queue)
//! against a reference written here from the IOMMU specification's "Process to translate addresses of MSIs" and the AIA
//! specification's MRIF rules, as QEMU 11.1.3 implements them (IOMMU0 items 5, 10, 11, 13). The reference also models the
//! MSI-entry cache that the simulator keeps: an entry changed without `IOTINVAL.GVMA` may still be served from the cache. The
//! kernel's side of the files (enable, disable, take, stray) runs interleaved with the device's writes.

mod common;

use common::{Rng, SEEDS, STEPS};
use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{Ddt, DeviceContext, Format, Levels};
use iommucore::faultq::{self, FaultQueue, TransactionType};
use iommucore::iopt::{Access, Iopt, PtMode};
use iommucore::msi::{self, Mrif, MsiPte, MsiTable, MsiWindow};
use iommucore::regs;
use iommucore::sim::{Delivery, FakeFrames, FakeMem, Sim, SimConfig, Via, WriteOutcome};
use iommucore::{PhysMem64, Regs};
use std::collections::BTreeMap;

const IO_BASE: u64 = 1 << 40;
const DID: u32 = 0x18;
const GSCID: u16 = 9;
const FILES: [u64; 4] = [0x8600_0000, 0x8600_0200, 0x8600_0400, 0x8601_0e00];

#[test]
fn pext_against_a_naive_loop_over_every_mask_of_a_byte() {
    for mask in 0u64..256 {
        for val in 0u64..256 {
            let mut want = 0;
            let mut out = 0;
            for bit in 0..8 {
                if mask >> bit & 1 != 0 {
                    want |= (val >> bit & 1) << out;
                    out += 1;
                }
            }
            assert_eq!(msi::pext(val, mask), want, "val {val:#x} mask {mask:#x}");
            // The same with garbage above the byte in the value and the mask's bits shifted up.
            assert_eq!(msi::pext((val << 12) | 0xabc, mask << 12), want);
        }
    }
}

/// The reference: what the IOMMU does with a device's 4-byte write.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    Memory(u64),
    Basic(u64, u32),
    Mrif(usize, u16, Option<(u64, u16)>),
    Fault(u16, u64),
}

/// What an MSI page-table entry is meant to be. The reference works from this, not from the encoded words, so an encoder that
/// writes the wrong bits is caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Invalid,
    Mrif {
        file: usize,
        notice: u64,
        id: u16,
    },
    Basic {
        page: u64,
    },
    /// Valid with the custom bit C set: treated as not valid (iommu.c:686).
    Custom,
    /// Basic with a reserved bit set, or a mode that is neither MRIF nor basic: misconfigured.
    Misconfigured,
}

struct Reference {
    /// The MSI table as written (index -> entry).
    table: BTreeMap<u64, Entry>,
    /// The entries the IOMMU may have cached (valid ones it has used, not invalidated since).
    cache: BTreeMap<u64, Entry>,
    /// Per file: pending and enable bits.
    pending: Vec<[u64; 32]>,
    enable: Vec<[u64; 32]>,
    /// DMA mappings: page -> (pa, writable).
    dma: BTreeMap<u64, (u64, bool)>,
}

impl Reference {
    fn write(&mut self, addr: u64, data: u32) -> Expect {
        let page = addr >> 12;
        // The window: (page ^ pattern) & !mask == 0, pattern = IO_BASE's page, mask = 0xff.
        if (page ^ (IO_BASE >> 12)) & !0xff != 0 {
            return match self.dma.get(&page) {
                Some((pa, true)) => Expect::Memory(pa + (addr & 0xfff)),
                _ => Expect::Fault(faultq::CAUSE_WRITE_GUEST_PAGE_FAULT, addr),
            };
        }
        let index = page & 0xff;
        let entry = match self.cache.get(&index) {
            Some(e) => *e,
            None => {
                let e = self.table.get(&index).copied().unwrap_or(Entry::Invalid);
                if matches!(e, Entry::Invalid | Entry::Custom) {
                    return Expect::Fault(faultq::CAUSE_MSI_INVALID, 0);
                }
                self.cache.insert(index, e);
                e
            }
        };
        match entry {
            Entry::Invalid | Entry::Custom => unreachable!(),
            Entry::Misconfigured => Expect::Fault(faultq::CAUSE_MSI_MISCONFIGURED, 0),
            Entry::Basic { page } => Expect::Basic(page, data),
            Entry::Mrif { file, notice, id } => {
                if data > 2047 || addr & 3 != 0 {
                    return Expect::Fault(faultq::CAUSE_MSI_MISCONFIGURED, 0);
                }
                let (w, bit) = ((data / 64) as usize, 1u64 << (data % 64));
                self.pending[file][w] |= bit;
                let notice = (self.enable[file][w] & bit != 0).then_some((notice, id));
                Expect::Mrif(file, data as u16, notice)
            }
        }
    }
}

/// A random entry: what it means, and its words (from the constructors where one exists, by hand for the malformed ones).
fn random_entry(rng: &mut Rng) -> (Entry, Result<MsiPte, [u64; 2]>) {
    let f = rng.below(4) as usize;
    let file = FILES[f];
    let notice = 0x2800_0000 + (rng.below(4) << 12);
    let id = if rng.chance(30) {
        1
    } else {
        1 + rng.below(2047) as u16
    };
    match rng.below(10) {
        0..=4 => (
            Entry::Mrif {
                file: f,
                notice,
                id,
            },
            Ok(MsiPte::mrif(file, notice, id).unwrap()),
        ),
        5 => (Entry::Invalid, Ok(MsiPte::invalid())),
        6 => (Entry::Basic { page: notice }, Ok(MsiPte::basic(notice))),
        // V, M = 1, the file's address, and C (bit 63).
        7 => (Entry::Custom, Err([(1 << 63) | ((file >> 9) << 7) | 3, 0])),
        // V, M = 3, a page, and one of the reserved bits 9:3.
        8 => (
            Entry::Misconfigured,
            Err([(notice >> 12 << 10) | 7 | (1 << (3 + rng.below(7))), 0]),
        ),
        // V and M = 0 or 2.
        _ => (
            Entry::Misconfigured,
            Err([(rng.below(2) << 2) | 1 | ((file >> 9) << 7), 0]),
        ),
    }
}

#[test]
fn msi_writes_follow_the_reference() {
    let mut seen = BTreeMap::new();
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 1 << 10);
        let mut d = Ddt::new(&mem, 0x8000_0000, Levels::One, Format::Extended).unwrap();
        let mut t = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
        let table = MsiTable::new(&mem, 0x8200_0000, MsiWindow::new(IO_BASE, 8).unwrap()).unwrap();
        let files: Vec<Mrif> = FILES.iter().map(|&p| Mrif::new(p).unwrap()).collect();
        for f in &files {
            f.reset(&mem);
        }
        let mut reference = Reference {
            table: BTreeMap::new(),
            cache: BTreeMap::new(),
            pending: vec![[0; 32]; 4],
            enable: vec![[0; 32]; 4],
            dma: BTreeMap::new(),
        };
        // Some DMA mappings just below and above the window, and inside it (the MSI check comes first: the page table is never
        // consulted for a write to the window).
        for k in 0..24u64 {
            let page = match k % 3 {
                0 => (IO_BASE >> 12) - 1 - k,
                1 => (IO_BASE >> 12) + 256 + k,
                _ => (IO_BASE >> 12) + k,
            };
            let writable = rng.chance(70);
            let pa = 0x3_0000_0000 + (k << 12);
            let access = if writable {
                Access::ReadWrite
            } else {
                Access::Read
            };
            t.map(&mem, &mut frames, page << 12, pa, access).unwrap();
            if !(IO_BASE >> 12..(IO_BASE >> 12) + 256).contains(&page) {
                reference.dma.insert(page, (pa, writable));
            }
        }
        d.set(
            &mem,
            &mut frames,
            DID,
            &DeviceContext::translate(t.context_root(u32::from(GSCID)).unwrap(), Some(&table)),
        )
        .unwrap();

        let mut sim = Sim::new(&mem, SimConfig::qemu());
        let mut cq = CmdQueue::new(0x8300_0000, 8).unwrap();
        let mut fq = FaultQueue::new(0x8300_1000, 128).unwrap();
        sim.write64(regs::CQB, cq.cqb());
        sim.write32(regs::CQCSR, regs::QCSR_EN);
        sim.write64(regs::FQB, fq.fqb());
        sim.write32(regs::FQCSR, regs::QCSR_EN);
        sim.write64(regs::DDTP, d.ddtp().bits());

        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(10) {
                0 | 1 => {
                    // Change an entry; usually with the invalidation and fence of IOMMU-SPEC 6.2.
                    let index = rng.below(16);
                    let (entry, words) = random_entry(&mut rng);
                    match words {
                        Ok(pte) => table.set(&mem, index, pte).unwrap(),
                        Err(raw) => {
                            // (Raw words: these are malformed on purpose, and no constructor makes them.)
                            mem.write(0x8200_0000 + index * 16, 0);
                            mem.write(0x8200_0000 + index * 16 + 8, raw[1]);
                            mem.write(0x8200_0000 + index * 16, raw[0]);
                        }
                    }
                    reference.table.insert(index, entry);
                    if rng.chance(85) {
                        let addr = IO_BASE + index * 4096;
                        cq.push(&mem, Command::iotinval_gvma(GSCID, Some(addr)))
                            .unwrap();
                        let tail = cq.push(&mem, Command::iofence_c(None)).unwrap();
                        sim.write32(regs::CQT, tail);
                        cq.completed(sim.read32(regs::CQH)).unwrap();
                        assert!(cq.is_done(cq.ticket()), "{ctx}");
                        reference.cache.remove(&index);
                    }
                }
                2 => {
                    // The kernel enables or disables an identity.
                    let f = rng.below(4) as usize;
                    let id = if rng.chance(50) {
                        1
                    } else {
                        rng.below(2048) as u16
                    };
                    let (w, bit) = ((id / 64) as usize, 1u64 << (id % 64));
                    if rng.chance(60) {
                        files[f].enable(&mem, id).unwrap();
                        reference.enable[f][w] |= bit;
                    } else {
                        files[f].disable(&mem, id).unwrap();
                        reference.enable[f][w] &= !bit;
                    }
                }
                3 => {
                    // The kernel takes a pending bit, or the strays.
                    let f = rng.below(4) as usize;
                    if rng.chance(50) {
                        let id = if rng.chance(50) {
                            1
                        } else {
                            rng.below(2048) as u16
                        };
                        let (w, bit) = ((id / 64) as usize, 1u64 << (id % 64));
                        let was = reference.pending[f][w] & bit != 0;
                        reference.pending[f][w] &= !bit;
                        assert_eq!(files[f].take_pending(&mem, id).unwrap(), was, "{ctx}");
                    } else {
                        let mut want = [0u64; 32];
                        for w in 0..32 {
                            want[w] = reference.pending[f][w] & !reference.enable[f][w];
                            reference.pending[f][w] &= reference.enable[f][w];
                        }
                        assert_eq!(files[f].stray(&mem), want, "{ctx}");
                    }
                }
                _ => {
                    // A device write.
                    let addr = match rng.below(6) {
                        0..=2 => IO_BASE + rng.below(16) * 4096,
                        3 => {
                            IO_BASE
                                + rng.below(256) * 4096
                                + (rng.below(1024) << 2)
                                + if rng.chance(10) { 2 } else { 0 }
                        }
                        4 => {
                            (*reference
                                .dma
                                .keys()
                                .nth(rng.below(reference.dma.len() as u64) as usize)
                                .unwrap()
                                << 12)
                                + 0x40
                        }
                        _ => rng.next() & ((1 << 41) - 1) & !3,
                    };
                    let data = match rng.below(5) {
                        0 | 1 => 1,
                        2 => rng.below(128) as u32,
                        3 => rng.below(2048) as u32,
                        _ => 2040 + rng.below(1000) as u32,
                    };
                    let want = reference.write(addr, data);
                    let deliveries = sim.deliveries().len();
                    let got = sim.write(DID, addr, data);
                    let kind = match &want {
                        Expect::Memory(pa) => {
                            assert_eq!(got, Ok(WriteOutcome::Memory(*pa)), "{ctx} {addr:#x}");
                            assert_eq!(
                                mem.read(pa & !7) >> ((pa & 4) * 8) & 0xffff_ffff,
                                u64::from(data)
                            );
                            "memory"
                        }
                        Expect::Basic(page, d) => {
                            assert_eq!(
                                got,
                                Ok(WriteOutcome::Basic {
                                    page: *page,
                                    data: *d
                                }),
                                "{ctx} {addr:#x}"
                            );
                            assert_eq!(
                                sim.deliveries()[deliveries..],
                                [Delivery {
                                    page: *page,
                                    identity: *d,
                                    via: Via::Basic
                                }]
                            );
                            "basic"
                        }
                        Expect::Mrif(_, _, notice) => {
                            assert_eq!(
                                got,
                                Ok(WriteOutcome::Mrif { notice: *notice }),
                                "{ctx} {addr:#x}"
                            );
                            let delivered: Vec<_> = notice
                                .iter()
                                .map(|(page, id)| Delivery {
                                    page: *page,
                                    identity: u32::from(*id),
                                    via: Via::Notice,
                                })
                                .collect();
                            assert_eq!(sim.deliveries()[deliveries..], delivered[..], "{ctx}");
                            if notice.is_some() {
                                "notice"
                            } else {
                                "pending only"
                            }
                        }
                        Expect::Fault(cause, iotval) => {
                            assert_eq!(got, Err(*cause), "{ctx} {addr:#x} data {data}");
                            assert_eq!(sim.deliveries().len(), deliveries, "{ctx}");
                            let rec = fq.pop(&mem, sim.read32(regs::FQT)).expect("a fault record");
                            sim.write32(regs::FQH, fq.head());
                            assert_eq!(
                                (rec.cause, rec.device_id, rec.iotval, rec.ttype),
                                (*cause, DID, *iotval, TransactionType::UntranslatedWrite),
                                "{ctx}"
                            );
                            match *cause {
                                faultq::CAUSE_MSI_INVALID => "invalid",
                                faultq::CAUSE_MSI_MISCONFIGURED => "misconfigured",
                                _ => "page fault",
                            }
                        }
                    };
                    *seen.entry(kind).or_insert(0u64) += 1;
                    assert_eq!(
                        fq.pop(&mem, sim.read32(regs::FQT)),
                        None,
                        "{ctx}: one record at most"
                    );
                }
            }
            // The files in memory are the reference's.
            for (f, file) in FILES.iter().enumerate() {
                for w in 0..32u64 {
                    assert_eq!(
                        mem.read(file + w * 16),
                        reference.pending[f][w as usize],
                        "{ctx} file {f} pending {w}"
                    );
                    assert_eq!(
                        mem.read(file + w * 16 + 8),
                        reference.enable[f][w as usize],
                        "{ctx} file {f} enable {w}"
                    );
                }
            }
        }
    }
    // Every outcome was exercised.
    for kind in [
        "memory",
        "basic",
        "notice",
        "pending only",
        "invalid",
        "misconfigured",
        "page fault",
    ] {
        assert!(
            seen.get(kind).copied().unwrap_or(0) > 20,
            "{kind}: {seen:?}"
        );
    }
}

#[test]
fn an_unflushed_msi_entry_is_served_from_the_cache() {
    // IO10's host proof needs a cache the kernel must flush (IOMMU0 item 12, IOMMU-SPEC 15.1 L4): after a revoke without
    // IOTINVAL.GVMA the simulated IOMMU still delivers through the old entry; after the invalidation it faults.
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 16);
    let mut d = Ddt::new(&mem, 0x8000_0000, Levels::One, Format::Extended).unwrap();
    let t = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    let table = MsiTable::new(&mem, 0x8200_0000, MsiWindow::new(IO_BASE, 8).unwrap()).unwrap();
    d.set(
        &mem,
        &mut frames,
        DID,
        &DeviceContext::translate(t.context_root(3).unwrap(), Some(&table)),
    )
    .unwrap();
    let file = Mrif::new(FILES[0]).unwrap();
    file.reset(&mem);
    file.enable(&mem, 1).unwrap();
    table
        .set(&mem, 4, MsiPte::mrif(FILES[0], 0x2800_0000, 77).unwrap())
        .unwrap();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    let mut cq = CmdQueue::new(0x8300_0000, 8).unwrap();
    sim.write64(regs::CQB, cq.cqb());
    sim.write32(regs::CQCSR, regs::QCSR_EN);
    sim.write64(regs::DDTP, d.ddtp().bits());
    let vector4 = IO_BASE + 4 * 4096;
    assert_eq!(
        sim.write(DID, vector4, 1),
        Ok(WriteOutcome::Mrif {
            notice: Some((0x2800_0000, 77))
        })
    );
    table.clear(&mem, 4).unwrap();
    assert!(file.take_pending(&mem, 1).unwrap());
    // Stale: still delivered.
    assert_eq!(
        sim.write(DID, vector4, 1),
        Ok(WriteOutcome::Mrif {
            notice: Some((0x2800_0000, 77))
        })
    );
    // The wrong invalidations do not flush it: another soft context, another page, and IOTINVAL.VMA.
    for cmd in [
        Command::iotinval_gvma(4, Some(vector4)),
        Command::iotinval_gvma(3, Some(vector4 + 4096)),
        Command::iotinval_vma(None, None),
        Command::iodir_inval_ddt(Some(DID)),
    ] {
        sim.write32(regs::CQT, cq.push(&mem, cmd).unwrap());
        assert!(sim.write(DID, vector4, 1).is_ok(), "{cmd:x?}");
    }
    sim.write32(
        regs::CQT,
        cq.push(&mem, Command::iotinval_gvma(3, Some(vector4)))
            .unwrap(),
    );
    assert_eq!(sim.write(DID, vector4, 1), Err(faultq::CAUSE_MSI_INVALID));
    assert_eq!(sim.cached().2, 0);
    // A notice target rewritten with one store (a move, IOMMU-SPEC 8.5) is seen after the invalidation.
    table
        .set(&mem, 4, MsiPte::mrif(FILES[0], 0x2800_0000, 77).unwrap())
        .unwrap();
    assert!(sim.write(DID, vector4, 1).is_ok());
    let before = mem.read(0x8200_0000 + 4 * 16);
    table
        .set(&mem, 4, MsiPte::mrif(FILES[0], 0x2800_3000, 1500).unwrap())
        .unwrap();
    assert_eq!(
        mem.read(0x8200_0000 + 4 * 16),
        before,
        "only the notice doubleword changed"
    );
    sim.write32(
        regs::CQT,
        cq.push(&mem, Command::iotinval_gvma(3, Some(vector4)))
            .unwrap(),
    );
    assert_eq!(
        sim.write(DID, vector4, 1),
        Ok(WriteOutcome::Mrif {
            notice: Some((0x2800_3000, 1500))
        })
    );
}

#[test]
fn a_quarantined_device_cannot_interrupt() {
    // IOMMU-SPEC 3.1, 8.5: the deny context has no MSI page table, so a write to the window is an ordinary (suppressed) fault.
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 16);
    let mut d = Ddt::new(&mem, 0x8000_0000, Levels::One, Format::Extended).unwrap();
    let empty = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    d.set(
        &mem,
        &mut frames,
        DID,
        &DeviceContext::deny(empty.context_root(0).unwrap()),
    )
    .unwrap();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    sim.write64(regs::DDTP, d.ddtp().bits());
    assert_eq!(
        sim.write(DID, IO_BASE, 1),
        Err(faultq::CAUSE_WRITE_GUEST_PAGE_FAULT)
    );
    assert_eq!(sim.faults_reported(), 0);
    assert_eq!(sim.faults_suppressed(), 1);
    assert!(sim.deliveries().is_empty());
}
