//! L17 (KERNEL-V1-SPEC 4.5): a fault is three steps, so that a caller can let go of the address space between the lookup and the map
//! (a pager wait will have to). A region unmapped (or replaced) in between makes the last step refuse with `Stale`: the fault must not
//! map into the hole.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, FaultError, FaultMapError, Lookup, Vmos};
use kcore::frame::FrameDb;
use kcore::job::Jobs;
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
fn lookup_finds_the_page_to_fetch_and_a_mapped_page_is_a_spurious_fault() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let id = vmos.create(&mut jobs, 4);
    a.map_region(&mut vmos, BASE, 4, id, 0, Perms::RW).unwrap();
    let Lookup::Plan(plan) = a.fault_lookup(&mm, BASE + 4096 + 8, Access::Write).unwrap() else {
        panic!("an unmapped page needs a fetch")
    };
    assert_eq!(plan.vmo_page(), (id, 1));
    vmos.commit(&mut mm, &mut jobs, id, 1, 1).unwrap();
    let pfn = vmos.frame(id, 1).unwrap();
    let m = a.fault_map(&mut mm, &mut vmos, &plan, pfn).unwrap();
    assert!(m.first <= BASE + 4096 && BASE + 4096 <= m.last);
    assert!(matches!(
        a.fault_lookup(&mm, BASE + 4096, Access::Read).unwrap(),
        Lookup::Mapped(_)
    ));
    assert_eq!(
        a.fault_lookup(&mm, BASE + 4 * 4096, Access::Read),
        Err(FaultError::NoRegion)
    );
}

#[test]
fn a_region_unmapped_between_the_lookup_and_the_map_is_not_mapped_into() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let id = vmos.create(&mut jobs, 4);
    a.map_region(&mut vmos, BASE, 4, id, 0, Perms::RW).unwrap();
    let Lookup::Plan(plan) = a.fault_lookup(&mm, BASE, Access::Write).unwrap() else {
        panic!()
    };
    vmos.commit(&mut mm, &mut jobs, id, 0, 1).unwrap();
    let pfn = vmos.frame(id, 0).unwrap();
    // another thread unmaps the region while this fault has let go of the address space
    a.unmap_region(&mut mm, &mut vmos, &mut jobs, BASE).unwrap();
    let before = mm.db.free_count();
    assert_eq!(
        a.fault_map(&mut mm, &mut vmos, &plan, pfn),
        Err(FaultMapError::Stale)
    );
    assert_eq!(mm.db.free_count(), before, "nothing was taken or mapped");
    assert!(
        a.table().translate(&mm, BASE).is_none(),
        "the hole stays a hole"
    );
    assert_eq!(
        a.fault_lookup(&mm, BASE, Access::Write),
        Err(FaultError::NoRegion)
    );
}

#[test]
fn a_region_replaced_at_the_same_address_is_not_the_one_the_fault_looked_up() {
    let (mut mm, mut vmos, mut jobs, mut a) = world();
    let one = vmos.create(&mut jobs, 4);
    let two = vmos.create(&mut jobs, 4);
    a.map_region(&mut vmos, BASE, 4, one, 0, Perms::RW).unwrap();
    let Lookup::Plan(plan) = a.fault_lookup(&mm, BASE, Access::Write).unwrap() else {
        panic!()
    };
    vmos.commit(&mut mm, &mut jobs, one, 0, 1).unwrap();
    let pfn = vmos.frame(one, 0).unwrap();
    a.unmap_region(&mut mm, &mut vmos, &mut jobs, BASE).unwrap();
    a.map_region(&mut vmos, BASE, 4, two, 0, Perms::RW).unwrap(); // same start, another VMO
    assert_eq!(
        a.fault_map(&mut mm, &mut vmos, &plan, pfn),
        Err(FaultMapError::Stale),
        "a new region at the same address is a different region"
    );
    assert!(a.table().translate(&mm, BASE).is_none());
    // the retry (a fresh lookup) maps the new region's page
    assert!(matches!(
        a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write),
        Ok(())
    ));
}
