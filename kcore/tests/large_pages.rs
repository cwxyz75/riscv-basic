//! Large pages (MM3): the range splitter and `PageTable::map_large`, in both page-table formats. The splitter's property: exactly the
//! requested frames, each once, in address order, each page the largest legal one at its place. The mapper's: every frame translates
//! to itself through the leaf that covers it, the leaf walk reports the planned levels, and the table frames used stay few.
mod common;
use common::{FakeMem, Rng, TestFmt48, SEEDS};
use kcore::frame::FrameDb;
use kcore::pagetable::{plan_range, MapError, PageTable, PageTableFormat, Perms, Sv39};
use kcore::Mm;

const D: u64 = 0xffff_ffc0_0000_0000; // the direct map's base: upper half, aligned to 1 GiB

#[test]
fn the_splitter_covers_each_frame_once_with_the_largest_legal_pages() {
    for seed in 0..SEEDS * 4 {
        let mut rng = Rng::new(seed);
        let first = rng.below(1 << 21) * if rng.chance(50) { 1 } else { 512 } + rng.below(3);
        let count = rng.below(1 << 20) + rng.below(2);
        let max = 2u32.min(rng.below(3) as u32);
        let mut at = first;
        for (pfn, level) in plan_range(first, count, max) {
            assert_eq!(pfn, at, "seed {seed}: in address order, no gap, no overlap");
            let span = 1u64 << (9 * level);
            assert!(level <= max);
            assert_eq!(pfn % span, 0, "seed {seed}: aligned to its span");
            assert!(pfn + span <= first + count, "seed {seed}: inside the range");
            if level < max {
                // the next level up would not have been legal here
                let up = 1u64 << (9 * (level + 1));
                assert!(
                    pfn % up != 0 || pfn + up > first + count,
                    "seed {seed}: a larger page was legal at {pfn}"
                );
            }
            at += span;
        }
        assert_eq!(at, first + count, "seed {seed}: every frame covered");
    }
    assert_eq!(plan_range(7, 0, 2).count(), 0);
}

fn direct_map<F: PageTableFormat>(first: u64, count: u64) -> (Mm<FakeMem>, PageTable<F>, usize) {
    let mut mm = Mm {
        db: FrameDb::new(1, 100),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::<F>::create(&mut mm).unwrap();
    let before = mm.db.free_count();
    for (pfn, level) in plan_range(first, count, 2) {
        pt.map_large(&mut mm, D + pfn * 4096, pfn, level, Perms::RW)
            .unwrap_or_else(|e| panic!("frame {pfn:#x} level {level}: {e:?}"));
    }
    let used = before - mm.db.free_count();
    (mm, pt, used)
}

fn check_direct_map<F: PageTableFormat>() {
    // RAM at 2 GiB, 4 GiB of it, starting at an unaligned frame: the shape of a 4 GiB machine
    let (first, count) = (0x80000u64 + 0x205, 0x100000u64 - 0x205 - 0x33);
    let (mm, pt, used) = direct_map::<F>(first, count);
    assert!(used < 16, "page-table frames for the direct map: {used}");
    // every frame translates to itself with RW
    let mut rng = Rng::new(3);
    for _ in 0..2000 {
        let pfn = first + rng.below(count);
        let t = pt.translate(&mm, D + pfn * 4096 + rng.below(4096)).unwrap();
        assert_eq!((t.pfn, t.perms), (pfn, Perms::RW), "frame {pfn:#x}");
    }
    assert!(pt.translate(&mm, D + (first - 1) * 4096).is_none());
    assert!(pt.translate(&mm, D + (first + count) * 4096).is_none());
    // the leaf walk reports exactly the plan
    let mut walked = Vec::new();
    pt.for_each_leaf(&mm, &mut |va, t, level| walked.push((va, t.pfn, level)));
    let planned: Vec<(u64, u64, u32)> = plan_range(first, count, 2)
        .map(|(pfn, level)| (D + pfn * 4096, pfn, level))
        .collect();
    assert_eq!(walked, planned);
    assert!(walked.iter().any(|w| w.2 == 2), "a 1 GiB page was used");
    assert!(walked.iter().any(|w| w.2 == 1), "a 2 MiB page was used");
    assert!(walked.iter().any(|w| w.2 == 0), "4 KiB pages at the edges");
}

#[test]
fn a_four_gib_direct_map_uses_large_pages_and_few_table_frames_in_both_formats() {
    check_direct_map::<Sv39>();
    check_direct_map::<TestFmt48>();
}

fn refusals<F: PageTableFormat>() {
    let mut mm = Mm {
        db: FrameDb::new(1, 40),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::<F>::create(&mut mm).unwrap();
    let two_mib = 512u64;
    // misaligned address or frame, a level the format lacks, bad permissions
    assert_eq!(
        pt.map_large(&mut mm, D + 4096, 0x1000, 1, Perms::RW),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        pt.map_large(&mut mm, D, 0x1001, 1, Perms::RW),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        pt.map_large(&mut mm, D, 0x1000, F::LEVELS, Perms::RW),
        Err(MapError::BadLevel)
    );
    assert_eq!(
        pt.map_large(&mut mm, D, 0x1000, 1, Perms::RW.user().into_wx()),
        Err(MapError::WriteExec)
    );
    pt.map_large(&mut mm, D, 0x1000, 1, Perms::RW).unwrap();
    // a leaf is in the way; a 4 KiB page cannot go inside it; a larger page cannot cover it
    assert_eq!(
        pt.map_large(&mut mm, D, 0x2000, 1, Perms::RW),
        Err(MapError::AlreadyMapped)
    );
    assert_eq!(
        pt.map(&mut mm, D + 4096, 0x5000, Perms::RW),
        Err(MapError::HugeConflict)
    );
    assert_eq!(
        pt.map_large(&mut mm, D, 0x40000, 2, Perms::RW),
        Err(MapError::HugeConflict)
    );
    // a table with small pages under it blocks a large page there
    pt.map(&mut mm, D + two_mib * 4096 * 2, 0x7000, Perms::RW)
        .unwrap();
    assert_eq!(
        pt.map_large(&mut mm, D + two_mib * 4096 * 2, 0x2000, 1, Perms::RW),
        Err(MapError::HugeConflict)
    );
    // no frames for the tables: nothing changes
    let mut starved = Mm {
        db: FrameDb::new(1, 1),
        mem: FakeMem::default(),
    };
    let mut p2 = PageTable::<F>::create(&mut starved).unwrap();
    assert_eq!(starved.db.free_count(), 0);
    assert_eq!(
        p2.map_large(&mut starved, D, 0x1000, 1, Perms::RW),
        Err(MapError::NoMemory)
    );
    assert!(p2.translate(&starved, D).is_none());
}

trait IntoWx {
    fn into_wx(self) -> Perms;
}
impl IntoWx for Perms {
    fn into_wx(self) -> Perms {
        Perms { exec: true, ..self }
    }
}

#[test]
fn large_page_refusals_are_specific_and_change_nothing_in_both_formats() {
    refusals::<Sv39>();
    refusals::<TestFmt48>();
}
