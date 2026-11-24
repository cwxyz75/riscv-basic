//! MM7: range unmap, protect, split and merge against a per-page reference. A window of 24 pages; random map (with a ceiling on the
//! permissions), unmap of any range (holes allowed), protect of any range, and touches. The model knows, for every page, which VMO page
//! it shows with which permissions and ceiling. After every operation: the regions say what the model says, page by page; no two
//! neighbouring regions could have been merged; every translation has the model's permissions and the VMO's own frame; each frame's
//! count is the VMO's reference plus its mappings. At the end everything is returned.
mod common;
use common::{FakeMem, Rng};
use kcore::aspace::{Access, AddressSpace, FaultError, ProtectError, RegionError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::Jobs;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const PAGE: u64 = 4096;
const WIN: u64 = 24;
const BASE: u64 = 0xffff_ffd0_0000_0000;
const VPAGES: u64 = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pg {
    vmo: usize,
    vmo_page: u64,
    perms: Perms,
    max: Perms,
}

fn pm(read: bool, write: bool, exec: bool) -> Perms {
    Perms {
        read,
        write,
        exec,
        user: false,
    }
}

fn random_max(rng: &mut Rng) -> Perms {
    match rng.below(4) {
        0 => pm(true, false, false),
        1 => pm(true, true, false),
        2 => pm(true, false, true),
        _ => pm(true, true, true), // may be raised to write or to execute, never both at once
    }
}

fn random_perms(rng: &mut Rng) -> Perms {
    match rng.below(4) {
        0 => pm(true, false, false),
        1 => pm(true, true, false),
        2 => pm(true, false, true),
        _ => pm(true, true, true), // never allowed: write with execute
    }
}

#[test]
fn range_unmap_protect_split_and_merge_agree_with_a_per_page_model() {
    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 500),
            mem: FakeMem::default(),
        };
        let total = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut jobs = Jobs::new();
        let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
        let ids: Vec<u64> = (0..3).map(|_| vmos.create(&mut jobs, VPAGES)).collect();
        let mut model: Vec<Option<Pg>> = vec![None; WIN as usize];
        for step in 0..500 {
            let ctx = format!("seed {seed} step {step}");
            let s = rng.below(WIN);
            let n = 1 + rng.below((WIN - s).min(8));
            match rng.below(6) {
                0 | 1 => {
                    // map
                    let vi = rng.below(3) as usize;
                    let vp = rng.below(VPAGES - n + 1);
                    let max = random_max(&mut rng);
                    let mut perms = random_perms(&mut rng);
                    if perms.write && perms.exec {
                        perms.exec = false;
                    }
                    perms.write &= max.write;
                    perms.exec &= max.exec;
                    let free = (s..s + n).all(|p| model[p as usize].is_none());
                    let r = a.map_region_max(
                        &mut vmos,
                        BASE + s * PAGE,
                        n,
                        ids[vi],
                        vp,
                        Perms {
                            user: true,
                            ..perms
                        },
                        Perms { user: true, ..max },
                    );
                    if free {
                        r.unwrap();
                        // (the syscall layer merges a new mapping into its neighbours)
                        a.merge_near(
                            &mut mm,
                            &mut vmos,
                            &mut jobs,
                            BASE + s * PAGE,
                            BASE + (s + n) * PAGE,
                        );
                        for k in 0..n {
                            model[(s + k) as usize] = Some(Pg {
                                vmo: vi,
                                vmo_page: vp + k,
                                perms,
                                max,
                            });
                        }
                    } else {
                        assert_eq!(r, Err(RegionError::Overlap), "{ctx}");
                    }
                }
                2 | 3 => {
                    // unmap a range (holes allowed)
                    a.unmap_range(&mut mm, &mut vmos, &mut jobs, BASE + s * PAGE, n * PAGE)
                        .unwrap();
                    for p in s..s + n {
                        model[p as usize] = None;
                    }
                }
                4 => {
                    // protect a range
                    let want = random_perms(&mut rng);
                    let covered = (s..s + n).all(|p| model[p as usize].is_some());
                    let allowed = (s..s + n).all(|p| {
                        model[p as usize].is_none_or(|g| {
                            (!want.write || g.max.write) && (!want.exec || g.max.exec)
                        })
                    });
                    let r = a.protect_range(
                        &mut mm,
                        &mut vmos,
                        &mut jobs,
                        BASE + s * PAGE,
                        n * PAGE,
                        Perms { user: true, ..want },
                    );
                    if want.write && want.exec {
                        assert_eq!(
                            r.unwrap_err(),
                            ProtectError::Denied,
                            "{ctx}: write with execute"
                        );
                    } else if !covered {
                        assert_eq!(r.unwrap_err(), ProtectError::NotMapped, "{ctx}");
                    } else if !allowed {
                        assert_eq!(
                            r.unwrap_err(),
                            ProtectError::Denied,
                            "{ctx}: beyond the ceiling"
                        );
                    } else {
                        r.unwrap();
                        for p in s..s + n {
                            if let Some(g) = model[p as usize].as_mut() {
                                g.perms = want;
                            }
                        }
                    }
                }
                _ => {
                    // touch a page
                    let p = rng.below(WIN);
                    let write = rng.below(2) == 0;
                    let r = a.handle_fault(
                        &mut mm,
                        &mut vmos,
                        &mut jobs,
                        BASE + p * PAGE,
                        if write { Access::Write } else { Access::Read },
                    );
                    match model[p as usize] {
                        None => assert_eq!(r, Err(FaultError::NoRegion), "{ctx}"),
                        Some(g) if write && !g.perms.write => {
                            assert_eq!(
                                r,
                                Err(FaultError::Denied),
                                "{ctx}: a write to a read-only page"
                            )
                        }
                        Some(_) => assert!(r.is_ok(), "{ctx}: {r:?}"),
                    }
                }
            }
            // the regions, page by page, and no two neighbours that could have merged
            let infos = a.regions_info();
            let mut seen: Vec<Option<Pg>> = vec![None; WIN as usize];
            for r in &infos {
                for k in 0..r.pages {
                    let page = (r.start - BASE) / PAGE + k;
                    let vi = ids.iter().position(|&i| i == r.vmo).unwrap();
                    seen[page as usize] = Some(Pg {
                        vmo: vi,
                        vmo_page: r.vmo_page + k,
                        perms: Perms {
                            user: false,
                            ..r.perms
                        },
                        max: Perms {
                            user: false,
                            ..r.max_perms
                        },
                    });
                }
            }
            assert_eq!(
                seen, model,
                "{ctx}: the regions are not what the model says"
            );
            for w in infos.windows(2) {
                let (x, y) = (w[0], w[1]);
                let touching = x.start + x.pages * PAGE == y.start;
                let mergeable = touching
                    && x.vmo == y.vmo
                    && x.vmo_page + x.pages == y.vmo_page
                    && x.perms == y.perms
                    && x.max_perms == y.max_perms;
                assert!(!mergeable, "{ctx}: two regions that should have merged");
            }
            // ML1: every region is in its VMO's list of mappings with the same window, and nothing else is
            vmos.audit_maps(&[&a])
                .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            // translations: the model's permissions, the VMO's frame, and each frame's references
            let mut mappers: std::collections::BTreeMap<u64, u64> = Default::default();
            for p in 0..WIN {
                let t = a.table().translate(&mm, BASE + p * PAGE);
                match (t, model[p as usize]) {
                    (None, _) => {}
                    (Some(t), Some(g)) => {
                        assert_eq!(
                            Perms {
                                user: false,
                                ..t.perms
                            },
                            g.perms,
                            "{ctx}: page {p}'s permissions"
                        );
                        assert_eq!(
                            Some(t.pfn),
                            vmos.frame(ids[g.vmo], g.vmo_page),
                            "{ctx}: page {p}'s frame"
                        );
                        *mappers.entry(t.pfn).or_default() += 1;
                    }
                    (Some(_), None) => panic!("{ctx}: page {p} is mapped but in no region"),
                }
            }
            for (&pfn, &m) in &mappers {
                assert_eq!(mm.db.refs(pfn), Some(1 + m as u32), "{ctx}: frame {pfn}");
            }
        }
        // everything back
        a.unmap_range(&mut mm, &mut vmos, &mut jobs, BASE, WIN * PAGE)
            .unwrap();
        assert_eq!(a.region_count(), 0);
        for id in ids {
            vmos.release(&mut mm, &mut jobs, id).unwrap();
        }
        a.destroy(&mut mm, &mut vmos, &mut jobs);
        assert_eq!(mm.db.free_count(), total, "seed {seed}: no frame lost");
        mm.db.audit().unwrap();
    }
}

/// The region limit (MM7 review): a range operation that needs no split works at the limit, and one that needs a split it cannot make fails
/// before it changes anything (no half-done split, no new generations).
#[test]
fn range_operations_at_the_region_limit_need_exactly_the_room_they_use() {
    use abi::limits::MAX_REGIONS;
    let mut mm = Mm {
        db: FrameDb::new(2000, 50),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let id = vmos.create(&mut jobs, 2);
    // regions of two pages every four pages: each has an interior boundary and the gaps between them are holes
    let at = |i: usize| BASE + (i as u64) * 4 * PAGE;
    for i in 0..MAX_REGIONS - 1 {
        a.map_region(&mut vmos, at(i), 2, id, 0, pm(true, true, false))
            .unwrap();
    }
    let before = a.regions_info();
    // a range from inside one region to inside the next needs two splits, and there is room for one
    assert_eq!(
        a.unmap_range(&mut mm, &mut vmos, &mut jobs, at(3) + PAGE, 4 * PAGE),
        Err(RegionError::TooMany)
    );
    assert_eq!(
        a.protect_range(
            &mut mm,
            &mut vmos,
            &mut jobs,
            at(3) + PAGE,
            4 * PAGE,
            pm(true, false, false)
        )
        .unwrap_err(),
        ProtectError::NotMapped,
        "(the hole between them is checked first)"
    );
    // a protect that is covered but needs two splits: one region, an inner range
    assert_eq!(
        a.protect_range(
            &mut mm,
            &mut vmos,
            &mut jobs,
            at(5),
            PAGE,
            pm(true, false, false)
        )
        .map(|_| ()),
        Ok(()),
        "one split is room enough"
    );
    // (that used the last slot: put it back)
    a.protect_range(
        &mut mm,
        &mut vmos,
        &mut jobs,
        at(5),
        PAGE,
        pm(true, true, false),
    )
    .unwrap();
    assert_eq!(
        a.regions_info(),
        before,
        "protect and back merges to what was there"
    );
    assert_eq!(a.region_count(), MAX_REGIONS - 1);
    // a failed range operation changed nothing
    let _ = a.unmap_range(&mut mm, &mut vmos, &mut jobs, at(3) + PAGE, 4 * PAGE);
    assert_eq!(
        a.regions_info(),
        before,
        "TooMany leaves every region as it was"
    );
    // at the limit itself, a range that lands on region boundaries needs no split and frees a slot
    a.map_region(
        &mut vmos,
        at(MAX_REGIONS - 1),
        2,
        id,
        0,
        pm(true, true, false),
    )
    .unwrap();
    assert_eq!(a.region_count(), MAX_REGIONS);
    assert_eq!(
        a.unmap_range(&mut mm, &mut vmos, &mut jobs, at(7), 2 * PAGE),
        Ok(1),
        "a whole region at the limit"
    );
    assert_eq!(
        a.protect_range(
            &mut mm,
            &mut vmos,
            &mut jobs,
            at(9),
            2 * PAGE,
            pm(true, false, false)
        )
        .map(|s| s.downgraded),
        Ok(false),
        "protecting exactly one region at the limit (nothing mapped: no downgrade to shoot down)"
    );
    assert_eq!(a.region_count(), MAX_REGIONS - 1);
    // and a range that needs a split at the limit is refused
    a.map_region(&mut vmos, at(7), 2, id, 0, pm(true, true, false))
        .unwrap();
    assert_eq!(a.region_count(), MAX_REGIONS);
    assert_eq!(
        a.unmap_range(&mut mm, &mut vmos, &mut jobs, at(11) + PAGE, PAGE),
        Err(RegionError::TooMany),
        "a tail trim that needs a split with no room left (end at the region's own end needs none, so this is a head split)"
    );
}
