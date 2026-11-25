//! Property tests of the command queue's ring (IOMMU2; IOMMU-SPEC 6) and the fault queue's consumer (7.1). The command queue at
//! every size from 2 to 256: random pushes and consumption by a model IOMMU; a push never overwrites a command the IOMMU has not
//! consumed, the queue is full exactly when one slot is left, a ticket is done exactly when everything before it was consumed,
//! and a head outside the submitted commands is refused. The fault queue against the simulated IOMMU: records in order across
//! wraps, the overflow bit when the ring is full, and its acknowledgement.

mod common;

use common::Rng;
use iommucore::cmdq::{CmdQueue, Command, QueueError};
use iommucore::ddt::{Ddt, DeviceContext, Format, Levels};
use iommucore::faultq::{self, FaultQueue};
use iommucore::iopt::{Iopt, PtMode};
use iommucore::regs::{self, Fqcsr};
use iommucore::sim::{FakeFrames, FakeMem, Sim, SimConfig};
use iommucore::{PhysMem64, Regs};
use std::collections::VecDeque;

const BASE: u64 = 0x8020_0000;

#[test]
fn the_command_ring_never_overwrites_an_unconsumed_command() {
    for log in 1..=8u32 {
        let size = 1u32 << log;
        for seed in 0..20u64 {
            let ctx = format!("size {size} seed {seed}");
            let mut rng = Rng::new(seed * 31 + u64::from(log));
            let mem = FakeMem::new();
            let mut q = CmdQueue::new(BASE, size).unwrap();
            let mut device_head = 0u32;
            let mut q_head = 0u32;
            let mut unconsumed: VecDeque<(u32, u64)> = VecDeque::new();
            let mut consumed_total = 0u64;
            let mut next_id = 1u64;
            let mut tickets: Vec<u64> = Vec::new();
            for _ in 0..(size as u64 * 8).max(64) {
                match rng.below(3) {
                    0 | 1 => {
                        let tail = q.tail();
                        let r = q.push(
                            &mem,
                            Command {
                                dword0: next_id,
                                dword1: !next_id,
                            },
                        );
                        if unconsumed.len() == size as usize - 1 {
                            assert_eq!(r, Err(QueueError::Full), "{ctx}");
                            assert!(q.is_full());
                        } else {
                            assert_eq!(r, Ok((tail + 1) % size), "{ctx}");
                            assert!(
                                unconsumed.iter().all(|(slot, _)| *slot != tail),
                                "{ctx}: overwrote slot {tail}"
                            );
                            unconsumed.push_back((tail, next_id));
                            next_id += 1;
                            if rng.chance(30) {
                                tickets.push(q.ticket());
                            }
                        }
                    }
                    _ => {
                        // The IOMMU consumes some commands, each still intact in its slot.
                        let k = rng.below(unconsumed.len() as u64 + 1);
                        for _ in 0..k {
                            let (slot, id) = unconsumed.pop_front().unwrap();
                            assert_eq!(slot, device_head);
                            assert_eq!(mem.read(BASE + u64::from(slot) * 16), id, "{ctx}");
                            assert_eq!(mem.read(BASE + u64::from(slot) * 16 + 8), !id, "{ctx}");
                            device_head = (device_head + 1) % size;
                            consumed_total += 1;
                        }
                        if rng.chance(30) {
                            // A head past the tail (in ring order from the last head the kernel recorded) is refused and
                            // changes nothing.
                            let in_flight = (q.tail() + size - q_head) % size;
                            if in_flight < size - 1 {
                                let bad = (q.tail()
                                    + 1
                                    + rng.below(u64::from(size - 1 - in_flight)) as u32)
                                    % size;
                                let before = q.clone();
                                assert_eq!(
                                    q.completed(bad),
                                    Err(QueueError::BadHead),
                                    "{ctx}: head {bad}"
                                );
                                assert_eq!(q, before, "{ctx}");
                            }
                            assert_eq!(q.completed(size), Err(QueueError::BadHead));
                        }
                        q.completed(device_head).unwrap();
                        q_head = device_head;
                        assert_eq!(q.outstanding(), unconsumed.len() as u64, "{ctx}");
                        assert_eq!(q.is_empty(), unconsumed.is_empty());
                        for t in &tickets {
                            assert_eq!(q.is_done(*t), *t <= consumed_total, "{ctx}: ticket {t}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_head_beyond_the_tail_is_refused() {
    let mem = FakeMem::new();
    let mut q = CmdQueue::new(BASE, 8).unwrap();
    for _ in 0..3 {
        q.push(&mem, Command::iofence_c(None)).unwrap();
    }
    assert_eq!(q.completed(4), Err(QueueError::BadHead));
    assert_eq!(q.completed(7), Err(QueueError::BadHead));
    assert_eq!(q.completed(3), Ok(()));
    assert!(q.is_done(3));
    assert!(!q.is_done(4));
    assert!(CmdQueue::new(BASE + 8, 8).is_err());
    assert!(CmdQueue::new(BASE, 6).is_err());
    assert!(CmdQueue::new(BASE, 1).is_err());
}

#[test]
fn fences_complete_through_the_simulated_iommu() {
    // The kernel's loop (IOMMU-SPEC 6.1): push, write cqt, read cqh until the ticket is done. With the simulator holding the
    // commands back, the ticket is not done; after it runs them, it is, and the fence's completion word was written.
    let mem = FakeMem::new();
    let mut sim = Sim::new(
        &mem,
        SimConfig {
            auto_process: false,
            ..SimConfig::qemu()
        },
    );
    let mut q = CmdQueue::new(BASE, 4).unwrap();
    sim.write64(regs::CQB, q.cqb());
    sim.write32(regs::CQCSR, regs::QCSR_EN | regs::QCSR_IE);
    assert_eq!(sim.read32(regs::CQCSR) & regs::QCSR_ON, regs::QCSR_ON);
    for round in 0..10u32 {
        q.push(&mem, Command::iotinval_gvma(3, None)).unwrap();
        let tail = q
            .push(&mem, Command::iofence_c(Some((0x8030_0004, round))))
            .unwrap();
        let ticket = q.ticket();
        sim.write32(regs::CQT, tail);
        q.completed(sim.read32(regs::CQH)).unwrap();
        assert!(!q.is_done(ticket));
        sim.process_commands();
        q.completed(sim.read32(regs::CQH)).unwrap();
        assert!(q.is_done(ticket));
        assert_eq!(mem.read(0x8030_0000) >> 32, u64::from(round));
    }
    // An illegal command stops the queue: the wait ends as a failure (6.4).
    q.push(
        &mem,
        Command {
            dword0: 0x7f,
            dword1: 0,
        },
    )
    .unwrap();
    let tail = q.push(&mem, Command::iofence_c(None)).unwrap();
    sim.write32(regs::CQT, tail);
    sim.process_commands();
    let csr = regs::Cqcsr(sim.read32(regs::CQCSR));
    assert!(csr.failed() && csr.illegal());
    assert_eq!(sim.read32(regs::IPSR) & regs::IPSR_CIP, regs::IPSR_CIP);
    q.completed(sim.read32(regs::CQH)).unwrap();
    assert!(!q.is_done(q.ticket()));
}

#[test]
fn fault_records_arrive_in_order_and_overflow_is_reported() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 16);
    let mut d = Ddt::new(&mem, 0x8000_0000, Levels::One, Format::Extended).unwrap();
    let t = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    for did in 0..4 {
        d.set(
            &mem,
            &mut frames,
            did,
            &DeviceContext::translate(t.context_root(1).unwrap(), None),
        )
        .unwrap();
    }
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    let mut fq = FaultQueue::new(0x8040_0000, 4).unwrap();
    sim.write64(regs::FQB, fq.fqb());
    sim.write32(regs::FQCSR, regs::QCSR_EN | regs::QCSR_IE);
    sim.write64(regs::DDTP, d.ddtp().bits());
    let mut n = 0u64;
    // Many rounds of faults, draining each time: records in order across the wrap.
    for round in 0..20 {
        let k = 1 + round % 3;
        for _ in 0..k {
            let _ = sim.read((n % 4) as u32, 0x1000 * (n + 1));
            n += 1;
        }
        assert_eq!(sim.read32(regs::IPSR) & regs::IPSR_FIP, regs::IPSR_FIP);
        sim.write32(regs::IPSR, regs::IPSR_FIP);
        for j in (n - k)..n {
            let r = fq.pop(&mem, sim.read32(regs::FQT)).unwrap();
            assert_eq!(
                (r.cause, r.device_id, r.iotval),
                (
                    faultq::CAUSE_READ_GUEST_PAGE_FAULT,
                    (j % 4) as u32,
                    0x1000 * (j + 1)
                )
            );
            assert_eq!(r.ttype.direction(), "read");
        }
        assert_eq!(fq.pop(&mem, sim.read32(regs::FQT)), None);
        sim.write32(regs::FQH, fq.head());
    }
    // Without draining, the fourth fault finds the ring full (one slot stays empty): the overflow bit, and nothing more stored.
    for i in 0..5 {
        let _ = sim.read(0, 0x10_0000 + i * 0x1000);
    }
    let csr = Fqcsr(sim.read32(regs::FQCSR));
    assert!(csr.overflow() && !csr.mem_fault());
    let mut got = 0;
    while fq.pop(&mem, sim.read32(regs::FQT)).is_some() {
        got += 1;
    }
    assert_eq!(got, 3);
    sim.write32(regs::FQH, fq.head());
    sim.write32(regs::FQCSR, csr.acknowledge());
    let csr = Fqcsr(sim.read32(regs::FQCSR));
    assert!(!csr.overflow() && csr.on() && csr.enabled());
    let _ = sim.read(1, 0x1000);
    assert_eq!(
        fq.pop(&mem, sim.read32(regs::FQT)).map(|r| r.device_id),
        Some(1)
    );
    // A tail out of range is refused.
    assert_eq!(fq.pop(&mem, 4), None);
}
