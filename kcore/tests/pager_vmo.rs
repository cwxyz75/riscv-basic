//! MM9: a pager's VMO in the store. A page nobody supplied is asked for (a fault, a kernel read, a PRIVATE child's), `supply` moves frames from an
//! anonymous VMO (nothing is copied, the charge follows), and what a supply must refuse.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, ChildMode, FaultError, PagerNeed, VmoError, Vmos};

fn child(
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    source: u64,
    off: u64,
    len: u64,
    mode: ChildMode,
) -> Result<u64, VmoError> {
    let c = vmos.child_prepare(jobs, source, off, len, JobId::ROOT, mode)?;
    vmos.child_attach(jobs, c, source, off, len, mode);
    Ok(c)
}
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0xffff_ffd0_0000_0000;

fn world() -> (Mm<FakeMem>, Vmos, Jobs, AddressSpace) {
    let mut mm = Mm {
        db: FrameDb::new(2000, 300),
        mem: FakeMem::default(),
    };
    let a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    (mm, Vmos::new(), Jobs::new(), a)
}

#[test]
fn an_absent_page_is_asked_for_and_a_supply_moves_the_frames_and_the_charge() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let mine = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let pv = vmos.create_pager_in(&mut jobs, 4, mine, 7, 0xabc);
    assert_eq!(vmos.pager_of(pv), Some((7, 0xabc)));
    a.map_region(&mut vmos, BASE, 4, pv, 0, Perms::RW).unwrap();
    // a fault, a kernel read and a kernel write all name the page
    let need = PagerNeed {
        pager: 7,
        vmo: pv,
        page: 2,
    };
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 2 * 4096, Access::Read),
        Err(FaultError::NeedPager(need))
    );
    assert_eq!(vmos.take_need(), Some(need));
    let mut buf = [0u8; 8];
    assert_eq!(
        vmos.read(&mut mm, pv, 2 * 4096, &mut buf),
        Err(VmoError::NeedPager)
    );
    assert_eq!(vmos.take_need(), Some(need));
    assert_eq!(
        vmos.write(&mut mm, &mut jobs, pv, 2 * 4096, &buf),
        Err(VmoError::NeedPager)
    );
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, pv, 2, 1),
        Err(VmoError::NeedPager)
    );
    assert_eq!(
        vmos.committed_pages(pv),
        Some(0),
        "nothing was committed by asking"
    );

    // the pager's source: two pages with a pattern, in a job of its own
    let other = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let src = vmos.create_in(&mut jobs, 2, other);
    vmos.write(&mut mm, &mut jobs, src, 0, &[0x5a; 4096])
        .unwrap();
    vmos.write(&mut mm, &mut jobs, src, 4096, &[0xa5; 4096])
        .unwrap();
    let before = (
        jobs.usage(other).unwrap().get(Resource::Pages),
        jobs.usage(mine).unwrap().get(Resource::Pages),
    );
    let free = mm.db.free_count();
    let pfn0 = vmos.frame(src, 0).unwrap();
    assert_eq!(vmos.supply(&mut mm, &mut jobs, pv, 2, 2, src, 0), Ok(2));
    assert_eq!(
        mm.db.free_count(),
        free,
        "frames moved, none copied or freed"
    );
    assert_eq!(vmos.frame(pv, 2), Some(pfn0));
    assert_eq!(vmos.committed_pages(src), Some(0));
    assert_eq!(
        jobs.usage(other).unwrap().get(Resource::Pages),
        before.0 - 2
    );
    assert_eq!(
        jobs.usage(mine).unwrap().get(Resource::Pages),
        before.1 + 2,
        "the charge moved to the pager VMO's job"
    );
    // and the access now goes through
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 3 * 4096, Access::Read)
        .unwrap();
    vmos.read(&mut mm, pv, 2 * 4096 + 5, &mut buf).unwrap();
    assert_eq!(buf, [0x5a; 8]);
    // page 0 and 1 are still asked for
    assert!(matches!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write),
        Err(FaultError::NeedPager(PagerNeed { page: 0, .. }))
    ));
}

#[test]
fn a_supply_refuses_what_is_not_the_sources_to_give() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let pv = vmos.create_pager_in(&mut jobs, 4, JobId::ROOT, 1, 1);
    let src = vmos.create(&mut jobs, 2);
    // nothing committed in the source
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 0, 1, src, 0),
        Err((0, VmoError::NotSuppliable))
    );
    vmos.commit(&mut mm, &mut jobs, src, 0, 2).unwrap();
    // a mapping still holds the frame
    a.map_region(&mut vmos, BASE, 2, src, 0, Perms::RW).unwrap();
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Read)
        .unwrap();
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 0, 1, src, 0),
        Err((0, VmoError::NotSuppliable))
    );
    a.unmap_region(&mut mm, &mut vmos, &mut jobs, BASE).unwrap();
    // ranges, and a destination that is not a pager's
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 3, 2, src, 0),
        Err((0, VmoError::OutOfRange))
    );
    let plain = vmos.create(&mut jobs, 2);
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, plain, 0, 1, src, 0),
        Err((0, VmoError::NotSuppliable))
    );
    // a source that is a pager's VMO, or has a child
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 0, 1, pv, 0),
        Err((0, VmoError::NotSuppliable))
    );
    let child = child(&mut vmos, &mut jobs, src, 0, 1, ChildMode::Private).unwrap();
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 0, 1, src, 0),
        Err((0, VmoError::NotSuppliable))
    );
    let _ = child;
    // and the right ones go through, a page already supplied is left alone
    let src2 = vmos.create(&mut jobs, 1);
    vmos.commit(&mut mm, &mut jobs, src2, 0, 1).unwrap();
    assert_eq!(vmos.supply(&mut mm, &mut jobs, pv, 1, 1, src2, 0), Ok(1));
    let src3 = vmos.create(&mut jobs, 1);
    vmos.commit(&mut mm, &mut jobs, src3, 0, 1).unwrap();
    assert_eq!(vmos.supply(&mut mm, &mut jobs, pv, 1, 1, src3, 0), Ok(1));
    assert_eq!(
        vmos.committed_pages(src3),
        Some(1),
        "the source keeps the page the destination already had"
    );
}

#[test]
fn a_private_child_asks_the_parents_pager_and_a_snapshot_is_refused() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let pv = vmos.create_pager_in(&mut jobs, 4, JobId::ROOT, 9, 5);
    assert!(matches!(
        child(&mut vmos, &mut jobs, pv, 0, 4, ChildMode::Snapshot),
        Err(VmoError::NotCopyable)
    ));
    let child = child(&mut vmos, &mut jobs, pv, 1, 2, ChildMode::Private).unwrap();
    a.map_region(&mut vmos, BASE, 2, child, 0, Perms::RW)
        .unwrap();
    // child page 1 is the parent's page 2; a write needs the page too (it copies it)
    let need = PagerNeed {
        pager: 9,
        vmo: pv,
        page: 2,
    };
    assert_eq!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 4096, Access::Write),
        Err(FaultError::NeedPager(need))
    );
    // once the parent has it, the child reads it, and a write copies it
    let src = vmos.create(&mut jobs, 1);
    vmos.write(&mut mm, &mut jobs, src, 0, &[7u8; 4096])
        .unwrap();
    assert_eq!(vmos.supply(&mut mm, &mut jobs, pv, 2, 1, src, 0), Ok(1));
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE + 4096, Access::Write)
        .unwrap();
    let mut b = [0u8; 4];
    vmos.read(&mut mm, child, 4096, &mut b).unwrap();
    assert_eq!(b, [7; 4]);
}
