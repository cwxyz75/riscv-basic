//! The simulated IOMMU against the behaviour of QEMU v11.1.3's model (`hw/riscv/riscv-iommu.c`) and the RISC-V IOMMU
//! specification, one register, command and check at a time (IOMMU2). The simulator is the oracle of the model tests here and of
//! IOMMU5's and IOMMU7's later, so what it does beyond what those tests reach is pinned here: queue control registers (QEMU
//! `riscv_iommu_process_cq_control`, `_fq_control`), command decoding and the reach of each invalidation (`_process_cq_tail`),
//! fault reporting, `DTF` and overflow (`riscv_iommu_report_fault`), the device-context checks (`riscv_iommu_validate_device_ctx`),
//! the directory walk (`riscv_iommu_ctx_fetch`), `fctl`, the debug interface, and the test memory and frame allocator themselves.

use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{Ddt, DeviceContext, Format, Levels};
use iommucore::faultq::{self, FaultQueue, FaultRecord, TransactionType};
use iommucore::iopt::{Access, Iopt, PtMode};
use iommucore::regs::{self, Ddtp};
use iommucore::sim::{FakeFrames, FakeMem, Handover, Sim, SimConfig, POISON, QEMU_CAPS};
use iommucore::{PhysMem64, Regs};

const CQ: u64 = 0x8010_0000;
const FQ: u64 = 0x8040_0000;
const DIR: u64 = 0x8000_0000;
const G_ROOT: u64 = 0x8100_0000; // 16 KiB
const S_ROOT: u64 = 0x8200_0000;
const MSI_BASE: u64 = 1 << 40;

const EN: u32 = regs::QCSR_EN;
const IE: u32 = regs::QCSR_IE;
const ON: u32 = regs::QCSR_ON;

fn caps_without(bits: u64) -> SimConfig {
    SimConfig {
        caps: QEMU_CAPS & !bits,
        ..SimConfig::qemu()
    }
}

/// A machine with a one-level extended directory: devices 1 and 2 translate through the second-stage table (soft ids 1 and 2),
/// devices 3 and 4 through the first-stage table (soft ids 5 and 6). Pages 0x1000 and 0x2000 are mapped read-write in both
/// tables, 0x3000 read-only.
struct Machine {
    mem: FakeMem,
}

impl Machine {
    fn new() -> Machine {
        let mem = FakeMem::new();
        {
            let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 64);
            let mut d = Ddt::new(&mem, DIR, Levels::One, Format::Extended).unwrap();
            let mut g = Iopt::new(&mem, G_ROOT, PtMode::Sv39x4).unwrap();
            let mut s = Iopt::new(&mem, S_ROOT, PtMode::Sv39).unwrap();
            for t in [&mut g, &mut s] {
                t.map(&mem, &mut frames, 0x1000, 0x2000_1000, Access::ReadWrite)
                    .unwrap();
                t.map(&mem, &mut frames, 0x2000, 0x2000_2000, Access::ReadWrite)
                    .unwrap();
                t.map(&mem, &mut frames, 0x3000, 0x2000_3000, Access::Read)
                    .unwrap();
                // A page whose address lies in the MSI window (1 << 40), for the MSI check after translation.
                t.map(
                    &mem,
                    &mut frames,
                    0x7000,
                    MSI_BASE + 0x3000,
                    Access::ReadWrite,
                )
                .unwrap();
            }
            // The top half of the first stage's space (an address sign-extended from bit 38).
            s.map(
                &mem,
                &mut frames,
                0xffff_ffc0_0000_1000,
                0x2000_4000,
                Access::Read,
            )
            .unwrap();
            for (did, t, id) in [(1, &g, 1), (2, &g, 2), (3, &s, 5), (4, &s, 6)] {
                d.set(
                    &mem,
                    &mut frames,
                    did,
                    &DeviceContext::translate(t.context_root(id).unwrap(), None),
                )
                .unwrap();
            }
        }
        Machine { mem }
    }
    /// Device `did`'s context, word `w`.
    fn ctx_word(&self, did: u64, w: u64) -> u64 {
        DIR + did * 64 + w * 8
    }
    fn sim(&self, cfg: SimConfig) -> Sim<'_, FakeMem> {
        let mut sim = Sim::new(&self.mem, cfg);
        sim.write64(regs::CQB, CmdQueue::new(CQ, 256).unwrap().cqb());
        sim.write32(regs::CQCSR, EN | IE);
        sim.write64(regs::FQB, FaultQueue::new(FQ, 4).unwrap().fqb());
        sim.write32(regs::FQCSR, EN | IE);
        sim.write64(regs::DDTP, Ddtp::directory(Levels::One, DIR).bits());
        sim
    }
}

/// The address of the leaf for `iova` in the three-level table at `root` (`top` index bits: 11 for Sv39x4, 9 for Sv39).
fn leaf_addr(mem: &FakeMem, root: u64, top: u32, iova: u64) -> u64 {
    let l2 = root + ((iova >> 30) & ((1 << top) - 1)) * 8;
    let l1 = ((mem.read(l2) >> 10) << 12) + ((iova >> 21) & 0x1ff) * 8;
    ((mem.read(l1) >> 10) << 12) + ((iova >> 12) & 0x1ff) * 8
}

/// Push `cmd` at the simulator's tail and run it: whether it was legal.
fn run(sim: &mut Sim<'_, FakeMem>, mem: &FakeMem, cmd: Command) -> bool {
    let t = sim.read32(regs::CQT);
    mem.write(CQ + u64::from(t) * 16, cmd.dword0);
    mem.write(CQ + u64::from(t) * 16 + 8, cmd.dword1);
    sim.write32(regs::CQT, t + 1);
    let ok = sim.read32(regs::CQCSR) & regs::CQCSR_CMD_ILL == 0;
    assert_eq!(sim.read32(regs::CQH), if ok { t + 1 } else { t });
    ok
}

#[test]
fn fake_memory_and_frames() {
    let mem = FakeMem::new();
    assert!(!mem.has_frame(0x5000));
    mem.write32(0x5004, 0xaabb_ccdd);
    assert!(mem.has_frame(0x5000) && mem.has_frame(0x5ff8));
    assert!(!mem.has_frame(0x6000) && !mem.has_frame(0x4ff8));
    assert_eq!(mem.read(0x5000), 0xaabb_ccdd_0000_0000);
    mem.write32(0x5000, 0x1122_3344);
    assert_eq!(mem.read(0x5000), 0xaabb_ccdd_1122_3344);
    assert_eq!(
        (mem.read32(0x5000), mem.read32(0x5004)),
        (0x1122_3344, 0xaabb_ccdd)
    );
    mem.write32(0x5004, 0);
    assert_eq!(mem.read(0x5000), 0x1122_3344);
    assert_eq!(mem.fetch_or(0x5000, 0xf0), 0x1122_3344);
    assert_eq!(mem.fetch_and(0x5000, 0xff), 0x1122_33f4);
    assert_eq!(mem.read(0x5000), 0xf4);

    let mut f = FakeFrames::new(&mem, 0x1_0000_0000, 2);
    use iommucore::TableFrames;
    let a = f.alloc().unwrap();
    let b = f.alloc().unwrap();
    assert_eq!((a, b), (0x1_0000_0000, 0x1_0000_1000));
    assert_eq!(f.alloc(), None);
    mem.write(b + 8, 5);
    f.free(b);
    assert_eq!(mem.read(b + 8), POISON);
    assert!(f.is_live(a) && !f.is_live(b));
    assert_eq!(f.live(), 1);
    assert_eq!(f.alloc(), Some(b));
    assert_eq!(mem.read(b + 8), 0, "a reused frame is zeroed");
    f.free(a);
    f.budget = Some(0);
    assert_eq!(f.alloc(), None);
    f.budget = Some(1);
    assert_eq!(f.alloc(), Some(a));
    assert_eq!(f.alloc(), None);
    assert_eq!((f.allocs, f.frees), (4, 2));
    // A budget counts down with each frame given, with frames to spare.
    let mut f = FakeFrames::new(&mem, 0x2_0000_0000, 8);
    f.budget = Some(2);
    assert!(f.alloc().is_some() && f.alloc().is_some());
    assert_eq!(f.alloc(), None);
    assert_eq!(f.budget, Some(0));
}

#[test]
fn queue_control_registers() {
    let mem = FakeMem::new();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    // cqb keeps PPN and LOG2SZ-1 only, and is read-only while the queue is on.
    sim.write64(regs::CQB, u64::MAX);
    assert_eq!(sim.read64(regs::CQB), 0x003f_ffff_ffff_fc1f);
    sim.write64(regs::CQB, 0x2004_0007);
    sim.write32(regs::CQCSR, EN | IE);
    assert_eq!(sim.read32(regs::CQCSR), EN | IE | ON);
    sim.write64(regs::CQB, 0);
    assert_eq!(sim.read64(regs::CQB), 0x2004_0007);
    assert_eq!((sim.read32(regs::CQH), sim.read32(regs::CQT)), (0, 0));
    // An illegal command stops the queue at it, sets cmd_ill and, with IE, the interrupt.
    assert!(!run(
        &mut sim,
        &mem,
        Command {
            dword0: 0x7f,
            dword1: 0
        }
    ));
    assert_eq!(sim.read32(regs::IPSR), regs::IPSR_CIP);
    sim.write32(regs::IPSR, regs::IPSR_FIP);
    assert_eq!(
        sim.read32(regs::IPSR),
        regs::IPSR_CIP,
        "ipsr is write-1-to-clear"
    );
    sim.write32(regs::IPSR, regs::IPSR_CIP);
    assert_eq!(sim.read32(regs::IPSR), 0);
    // A write without the bit keeps cmd_ill; a write of 1 clears it, and the next tail write runs again.
    sim.write32(regs::CQCSR, EN | IE);
    assert_eq!(sim.read32(regs::CQCSR), EN | IE | ON | regs::CQCSR_CMD_ILL);
    assert!(sim.executed().is_empty());
    mem.write(CQ, Command::iofence_c(None).unwrap().dword0);
    sim.write32(regs::CQCSR, EN | IE | regs::CQCSR_CMD_ILL);
    assert_eq!(sim.read32(regs::CQCSR), EN | IE | ON);
    sim.write32(regs::CQT, 1);
    assert_eq!(sim.read32(regs::CQH), 1);
    assert_eq!(sim.executed(), [Command::iofence_c(None).unwrap()]);
    // The 64-bit view of two 32-bit registers.
    assert_eq!(sim.read64(regs::CQH), 1 | (1 << 32));
    assert_eq!(sim.read64(regs::FCTL), u64::from(sim.read32(regs::FCTL)));
    // A second illegal command while the first's interrupt is still pending leaves it pending.
    assert!(!run(
        &mut sim,
        &mem,
        Command {
            dword0: 0x7f,
            dword1: 0
        }
    ));
    assert_eq!(sim.read32(regs::IPSR), regs::IPSR_CIP);
    sim.write32(regs::CQCSR, EN | IE | regs::CQCSR_CMD_ILL);
    mem.write(CQ + 16, 0x7f);
    sim.write32(regs::CQT, 2);
    assert_eq!(sim.read32(regs::IPSR), regs::IPSR_CIP);
    sim.write32(regs::IPSR, regs::IPSR_CIP);
    mem.write(CQ + 16, Command::iofence_c(None).unwrap().dword0);
    sim.write32(regs::CQCSR, EN | IE | regs::CQCSR_CMD_ILL);
    sim.write32(regs::CQT, 2);
    assert_eq!(sim.read32(regs::CQH), 2);
    // Without IE an illegal command raises no interrupt.
    sim.write32(regs::CQCSR, EN);
    assert!(!run(
        &mut sim,
        &mem,
        Command {
            dword0: 0x7f,
            dword1: 0
        }
    ));
    assert_eq!(sim.read32(regs::IPSR), 0);
    // Off, and on again: the error bits are cleared by the enable (QEMU `riscv_iommu_process_cq_control`).
    sim.write32(regs::CQCSR, 0);
    assert_eq!(sim.read32(regs::CQCSR), regs::CQCSR_CMD_ILL);
    sim.write32(regs::CQCSR, EN);
    assert_eq!(sim.read32(regs::CQCSR), EN | ON);
    // Off: on clears; a write of 0 while off leaves it off; on again: head and tail start at 0.
    sim.write32(regs::CQCSR, 0);
    assert_eq!(sim.read32(regs::CQCSR), 0);
    sim.write32(regs::CQCSR, regs::QCSR_IE);
    assert_eq!(sim.read32(regs::CQCSR), regs::QCSR_IE);
    sim.write32(regs::CQCSR, 0);
    assert_eq!(sim.read32(regs::CQCSR), 0);
    sim.write32(regs::CQCSR, EN);
    assert_eq!(sim.read32(regs::CQCSR), EN | ON);
    assert_eq!(sim.read64(regs::CQH), 0);

    // The fault queue the same way (its errors are tested with the faults).
    sim.write64(regs::FQB, u64::MAX);
    assert_eq!(sim.read64(regs::FQB), 0x003f_ffff_ffff_fc1f);
    sim.write64(regs::FQB, 0x2010_0001);
    sim.write32(regs::FQCSR, EN);
    assert_eq!(sim.read32(regs::FQCSR), EN | ON);
    sim.write64(regs::FQB, 0);
    assert_eq!(sim.read64(regs::FQB), 0x2010_0001);
    assert_eq!(
        sim.read64(regs::CQCSR),
        u64::from(EN | ON) | (u64::from(EN | ON) << 32)
    );
    sim.write32(regs::FQH, 0xffff);
    assert_eq!(sim.read32(regs::FQH), 3, "fqh is masked to the queue");
    sim.write32(regs::FQCSR, 0);
    assert_eq!(sim.read32(regs::FQCSR), 0);
    sim.write32(regs::FQCSR, regs::QCSR_IE);
    assert_eq!(
        sim.read32(regs::FQCSR),
        regs::QCSR_IE,
        "a write without EN leaves it off"
    );
}

#[test]
fn handed_over_queues_are_on() {
    let mem = FakeMem::new();
    let sim = &mut Sim::handed_over(
        &mem,
        SimConfig::qemu(),
        Handover {
            ddtp: (1 << 63) | (0x20_0000 << 10) | (1 << 4) | 1,
            cq: Some(0x2004_0407),
            fq: Some(0x2004_0806),
        },
    );
    assert_eq!(sim.read64(regs::DDTP), (0x20_0000 << 10) | 1);
    assert_eq!(sim.read32(regs::CQCSR), EN | ON);
    assert_eq!(sim.read32(regs::FQCSR), EN | ON);
    assert_eq!(
        (sim.read64(regs::CQB), sim.read64(regs::FQB)),
        (0x2004_0407, 0x2004_0806)
    );
    assert!(sim.ddtp_writes().is_empty());
}

#[test]
fn commands_are_decoded_as_qemu_does() {
    let m = Machine::new();
    let legal = |cmd: Command| {
        let mut sim = m.sim(SimConfig::qemu());
        let ok = run(&mut sim, &m.mem, cmd);
        assert_eq!(sim.executed().len(), usize::from(ok));
        ok
    };
    let c = |dword0| Command { dword0, dword1: 0 };
    assert!(legal(Command::iofence_c(None).unwrap()));
    assert!(!legal(c(0x3002 | 1 << 14)), "IOFENCE.C reserved bit");
    assert!(!legal(c(0x3002 | 1 << 31)), "IOFENCE.C reserved bit");
    assert!(legal(Command::iotinval_gvma(1, None)));
    assert!(
        !legal(c(Command::iotinval_gvma(1, None).dword0 | 1 << 32)),
        "GVMA with PSCV"
    );
    assert!(legal(Command::iotinval_vma(Some(5), None)));
    assert!(legal(Command::iodir_inval_ddt(Some(1))));
    assert!(legal(c(3 | 1 << 7 | 1 << 33)), "IODIR.INVAL_PDT with DV");
    assert!(!legal(c(3 | 1 << 7)), "IODIR.INVAL_PDT without DV");
    assert!(!legal(c(3 | 2 << 7)));
    assert!(!legal(c(1 | 2 << 7)));
    assert!(!legal(c(2 | 1 << 7)));
    assert!(!legal(c(4)));
    assert!(!legal(c(0)));

    // IOFENCE.C's completion is a 4-byte write: the other half of the doubleword is kept.
    let mut sim = m.sim(SimConfig::qemu());
    m.mem.write(0x8030_0000, 0x1111_1111_2222_2222);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iofence_c(Some((0x8030_0004, 0x55))).unwrap()
    ));
    assert_eq!(m.mem.read(0x8030_0000), 0x0000_0055_2222_2222);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iofence_c(Some((0x8030_0000, 0x66))).unwrap()
    ));
    assert_eq!(m.mem.read(0x8030_0000), 0x0000_0055_0000_0066);
    // Without a completion nothing is written.
    assert!(run(&mut sim, &m.mem, Command::iofence_c(None).unwrap()));
    assert_eq!(m.mem.read(0), 0);
    assert_eq!(m.mem.read(0x8030_0000), 0x0000_0055_0000_0066);
}

#[test]
fn each_invalidation_drops_exactly_what_it_names() {
    let m = Machine::new();
    let mut sim = m.sim(SimConfig::qemu());
    let fill = |sim: &mut Sim<'_, FakeMem>| {
        for did in 1..=4 {
            for page in [0x1000, 0x2000] {
                sim.read(did, page + 8).unwrap();
            }
        }
        assert_eq!(sim.cached().0, 8);
        assert_eq!(sim.cached().1, 4);
    };
    let tlb = |sim: &Sim<'_, FakeMem>| sim.cached().0;
    fill(&mut sim);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_gvma(1, Some(0x1000))
    ));
    assert_eq!(tlb(&sim), 7);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_gvma(2, Some(0x5000))
    ));
    assert_eq!(tlb(&sim), 7);
    assert!(run(&mut sim, &m.mem, Command::iotinval_gvma(1, None)));
    assert_eq!(tlb(&sim), 6);
    // GVMA without GV: every second-stage translation, and no first-stage one.
    assert!(run(
        &mut sim,
        &m.mem,
        Command {
            dword0: 1 | 1 << 7,
            dword1: 0
        }
    ));
    assert_eq!(tlb(&sim), 4);
    // VMA reaches only first-stage translations.
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_vma(Some(5), Some(0x1000))
    ));
    assert_eq!(tlb(&sim), 3);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_vma(Some(6), Some(0x7000))
    ));
    assert_eq!(tlb(&sim), 3);
    assert!(run(&mut sim, &m.mem, Command::iotinval_vma(Some(5), None)));
    assert_eq!(tlb(&sim), 2);
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_vma(None, Some(0x2000))
    ));
    assert_eq!(tlb(&sim), 1);
    fill(&mut sim);
    assert!(run(&mut sim, &m.mem, Command::iotinval_vma(None, None)));
    assert_eq!(tlb(&sim), 4);
    // VMA with GV names a nested context: nothing modelled is dropped.
    fill(&mut sim);
    assert!(run(
        &mut sim,
        &m.mem,
        Command {
            dword0: 1 | 1 << 33,
            dword1: 0
        }
    ));
    assert_eq!(tlb(&sim), 8);
    // Each soft id reaches only its own translations: with the leaves gone, the device whose entries were dropped faults and
    // the other is still served from its cached ones.
    let gl = leaf_addr(&m.mem, G_ROOT, 11, 0x1000);
    let sl = leaf_addr(&m.mem, S_ROOT, 9, 0x1000);
    let (g_leaf, s_leaf) = (m.mem.read(gl), m.mem.read(sl));
    m.mem.write(gl, 0);
    m.mem.write(sl, 0);
    assert!(run(&mut sim, &m.mem, Command::iotinval_gvma(1, None)));
    assert!(run(&mut sim, &m.mem, Command::iotinval_vma(Some(5), None)));
    assert!(sim.read(1, 0x1000).is_err() && sim.read(3, 0x1000).is_err());
    assert_eq!(
        (sim.read(2, 0x1000), sim.read(4, 0x1000)),
        (Ok(0x2000_1000), Ok(0x2000_1000))
    );
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_gvma(2, Some(0x1000))
    ));
    assert!(run(
        &mut sim,
        &m.mem,
        Command::iotinval_vma(Some(6), Some(0x1000))
    ));
    assert!(sim.read(2, 0x1000).is_err() && sim.read(4, 0x1000).is_err());
    assert_eq!(
        (sim.read(2, 0x2000), sim.read(4, 0x2000)),
        (Ok(0x2000_2000), Ok(0x2000_2000))
    );
    m.mem.write(gl, g_leaf);
    m.mem.write(sl, s_leaf);
    fill(&mut sim);
    // IODIR: one context, or all; translations stay (QEMU keeps them until IOTINVAL).
    assert!(run(&mut sim, &m.mem, Command::iodir_inval_ddt(Some(3))));
    assert_eq!(sim.cached().1, 3);
    assert!(run(&mut sim, &m.mem, Command::iodir_inval_ddt(None)));
    assert_eq!(sim.cached(), (8, 0, 0));
    // A cached translation is used without the table, and a write needs a cached writable one.
    let ro = leaf_addr(&m.mem, G_ROOT, 11, 0x3000);
    assert_eq!(m.mem.read(ro) >> 10, 0x2000_3000 >> 12);
    sim.read(1, 0x3000).unwrap();
    m.mem.write(ro, 0);
    assert_eq!(
        sim.read(1, 0x3010),
        Ok(0x2000_3010),
        "served from the cache"
    );
    assert!(
        sim.write(1, 0x3010, 1).is_err(),
        "a read-only cached entry does not serve a write"
    );
    // A writable leaf the crate wrote (D set): an entry cached by a read serves a later write.
    let rw = leaf_addr(&m.mem, G_ROOT, 11, 0x2000);
    sim.read(1, 0x2000).unwrap();
    m.mem.write(rw, 0);
    assert_eq!(
        sim.write(1, 0x2010, 1),
        Ok(iommucore::sim::WriteOutcome::Memory(0x2000_2010))
    );
}

#[test]
fn faults_are_reported_as_qemu_does() {
    let m = Machine::new();
    // Device 2's context suppresses faults (DTF); device 7 has none.
    m.mem
        .write(m.ctx_word(2, 0), m.mem.read(m.ctx_word(2, 0)) | 1 << 4);
    let mut sim = m.sim(SimConfig::qemu());
    let record = |slot: u64| {
        let a = FQ + slot * 32;
        FaultRecord::decode([
            m.mem.read(a),
            m.mem.read(a + 8),
            m.mem.read(a + 16),
            m.mem.read(a + 24),
        ])
    };
    assert_eq!(
        sim.read(2, 0x9000),
        Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT)
    );
    assert_eq!((sim.faults_reported(), sim.faults_suppressed()), (0, 1));
    assert_eq!(sim.read32(regs::FQT), 0);
    assert_eq!(sim.read32(regs::IPSR), 0);

    assert_eq!(
        sim.read(1, 0x9008),
        Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT)
    );
    let r = record(0);
    assert_eq!(
        (
            r.cause,
            r.device_id,
            r.ttype,
            r.iotval,
            r.iotval2,
            r.pv,
            r.privileged
        ),
        (
            21,
            1,
            TransactionType::UntranslatedRead,
            0x9008,
            0x9008,
            false,
            false
        )
    );
    assert_eq!(sim.read32(regs::IPSR), regs::IPSR_FIP);
    assert_eq!(
        sim.write(1, 0x3000, 9),
        Err(faultq::CAUSE_WRITE_GUEST_PAGE_FAULT)
    );
    let r = record(1);
    assert_eq!(
        (r.cause, r.ttype, r.iotval),
        (23, TransactionType::UntranslatedWrite, 0x3000)
    );
    // A first-stage fault: iotval2 is 0.
    assert_eq!(sim.write(3, 0x3000, 9), Err(faultq::CAUSE_WRITE_PAGE_FAULT));
    let r = record(2);
    assert_eq!(
        (r.cause, r.device_id, r.iotval, r.iotval2),
        (15, 3, 0x3000, 0)
    );
    assert_eq!(sim.read32(regs::FQT), 3);
    // The queue is full (4 entries, head 0): the next fault sets the overflow bit and is lost; so is the one after.
    assert_eq!(sim.read(7, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
    assert_eq!(sim.read(7, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
    assert_eq!(sim.read32(regs::FQT), 3);
    assert_eq!(sim.read32(regs::FQCSR), EN | IE | ON | regs::FQCSR_OF);
    assert_eq!(sim.faults_reported(), 5);
    // Disabled and enabled again, the overflow bit is cleared (QEMU `riscv_iommu_process_fq_control`) and the ring is empty.
    sim.write32(regs::FQCSR, 0);
    assert_eq!(sim.read32(regs::FQCSR), regs::FQCSR_OF);
    sim.write32(regs::FQCSR, EN | IE);
    assert_eq!(sim.read32(regs::FQCSR), EN | IE | ON);
    assert_eq!((sim.read32(regs::FQH), sim.read32(regs::FQT)), (0, 0));
    for slot in 0..3 {
        assert_eq!(sim.read(7, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
        assert_eq!(record(slot).device_id, 7);
    }
    assert_eq!(sim.read(7, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
    assert_eq!(sim.read32(regs::FQCSR), EN | IE | ON | regs::FQCSR_OF);
    // Drained but not acknowledged: still nothing is written while the overflow bit is set.
    let fqh = sim.read32(regs::FQH);
    let t = sim.read32(regs::FQT);
    sim.write32(regs::FQH, t);
    assert_eq!(sim.read(7, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
    assert_eq!(sim.read32(regs::FQT), t);
    sim.write32(regs::FQH, fqh);
    // Drained and acknowledged, the queue takes records again.
    sim.write32(regs::FQH, 3);
    let ack = regs::Fqcsr(sim.read32(regs::FQCSR)).acknowledge();
    sim.write32(regs::FQCSR, ack);
    assert_eq!(sim.read32(regs::FQCSR), EN | IE | ON);
    assert_eq!(sim.read(7, 0x4000), Err(faultq::CAUSE_DDT_INVALID));
    let r = record(3);
    assert_eq!(
        (r.cause, r.device_id, r.iotval, r.iotval2),
        (258, 7, 0x4000, 0)
    );
    // Without IE no interrupt; with the queue off, nothing is written (but the fault is counted).
    sim.write32(regs::IPSR, regs::IPSR_FIP);
    sim.write32(regs::FQCSR, EN);
    let _ = sim.read(7, 0x5000);
    assert_eq!(sim.read32(regs::IPSR), 0);
    assert_eq!(sim.read32(regs::FQT), 1);
    sim.write32(regs::FQCSR, 0);
    let before = m.mem.read(FQ + 32);
    let _ = sim.read(7, 0x6000);
    assert_eq!(m.mem.read(FQ + 32), before);
    assert_eq!(sim.faults_reported(), 13);
}

/// Device 1's read of page 0x1000 after `change` to the machine, on a fresh simulator with `cfg`.
fn outcome(cfg: SimConfig, change: impl FnOnce(&Machine)) -> Result<u64, u16> {
    let m = Machine::new();
    change(&m);
    let mut sim = m.sim(cfg);
    sim.read(1, 0x1000)
}

#[test]
fn device_context_checks() {
    const MISCONF: Result<u64, u16> = Err(faultq::CAUSE_DDT_MISCONFIGURED);
    let q = SimConfig::qemu();
    assert_eq!(outcome(q, |_| {}), Ok(0x2000_1000));
    // tc: V (0) and DTF (4) are the only bits this system's contexts may set: ATS, PRI, T2GPA, PDTV, PRPR, GADE and SADE
    // (without AMO_HWAD), DPE, SBE, SXL and every reserved bit are refused.
    // (Bits 31:24 are for custom use: QEMU ignores them, bits.h:312.)
    for bit in (24..32).chain(0..1) {
        let r = outcome(q, |m| m.mem.write(m.ctx_word(1, 0), 1 | 1 << bit));
        assert_eq!(r, Ok(0x2000_1000), "tc bit {bit}");
    }
    for bit in (1..64).filter(|&b| b != 4 && !(24..32).contains(&b)) {
        let r = outcome(q, |m| m.mem.write(m.ctx_word(1, 0), 1 | 1 << bit));
        assert_eq!(r, MISCONF, "tc bit {bit}");
    }
    assert_eq!(
        outcome(q, |m| m.mem.write(m.ctx_word(1, 0), 0)),
        Err(faultq::CAUSE_DDT_INVALID)
    );
    // With AMO_HWAD, GADE is allowed and the walk sets A and D itself.
    let hwad = SimConfig {
        caps: QEMU_CAPS | 1 << 24,
        ..q
    };
    let m = Machine::new();
    m.mem.write(m.ctx_word(1, 0), 1 | 1 << 7);
    let leaf = leaf_addr(&m.mem, G_ROOT, 11, 0x1000);
    m.mem.write(leaf, m.mem.read(leaf) & !0xc0);
    let mut sim = m.sim(hwad);
    assert_eq!(sim.read(1, 0x1000), Ok(0x2000_1000));
    assert_eq!(m.mem.read(leaf) & 0xc0, 0x40, "a read sets A");
    assert!(sim.write(1, 0x1000, 0).is_ok());
    assert_eq!(m.mem.read(leaf) & 0xc0, 0xc0, "a write sets D");
    // Without GADE the same leaf faults (A clear).
    let m = Machine::new();
    let leaf = leaf_addr(&m.mem, G_ROOT, 11, 0x1000);
    m.mem.write(leaf, m.mem.read(leaf) & !0x40);
    assert_eq!(
        m.sim(hwad).read(1, 0x1000),
        Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT)
    );

    // iohgatp: a mode the capabilities do not give, a reserved mode, a root not 16 KiB aligned.
    let g = |m: &Machine, f: &dyn Fn(u64) -> u64| {
        m.mem
            .write(m.ctx_word(1, 1), f(m.mem.read(m.ctx_word(1, 1))))
    };
    assert_eq!(outcome(caps_without(1 << 17), |_| {}), MISCONF, "Sv39x4");
    for (mode, cap) in [(9u64, 18), (10, 19)] {
        let set = move |m: &Machine| g(m, &|w| (w & !(0xf << 60)) | mode << 60);
        assert_ne!(outcome(q, set), MISCONF, "mode {mode}");
        assert_eq!(outcome(caps_without(1 << cap), set), MISCONF, "mode {mode}");
    }
    for mode in [1u64, 7, 11, 15] {
        assert_eq!(
            outcome(q, |m| g(m, &|w| (w & !(0xf << 60)) | mode << 60)),
            MISCONF,
            "mode {mode}"
        );
    }
    assert_eq!(outcome(q, |m| g(m, &|w| w + 1)), MISCONF, "a root at 4 KiB");
    assert_eq!(
        outcome(q, |m| g(m, &|w| w + 4)),
        Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT),
        "a root at 16 KiB"
    );
    // Both stages: nested translation is not what this system configures.
    assert_eq!(
        outcome(q, |m| m
            .mem
            .write(m.ctx_word(1, 3), m.mem.read(m.ctx_word(3, 3)))),
        MISCONF
    );
    // msiptp: Off and Flat only.
    for (mode, want) in [(0u64, true), (1, true), (2, false), (15, false)] {
        let r = outcome(q, |m| m.mem.write(m.ctx_word(1, 4), mode << 60));
        assert_eq!(r != MISCONF, want, "msiptp mode {mode}");
    }
}

#[test]
fn first_stage_modes_follow_the_capabilities() {
    let m = Machine::new();
    for (mode, cap) in [(8u64, 9), (9, 10), (10, 11)] {
        let fsc = (m.mem.read(m.ctx_word(3, 3)) & !(0xf << 60)) | mode << 60;
        let m = Machine::new();
        m.mem.write(m.ctx_word(3, 3), fsc);
        let r = m.sim(SimConfig::qemu()).read(3, 0x1000);
        assert_ne!(r, Err(faultq::CAUSE_DDT_MISCONFIGURED), "mode {mode}");
        assert_eq!(
            m.sim(caps_without(1 << cap)).read(3, 0x1000),
            Err(faultq::CAUSE_DDT_MISCONFIGURED),
            "mode {mode}"
        );
    }
    // A first-stage address must be sign-extended from its top bit (bit 38 for Sv39): the mapped top-half page is reached
    // only by its canonical address.
    let mut sim = m.sim(SimConfig::qemu());
    assert_eq!(sim.read(3, 0xffff_ffc0_0000_1000), Ok(0x2000_4000));
    assert_eq!(
        sim.read(3, 0x40_0000_1000),
        Err(faultq::CAUSE_READ_PAGE_FAULT)
    );
    assert_eq!(
        sim.read(3, 0x7fff_ffc0_0000_1000),
        Err(faultq::CAUSE_READ_PAGE_FAULT)
    );
    assert_eq!(
        sim.read(3, 0x40_0000_1000),
        Err(faultq::CAUSE_READ_PAGE_FAULT)
    );
    // A second-stage address must be below 2^41 (Sv39x4).
    assert_eq!(
        sim.read(1, 1 << 41 | 0x1000),
        Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT)
    );
}

#[test]
fn the_directory_walk() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 64);
    let mut g = Iopt::new(&mem, G_ROOT, PtMode::Sv39x4).unwrap();
    g.map(&mem, &mut frames, 0x1000, 0x2000_1000, Access::Read)
        .unwrap();
    let ctx = DeviceContext::translate(g.context_root(1).unwrap(), None);
    // Two levels, extended: device ids of 15 bits, the root entry at bits 14:6.
    let mut d = Ddt::new(&mem, DIR, Levels::Two, Format::Extended).unwrap();
    let did = 0x7abc;
    d.set(&mem, &mut frames, did, &ctx).unwrap();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    sim.write64(regs::DDTP, 0);
    assert_eq!(sim.read(did, 0x1000), Err(faultq::CAUSE_DMA_DISABLED));
    sim.write64(regs::DDTP, 1);
    assert_eq!(sim.read(did, 0x1000), Ok(0x1000), "Bare");
    sim.write64(regs::DDTP, 0);
    sim.write64(regs::DDTP, d.ddtp().bits());
    assert_eq!(sim.read(did, 0x1000), Ok(0x2000_1000));
    assert_eq!(sim.read(1 << 15, 0x1000), Err(faultq::CAUSE_TTYPE_BLOCKED));
    // One level, extended: 6 bits.
    let one = Ddt::new(&mem, 0x8700_0000, Levels::One, Format::Extended).unwrap();
    let mut sim1 = Sim::new(&mem, SimConfig::qemu());
    sim1.write64(regs::DDTP, one.ddtp().bits());
    assert_eq!(sim1.read(63, 0x1000), Err(faultq::CAUSE_DDT_INVALID));
    assert_eq!(sim1.read(64, 0x1000), Err(faultq::CAUSE_TTYPE_BLOCKED));
    let entry = DIR + u64::from(did >> 6) * 8;
    let good = mem.read(entry);
    for (bad, cause) in [
        (good & !1, faultq::CAUSE_DDT_INVALID),
        (good | 1 << 1, faultq::CAUSE_DDT_MISCONFIGURED),
        (good | 1 << 9, faultq::CAUSE_DDT_MISCONFIGURED),
        (good | 1 << 54, faultq::CAUSE_DDT_MISCONFIGURED),
        (good | 1 << 63, faultq::CAUSE_DDT_MISCONFIGURED),
    ] {
        mem.write(entry, bad);
        let mut sim = Sim::new(&mem, SimConfig::qemu());
        sim.write64(regs::DDTP, d.ddtp().bits());
        assert_eq!(sim.read(did, 0x1000), Err(cause), "{bad:#x}");
    }
    mem.write(entry, good);

    // Base format (no MSI_FLAT): 32-byte contexts, device ids of 7, 16 and 24 bits, the levels at 6:0, 15:7 and 23:16.
    let base = SimConfig {
        caps: QEMU_CAPS & !(1 << 22),
        ..SimConfig::qemu()
    };
    for (levels, did, bits) in [
        (Levels::One, 0x55, 7),
        (Levels::Two, 0xa5a5, 16),
        (Levels::Three, 0xa5_a5a5, 24),
    ] {
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 64);
        let mut g = Iopt::new(&mem, G_ROOT, PtMode::Sv39x4).unwrap();
        g.map(&mem, &mut frames, 0x1000, 0x2000_1000, Access::Read)
            .unwrap();
        let empty = Iopt::new(&mem, 0x8300_0000, PtMode::Sv39x4).unwrap();
        let mut d = Ddt::new(&mem, DIR, levels, Format::Base).unwrap();
        // A neighbour too, so that a context read at the wrong size or place finds the wrong one.
        let ctx = DeviceContext::translate(g.context_root(1).unwrap(), None);
        d.set(&mem, &mut frames, did, &ctx).unwrap();
        d.set(
            &mem,
            &mut frames,
            did ^ 1,
            &DeviceContext::deny(empty.context_root(2).unwrap()),
        )
        .unwrap();
        let mut sim = Sim::new(&mem, base);
        sim.write64(regs::DDTP, d.ddtp().bits());
        assert_eq!(sim.read(did, 0x1000), Ok(0x2000_1000), "{levels:?}");
        assert!(sim.read(did ^ 1, 0x1000).is_err(), "{levels:?}");
        assert_eq!(
            sim.read(1 << bits, 0x1000),
            Err(faultq::CAUSE_TTYPE_BLOCKED),
            "{levels:?}"
        );
    }
}

#[test]
fn the_msi_check_follows_the_first_stage_only() {
    // IOMMU-SPEC 3.3 and QEMU `riscv_iommu_spa_fetch`: with a first stage, the MSI window is checked against the address the
    // first stage produced; with the second stage only, against the device's own address, never the translated one.
    let m = Machine::new();
    for did in [1, 3] {
        m.mem.write(m.ctx_word(did, 4), 1 << 60 | 0x8600_0000 >> 12);
        m.mem.write(m.ctx_word(did, 5), 0xff);
        m.mem.write(m.ctx_word(did, 6), MSI_BASE >> 12);
    }
    let mut sim = m.sim(SimConfig::qemu());
    // (The table is empty: an MSI is refused as invalid; a memory write lands.)
    assert_eq!(sim.write(3, 0x7000, 5), Err(faultq::CAUSE_MSI_INVALID));
    assert_eq!(
        sim.write(1, 0x7000, 5),
        Ok(iommucore::sim::WriteOutcome::Memory(MSI_BASE + 0x3000))
    );
    assert_eq!(
        sim.write(1, MSI_BASE + 0x10, 5),
        Err(faultq::CAUSE_MSI_INVALID)
    );
    // A read never takes the MSI path.
    assert_eq!(sim.read(3, 0x7000), Ok(MSI_BASE + 0x3000));
}

#[test]
fn basic_msi_entries_are_checked() {
    // A basic-translate entry (M = 3; IOMMU-SPEC 3.3 keeps it to the fault-injection build): bits 63:54 and 9:3 are reserved.
    let m = Machine::new();
    m.mem.write(m.ctx_word(1, 4), 1 << 60 | 0x8600_0000 >> 12);
    m.mem.write(m.ctx_word(1, 5), 0xff);
    m.mem.write(m.ctx_word(1, 6), MSI_BASE >> 12);
    let entry = |m: &Machine, w0: u64| {
        m.mem.write(0x8600_0000 + 2 * 16, w0);
        m.sim(SimConfig::qemu()).write(1, MSI_BASE + 0x2000, 9)
    };
    let good = (0x2800_0000u64 >> 12) << 10 | 3 << 1 | 1;
    assert_eq!(
        entry(&m, good),
        Ok(iommucore::sim::WriteOutcome::Basic {
            page: 0x2800_0000,
            data: 9
        })
    );
    for bad in [1u64 << 54, 1 << 62, 1 << 3, 1 << 9] {
        assert_eq!(
            entry(&m, good | bad),
            Err(faultq::CAUSE_MSI_MISCONFIGURED),
            "{bad:#x}"
        );
    }
    assert_eq!(
        entry(&m, good | 1 << 63),
        Err(faultq::CAUSE_MSI_INVALID),
        "custom"
    );
    assert_eq!(entry(&m, good & !1), Err(faultq::CAUSE_MSI_INVALID));
    for mode in [0u64, 2] {
        assert_eq!(
            entry(&m, (good & !6) | mode << 1),
            Err(faultq::CAUSE_MSI_MISCONFIGURED)
        );
    }
}

#[test]
fn a_device_write_is_four_bytes_at_its_address() {
    let m = Machine::new();
    let mut sim = m.sim(SimConfig::qemu());
    m.mem.write(0x2000_1000, 0x1111_1111_2222_2222);
    assert_eq!(
        sim.write(1, 0x1004, 0xabcd),
        Ok(iommucore::sim::WriteOutcome::Memory(0x2000_1004))
    );
    assert_eq!(m.mem.read(0x2000_1000), 0x0000_abcd_2222_2222);
    assert_eq!(
        sim.write(1, 0x1000, 0x55),
        Ok(iommucore::sim::WriteOutcome::Memory(0x2000_1000))
    );
    assert_eq!(m.mem.read(0x2000_1000), 0x0000_abcd_0000_0055);
}

#[test]
fn fctl_follows_the_interrupt_capability() {
    let mem = FakeMem::new();
    for (igs, written, read) in [
        (2u64, regs::FCTL_WSI, regs::FCTL_WSI),
        (2, 0, 0),
        (
            2,
            regs::FCTL_WSI | regs::FCTL_BE | regs::FCTL_GXL,
            regs::FCTL_WSI,
        ),
        (1, 0, regs::FCTL_WSI),
        (0, regs::FCTL_WSI, 0),
    ] {
        let caps = (QEMU_CAPS & !(3 << 28)) | igs << 28;
        let mut sim = Sim::new(
            &mem,
            SimConfig {
                caps,
                ..SimConfig::qemu()
            },
        );
        assert_eq!(
            sim.read32(regs::FCTL),
            if igs == 1 { regs::FCTL_WSI } else { 0 },
            "reset, igs {igs}"
        );
        sim.write32(regs::FCTL, written);
        assert_eq!(
            sim.read32(regs::FCTL),
            read,
            "igs {igs}, wrote {written:#x}"
        );
    }
}

#[test]
fn the_debug_interface_and_other_registers() {
    let m = Machine::new();
    let mut sim = m.sim(SimConfig::qemu());
    sim.write64(regs::TR_REQ_IOVA, 0x3fff);
    assert_eq!(sim.read64(regs::TR_REQ_IOVA), 0x3000);
    sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(1, false));
    assert!(!regs::tr_busy(sim.read64(regs::TR_REQ_CTL)));
    assert_eq!(
        regs::decode_tr_response(sim.read64(regs::TR_RESPONSE)),
        Some(0x2000_3000)
    );
    sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(1, true));
    assert_eq!(
        regs::decode_tr_response(sim.read64(regs::TR_RESPONSE)),
        None,
        "read-only page"
    );
    sim.write64(regs::TR_REQ_IOVA, 0x1000);
    sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(1, true));
    assert_eq!(
        regs::decode_tr_response(sim.read64(regs::TR_RESPONSE)),
        Some(0x2000_1000)
    );
    // A write request in the device's MSI window answers with the window's page (the address goes to the MSI path, untranslated).
    m.mem.write(m.ctx_word(1, 4), 1 << 60 | 0x8600_0000 >> 12);
    m.mem.write(m.ctx_word(1, 5), 0xff);
    m.mem.write(m.ctx_word(1, 6), (1 << 40) >> 12);
    let mut sim = m.sim(SimConfig::qemu());
    sim.write64(regs::TR_REQ_IOVA, (1 << 40) + 0x5000);
    sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(1, true));
    assert_eq!(
        regs::decode_tr_response(sim.read64(regs::TR_RESPONSE)),
        Some((1 << 40) + 0x5000)
    );
    // The interface does not fill the translation cache, and reports no fault.
    assert_eq!(sim.cached().0, 0);
    assert_eq!(sim.faults_reported(), 0);
    // Without DBG the request stays pending and nothing answers.
    let mut sim = m.sim(caps_without(1 << 31));
    sim.write64(regs::TR_REQ_IOVA, 0x1000);
    sim.write64(regs::TR_REQ_CTL, regs::tr_req_ctl(1, false));
    assert!(regs::tr_busy(sim.read64(regs::TR_REQ_CTL)));
    assert_eq!(sim.read64(regs::TR_RESPONSE), 0);
    // icvec keeps four 4-bit fields.
    sim.write64(regs::ICVEC, u64::MAX);
    assert_eq!(sim.read64(regs::ICVEC), 0xffff);
    assert_eq!(sim.read64(regs::CAPABILITIES), QEMU_CAPS & !(1 << 31));
    assert_eq!(
        sim.read32(regs::CAPABILITIES),
        (QEMU_CAPS & !(1 << 31)) as u32
    );
    // Registers the simulator does not model read as 0, at either width.
    for off in [
        regs::MSI_CFG_TBL,
        regs::MSI_CFG_TBL + 8,
        0x0c,
        0x38,
        0x40,
        0x58,
    ] {
        assert_eq!(sim.read64(off), 0, "{off:#x}");
        assert_eq!(sim.read32(off), 0, "{off:#x}");
    }
    assert_eq!(
        sim.read32(regs::CAPABILITIES + 4),
        ((QEMU_CAPS & !(1 << 31)) >> 32) as u32
    );
}
