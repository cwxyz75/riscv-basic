//! The simulated IOMMU handed over in every state (IOMMU2 for IOMMU5; IOMMU-SPEC 2.4 step 1, A17, OD166 point 11; IOMMU0 item 15).
//! A bring-up sequence written here from IOMMU-SPEC 2.4 (IOMMU5 writes the kernel's own over the same traits) runs against the
//! simulator handed over Off, Bare and in each directory mode, with its queues on or off and with a busy bit that takes a few
//! reads to clear. The simulator's record of every `ddtp` write shows the rules: handed over Off, `ddtp` is written once, with the
//! directory mode; otherwise the first write is Off; Bare is never written; and an IOMMU that will not go Off ends in the refusal.

use iommucore::cmdq::CmdQueue;
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::faultq::{self, FaultQueue};
use iommucore::regs::{self, decode_ddtp, Caps, Ddtp, DdtpMode, Required};
use iommucore::sim::{FakeMem, Handover, Sim, SimConfig};
use iommucore::Regs;

const BOUND: u32 = 100;

/// IOMMU-SPEC 2.4 steps 1, 2, 4 and 5 (step 3, the allocations, is the caller's).
fn bring_up<R: Regs>(
    r: &mut R,
    d: &Ddt,
    cq: &CmdQueue,
    fq: &FaultQueue,
) -> Result<DdtpMode, String> {
    let wait_ddtp = |r: &mut R| {
        for _ in 0..BOUND {
            let v = decode_ddtp(r.read64(regs::DDTP));
            if !v.busy {
                return Some(v);
            }
        }
        None
    };
    let found = wait_ddtp(r).ok_or("IOMMU: will not go off")?;
    if found.mode != DdtpMode::Off {
        r.write64(regs::DDTP, Ddtp::off().bits());
        match wait_ddtp(r) {
            Some(v) if v.mode == DdtpMode::Off => {}
            _ => return Err("IOMMU: will not go off".into()),
        }
    }
    for csr in [regs::CQCSR, regs::FQCSR] {
        if r.read32(csr) & (regs::QCSR_EN | regs::QCSR_ON) != 0 {
            r.write32(csr, 0);
            if (0..BOUND).all(|_| r.read32(csr) & (regs::QCSR_ON | regs::QCSR_BUSY) != 0) {
                return Err(format!("IOMMU: refused {csr:#x}"));
            }
        }
    }
    Caps(r.read64(regs::CAPABILITIES))
        .check(&Required { pas_bits: 40 })
        .map_err(|m| m.to_string())?;
    r.write32(regs::FCTL, regs::fctl_wired());
    if r.read32(regs::FCTL) != regs::fctl_wired() {
        return Err("IOMMU: refused fctl".into());
    }
    r.write64(regs::CQB, cq.cqb());
    r.write64(regs::FQB, fq.fqb());
    for csr in [regs::CQCSR, regs::FQCSR] {
        r.write32(csr, regs::QCSR_EN | regs::QCSR_IE);
        if r.read32(csr) & regs::QCSR_ON == 0 {
            return Err(format!("IOMMU: refused {csr:#x}"));
        }
    }
    r.write64(regs::DDTP, d.ddtp().bits());
    let v = wait_ddtp(r).ok_or("IOMMU: refused ddtp")?;
    if v.mode != DdtpMode::Directory(d.levels()) || v.root_pa != d.root() {
        return Err("IOMMU: refused ddtp".into());
    }
    Ok(found.mode)
}

#[test]
fn bring_up_from_every_handover_state() {
    let modes = [
        0u64,
        1,
        Ddtp::directory(Levels::One, 0x9000_0000).bits(),
        Ddtp::directory(Levels::Two, 0x9000_0000).bits(),
        Ddtp::directory(Levels::Three, 0x9000_0000).bits(),
    ];
    for handed in modes {
        for queues in 0..4 {
            for busy_reads in [0, 3] {
                let ctx = format!("handed over {handed:#x}, queues {queues}, busy {busy_reads}");
                let mem = FakeMem::new();
                let h = Handover {
                    ddtp: handed,
                    cq: (queues & 1 != 0).then_some(0x2004_0407),
                    fq: (queues & 2 != 0).then_some(0x2004_0806),
                };
                let mut sim = Sim::handed_over(
                    &mem,
                    SimConfig {
                        busy_reads,
                        ..SimConfig::qemu()
                    },
                    h,
                );
                if handed == 1 {
                    // Bare is pass-through: what the kernel must end.
                    assert_eq!(sim.read(5, 0x1234_5000), Ok(0x1234_5000), "{ctx}");
                }
                let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
                let cq = CmdQueue::new(0x8010_0000, 256).unwrap();
                let fq = FaultQueue::new(0x8010_1000, 128).unwrap();
                let found =
                    bring_up(&mut sim, &d, &cq, &fq).unwrap_or_else(|e| panic!("{ctx}: {e}"));
                assert_eq!(found, decode_ddtp(handed).mode, "{ctx}");
                let writes = sim.ddtp_writes();
                if handed == 0 {
                    assert_eq!(writes, [d.ddtp().bits()], "{ctx}");
                } else {
                    assert_eq!(writes[0], 0, "{ctx}: the first write is Off");
                    assert_eq!(writes[1..], [d.ddtp().bits()], "{ctx}");
                }
                assert!(
                    writes.iter().all(|w| w & 0xf != 1),
                    "{ctx}: Bare was written"
                );
                // Every device is denied now, by an empty directory.
                assert_eq!(
                    sim.read(5, 0x1234_5000),
                    Err(faultq::CAUSE_DDT_INVALID),
                    "{ctx}"
                );
                assert!(regs::Cqcsr(sim.read32(regs::CQCSR)).on());
                assert!(regs::Fqcsr(sim.read32(regs::FQCSR)).on());
            }
        }
    }
}

#[test]
fn an_iommu_that_will_not_go_off_is_refused() {
    let mem = FakeMem::new();
    let cfg = SimConfig {
        never_off: true,
        ..SimConfig::qemu()
    };
    let mut sim = Sim::handed_over(
        &mem,
        cfg,
        Handover {
            ddtp: 1,
            ..Handover::default()
        },
    );
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let cq = CmdQueue::new(0x8010_0000, 256).unwrap();
    let fq = FaultQueue::new(0x8010_1000, 128).unwrap();
    assert_eq!(
        bring_up(&mut sim, &d, &cq, &fq),
        Err("IOMMU: will not go off".into())
    );
    assert_eq!(sim.ddtp_writes(), [0]);
    assert_eq!(decode_ddtp(sim.read64(regs::DDTP)).mode, DdtpMode::Bare);
    // The same IOMMU handed over Off brings up normally (it only refuses to go Off).
    let mut sim = Sim::new(&mem, cfg);
    assert!(bring_up(&mut sim, &d, &cq, &fq).is_ok());
}

#[test]
fn a_missing_capability_is_the_refusal() {
    let mem = FakeMem::new();
    let caps = iommucore::sim::QEMU_CAPS & !(1 << 23);
    let mut sim = Sim::new(
        &mem,
        SimConfig {
            caps,
            ..SimConfig::qemu()
        },
    );
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let cq = CmdQueue::new(0x8010_0000, 256).unwrap();
    let fq = FaultQueue::new(0x8010_1000, 128).unwrap();
    assert_eq!(
        bring_up(&mut sim, &d, &cq, &fq),
        Err("IOMMU: no MRIF".into())
    );
    assert!(sim.ddtp_writes().is_empty());
}

#[test]
fn ddtp_transitions_are_qemus() {
    // IOMMU0 item 15: {Off, Bare} -> anything and a directory mode -> {Off, Bare} directly; directory -> directory is ignored.
    let mem = FakeMem::new();
    let two = Ddtp::directory(Levels::Two, 0x9000_0000).bits();
    let one = Ddtp::directory(Levels::One, 0x9000_0000).bits();
    let mut sim = Sim::handed_over(
        &mem,
        SimConfig::qemu(),
        Handover {
            ddtp: two,
            ..Handover::default()
        },
    );
    sim.write64(regs::DDTP, one);
    assert_eq!(sim.read64(regs::DDTP), two);
    sim.write64(regs::DDTP, 0);
    assert_eq!(sim.read64(regs::DDTP), 0);
    sim.write64(regs::DDTP, one);
    assert_eq!(sim.read64(regs::DDTP), one);
    // A reserved mode is ignored; the busy bit is never left set in the value (iommu.c:1952-1954).
    sim.write64(regs::DDTP, 0);
    sim.write64(regs::DDTP, 7);
    assert_eq!(sim.read64(regs::DDTP), 0);
    sim.write64(regs::DDTP, one | 0x10);
    assert_eq!(sim.read64(regs::DDTP), one);
    // With a busy bit: the write is ignored while busy, and the new mode shows when it clears.
    let mut sim = Sim::new(
        &mem,
        SimConfig {
            busy_reads: 2,
            ..SimConfig::qemu()
        },
    );
    sim.write64(regs::DDTP, one);
    sim.write64(regs::DDTP, two);
    assert!(decode_ddtp(sim.read64(regs::DDTP)).busy);
    assert!(decode_ddtp(sim.read64(regs::DDTP)).busy);
    assert_eq!(sim.read64(regs::DDTP), one);
    assert_eq!(sim.ddtp_writes(), [one, two]);
}
