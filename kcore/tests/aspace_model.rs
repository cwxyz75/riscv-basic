//! Address spaces, VMOs and the fault path against a reference model.
//!
//! The model knows only: which regions exist (start, pages, vmo, offset, perms),
//! which VMO pages have been touched, and the rule "a fault inside a region whose
//! permissions allow the access succeeds and maps that page; anything else is
//! refused". Frame accounting is checked independently by formula:
//! frames in use = table frames + committed VMO pages + mapped pages that outlive
//! their VMO (the mapping holds its own reference).
mod common;
use common::{FakeMem, Rng, SEEDS, STEPS};
use kcore::aspace::{Access, AddressSpace, FaultError, Vmos};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;
use std::collections::{BTreeMap, BTreeSet};

const REGION_SLOTS: u64 = 6; // regions live at fixed slots so they never overlap by accident
const SLOT_PAGES: u64 = 8;
const BASE: u64 = 0xffff_ffd0_0000_0000; // upper half, an empty top-level slot

fn slot_va(slot: u64) -> u64 {
    // Each slot in its own 2 MiB area: distinct level-0 tables, same level-1 table.
    BASE + slot * (1 << 21)
}

struct ModelRegion {
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    perms: Perms,
}

fn allows(p: Perms, a: Access) -> bool {
    match a {
        Access::Read => p.read,
        Access::Write => p.write,
        Access::Exec => p.exec,
    }
}

#[test]
fn random_faults_regions_and_teardown_agree_with_the_model() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 300),
            mem: FakeMem::default(),
        };
        let baseline = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
        let mut model_regions: BTreeMap<u64, ModelRegion> = BTreeMap::new(); // by slot
        let mut model_vmos: BTreeMap<u64, u64> = BTreeMap::new(); // vmo id -> pages
        let mut committed: BTreeSet<(u64, u64)> = BTreeSet::new(); // (vmo, page)
        let mut mapped: BTreeMap<u64, (u64, u64)> = BTreeMap::new(); // va -> (vmo, page) as faulted in
        let mut orphans: BTreeSet<u64> = BTreeSet::new(); // mapped pages whose vmo was destroyed

        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(8) {
                0 => {
                    let pages = 1 + rng.below(SLOT_PAGES * 2);
                    let id = vmos.create(pages);
                    model_vmos.insert(id, pages);
                }
                1 => {
                    // map a region in a free slot
                    let slot = rng.below(REGION_SLOTS);
                    let Some((&vmo, &vpages)) = model_vmos
                        .iter()
                        .nth(rng.below(model_vmos.len().max(1) as u64) as usize)
                    else {
                        continue;
                    };
                    let pages = 1 + rng.below(SLOT_PAGES);
                    let vmo_page = rng.below(vpages);
                    let perms =
                        [Perms::R, Perms::RW, Perms::RX, Perms::RW.user()][rng.below(4) as usize];
                    let fits = vmo_page + pages <= vpages;
                    let result =
                        aspace.map_region(&vmos, slot_va(slot), pages, vmo, vmo_page, perms);
                    if model_regions.contains_key(&slot) || !fits {
                        assert!(result.is_err(), "{ctx}: bad map_region accepted");
                    } else {
                        assert_eq!(result, Ok(()), "{ctx}");
                        model_regions.insert(
                            slot,
                            ModelRegion {
                                pages,
                                vmo,
                                vmo_page,
                                perms,
                            },
                        );
                    }
                }
                2..=4 => {
                    // a fault, mostly inside regions, sometimes just outside
                    let slot = rng.below(REGION_SLOTS);
                    let page = rng.below(SLOT_PAGES + 2);
                    let va = slot_va(slot) + page * 4096 + rng.below(4096);
                    let access = [Access::Read, Access::Write, Access::Exec][rng.below(3) as usize];
                    let result = aspace.handle_fault(&mut mm, &mut vmos, va, access);
                    let region = model_regions.get(&slot).filter(|r| page < r.pages);
                    match region {
                        None => assert_eq!(
                            result,
                            Err(FaultError::NoRegion),
                            "{ctx}: fault outside any region"
                        ),
                        Some(r) if !allows(r.perms, access) => {
                            assert_eq!(result, Err(FaultError::Denied), "{ctx}: access not allowed")
                        }
                        Some(r) if mapped.contains_key(&(slot_va(slot) + page * 4096)) => {
                            assert_eq!(result, Ok(()), "{ctx}: a fault on an already mapped page is spurious, not an error");
                            let _ = r;
                        }
                        Some(r) if !model_vmos.contains_key(&r.vmo) => {
                            assert_eq!(
                                result,
                                Err(FaultError::VmoGone),
                                "{ctx}: the region's vmo was destroyed"
                            )
                        }
                        Some(r) => {
                            if result == Err(FaultError::NoMemory) {
                                // Legitimate under pressure; nothing may have changed (checked below).
                            } else {
                                assert_eq!(result, Ok(()), "{ctx}");
                                let vp = r.vmo_page + page;
                                committed.insert((r.vmo, vp));
                                mapped.insert(slot_va(slot) + page * 4096, (r.vmo, vp));
                                let t = aspace
                                    .table()
                                    .translate(&mm, va)
                                    .expect("mapped after fault");
                                assert_eq!(
                                    t.perms, r.perms,
                                    "{ctx}: page permissions are the region's"
                                );
                            }
                        }
                    }
                }
                5 => {
                    // unmap a region
                    let slot = rng.below(REGION_SLOTS);
                    let result = aspace.unmap_region(&mut mm, slot_va(slot));
                    match model_regions.remove(&slot) {
                        Some(r) => {
                            assert_eq!(result, Ok(()), "{ctx}");
                            for p in 0..r.pages {
                                let va = slot_va(slot) + p * 4096;
                                mapped.remove(&va);
                                orphans.remove(&va);
                                assert!(
                                    aspace.table().translate(&mm, va).is_none(),
                                    "{ctx}: still mapped"
                                );
                            }
                        }
                        None => assert!(result.is_err(), "{ctx}: unmapping nothing succeeded"),
                    }
                }
                6 => {
                    // destroy a VMO (its mappings keep their own frame references)
                    let Some((&id, _)) = model_vmos
                        .iter()
                        .nth(rng.below(model_vmos.len().max(1) as u64) as usize)
                    else {
                        continue;
                    };
                    vmos.destroy(&mut mm, id).unwrap();
                    model_vmos.remove(&id);
                    committed.retain(|&(v, _)| v != id);
                    for (&va, &(v, _)) in &mapped {
                        if v == id {
                            orphans.insert(va);
                        }
                    }
                }
                _ => {}
            }
            // Independent frame accounting by formula.
            let l1: BTreeSet<u64> = mapped.keys().map(|va| (va >> 21) & 0x3_ffff).collect();
            let l2: BTreeSet<u64> = mapped.keys().map(|va| (va >> 30) & 0x1ff).collect();
            let tables = 1 + if mapped.is_empty() {
                0
            } else {
                l1.len() + l2.len()
            };
            // Frames referenced by both a VMO and a mapping count once, so count distinct frames.
            let mut data_frames = BTreeSet::new();
            for &(v, p) in &committed {
                data_frames.insert(vmos.frame(v, p).expect("committed page has a frame"));
            }
            for &va in &orphans {
                data_frames.insert(aspace.table().translate(&mm, va).unwrap().pfn);
            }
            assert_eq!(
                baseline - mm.db.free_count(),
                tables + data_frames.len(),
                "{ctx}: frames in use (tables {tables}, data {})",
                data_frames.len()
            );
            mm.db.audit().unwrap();
        }
        // Tear everything down: back to baseline.
        for slot in 0..REGION_SLOTS {
            let _ = aspace.unmap_region(&mut mm, slot_va(slot));
        }
        for id in model_vmos.keys().copied().collect::<Vec<_>>() {
            vmos.destroy(&mut mm, id).unwrap();
        }
        aspace.destroy(&mut mm);
        assert_eq!(
            mm.db.free_count(),
            baseline,
            "seed {seed}: not back to baseline"
        );
    }
}

#[test]
fn a_lazy_page_is_zero_and_faults_in_exactly_once() {
    let mut mm = Mm {
        db: FrameDb::new(2000, 50),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(4);
    aspace.map_region(&vmos, BASE, 4, id, 0, Perms::RW).unwrap();
    let before = mm.db.free_count();
    assert_eq!(
        aspace.table().translate(&mm, BASE),
        None,
        "nothing is mapped until touched"
    );
    aspace
        .handle_fault(&mut mm, &mut vmos, BASE + 8, Access::Read)
        .unwrap();
    let t = aspace.table().translate(&mm, BASE).unwrap();
    for word in 0..512 {
        assert_eq!(
            kcore::pagetable::PhysMem::read_u64(&mm.mem, (t.pfn << 12) + word * 8),
            0,
            "lazy pages are zeroed"
        );
    }
    let after_first = mm.db.free_count();
    assert!(after_first < before, "the fault allocated");
    aspace
        .handle_fault(&mut mm, &mut vmos, BASE + 16, Access::Write)
        .unwrap();
    assert_eq!(
        mm.db.free_count(),
        after_first,
        "a second fault on a mapped page allocates nothing"
    );
}

#[test]
fn mappings_keep_frames_alive_after_the_vmo_goes() {
    let mut mm = Mm {
        db: FrameDb::new(2000, 50),
        mem: FakeMem::default(),
    };
    let baseline = mm.db.free_count();
    let mut vmos = Vmos::new();
    let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(1);
    aspace.map_region(&vmos, BASE, 1, id, 0, Perms::R).unwrap();
    aspace
        .handle_fault(&mut mm, &mut vmos, BASE, Access::Read)
        .unwrap();
    let pfn = aspace.table().translate(&mm, BASE).unwrap().pfn;
    assert_eq!(
        mm.db.refs(pfn),
        Some(2),
        "one reference for the VMO, one for the mapping"
    );
    vmos.destroy(&mut mm, id).unwrap();
    assert_eq!(
        mm.db.refs(pfn),
        Some(1),
        "the mapping still holds the frame"
    );
    aspace.unmap_region(&mut mm, BASE).unwrap();
    assert_eq!(mm.db.refs(pfn), None, "and now it is free");
    aspace.destroy(&mut mm);
    assert_eq!(mm.db.free_count(), baseline);
}

fn setup(frames: usize) -> (Mm<FakeMem>, Vmos, AddressSpace) {
    let mut mm = Mm {
        db: FrameDb::new(2000, frames),
        mem: FakeMem::default(),
    };
    let vmos = Vmos::new();
    let aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    (mm, vmos, aspace)
}

#[test]
fn region_arguments_are_checked_with_specific_errors() {
    use kcore::aspace::{RegionError, MAX_REGION_PAGES};
    let (mut mm, mut vmos, mut a) = setup(40);
    let id = vmos.create(8);
    let r = |a: &mut AddressSpace, start, pages, vmo, off, p| {
        a.map_region(&vmos, start, pages, vmo, off, p)
    };
    assert_eq!(
        r(&mut a, BASE + 1, 1, id, 0, Perms::R),
        Err(RegionError::Misaligned)
    );
    assert_eq!(
        r(&mut a, 0x0000_0040_0000_0000, 1, id, 0, Perms::R),
        Err(RegionError::NonCanonical)
    );
    assert_eq!(
        r(&mut a, BASE, 0, id, 0, Perms::R),
        Err(RegionError::BadRange)
    );
    assert_eq!(
        r(&mut a, BASE, MAX_REGION_PAGES + 1, id, 0, Perms::R),
        Err(RegionError::BadRange)
    );
    assert_eq!(
        r(&mut a, u64::MAX - 4095, 2, id, 0, Perms::R),
        Err(RegionError::BadRange),
        "wraps"
    );
    assert_eq!(
        r(&mut a, 0x3f_ffff_f000, 2, id, 0, Perms::R),
        Err(RegionError::BadRange),
        "crosses the Sv39 hole"
    );
    let rwx = Perms {
        read: true,
        write: true,
        exec: true,
        user: false,
    };
    assert!(matches!(
        r(&mut a, BASE, 1, id, 0, rwx),
        Err(RegionError::BadPerms(_))
    ));
    assert_eq!(
        r(&mut a, BASE, 1, 999, 0, Perms::R),
        Err(RegionError::NoSuchVmo)
    );
    assert_eq!(
        r(&mut a, BASE, 9, id, 0, Perms::R),
        Err(RegionError::OutsideVmo)
    );
    assert_eq!(
        r(&mut a, BASE, 2, id, 7, Perms::R),
        Err(RegionError::OutsideVmo)
    );
    assert_eq!(
        r(&mut a, BASE, 2, id, u64::MAX, Perms::R),
        Err(RegionError::OutsideVmo),
        "offset overflow"
    );
    // Overlap in every direction.
    assert_eq!(r(&mut a, BASE + 4 * 4096, 4, id, 0, Perms::R), Ok(()));
    assert_eq!(
        r(&mut a, BASE + 6 * 4096, 4, id, 0, Perms::R),
        Err(RegionError::Overlap),
        "starts inside"
    );
    assert_eq!(
        r(&mut a, BASE + 2 * 4096, 3, id, 0, Perms::R),
        Err(RegionError::Overlap),
        "ends inside"
    );
    assert_eq!(
        r(&mut a, BASE + 3 * 4096, 7, id, 0, Perms::R),
        Err(RegionError::Overlap),
        "swallows it"
    );
    assert_eq!(
        r(&mut a, BASE + 5 * 4096, 1, id, 0, Perms::R),
        Err(RegionError::Overlap),
        "inside it"
    );
    assert_eq!(
        r(&mut a, BASE, 4, id, 0, Perms::R),
        Ok(()),
        "touching is not overlapping"
    );
    assert_eq!(r(&mut a, BASE + 8 * 4096, 1, id, 0, Perms::R), Ok(()));
    a.destroy(&mut mm);
}

#[test]
fn a_fault_the_existing_mapping_cannot_satisfy_is_refused_not_retried_forever() {
    let (mut mm, mut vmos, mut a) = setup(40);
    let id = vmos.create(2);
    // The region says RW, but the page tables already hold a read-only mapping there
    // (as if some other code had mapped it).
    a.map_region(&vmos, BASE, 2, id, 0, Perms::RW).unwrap();
    let mut pt2 = PageTable::from_root(a.table().root());
    pt2.map(&mut mm, BASE, 3000, Perms::R).unwrap();
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, BASE, Access::Read),
        Ok(()),
        "spurious read: fine"
    );
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, BASE, Access::Write),
        Err(FaultError::Denied),
        "a write the mapping cannot do must not 'succeed' and re-fault forever"
    );
    pt2.unmap(&mut mm, BASE);
    a.destroy(&mut mm);
}

#[test]
fn running_out_of_frames_changes_nothing_and_leaks_nothing() {
    // Root (1) + two tables + one data page = 4 frames; give it 3 so the first
    // fault runs out part-way, and check every kind of state after each attempt.
    let (mut mm, mut vmos, mut a) = setup(3);
    let id = vmos.create(4);
    a.map_region(&vmos, BASE, 4, id, 0, Perms::RW).unwrap();
    let free = mm.db.free_count();
    for _ in 0..3 {
        let result = a.handle_fault(&mut mm, &mut vmos, BASE, Access::Write);
        assert_eq!(result, Err(FaultError::NoMemory));
        mm.db.audit().unwrap();
        assert!(
            a.table().translate(&mm, BASE).is_none(),
            "nothing was mapped"
        );
        // The VMO may have committed its page (it keeps it, owning one reference);
        // the free count must not shrink further on retries.
        assert!(mm.db.free_count() <= free);
    }
    let stable = mm.db.free_count();
    let _ = a.handle_fault(&mut mm, &mut vmos, BASE, Access::Write);
    assert_eq!(
        mm.db.free_count(),
        stable,
        "retrying a failing fault does not leak"
    );
    a.unmap_region(&mut mm, BASE).unwrap();
    vmos.destroy(&mut mm, id).unwrap();
    a.destroy(&mut mm);
    assert_eq!(mm.db.free_count(), 3, "everything came back");
}
