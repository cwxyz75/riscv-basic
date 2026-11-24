//! T207 PR 3b: `process_unmap` goes in steps (`unmap_begin`, `unmap_step` until done). Between steps the regions are *dying*: a fault on one is
//! `NoRegion`, a map over one is an overlap, a protect finds nothing there, a map-info skips it; the list of mappings (ML1) holds at every step;
//! and the whole is what the one-shot unmap does.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, FaultError, RegionError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0x10_0000_0000;
const PAGE: u64 = 4096;

fn world() -> (Mm<FakeMem>, Vmos, Jobs, AddressSpace, JobId) {
    let mut mm = Mm {
        db: FrameDb::new(4000, 3000),
        mem: FakeMem::default(),
    };
    let vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    (mm, vmos, jobs, a, job)
}

/// Two regions of 300 pages each, every page touched, with a one-page hole between.
fn fill(
    mm: &mut Mm<FakeMem>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    a: &mut AddressSpace,
    job: JobId,
) -> Vec<u64> {
    let mut ids = Vec::new();
    for i in 0..2u64 {
        let id = vmos.create_in(jobs, 300, job);
        let at = BASE + i * 301 * PAGE;
        a.map_region(vmos, at, 300, id, 0, Perms::RW).unwrap();
        for p in 0..300 {
            a.handle_fault(mm, vmos, jobs, at + p * PAGE, Access::Write)
                .unwrap();
        }
        ids.push(id);
    }
    ids
}

#[test]
fn dying_regions_are_no_regions_to_faults_maps_and_protects_and_the_list_holds() {
    let (mut mm, mut vmos, mut jobs, mut a, job) = world();
    let ids = fill(&mut mm, &mut vmos, &mut jobs, &mut a, job);
    assert_eq!(a.unmap_begin(&mut vmos, BASE, 300 * PAGE), Ok(1));
    // a fault in the dying region
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 5 * PAGE, Access::Read),
        Err(FaultError::NoRegion)
    );
    // the other region still works
    a.handle_fault(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE + 301 * PAGE,
        Access::Read,
    )
    .unwrap();
    // a map over it is in the way
    let other = vmos.create_in(&mut jobs, 4, job);
    assert_eq!(
        a.map_region(&mut vmos, BASE + 10 * PAGE, 4, other, 0, Perms::RW),
        Err(RegionError::Overlap)
    );
    // map-info skips it
    assert_eq!(a.region_at_or_after(BASE).unwrap().start, BASE + 301 * PAGE);
    let (mut steps, mut cleared_all) = (0, 0u64);
    loop {
        vmos.audit_maps(&[&a]).unwrap();
        let (done, cleared) = a.unmap_step(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE,
            BASE + 300 * PAGE,
            64,
            2048,
        );
        cleared_all += cleared;
        steps += 1;
        assert!(cleared <= 64);
        if done {
            break;
        }
    }
    vmos.audit_maps(&[&a]).unwrap();
    assert_eq!(steps, 300usize.div_ceil(64));
    assert_eq!(cleared_all, 300);
    assert_eq!(a.region_count(), 1);
    // the freed range can be mapped again
    a.map_region(&mut vmos, BASE + 10 * PAGE, 4, other, 0, Perms::RW)
        .unwrap();
    for id in ids {
        vmos.release(&mut mm, &mut jobs, id).unwrap();
    }
    vmos.release(&mut mm, &mut jobs, other).unwrap();
}

#[test]
fn a_protect_over_a_dying_region_finds_it_unmapped_and_a_second_unmap_shares_the_work() {
    let (mut mm, mut vmos, mut jobs, mut a, job) = world();
    let _ids = fill(&mut mm, &mut vmos, &mut jobs, &mut a, job);
    a.unmap_begin(&mut vmos, BASE + 100 * PAGE, 100 * PAGE)
        .unwrap();
    assert!(a
        .protect_range(&mut mm, &mut vmos, &mut jobs, BASE, 300 * PAGE, Perms::R)
        .is_err());
    // a second unmap that overlaps: it splits nothing more and marks the same regions
    a.unmap_begin(&mut vmos, BASE + 50 * PAGE, 200 * PAGE)
        .unwrap();
    vmos.audit_maps(&[&a]).unwrap();
    while !a
        .unmap_step(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 50 * PAGE,
            BASE + 250 * PAGE,
            64,
            2048,
        )
        .0
    {
        vmos.audit_maps(&[&a]).unwrap();
    }
    // what is left: [0,50) and [250,300) of the first region, and the second whole
    assert_eq!(a.region_count(), 3);
    assert_eq!(
        a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + 120 * PAGE,
            Access::Read
        ),
        Err(FaultError::NoRegion)
    );
}

#[test]
fn a_scan_budget_stops_a_walk_over_empty_tables_and_the_walk_goes_on() {
    let (mut mm, mut vmos, mut jobs, mut a, job) = world();
    // one page touched in a 300-page region: the rest has no table
    let id = vmos.create_in(&mut jobs, 300, job);
    a.map_region(&mut vmos, BASE, 300, id, 0, Perms::RW)
        .unwrap();
    a.handle_fault(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE + 299 * PAGE,
        Access::Write,
    )
    .unwrap();
    a.unmap_begin(&mut vmos, BASE, 300 * PAGE).unwrap();
    // a scan budget of one entry cannot reach the page: no mapping cleared, the region is not gone, and the call must not spin
    let (done, cleared) = a.unmap_step(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE,
        BASE + 300 * PAGE,
        64,
        1,
    );
    assert!(!done || cleared == 1);
    let mut n = 0;
    while !a
        .unmap_step(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE,
            BASE + 300 * PAGE,
            64,
            3,
        )
        .0
    {
        n += 1;
        assert!(n < 1000);
    }
    assert_eq!(a.region_count(), 0);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(jobs.usage(job).unwrap().get(Resource::Pages), 0);
}

#[test]
fn the_stepped_unmap_equals_the_one_shot() {
    let mut out = Vec::new();
    for stepped in [false, true] {
        let (mut mm, mut vmos, mut jobs, mut a, job) = world();
        let ids = fill(&mut mm, &mut vmos, &mut jobs, &mut a, job);
        let (start, len) = (BASE + 17 * PAGE, 400 * PAGE);
        if stepped {
            a.unmap_begin(&mut vmos, start, len).unwrap();
            while !a
                .unmap_step(&mut mm, &mut vmos, &mut jobs, start, start + len, 7, 100)
                .0
            {}
        } else {
            a.unmap_range(&mut mm, &mut vmos, &mut jobs, start, len)
                .unwrap();
        }
        vmos.audit_maps(&[&a]).unwrap();
        for id in ids {
            vmos.release(&mut mm, &mut jobs, id).unwrap();
        }
        out.push((
            a.region_count(),
            mm.db.free_count(),
            jobs.usage(job).unwrap().get(Resource::Pages),
        ));
    }
    assert_eq!(out[0], out[1]);
}

#[test]
fn a_stepped_protect_gives_the_regions_their_permissions_at_once_and_the_entries_follow() {
    let (mut mm, mut vmos, mut jobs, mut a, job) = world();
    let ids = fill(&mut mm, &mut vmos, &mut jobs, &mut a, job);
    let (start, len) = (BASE, 300 * PAGE);
    a.protect_begin(&mut mm, &mut vmos, &mut jobs, start, len, Perms::R)
        .unwrap();
    // the region says read-only now; the entries still allow a write until the steps get to them
    assert_eq!(a.region_perms_at(BASE + 200 * PAGE), Some(Perms::R));
    let t = a.table().translate(&mm, BASE + 200 * PAGE).unwrap();
    assert!(t.perms.write);
    let (mut cursor, mut downgraded, mut steps) = (start, false, 0);
    loop {
        let (done, st) = a.protect_step(&mut mm, &vmos, &mut cursor, start + len, 64, 2048);
        downgraded |= st.downgraded;
        steps += 1;
        vmos.audit_maps(&[&a]).unwrap();
        if done {
            break;
        }
    }
    assert!(downgraded);
    assert_eq!(steps, 300usize.div_ceil(64));
    for p in [0u64, 100, 299] {
        let t = a.table().translate(&mm, BASE + p * PAGE).unwrap();
        assert!(!t.perms.write, "page {p}");
    }
    for id in ids {
        vmos.release(&mut mm, &mut jobs, id).unwrap();
    }
}
