//! The I/O page table against a reference model (IOMMU2; IOMMU-SPEC 13): random maps and unmaps, in every format, against a
//! `BTreeMap`. After every step `translate` agrees with the reference for sampled addresses, `for_each_leaf` lists exactly the
//! reference, and the table frames in use are exactly what the reference predicts (one table per distinct prefix per level
//! below the root). At the end everything is unmapped and the frame count is back at its baseline, zero (the M3 shape).

mod common;

use common::{Rng, SEEDS, STEPS};
use iommucore::iopt::{Access, Iopt, MapError, Op, PageFault, PtMode};
use iommucore::sim::{FakeFrames, FakeMem};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

const MODES: [PtMode; 6] = [
    PtMode::Sv39,
    PtMode::Sv48,
    PtMode::Sv57,
    PtMode::Sv39x4,
    PtMode::Sv48x4,
    PtMode::Sv57x4,
];

/// The table frames the reference needs: for each level from 1 to levels - 1, one table per distinct `iova >> (12 + 9 * level)`.
fn predicted_tables(mode: PtMode, mapped: &BTreeMap<u64, (u64, Access)>) -> usize {
    (1..mode.levels())
        .map(|level| {
            mapped
                .keys()
                .map(|iova| {
                    (iova >> (12 + 9 * level)) & ((1u64 << (mode.iova_bits() - 12 - 9 * level)) - 1)
                })
                .collect::<BTreeSet<_>>()
                .len()
        })
        .sum()
}

/// A random page-aligned I/O address of the format: a few clusters so that tables are shared, plus anywhere in range.
fn random_iova(rng: &mut Rng, mode: PtMode) -> u64 {
    let bits = mode.iova_bits();
    let raw = match rng.below(4) {
        0 => rng.below(64) << 12,
        1 => (1u64 << (bits - 2)) + (rng.below(1024) << 12),
        2 => (rng.below(8) << 30) + (rng.below(16) << 21) + (rng.below(4) << 12),
        _ => rng.next() & ((1u64 << bits) - 1) & !0xfff,
    };
    if mode.second_stage() {
        raw
    } else {
        // Sign-extend from the top bit: the upper half is a valid first-stage address too.
        (((raw << (64 - bits)) as i64) >> (64 - bits)) as u64
    }
}

fn check(
    t: &Iopt,
    mem: &FakeMem,
    frames: &FakeFrames<FakeMem>,
    model: &BTreeMap<u64, (u64, Access)>,
    rng: &mut Rng,
    ctx: &str,
) {
    assert_eq!(
        frames.live(),
        predicted_tables(t.mode(), model),
        "{ctx}: table frames"
    );
    if rng.below(8) != 0 {
        return check_translations(t, mem, model, rng, ctx);
    }
    let mut leaves = BTreeMap::new();
    t.for_each_leaf(mem, |iova, pa, access| {
        leaves.insert(iova, (pa, access));
    });
    assert_eq!(&leaves, model, "{ctx}: for_each_leaf");
    check_translations(t, mem, model, rng, ctx);
}

fn check_translations(
    t: &Iopt,
    mem: &FakeMem,
    model: &BTreeMap<u64, (u64, Access)>,
    rng: &mut Rng,
    ctx: &str,
) {
    for _ in 0..8 {
        let iova = if !model.is_empty() && rng.chance(60) {
            *model
                .keys()
                .nth(rng.below(model.len() as u64) as usize)
                .unwrap()
        } else {
            random_iova(rng, t.mode())
        };
        let off = rng.below(4096) & !3;
        let want_read = model.get(&iova).map(|(pa, _)| pa + off).ok_or(PageFault);
        let want_write = match model.get(&iova) {
            Some((pa, Access::ReadWrite)) => Ok(pa + off),
            _ => Err(PageFault),
        };
        assert_eq!(
            t.translate(mem, iova + off, Op::Read),
            want_read,
            "{ctx}: read {iova:#x}"
        );
        assert_eq!(
            t.translate(mem, iova + off, Op::Write),
            want_write,
            "{ctx}: write {iova:#x}"
        );
    }
}

#[test]
fn page_tables_follow_the_reference() {
    for seed in 0..SEEDS {
        let mode = MODES[(seed % 6) as usize];
        let mut rng = Rng::new(seed);
        let mem = FakeMem::new();
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 1 << 16);
        let mut t = Iopt::new(&mem, 0x8000_0000, mode).unwrap();
        let mut model: BTreeMap<u64, (u64, Access)> = BTreeMap::new();
        for step in 0..STEPS {
            let ctx = format!("seed {seed} {mode:?} step {step}");
            if rng.chance(10) {
                frames.budget = Some(rng.below(3));
            }
            match rng.below(10) {
                0..=5 => {
                    let iova = random_iova(&mut rng, mode);
                    let pa = 0x2_0000_0000 + (rng.below(1 << 20) << 12);
                    let access = if rng.chance(50) {
                        Access::Read
                    } else {
                        Access::ReadWrite
                    };
                    let r = t.map(&mem, &mut frames, iova, pa, access);
                    match model.entry(iova) {
                        Entry::Occupied(_) => assert_eq!(r, Err(MapError::AlreadyMapped), "{ctx}"),
                        // Refused for want of a table: nothing changed and nothing leaked (checked below).
                        Entry::Vacant(_) if r == Err(MapError::NoFrame) => {}
                        Entry::Vacant(v) => {
                            assert_eq!(r, Ok(()), "{ctx}");
                            v.insert((pa, access));
                        }
                    }
                }
                _ => {
                    let iova = if !model.is_empty() && rng.chance(80) {
                        *model
                            .keys()
                            .nth(rng.below(model.len() as u64) as usize)
                            .unwrap()
                    } else {
                        random_iova(&mut rng, mode)
                    };
                    let allocs = frames.allocs;
                    assert_eq!(
                        t.unmap(&mem, &mut frames, iova),
                        model.remove(&iova).map(|(pa, _)| pa),
                        "{ctx}"
                    );
                    assert_eq!(frames.allocs, allocs, "{ctx}: unmap allocated");
                }
            }
            frames.budget = None;
            check(&t, &mem, &frames, &model, &mut rng, &ctx);
        }
        // Everything comes back: unmap half, destroy the rest.
        let keys: Vec<u64> = model.keys().copied().collect();
        for iova in keys.iter().step_by(2) {
            assert!(t.unmap(&mem, &mut frames, *iova).is_some());
            model.remove(iova);
        }
        assert_eq!(frames.live(), predicted_tables(mode, &model), "seed {seed}");
        let allocs = frames.allocs;
        assert_eq!(t.destroy(&mem, &mut frames), 0x8000_0000);
        assert_eq!(frames.live(), 0, "seed {seed}: frames after destroy");
        assert_eq!(frames.allocs, allocs);
        // The root is clear again.
        for i in 0..mode.root_bytes() / 8 {
            assert_eq!(iommucore::PhysMem64::read(&mem, 0x8000_0000 + i * 8), 0);
        }
    }
}

#[test]
fn misaligned_and_out_of_range_arguments_change_nothing() {
    let mem = FakeMem::new();
    let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 16);
    let mut t = Iopt::new(&mem, 0x8000_0000, PtMode::Sv39x4).unwrap();
    assert_eq!(
        t.map(&mem, &mut frames, 0x1001, 0x2000, Access::Read),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        t.map(&mem, &mut frames, 0x1000, 0x2001, Access::Read),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        t.map(&mem, &mut frames, 0x1000, 1 << 56, Access::Read),
        Err(MapError::OutOfRange)
    );
    assert_eq!(t.unmap(&mem, &mut frames, 0x1001), None);
    assert_eq!(t.unmap(&mem, &mut frames, 1 << 41), None);
    assert_eq!(frames.live(), 0);
    assert_eq!(t.translate(&mem, 1 << 41, Op::Read), Err(PageFault));
}
