//! The simulated IOMMU handed over in every state (IOMMU2 for IOMMU5; IOMMU-SPEC 2.4 step 1, A17, OD166 point 11; IOMMU0 item 15).
//! The kernel's bring-up sequence (`iommucore::driver::bring_up`, IOMMU5) runs against the simulator handed over Off, Bare and in
//! each directory mode, with its queues on or off and with a busy bit that takes a few reads to clear. The simulator's record of
//! every `ddtp` write shows the rules: handed over Off, `ddtp` is written once, with the directory mode; otherwise the first write
//! is Off; Bare is never written; and an IOMMU that will not go Off ends in the refusal. Then the driver's other sequences: the
//! boot invalidation and its fence, the fault drain (bounded, acknowledged, overflow cleared) and the console's rate limit.

use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::driver::{self, Drained, Fence, Plan, RateLimit, Refusal};
use iommucore::faultq::{self, FaultQueue, FaultRecord, TransactionType};
use iommucore::regs::{self, decode_ddtp, Ddtp, DdtpMode, Required};
use iommucore::sim::{FakeMem, Handover, Sim, SimConfig};
use iommucore::Regs;

const BOUND: u64 = 100;
const ICVEC: u64 = 0x3210;

/// The driver's bring-up with a clock that counts its own reads, so a busy bit that never clears ends the wait.
fn bring_up<R: Regs>(
    r: &mut R,
    d: &Ddt,
    cq: &CmdQueue,
    fq: &FaultQueue,
) -> Result<DdtpMode, String> {
    let mut t = 0u64;
    let mut now = || {
        t += 1;
        t
    };
    let plan = Plan {
        ddt: d,
        cq,
        fq,
        required: Required { pas_bits: 40 },
        icvec: ICVEC,
    };
    driver::bring_up(r, &plan, &mut now, BOUND)
        .map(|up| {
            assert_eq!(up.mode, DdtpMode::Directory(d.levels()));
            up.handover
        })
        .map_err(|e| e.to_string())
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
    // The same directory mode with another root is accepted (iommu.c:1939: new_mode == old_mode).
    let two_b = Ddtp::directory(Levels::Two, 0x9100_0000).bits();
    sim.write64(regs::DDTP, two_b);
    assert_eq!(sim.read64(regs::DDTP), two_b);
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
    // While busy the register shows the old value (Off) with the busy bit.
    assert_eq!(sim.read64(regs::DDTP), 0x10);
    assert_eq!(sim.read64(regs::DDTP), 0x10);
    assert_eq!(sim.read64(regs::DDTP), one);
    assert_eq!(sim.ddtp_writes(), [one, two]);
}

#[test]
fn refusals_have_the_fixed_lines() {
    assert_eq!(Refusal::WillNotGoOff.to_string(), "IOMMU: will not go off");
    assert_eq!(Refusal::Refused("fctl").to_string(), "IOMMU: refused fctl");
    assert_eq!(
        Refusal::Missing(regs::Missing::MsiMrif).to_string(),
        "IOMMU: no MRIF"
    );
    assert_eq!(
        Refusal::Missing(regs::Missing::Sv39x4).to_string(),
        "IOMMU: missing Sv39x4"
    );
}

#[test]
fn bring_up_programs_the_vectors_and_wired_interrupts() {
    let mem = FakeMem::new();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let cq = CmdQueue::new(0x8010_0000, driver::CQ_ENTRIES).unwrap();
    let fq = FaultQueue::new(0x8010_1000, driver::FQ_RECORDS).unwrap();
    assert_eq!(bring_up(&mut sim, &d, &cq, &fq), Ok(DdtpMode::Off));
    assert_eq!(sim.read32(regs::FCTL), regs::FCTL_WSI);
    assert_eq!(sim.read64(regs::ICVEC), ICVEC);
    assert_eq!(sim.read64(regs::CQB), cq.cqb());
    assert_eq!(sim.read64(regs::FQB), fq.fqb());
    assert_eq!(
        driver::mode_name(decode_ddtp(sim.read64(regs::DDTP)).mode),
        "3LVL"
    );
}

/// The boot's invalidation and fence: executed in order, and the poll sees the ticket done.
#[test]
fn the_boot_invalidation_is_fenced() {
    let mem = FakeMem::new();
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let mut cq = CmdQueue::new(0x8010_0000, driver::CQ_ENTRIES).unwrap();
    let fq = FaultQueue::new(0x8010_1000, driver::FQ_RECORDS).unwrap();
    bring_up(&mut sim, &d, &cq, &fq).unwrap();
    let cmds = driver::invalidate_everything();
    let ticket = driver::submit(&mut sim, &mem, &mut cq, &cmds).unwrap();
    assert_eq!(driver::fence_poll(&mut sim, &mut cq, ticket), Fence::Done);
    assert_eq!(sim.executed(), cmds);
}

/// A fence the IOMMU has not yet run is pending; an illegal command fails the wait.
#[test]
fn a_fence_waits_and_a_queue_error_fails_it() {
    let mem = FakeMem::new();
    let cfg = SimConfig {
        auto_process: false,
        ..SimConfig::qemu()
    };
    let mut sim = Sim::new(&mem, cfg);
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let mut cq = CmdQueue::new(0x8010_0000, driver::CQ_ENTRIES).unwrap();
    let fq = FaultQueue::new(0x8010_1000, driver::FQ_RECORDS).unwrap();
    bring_up(&mut sim, &d, &cq, &fq).unwrap();
    let ticket = driver::submit(&mut sim, &mem, &mut cq, &driver::invalidate_everything()).unwrap();
    assert_eq!(
        driver::fence_poll(&mut sim, &mut cq, ticket),
        Fence::Pending
    );
    sim.process_commands();
    assert_eq!(driver::fence_poll(&mut sim, &mut cq, ticket), Fence::Done);
    let bad = Command {
        dword0: 0x7f,
        dword1: 0,
    };
    let ticket = driver::submit(&mut sim, &mem, &mut cq, &[bad, Command::fence()]).unwrap();
    sim.process_commands();
    assert_eq!(driver::fence_poll(&mut sim, &mut cq, ticket), Fence::Failed);
}

/// A ring without room refuses a submission whole, and writes nothing to `cqt`.
#[test]
fn a_submission_that_does_not_fit_is_refused_whole() {
    let mem = FakeMem::new();
    let cfg = SimConfig {
        auto_process: false,
        ..SimConfig::qemu()
    };
    let mut sim = Sim::new(&mem, cfg);
    let d = Ddt::new(&mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let mut cq = CmdQueue::new(0x8010_0000, 4).unwrap();
    let fq = FaultQueue::new(0x8010_1000, driver::FQ_RECORDS).unwrap();
    bring_up(&mut sim, &d, &cq, &fq).unwrap();
    let cmds = driver::invalidate_everything();
    assert!(driver::submit(&mut sim, &mem, &mut cq, &cmds).is_err());
    assert_eq!(sim.read32(regs::CQT), 0);
    assert!(driver::submit(&mut sim, &mem, &mut cq, &cmds[..3]).is_ok());
}

fn up_with_faults(mem: &FakeMem, records: u32) -> (Sim<'_, FakeMem>, FaultQueue) {
    let mut sim = Sim::new(mem, SimConfig::qemu());
    let d = Ddt::new(mem, 0x8000_0000, Levels::Three, Format::Extended).unwrap();
    let cq = CmdQueue::new(0x8010_0000, driver::CQ_ENTRIES).unwrap();
    let fq = FaultQueue::new(0x8010_1000, records).unwrap();
    bring_up(&mut sim, &d, &cq, &fq).unwrap();
    (sim, fq)
}

/// IOMMU-SPEC 7.1: a device with no directory entry is a fault record naming it, its address and its direction; the drain
/// acknowledges `ipsr.fip` and moves the head.
#[test]
fn a_denied_device_is_drained_with_its_address_and_direction() {
    let mem = FakeMem::new();
    let (mut sim, mut fq) = up_with_faults(&mem, driver::FQ_RECORDS);
    assert_eq!(sim.read(0x20, 0x8123_4000), Err(faultq::CAUSE_DDT_INVALID));
    assert!(sim.write(0x20, 0x8123_4800, 0xdead_beef).is_err());
    assert_ne!(sim.read32(regs::IPSR) & regs::IPSR_FIP, 0);
    let mut got: Vec<FaultRecord> = Vec::new();
    let d = driver::drain_faults(&mut sim, &mem, &mut fq, driver::DRAIN_PER_INTERRUPT, |r| {
        got.push(r)
    });
    assert_eq!(
        d,
        Drained {
            taken: 2,
            ..Drained::default()
        }
    );
    assert_eq!(sim.read32(regs::IPSR) & regs::IPSR_FIP, 0);
    assert_eq!(sim.read32(regs::FQH), sim.read32(regs::FQT));
    assert_eq!(got[0].device_id, 0x20);
    assert_eq!(got[0].iotval, 0x8123_4000);
    assert_eq!(got[0].ttype, TransactionType::UntranslatedRead);
    assert_eq!(got[1].iotval, 0x8123_4800);
    assert_eq!(got[1].ttype.direction(), "write");
    assert_eq!(
        driver::FaultLine(&got[1]).to_string(),
        "## iommu: fault cause=258 dev=00:04.0 iova=0x81234800 write"
    );
    assert_eq!(
        driver::FenceTimeoutLine(Some(0x20)).to_string(),
        "## iommu: fence timeout dev=00:04.0"
    );
    assert_eq!(
        driver::FenceTimeoutLine(None).to_string(),
        "## iommu: fence timeout dev=none"
    );
}

/// At most the bound per drain, with the rest left for the next (`more`); an overflow is reported once and cleared.
#[test]
fn the_drain_is_bounded_and_clears_an_overflow() {
    let mem = FakeMem::new();
    let (mut sim, mut fq) = up_with_faults(&mem, 8);
    for i in 0..20u64 {
        let _ = sim.read(0x28, 0x9000_0000 + i * 0x1000);
    }
    // seven records fit in a ring of eight; the eighth overflowed it
    assert!(regs::Fqcsr(sim.read32(regs::FQCSR)).overflow());
    let mut n = 0;
    let d = driver::drain_faults(&mut sim, &mem, &mut fq, 4, |_| n += 1);
    assert_eq!((d.taken, d.overflow, d.more), (4, true, true));
    assert!(!regs::Fqcsr(sim.read32(regs::FQCSR)).overflow());
    let d = driver::drain_faults(&mut sim, &mem, &mut fq, 4, |_| n += 1);
    assert_eq!((d.taken, d.overflow, d.more), (3, false, false));
    assert_eq!(n, 7);
    // after the overflow is cleared, new records arrive again
    let _ = sim.read(0x28, 0x9100_0000);
    let d = driver::drain_faults(&mut sim, &mem, &mut fq, 4, |_| n += 1);
    assert_eq!(d.taken, 1);
}

#[test]
fn the_console_is_rate_limited_per_second() {
    let mut rl = RateLimit::new();
    let admitted = (0..40).filter(|_| rl.admit(5_000, 1_000)).count();
    assert_eq!(admitted, driver::LINES_PER_SECOND as usize);
    assert!(!rl.admit(5_999, 1_000));
    assert!(rl.admit(6_000, 1_000));
}

#[cfg(feature = "fault-iommu-bare")]
#[test]
fn the_negative_control_is_pass_through() {
    let mem = FakeMem::new();
    let (mut sim, _) = up_with_faults(&mem, 8);
    driver::write_bare(&mut sim);
    assert_eq!(sim.read(0x20, 0x8123_4000), Ok(0x8123_4000));
}
