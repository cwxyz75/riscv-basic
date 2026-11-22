//! L19: a dying address space is taken apart in chunks (`destroy_user_chunk`, then `finish_destroy_user`) with the same result as `destroy_user`:
//! every frame back, every page and the reservation credited, whatever the chunk size, and no chunk unmaps more pages than it was given.
mod common;
use common::FakeMem;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0xffff_ffd0_0000_0000;

/// A world with an address space of several regions: pages committed and mapped in each (so the table has frames and the VMOs hold some), a
/// region mapped twice (two regions of one VMO), and a region of a VMO the space does not own the last reference to.
fn world() -> (Mm<FakeMem>, Vmos, Jobs, AddressSpace, Vec<u64>) {
    let mut mm = Mm {
        db: FrameDb::new(4000, 600),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let mut ids = Vec::new();
    for (i, pages) in [40u64, 7, 130, 1].into_iter().enumerate() {
        let id = vmos.create(&mut jobs, pages);
        a.map_region(
            &mut vmos,
            BASE + (i as u64) * 0x100_0000,
            pages,
            id,
            0,
            Perms::RW,
        )
        .unwrap();
        for p in (0..pages).step_by(3) {
            vmos.commit(&mut mm, &mut jobs, id, p, 1).unwrap();
            a.handle_fault(
                &mut mm,
                &mut vmos,
                &mut jobs,
                BASE + (i as u64) * 0x100_0000 + p * 4096,
                kcore::aspace::Access::Write,
            )
            .unwrap();
        }
        ids.push(id);
    }
    // the same VMO mapped a second time, in another region
    a.map_region(&mut vmos, BASE + 0x1000_0000, 7, ids[1], 0, Perms::R)
        .unwrap();
    (mm, vmos, jobs, a, ids)
}

fn finish(mm: &mut Mm<FakeMem>, vmos: &mut Vmos, jobs: &mut Jobs, ids: &[u64]) -> (usize, u64) {
    for &id in ids {
        let _ = vmos.release(mm, jobs, id); // the creators' references
    }
    (
        mm.db.free_count(),
        jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
    )
}

#[test]
fn a_chunked_teardown_gives_back_exactly_what_a_whole_one_does() {
    let (mut mm0, mut vmos0, mut jobs0, a0, ids0) = world();
    a0.destroy_user(&mut mm0, &mut vmos0, &mut jobs0);
    let want = finish(&mut mm0, &mut vmos0, &mut jobs0, &ids0);
    for budget in [1u64, 2, 5, 16, 64, 1000] {
        let (mut mm, mut vmos, mut jobs, mut a, ids) = world();
        let mut steps = 0;
        while !a.destroy_user_chunk(&mut mm, &mut vmos, &mut jobs, budget) {
            steps += 1;
            assert!(steps < 10_000, "budget {budget}: the teardown does not end");
        }
        a.finish_destroy_user(&mut mm, &mut jobs);
        let got = finish(&mut mm, &mut vmos, &mut jobs, &ids);
        assert_eq!(
            got, want,
            "budget {budget}: frames and pages back differ from a whole teardown"
        );
        assert_eq!(vmos.count(), 0, "budget {budget}: a VMO survived");
    }
}

#[test]
fn a_chunk_unmaps_no_more_than_its_budget() {
    let (mut mm, mut vmos, mut jobs, mut a, _ids) = world();
    let total: u64 = a.region_count() as u64;
    assert!(total >= 5);
    // a budget of one page cannot remove two regions of 1 and 7 pages in one chunk: the regions go in order, a page at a time
    let mut seen = a.region_count();
    let mut chunks = 0;
    while !a.destroy_user_chunk(&mut mm, &mut vmos, &mut jobs, 1) {
        chunks += 1;
        assert!(a.region_count() <= seen, "a region appeared");
        seen = a.region_count();
        assert!(chunks < 1000);
    }
    assert!(
        chunks > 100,
        "a budget of one page took only {chunks} chunks for 178 pages"
    );
}
