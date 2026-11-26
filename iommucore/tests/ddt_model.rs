//! The device directory against a reference model (IOMMU2; IOMMU-SPEC 13): random sets and clears of the three context shapes,
//! in every directory mode and both formats, against a `BTreeMap`. After every step `get` agrees with the reference, the
//! directory pages in use are exactly the reference's prediction, and the **simulated IOMMU** (its own directory walker, with its
//! device-context cache flushed by `IODIR.INVAL_DDT` through the real command queue) sees each sampled device in the state the
//! reference says: invalid (cause 258), denied (a page fault it does not report) or attached (the mapping works). At the end
//! every context is cleared and the page count is back at zero.

mod common;

use common::{Rng, SEEDS, STEPS};
use iommucore::cmdq::{CmdQueue, Command};
use iommucore::ddt::{self, Ddt, DeviceContext, Format, Levels};
use iommucore::faultq;
use iommucore::iopt::{Access, Iopt, PtMode};
use iommucore::regs;
use iommucore::sim::{FakeFrames, FakeMem, Sim, SimConfig, QEMU_CAPS};
use iommucore::Regs;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Deny,
    Translate,
}

const MAPPED_IOVA: u64 = 0x1000;
const MAPPED_PA: u64 = 0x5000_0000;

fn predicted_pages(levels: Levels, format: Format, model: &BTreeMap<u32, Shape>) -> usize {
    let leaf = format.leaf_bits();
    let distinct = |shift: u32| {
        model
            .keys()
            .map(|id| id >> shift)
            .collect::<BTreeSet<_>>()
            .len()
    };
    match levels {
        Levels::One => 0,
        Levels::Two => distinct(leaf),
        Levels::Three => distinct(leaf) + distinct(leaf + 9),
    }
}

fn random_id(rng: &mut Rng, bits: u32) -> u32 {
    let raw = match rng.below(3) {
        0 => rng.below(200) as u32,
        1 => (rng.below(4) << 15) as u32 | rng.below(64) as u32,
        _ => rng.next() as u32,
    };
    raw & ((1 << bits) - 1)
}

#[test]
fn the_directory_follows_the_reference_and_the_simulated_iommu_agrees() {
    let shapes = [
        (Levels::One, Format::Extended),
        (Levels::Two, Format::Extended),
        (Levels::Three, Format::Extended),
        (Levels::One, Format::Base),
        (Levels::Two, Format::Base),
        (Levels::Three, Format::Base),
    ];
    for seed in 0..SEEDS {
        let (levels, format) = shapes[(seed % 6) as usize];
        let ctx_name = |step: u64| format!("seed {seed} {levels:?} {format:?} step {step}");
        let mut rng = Rng::new(seed);
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 1 << 14);
        let mut tables = FakeFrames::new(&mem, 0x2_0000_0000, 64);
        let mut d = Ddt::new(&mem, 0x8000_0000, levels, format).unwrap();

        // One domain's table with one page mapped, and the empty table of the quarantine.
        let mut domain = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
        domain
            .map(&mem, &mut tables, MAPPED_IOVA, MAPPED_PA, Access::ReadWrite)
            .unwrap();
        let empty = Iopt::new(&mem, 0x8104_0000, PtMode::Sv39x4).unwrap();

        // The simulated IOMMU: base format when it lacks MSI_FLAT (iommu.c:1180).
        let caps = if format == Format::Base {
            QEMU_CAPS & !(1 << 22)
        } else {
            QEMU_CAPS
        };
        let mut sim = Sim::new(
            &mem,
            SimConfig {
                caps,
                ..SimConfig::qemu()
            },
        );
        let mut cq = CmdQueue::new(0x8200_0000, 4).unwrap();
        sim.write64(regs::CQB, cq.cqb());
        sim.write32(regs::CQCSR, regs::QCSR_EN);
        sim.write64(regs::DDTP, d.ddtp().bits());

        let bits = ddt::device_id_bits(levels, format);
        let mut model: BTreeMap<u32, Shape> = BTreeMap::new();
        for step in 0..STEPS {
            let ctx = ctx_name(step);
            let id = if !model.is_empty() && rng.chance(40) {
                *model
                    .keys()
                    .nth(rng.below(model.len() as u64) as usize)
                    .unwrap()
            } else {
                random_id(&mut rng, bits)
            };
            if rng.chance(10) {
                frames.budget = Some(rng.below(2));
            }
            match rng.below(6) {
                0 | 1 => {
                    let r = d.set(
                        &mem,
                        &mut frames,
                        id,
                        &DeviceContext::deny(empty.context_root(0).unwrap()),
                    );
                    if r.is_ok() {
                        model.insert(id, Shape::Deny);
                    } else {
                        assert_eq!(r, Err(ddt::DdtError::NoFrame), "{ctx}");
                    }
                }
                2 | 3 => {
                    let root = domain.context_root(1 + id % 7).unwrap();
                    let r = d.set(&mem, &mut frames, id, &DeviceContext::translate(root, None));
                    if r.is_ok() {
                        model.insert(id, Shape::Translate);
                    } else {
                        assert_eq!(r, Err(ddt::DdtError::NoFrame), "{ctx}");
                    }
                }
                4 => {
                    let allocs = frames.allocs;
                    d.clear(&mem, &mut frames, id);
                    model.remove(&id);
                    assert_eq!(frames.allocs, allocs, "{ctx}: clear allocated");
                }
                _ => {
                    d.set(&mem, &mut frames, id, &DeviceContext::invalid())
                        .unwrap();
                    model.remove(&id);
                }
            }
            frames.budget = None;
            // The kernel's protocol (IOMMU-SPEC 3.1): every change is followed by IODIR.INVAL_DDT for the device.
            sim.write32(
                regs::CQT,
                cq.push(&mem, Command::iodir_inval_ddt(Some(id))).unwrap(),
            );
            cq.completed(sim.read32(regs::CQH)).unwrap();
            assert!(cq.is_done(cq.ticket()), "{ctx}");

            assert_eq!(
                frames.live(),
                predicted_pages(levels, format, &model),
                "{ctx}: directory pages"
            );
            // (IOMMU6: the walk the kernel's leak check uses counts the same pages, and the root)
            assert_eq!(
                d.pages(&mem) as usize,
                1 + predicted_pages(levels, format, &model),
                "{ctx}: Ddt::pages"
            );
            for _ in 0..6 {
                let probe = if !model.is_empty() && rng.chance(70) {
                    *model
                        .keys()
                        .nth(rng.below(model.len() as u64) as usize)
                        .unwrap()
                } else {
                    random_id(&mut rng, bits)
                };
                let words = d.get(&mem, probe);
                let suppressed = sim.faults_suppressed();
                match model.get(&probe) {
                    None => {
                        assert!(
                            words.is_none_or(|w| w == [0; 8]),
                            "{ctx}: {probe:#x} {words:x?}"
                        );
                        assert_eq!(
                            sim.read(probe, MAPPED_IOVA),
                            Err(faultq::CAUSE_DDT_INVALID),
                            "{ctx}: {probe:#x}"
                        );
                    }
                    Some(Shape::Deny) => {
                        assert_eq!(words.unwrap()[0], 0x11, "{ctx}");
                        assert_eq!(
                            sim.read(probe, MAPPED_IOVA),
                            Err(faultq::CAUSE_READ_GUEST_PAGE_FAULT),
                            "{ctx}: {probe:#x}"
                        );
                        assert_eq!(
                            sim.faults_suppressed(),
                            suppressed + 1,
                            "{ctx}: a denied device's fault is not reported"
                        );
                    }
                    Some(Shape::Translate) => {
                        assert_eq!(words.unwrap()[0], 0x1, "{ctx}");
                        assert_eq!(
                            sim.read(probe, MAPPED_IOVA + 8),
                            Ok(MAPPED_PA + 8),
                            "{ctx}: {probe:#x}"
                        );
                    }
                }
            }
        }
        // Everything comes back.
        let ids: Vec<u32> = model.keys().copied().collect();
        for id in ids {
            d.clear(&mem, &mut frames, id);
        }
        assert_eq!(frames.live(), 0, "seed {seed}");
    }
}

#[test]
fn a_device_id_too_wide_is_refused() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 16);
    let mut d = Ddt::new(&mem, 0x8000_0000, Levels::Two, Format::Extended).unwrap();
    let t = Iopt::new(&mem, 0x8100_0000, PtMode::Sv39x4).unwrap();
    let ctx = DeviceContext::deny(t.context_root(0).unwrap());
    assert_eq!(
        d.set(&mem, &mut frames, 1 << 15, &ctx),
        Err(ddt::DdtError::DeviceIdTooWide)
    );
    assert!(d.set(&mem, &mut frames, (1 << 15) - 1, &ctx).is_ok());
    assert_eq!(d.get(&mem, 1 << 15), None);
    // The simulated IOMMU refuses the same id (cause 260, iommu.c:1228).
    let mut sim = Sim::new(&mem, SimConfig::qemu());
    sim.write64(regs::DDTP, d.ddtp().bits());
    assert_eq!(sim.read(1 << 15, 0x1000), Err(faultq::CAUSE_TTYPE_BLOCKED));
}
