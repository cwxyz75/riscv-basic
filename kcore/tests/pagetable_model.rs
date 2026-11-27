//! The Sv39 page-table walker against a reference model (a map from virtual page
//! to (frame, permissions)), over a fake physical memory.
mod common;
use common::{FakeMem, Rng, TestFmt48, SEEDS, STEPS};
use kcore::frame::FrameDb;
use kcore::pagetable::{MapError, PageTable, PageTableFormat, Perms, Sv39};
use kcore::Mm;
use std::collections::{BTreeMap, BTreeSet};

fn perms(rng: &mut Rng) -> Perms {
    match rng.below(5) {
        0 => Perms::R,
        1 => Perms::RW,
        2 => Perms::RX,
        3 => Perms::X,
        _ => Perms::RW.user(),
    }
}

/// A virtual address built from chosen page-table indices, so random picks
/// repeatedly share (and sometimes do not share) upper-level tables. Upper
/// half addresses are sign-extended as the format requires.
fn address<F: PageTableFormat>(rng: &mut Rng) -> u64 {
    let top = rng.below(4) * 100 + rng.below(2);
    let (i1, i0) = (rng.below(3), rng.below(4));
    let mut va = (i1 << 21) | (i0 << 12);
    // the top index sits just below the format's address width; the levels between are zero
    va |= top << (12 + 9 * (F::LEVELS - 1));
    F::extend(va)
}

/// How many table frames a set of mapped pages needs: the root, and one table per distinct prefix at each level below it.
fn tables_needed<F: PageTableFormat>(model: &BTreeMap<u64, (u64, Perms)>) -> usize {
    let mut n = 1;
    for j in 1..F::LEVELS {
        let mask = (1u64 << (9 * (F::LEVELS - j))) - 1;
        let prefixes: BTreeSet<u64> = model.keys().map(|va| (va >> (12 + 9 * j)) & mask).collect();
        n += prefixes.len();
    }
    n
}

#[test]
fn random_map_unmap_translate_agree_with_the_model_sv39() {
    random_model::<Sv39>();
}

#[test]
fn random_map_unmap_translate_agree_with_the_model_testfmt48() {
    random_model::<TestFmt48>();
}

fn random_model<F: PageTableFormat>() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(1000, 200),
            mem: FakeMem::default(),
        };
        let baseline = mm.db.free_count();
        let mut pt = PageTable::<F>::create(&mut mm).unwrap();
        let mut model: BTreeMap<u64, (u64, Perms)> = BTreeMap::new();
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            let va = address::<F>(&mut rng);
            match rng.below(4) {
                0 | 1 => {
                    let (pfn, p) = (5000 + rng.below(100), perms(&mut rng));
                    let result = pt.map(&mut mm, va, pfn, p);
                    match model.entry(va) {
                        std::collections::btree_map::Entry::Occupied(_) => {
                            assert_eq!(result, Err(MapError::AlreadyMapped), "{ctx}")
                        }
                        // Table frames ran out: nothing may have changed.
                        std::collections::btree_map::Entry::Vacant(_)
                            if result == Err(MapError::NoMemory) => {}
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            assert_eq!(result, Ok(()), "{ctx}");
                            slot.insert((pfn, p));
                        }
                    }
                }
                2 => {
                    let got = pt.unmap(&mut mm, va);
                    assert_eq!(got, model.remove(&va), "{ctx}: unmap");
                }
                _ => {
                    let got = pt.translate(&mm, va + rng.below(4096));
                    let want = model.get(&va);
                    // Translation of any byte within the page; compare on the page.
                    let page = pt.translate(&mm, va);
                    assert_eq!(
                        page.map(|t| (t.pfn, t.perms)),
                        want.copied(),
                        "{ctx}: translate"
                    );
                    if let (Some(t), Some(_)) = (got, want) {
                        assert_eq!(t.pfn, want.unwrap().0, "{ctx}");
                    }
                }
            }
            // Independent accounting: frames in use are exactly the tables the
            // mapped pages need (pruned tables really were given back).
            assert_eq!(
                baseline - mm.db.free_count(),
                tables_needed::<F>(&model),
                "{ctx}: table frames in use"
            );
            mm.db.audit().unwrap();
        }
        // Every mapped page, found by walking the tables, is exactly the model's.
        let mut seen = BTreeMap::new();
        pt.for_each_leaf(&mm, &mut |va, t, level| {
            assert_eq!(level, 0, "only 4 KiB leaves were mapped");
            seen.insert(va, (t.pfn, t.perms));
        });
        assert_eq!(
            seen, model,
            "seed {seed}: leaf walk disagrees with the model"
        );
        for va in model.keys().copied().collect::<Vec<_>>() {
            assert!(pt.unmap(&mut mm, va).is_some());
        }
        assert_eq!(
            baseline - mm.db.free_count(),
            1,
            "seed {seed}: only the root remains"
        );
        pt.destroy(&mut mm);
        assert_eq!(
            mm.db.free_count(),
            baseline,
            "seed {seed}: not back to baseline"
        );
    }
}

#[test]
fn write_xor_execute_and_bad_arguments_are_refused() {
    let mut mm = Mm {
        db: FrameDb::new(1000, 50),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::new(&mut mm).unwrap();
    let rwx = Perms {
        read: true,
        write: true,
        exec: true,
        user: false,
    };
    assert_eq!(pt.map(&mut mm, 0x1000, 5, rwx), Err(MapError::WriteExec));
    let w_only = Perms {
        read: false,
        write: true,
        exec: false,
        user: false,
    };
    assert_eq!(
        pt.map(&mut mm, 0x1000, 5, w_only),
        Err(MapError::BadPerms),
        "write without read is reserved"
    );
    assert_eq!(
        pt.map(&mut mm, 0x1001, 5, Perms::R),
        Err(MapError::Misaligned)
    );
    assert_eq!(
        pt.map(&mut mm, 0x0000_0040_0000_0000, 5, Perms::R),
        Err(MapError::NonCanonical)
    );
    assert_eq!(
        pt.map(&mut mm, 0xffff_ff80_0000_0000, 5, Perms::R),
        Err(MapError::NonCanonical)
    );
    // A refused map allocates nothing.
    assert_eq!(1000 + 50 - 1, 1049);
    assert_eq!(mm.db.free_count(), 49, "only the root is in use");
    assert_eq!(pt.translate(&mm, 0x1000), None);
}

#[test]
fn leaf_bits_are_what_the_hardware_expects() {
    let mut mm = Mm {
        db: FrameDb::new(1000, 50),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::new(&mut mm).unwrap();
    pt.map(&mut mm, 0x20_0000, 0x1234, Perms::RW).unwrap();
    // Walk by hand: root -> level 1 -> level 0.
    let root = pt.root() << 12;
    let e2 = mm.mem.read_u64_for_test(root);
    assert_eq!(e2 & 0xff, 0b0000_0001, "non-leaf: V only");
    let t1 = (e2 >> 10) << 12;
    let e1 = mm.mem.read_u64_for_test(t1 + 8); // index 1 at level 1 (0x20_0000 >> 21 == 1)
    assert_eq!(e1 & 0xff, 0b0000_0001);
    let t0 = (e1 >> 10) << 12;
    let e0 = mm.mem.read_u64_for_test(t0);
    // V R W A D = bits 0,1,2,6,7
    assert_eq!(
        e0 & 0x3ff,
        0b00_1100_0111,
        "leaf flags: V R W A D (bits 0, 1, 2, 6, 7)"
    );
    assert_eq!(e0 >> 10, 0x1234, "leaf frame number");
}

trait ReadForTest {
    fn read_u64_for_test(&self, pa: u64) -> u64;
}
impl ReadForTest for FakeMem {
    fn read_u64_for_test(&self, pa: u64) -> u64 {
        kcore::pagetable::PhysMem::read_u64(self, pa)
    }
}

// --- Huge leaves, written by hand (the table code only ever creates 4 KiB ones,
// but the kernel's boot table and future mappings contain larger leaves). ---

fn with_giga_leaf() -> (Mm<FakeMem>, PageTable, u64) {
    let mut mm = Mm {
        db: FrameDb::new(1000, 50),
        mem: FakeMem::default(),
    };
    let pt = PageTable::new(&mut mm).unwrap();
    // root[2]: a 1 GiB leaf to physical 0x8000_0000 (frame 0x80000), R W A D V
    let slot = (pt.root() << 12) + 2 * 8;
    mm.mem_write(slot, (0x80000 << 10) | 0b1100_0111);
    // root[258 + ...]: a 1 GiB leaf in the upper half: index 510 -> frame 0x40000 (1 GiB), R X
    let slot = (pt.root() << 12) + 510 * 8;
    mm.mem_write(slot, (0x40000 << 10) | 0b1100_1011);
    (mm, pt, 0)
}

trait MemWrite {
    fn mem_write(&mut self, pa: u64, v: u64);
}
impl MemWrite for Mm<FakeMem> {
    fn mem_write(&mut self, pa: u64, v: u64) {
        kcore::pagetable::PhysMem::write_u64(&mut self.mem, pa, v);
    }
}

#[test]
fn huge_leaves_translate_with_the_right_offset() {
    let (mm, pt, _) = with_giga_leaf();
    // 1 GiB leaf at va 0x8000_0000: the frame number advances with the address.
    let t = pt.translate(&mm, 0x8000_0000 + 0x12_3456).unwrap();
    assert_eq!(
        t.pfn,
        0x80000 + 0x123,
        "offset within the gigapage in whole frames"
    );
    assert_eq!(t.perms, Perms::RW);
    // The upper-half leaf: va = 0xffff_ffff_8000_0000 + 5 pages
    let t = pt.translate(&mm, 0xffff_ffff_8000_0000 + 5 * 4096).unwrap();
    assert_eq!(t.pfn, 0x40000 + 5);
    assert_eq!(t.perms, Perms::RX);
    assert_eq!(
        pt.translate(&mm, 0x4000_0000),
        None,
        "the slot next to a leaf is empty"
    );
}

#[test]
fn huge_leaves_block_small_mappings_and_cannot_be_unmapped_page_by_page() {
    let (mut mm, mut pt, _) = with_giga_leaf();
    let free = mm.db.free_count();
    assert_eq!(
        pt.map(&mut mm, 0x8000_1000, 7, Perms::R),
        Err(MapError::HugeConflict)
    );
    assert_eq!(
        pt.unmap(&mut mm, 0x8000_1000),
        None,
        "a huge leaf is not ours to unmap one page at a time"
    );
    assert_eq!(
        mm.db.free_count(),
        free,
        "refused operations allocate and free nothing"
    );
    assert_eq!(
        pt.translate(&mm, 0x8000_1000).unwrap().pfn,
        0x80001,
        "still mapped"
    );
}

#[test]
fn leaf_walk_reports_huge_leaves_with_their_level_and_sign_extension() {
    let (mm, pt, _) = with_giga_leaf();
    let mut seen = Vec::new();
    pt.for_each_leaf(&mm, &mut |va, t, level| seen.push((va, t.pfn, level)));
    assert_eq!(
        seen,
        vec![
            (0x8000_0000, 0x80000, 2),
            (0xffff_ffff_8000_0000, 0x40000, 2)
        ]
    );
}

#[test]
fn pte_encodings_for_user_and_exec_only_pages() {
    let mut mm = Mm {
        db: FrameDb::new(1000, 50),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::new(&mut mm).unwrap();
    pt.map(&mut mm, 0x1000, 0x55, Perms::X).unwrap();
    pt.map(&mut mm, 0x2000, 0x66, Perms::RW.user()).unwrap();
    let leaf = |va: u64| -> u64 {
        let mut table = pt.root();
        for level in (1..=2).rev() {
            let e = kcore::pagetable::PhysMem::read_u64(
                &mm.mem,
                (table << 12) + ((va >> (12 + 9 * level)) & 0x1ff) * 8,
            );
            // intermediate entries carry only V
            assert_eq!(e & 0x3ff, 1, "non-leaf entry has flags besides V: {e:#x}");
            table = e >> 10;
        }
        kcore::pagetable::PhysMem::read_u64(&mm.mem, (table << 12) + ((va >> 12) & 0x1ff) * 8)
    };
    // X only: V, X, A, D = bits 0, 3, 6, 7
    assert_eq!(leaf(0x1000) & 0x3ff, 0b00_1100_1001);
    // user read-write: V R W U A D = bits 0, 1, 2, 4, 6, 7
    assert_eq!(leaf(0x2000) & 0x3ff, 0b00_1101_0111);
}

#[test]
fn running_out_of_table_frames_changes_nothing() {
    // Enough for the root only: any map needing new tables must fail cleanly.
    let mut mm = Mm {
        db: FrameDb::new(1000, 1),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::new(&mut mm).unwrap();
    assert_eq!(
        pt.map(&mut mm, 0x1000, 5, Perms::R),
        Err(MapError::NoMemory)
    );
    assert_eq!(mm.db.free_count(), 0);
    mm.db.audit().unwrap();
    assert_eq!(pt.translate(&mm, 0x1000), None);
    pt.destroy(&mut mm);
    assert_eq!(mm.db.free_count(), 1);
    // Two frames: root plus one table is not enough for three levels (needs two more).
    let mut mm = Mm {
        db: FrameDb::new(1000, 2),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::new(&mut mm).unwrap();
    assert_eq!(
        pt.map(&mut mm, 0x1000, 5, Perms::R),
        Err(MapError::NoMemory)
    );
    assert_eq!(
        mm.db.free_count(),
        1,
        "the one spare frame was not consumed by the failed map"
    );
}

/// A block (huge) entry in the second format, written by hand: it translates with the in-block offset, blocks a small mapping
/// under it, cannot be unmapped one page at a time and is reported by the leaf walk with its level.
#[test]
fn testfmt48_block_entries_are_leaves_above_level_zero() {
    use kcore::pagetable::PhysMem;
    let mut mm = Mm {
        db: FrameDb::new(1000, 50),
        mem: FakeMem::default(),
    };
    let mut pt = PageTable::<TestFmt48>::create(&mut mm).unwrap();
    let f3 = mm.db.alloc().unwrap(); // the level-2 table under root entry 0
    for w in 0..512 {
        mm.mem.write_u64((f3 << 12) + w * 8, 0);
    }
    mm.mem
        .write_u64(pt.root() << 12, TestFmt48::table_entry(f3));
    // a 1 GiB block at index 1 of the level-2 table (f3): no "table" bit, readable, user, execute-never
    let block = (7000u64 << 12) | 1 | (1 << 5) | (1 << 7) | (1 << 6) | (1 << 54);
    mm.mem.write_u64((f3 << 12) + 8, block);
    let va = (1u64 << 30) + 0x5000;
    let t = pt.translate(&mm, va).unwrap();
    assert_eq!(
        t.pfn,
        7000 + 5,
        "the in-block offset comes from the address"
    );
    assert_eq!(t.perms, Perms::R.user());
    assert_eq!(
        pt.map(&mut mm, va, 8000, Perms::RW),
        Err(MapError::HugeConflict)
    );
    assert_eq!(
        pt.unmap(&mut mm, va),
        None,
        "a block is not unmapped a page at a time"
    );
    let mut seen = Vec::new();
    pt.for_each_leaf(&mm, &mut |va, t, level| seen.push((va, t.pfn, level)));
    assert_eq!(seen, vec![(1u64 << 30, 7000, 2)]);
}

/// T227's walk oracle, its pure part: `frames_in_range` counts what the model says a range holds (the tables whose span meets it, which
/// are the distinct prefixes of the mapped pages, since empty tables are pruned; and the leaves inside it), over random maps, unmaps and
/// ranges, in both formats. A range over everything counts every frame the table holds but the root.
#[test]
fn the_range_walk_counts_what_the_model_holds_sv39() {
    range_walk_model::<Sv39>();
}

#[test]
fn the_range_walk_counts_what_the_model_holds_testfmt48() {
    range_walk_model::<TestFmt48>();
}

/// The model's count of `[start, end)` (`end` 0: the top): for each level of table below the root, the distinct spans of mapped pages
/// that meet the range, and the mapped pages inside it.
fn model_range<F: PageTableFormat>(
    model: &BTreeMap<u64, (u64, Perms)>,
    start: u64,
    end: u64,
) -> (u64, u64) {
    let meets = |lo: u64, span: u64| {
        let hi = lo.wrapping_add(span);
        !(end != 0 && lo >= end) && !(hi != 0 && hi <= start)
    };
    let mut tables = 0;
    for j in 1..F::LEVELS {
        let shift = 12 + 9 * j;
        let spans: BTreeSet<u64> = model.keys().map(|va| (va >> shift) << shift).collect();
        tables += spans.iter().filter(|&&lo| meets(lo, 1u64 << shift)).count() as u64;
    }
    let pages = model.keys().filter(|&&va| meets(va, 4096)).count() as u64;
    (tables, pages)
}

fn range_walk_model<F: PageTableFormat>() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed ^ 0x7227);
        let mut mm = Mm {
            db: FrameDb::new(1000, 200),
            mem: FakeMem::default(),
        };
        let baseline = mm.db.free_count();
        let mut pt = PageTable::<F>::create(&mut mm).unwrap();
        let mut model: BTreeMap<u64, (u64, Perms)> = BTreeMap::new();
        for step in 0..STEPS {
            let va = address::<F>(&mut rng);
            if rng.below(3) == 0 {
                if pt.unmap(&mut mm, va).is_some() {
                    model.remove(&va);
                }
            } else if pt
                .map(&mut mm, va, 5000 + rng.below(100), Perms::RW)
                .is_ok()
            {
                model.insert(va, (0, Perms::RW));
            }
            // the whole of each half: every table frame but the root (the leaves' frames are not the frame database's here)
            let top = F::extend(1u64 << (12 + 9 * F::LEVELS - 1));
            let lower = pt.frames_in_range(&mm, 0, 1u64 << (12 + 9 * F::LEVELS - 1));
            let upper = pt.frames_in_range(&mm, top, 0);
            assert_eq!(
                (lower.tables + upper.tables) as usize,
                baseline - mm.db.free_count() - 1,
                "seed {seed} step {step}: both halves hold every table but the root"
            );
            assert_eq!(
                upper.pages + lower.pages,
                model.len() as u64,
                "seed {seed} step {step}: the leaves"
            );
            // a random range from two of the model's addresses (or near them): page, 2 MiB and 1 GiB edges
            for _ in 0..4 {
                let a = address::<F>(&mut rng);
                let b = address::<F>(&mut rng);
                let (lo, hi) = (a.min(b), a.max(b).wrapping_add(4096 << (9 * rng.below(3))));
                // (one half at a time, as the function asks)
                if (lo as i64) < 0 && (hi as i64) >= 0 && hi != 0 {
                    continue;
                }
                if (lo as i64) >= 0 && (hi as i64) < 0 {
                    continue;
                }
                let got = pt.frames_in_range(&mm, lo, hi);
                let want = model_range::<F>(&model, lo, hi);
                assert_eq!(
                    (got.tables, got.pages),
                    want,
                    "seed {seed} step {step}: [{lo:#x}, {hi:#x})"
                );
            }
        }
    }
}
