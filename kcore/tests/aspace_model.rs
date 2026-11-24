//! Address spaces, VMOs and the fault path against a reference model.
//!
//! The model knows only: which regions exist (start, pages, vmo, offset, perms),
//! which VMO pages have been touched, and the rule "a fault inside a region whose
//! permissions allow the access succeeds and maps that page; anything else is
//! refused". Frame accounting is checked independently by formula:
//! frames in use = table frames + committed VMO pages + mapped pages that outlive
//! their VMO (the mapping holds its own reference).
mod common;
use common::{FakeMem, Rng, TestFmt48, SEEDS, STEPS};
use kcore::aspace::{Access, AddressSpace, FaultError, Vmos};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, PageTableFormat, Perms, Sv39};
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
fn random_faults_regions_and_teardown_agree_with_the_model_sv39() {
    random_model::<Sv39>();
}

#[test]
fn random_faults_regions_and_teardown_agree_with_the_model_testfmt48() {
    random_model::<TestFmt48>();
}

/// Drop from the model the VMOs nothing holds any more (no creator, no region), and check the store agrees.
fn free_unheld(
    model_vmos: &mut BTreeMap<u64, u64>,
    creator: &BTreeSet<u64>,
    regions: &BTreeMap<u64, ModelRegion>,
    committed: &mut BTreeSet<(u64, u64)>,
    vmos: &Vmos,
    ctx: &str,
) {
    let gone: Vec<u64> = model_vmos
        .keys()
        .copied()
        .filter(|id| !creator.contains(id) && !regions.values().any(|r| r.vmo == *id))
        .collect();
    for id in gone {
        model_vmos.remove(&id);
        committed.retain(|&(v, _)| v != id);
        assert!(!vmos.exists(id), "{ctx}: a VMO nothing holds was kept");
    }
    for &id in model_vmos.keys() {
        assert!(vmos.exists(id), "{ctx}: a held VMO was freed");
    }
}

fn random_model<F: PageTableFormat>() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 300),
            mem: FakeMem::default(),
        };
        let baseline = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut jobs = kcore::job::Jobs::new();
        let mut aspace = AddressSpace::new(PageTable::<F>::create(&mut mm).unwrap());
        let mut model_regions: BTreeMap<u64, ModelRegion> = BTreeMap::new(); // by slot
        let mut model_vmos: BTreeMap<u64, u64> = BTreeMap::new(); // vmo id -> pages (the VMOs in the store: held by their creator or a region)
        let mut creator: BTreeSet<u64> = BTreeSet::new(); // the VMOs whose creator has not let go
        let mut committed: BTreeSet<(u64, u64)> = BTreeSet::new(); // (vmo, page)
        let mut mapped: BTreeMap<u64, (u64, u64)> = BTreeMap::new(); // va -> (vmo, page) as faulted in

        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(8) {
                0 => {
                    let pages = 1 + rng.below(SLOT_PAGES * 2);
                    let id = vmos.create(&mut jobs, pages);
                    model_vmos.insert(id, pages);
                    creator.insert(id);
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
                        aspace.map_region(&mut vmos, slot_va(slot), pages, vmo, vmo_page, perms);
                    if model_regions.contains_key(&slot) || !fits {
                        assert!(result.is_err(), "{ctx}: bad map_region accepted");
                    } else {
                        assert_eq!(result, Ok(()), "{ctx}");
                        let holders = u32::from(creator.contains(&vmo))
                            + 1
                            + model_regions.values().filter(|r| r.vmo == vmo).count() as u32;
                        assert_eq!(vmos.refs(vmo), Some(holders), "{ctx}: refs");
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
                    let result = aspace.handle_fault(&mut mm, &mut vmos, &mut jobs, va, access);
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
                        Some(r) => {
                            if result == Err(FaultError::NoMemory) {
                                // Legitimate under pressure; nothing may have changed (checked below).
                            } else {
                                assert_eq!(result, Ok(()), "{ctx}");
                                let vp = r.vmo_page + page;
                                committed.insert((r.vmo, vp));
                                mapped.insert(slot_va(slot) + page * 4096, (r.vmo, vp));
                                // fault-around (MM1): the committed pages around it in its 64 KiB window and region are mapped
                                // too, and the others are not (a fault never commits a page for a neighbour)
                                let fva = slot_va(slot) + page * 4096;
                                let win = fva & !0xffff;
                                for q in 0..16u64 {
                                    let qva = win + q * 4096;
                                    let in_region = qva >= slot_va(slot)
                                        && qva < slot_va(slot) + r.pages * 4096;
                                    if !in_region || qva == fva {
                                        continue;
                                    }
                                    let qv = r.vmo_page + (qva - slot_va(slot)) / 4096;
                                    let was = mapped.contains_key(&qva);
                                    let now = aspace.table().translate(&mm, qva).is_some();
                                    if !was && now {
                                        assert!(
                                            committed.contains(&(r.vmo, qv)),
                                            "{ctx}: fault-around mapped a page the VMO had not committed"
                                        );
                                        mapped.insert(qva, (r.vmo, qv));
                                    }
                                    if !was && !now {
                                        assert!(
                                            !committed.contains(&(r.vmo, qv)) || vmos.frame(r.vmo, qv).is_none(),
                                            "{ctx}: a committed neighbour in the window was not mapped"
                                        );
                                    }
                                }
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
                    let result = aspace.unmap_region(&mut mm, &mut vmos, &mut jobs, slot_va(slot));
                    match model_regions.remove(&slot) {
                        Some(r) => {
                            assert_eq!(result, Ok(()), "{ctx}");
                            for p in 0..r.pages {
                                let va = slot_va(slot) + p * 4096;
                                mapped.remove(&va);
                                assert!(
                                    aspace.table().translate(&mm, va).is_none(),
                                    "{ctx}: still mapped"
                                );
                            }
                        }
                        None => assert!(result.is_err(), "{ctx}: unmapping nothing succeeded"),
                    }
                    // (the region's reference on its VMO went: the VMO is freed if nothing else holds it)
                    free_unheld(
                        &mut model_vmos,
                        &creator,
                        &model_regions,
                        &mut committed,
                        &vmos,
                        &ctx,
                    );
                }
                6 => {
                    // the creator lets go of a VMO: it lives on while a region maps it
                    let Some(&id) = creator
                        .iter()
                        .nth(rng.below(creator.len().max(1) as u64) as usize)
                    else {
                        continue;
                    };
                    vmos.release(&mut mm, &mut jobs, id).unwrap();
                    creator.remove(&id);
                    free_unheld(
                        &mut model_vmos,
                        &creator,
                        &model_regions,
                        &mut committed,
                        &vmos,
                        &ctx,
                    );
                }
                _ => {}
            }
            // Independent frame accounting by formula.
            let mut below_root = 0;
            for j in 1..F::LEVELS {
                let mask = (1u64 << (9 * (F::LEVELS - j))) - 1;
                let prefixes: BTreeSet<u64> = mapped
                    .keys()
                    .map(|va| (va >> (12 + 9 * j)) & mask)
                    .collect();
                below_root += prefixes.len();
            }
            let tables = 1 + below_root;
            // Frames referenced by both a VMO and a mapping count once, so count distinct frames.
            let mut data_frames = BTreeSet::new();
            for &(v, p) in &committed {
                data_frames.insert(vmos.frame(v, p).expect("committed page has a frame"));
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
            let _ = aspace.unmap_region(&mut mm, &mut vmos, &mut jobs, slot_va(slot));
        }
        for id in creator.iter().copied().collect::<Vec<_>>() {
            vmos.release(&mut mm, &mut jobs, id).unwrap();
        }
        assert_eq!(vmos.count(), 0, "seed {seed}: a VMO is left");
        aspace.destroy(&mut mm, &mut vmos, &mut jobs);
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
    let mut jobs = kcore::job::Jobs::new();
    let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(&mut jobs, 4);
    aspace
        .map_region(&mut vmos, BASE, 4, id, 0, Perms::RW)
        .unwrap();
    let before = mm.db.free_count();
    assert_eq!(
        aspace.table().translate(&mm, BASE),
        None,
        "nothing is mapped until touched"
    );
    aspace
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 8, Access::Read)
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
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 16, Access::Write)
        .unwrap();
    assert_eq!(
        mm.db.free_count(),
        after_first,
        "a second fault on a mapped page allocates nothing"
    );
}

#[test]
fn a_mapping_keeps_its_vmo_and_frames_alive_after_the_creator_goes() {
    let mut mm = Mm {
        db: FrameDb::new(2000, 50),
        mem: FakeMem::default(),
    };
    let baseline = mm.db.free_count();
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(&mut jobs, 1);
    assert_eq!(vmos.refs(id), Some(1), "the creator's reference");
    aspace
        .map_region(&mut vmos, BASE, 1, id, 0, Perms::R)
        .unwrap();
    assert_eq!(vmos.refs(id), Some(2), "and the region's");
    aspace
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Read)
        .unwrap();
    let pfn = aspace.table().translate(&mm, BASE).unwrap().pfn;
    assert_eq!(
        mm.db.refs(pfn),
        Some(2),
        "one reference for the VMO, one for the mapping"
    );
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert!(vmos.exists(id), "the region still holds the VMO");
    assert_eq!(mm.db.refs(pfn), Some(2), "and the VMO its page");
    // a page nobody has touched yet can still be faulted in: the VMO is there
    aspace
        .unmap_region(&mut mm, &mut vmos, &mut jobs, BASE)
        .unwrap();
    assert!(!vmos.exists(id), "the last holder went: the VMO is freed");
    assert_eq!(mm.db.refs(pfn), None, "and so is its frame");
    aspace.destroy(&mut mm, &mut vmos, &mut jobs);
    assert_eq!(mm.db.free_count(), baseline);
}

#[test]
fn a_vmo_closed_before_a_region_of_it_faults_still_serves_the_fault() {
    let mut mm = Mm {
        db: FrameDb::new(2000, 50),
        mem: FakeMem::default(),
    };
    let baseline = mm.db.free_count();
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let mut aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(&mut jobs, 2);
    aspace
        .map_region(&mut vmos, BASE, 2, id, 0, Perms::RW)
        .unwrap();
    vmos.release(&mut mm, &mut jobs, id).unwrap(); // the handle is closed with nothing faulted in yet
    aspace
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 4096, Access::Write)
        .unwrap();
    aspace.destroy(&mut mm, &mut vmos, &mut jobs);
    assert!(!vmos.exists(id));
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
    use kcore::aspace::RegionError;
    let (mut mm, mut vmos, mut a) = setup(40);
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 8);
    let mut r = |a: &mut AddressSpace, start, pages, vmo, off, p| {
        a.map_region(&mut vmos, start, pages, vmo, off, p)
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
        r(&mut a, BASE, 1 << 40, id, 0, Perms::R),
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
    a.destroy(&mut mm, &mut vmos, &mut jobs);
}

#[test]
fn a_fault_the_translation_cannot_satisfy_but_the_region_allows_maps_the_page_afresh() {
    // (a chunked protect changes the region's permissions first and the translations a step at a time: a fault in between must neither be
    // refused nor retried forever; it replaces the stale translation with one the region allows)
    let (mut mm, mut vmos, mut a) = setup(40);
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 2);
    let rx = Perms {
        exec: true,
        ..Perms::R
    };
    a.map_region(&mut vmos, BASE, 2, id, 0, rx).unwrap();
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Read)
        .unwrap();
    let mut pt2 = PageTable::from_root(a.table().root());
    pt2.protect(&mut mm, BASE, Perms::R).unwrap();
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Exec),
        Ok(()),
        "the region allows exec: the stale read-only translation is replaced"
    );
    assert!(a.table().translate(&mm, BASE).unwrap().perms.exec);
    // an access the region itself refuses stays refused
    let ro = Perms::R;
    let id2 = vmos.create(&mut jobs, 1);
    a.map_region(&mut vmos, BASE + 0x10_0000, 1, id2, 0, ro)
        .unwrap();
    assert_eq!(
        a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 0x10_0000,
            Access::Exec
        ),
        Err(FaultError::Denied)
    );
    a.destroy(&mut mm, &mut vmos, &mut jobs);
}

#[test]
fn running_out_of_frames_changes_nothing_and_leaks_nothing() {
    // Root (1) + two tables + one data page = 4 frames; give it 3 so the first
    // fault runs out part-way, and check every kind of state after each attempt.
    let (mut mm, mut vmos, mut a) = setup(3);
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 4);
    a.map_region(&mut vmos, BASE, 4, id, 0, Perms::RW).unwrap();
    let free = mm.db.free_count();
    for _ in 0..3 {
        let result = a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write);
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
    let _ = a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write);
    assert_eq!(
        mm.db.free_count(),
        stable,
        "retrying a failing fault does not leak"
    );
    a.unmap_region(&mut mm, &mut vmos, &mut jobs, BASE).unwrap();
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    a.destroy(&mut mm, &mut vmos, &mut jobs);
    assert_eq!(mm.db.free_count(), 3, "everything came back");
}

#[test]
fn committed_pages_are_charged_to_the_vmos_job_and_credited_back() {
    use kcore::job::{JobId, Limits, Resource, UNLIMITED};
    let (mut mm, mut vmos, mut aspace) = setup(200);
    let mut jobs = kcore::job::Jobs::new();
    let job = jobs
        .create(
            JobId::ROOT,
            Limits {
                processes: UNLIMITED,
                threads: UNLIMITED,
                pages: 3,
                jobs: UNLIMITED,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let id = vmos.create_in(&mut jobs, 8, job);
    aspace
        .map_region(&mut vmos, BASE, 8, id, 0, Perms::RW)
        .unwrap();
    let free_before = mm.db.free_count();
    // three pages fit; the fourth fault is refused, allocates nothing and charges nothing
    for page in 0..3 {
        aspace
            .handle_fault(
                &mut mm,
                &mut vmos,
                &mut jobs,
                BASE + page * 4096,
                Access::Write,
            )
            .unwrap();
    }
    assert_eq!(jobs.usage(job).unwrap().pages, 3);
    let free_full = mm.db.free_count();
    assert_eq!(
        aspace.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 3 * 4096,
            Access::Write
        ),
        Err(FaultError::NoMemory)
    );
    assert_eq!(jobs.usage(job).unwrap().pages, 3);
    assert_eq!(mm.db.free_count(), free_full);
    // a page that is already committed is not charged again, and the root sees the same total
    aspace
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 8, Access::Read)
        .unwrap();
    assert_eq!(jobs.usage(JobId::ROOT).unwrap().pages, 3);
    // another VMO in the same job shares the allowance; one in the root job does not
    let other = vmos.create_in(&mut jobs, 2, job);
    aspace
        .map_region(&mut vmos, BASE + 16 * 4096, 2, other, 0, Perms::RW)
        .unwrap();
    assert_eq!(
        aspace.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 16 * 4096,
            Access::Write
        ),
        Err(FaultError::NoMemory)
    );
    let free_root = vmos.create(&mut jobs, 1);
    aspace
        .map_region(&mut vmos, BASE + 32 * 4096, 1, free_root, 0, Perms::RW)
        .unwrap();
    aspace
        .handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 32 * 4096,
            Access::Write,
        )
        .unwrap();
    // a VMO is freed, crediting what it committed and letting go of the job, when its last holder goes: the creator, and (L15) the region that
    // maps it
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(
        jobs.usage(job).unwrap().pages,
        3,
        "the creator let go, the region still holds the VMO"
    );
    aspace
        .unmap_region(&mut mm, &mut vmos, &mut jobs, BASE)
        .unwrap();
    assert_eq!(jobs.usage(job).unwrap().pages, 0);
    // (the root job has no limit: the address space prepaid a batch from it, which counts as usage until it is released)
    assert_eq!(
        jobs.usage(JobId::ROOT).unwrap().pages,
        1 + aspace.reserved_pages()
    );
    assert_eq!(
        aspace.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 16 * 4096,
            Access::Write
        ),
        Ok(()),
        "room again after the first VMO went"
    );
    let _ = (free_before, Resource::Pages);
    aspace
        .unmap_region(&mut mm, &mut vmos, &mut jobs, BASE + 16 * 4096)
        .unwrap(); // (the region held `other` too)
    vmos.release(&mut mm, &mut jobs, other).unwrap();
    jobs.release(job);
    assert!(!jobs.exists(job), "the last use was the creator's");
}

#[test]
fn running_out_of_frames_gives_the_page_charge_back() {
    use kcore::job::{JobId, Limits};
    let (mut mm, mut vmos, mut aspace) = setup(7); // enough for the tables, almost nothing else
    let mut jobs = kcore::job::Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 64, job);
    aspace
        .map_region(&mut vmos, BASE, 64, id, 0, Perms::RW)
        .unwrap();
    let mut ok = 0;
    for page in 0..64 {
        match aspace.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + page * 4096,
            Access::Write,
        ) {
            Ok(()) => ok += 1,
            Err(e) => {
                assert_eq!(e, FaultError::NoMemory);
                break;
            }
        }
    }
    assert!(ok < 64, "the frames ran out");
    assert_eq!(
        jobs.usage(job).unwrap().pages,
        ok + aspace.reserved_pages(),
        "a refused page is not charged: the committed pages and the prepaid ones are all there is"
    );
    aspace.release_reservation(&mut jobs);
    assert_eq!(
        jobs.usage(job).unwrap().pages,
        ok,
        "and the prepaid remainder goes back"
    );
}

/// MM1: touching committed pages in order takes at most one fault per 64 KiB window (fault-around maps the committed neighbours),
/// and touching uncommitted pages takes one fault each (a fault never commits a page for its neighbours).
fn touch_in_order(committed_first: bool) -> usize {
    let (mut mm, mut vmos, mut aspace) = setup(400);
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 64);
    if committed_first {
        vmos.commit(&mut mm, &mut jobs, id, 0, 64).unwrap();
    }
    aspace
        .map_region(&mut vmos, BASE, 64, id, 0, Perms::RW)
        .unwrap();
    let mut faults = 0;
    for page in 0..64u64 {
        let va = BASE + page * 4096;
        if aspace.table().translate(&mm, va).is_none() {
            faults += 1;
            aspace
                .handle_fault(&mut mm, &mut vmos, &mut jobs, va, Access::Write)
                .unwrap();
        }
    }
    faults
}

#[test]
fn fault_around_maps_committed_neighbours_and_never_commits_one() {
    assert!(
        touch_in_order(true) <= 5,
        "64 committed pages touched in order took {} faults",
        touch_in_order(true)
    );
    assert_eq!(
        touch_in_order(false),
        64,
        "an uncommitted page costs its own fault"
    );
}

#[test]
fn fault_around_stays_inside_the_region_and_the_window() {
    let (mut mm, mut vmos, mut aspace) = setup(400);
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 64);
    vmos.commit(&mut mm, &mut jobs, id, 0, 64).unwrap();
    // a region of 5 pages that starts 3 pages into a 64 KiB window and runs to the window's end: BASE is 64 KiB aligned
    aspace
        .map_region(&mut vmos, BASE + 3 * 4096, 5, id, 10, Perms::RW)
        .unwrap();
    aspace
        .handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 5 * 4096, Access::Read)
        .unwrap();
    for page in 0..16u64 {
        let mapped = aspace.table().translate(&mm, BASE + page * 4096).is_some();
        assert_eq!(
            mapped,
            (3..8).contains(&page),
            "page {page} of the window: only the region's own pages are mapped"
        );
    }
    // the next window is untouched
    assert!(aspace.table().translate(&mm, BASE + 16 * 4096).is_none());
}
