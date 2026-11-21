//! `Vmos::commit` (slice V): the creator pays up front. Hand-derived scenarios: a commit charges the VMO's
//! job and takes frames now, later touches charge nothing, a refusal changes nothing (all or nothing, the
//! ledger and the frame count exactly as before), and destroying credits exactly what was committed.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, VmoError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

fn mm(frames: usize) -> Mm<FakeMem> {
    Mm {
        db: FrameDb::new(2000, frames),
        mem: FakeMem::default(),
    }
}

fn job(jobs: &mut kcore::job::Jobs, pages: u64) -> JobId {
    jobs.create(
        JobId::ROOT,
        Limits {
            processes: 8,
            threads: 8,
            pages,
            jobs: 8,
            objects: 64,
            ipc_bytes: 1 << 20,
        },
    )
    .unwrap()
}

fn used(jobs: &kcore::job::Jobs, j: JobId) -> u64 {
    jobs.usage(j).unwrap().get(Resource::Pages)
}

#[test]
fn a_commit_charges_now_and_a_later_touch_charges_nothing() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let j = job(&mut jobs, 10);
    let id = vmos.create_in(&mut jobs, 8, j);
    assert_eq!(used(&jobs, j), 0, "creation is lazy");
    let free = mm.db.free_count();
    vmos.commit(&mut mm, &mut jobs, id, 2, 4).unwrap();
    assert_eq!(used(&jobs, j), 4);
    assert_eq!(mm.db.free_count(), free - 4);
    assert!(vmos.is_committed(id, 2, 4));
    assert!(
        !vmos.is_committed(id, 0, 3),
        "page 0 and 1 are not committed"
    );
    // committing again (overlapping) charges only the new pages
    vmos.commit(&mut mm, &mut jobs, id, 0, 4).unwrap();
    assert_eq!(used(&jobs, j), 6);
    // a touch of a committed page through a mapping charges nothing and cannot fail
    let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let va = 0xffff_ffd0_0000_0000u64;
    a.map_region(&mut vmos, va, 8, id, 0, Perms::RW).unwrap();
    let before = (used(&jobs, j), mm.db.free_count());
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, va + 3 * 4096, Access::Write)
        .unwrap();
    assert_eq!(
        before.0,
        used(&jobs, j),
        "a touch of a committed page is free"
    );
    let _ = before;
}

#[test]
fn a_refused_commit_changes_nothing() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let j = job(&mut jobs, 5);
    let id = vmos.create_in(&mut jobs, 8, j);
    vmos.commit(&mut mm, &mut jobs, id, 0, 2).unwrap();
    let (used0, free0) = (used(&jobs, j), mm.db.free_count());
    // 6 more would exceed the job's 5 pages: refused, and the 3 that fit are given back
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, id, 2, 6),
        Err(VmoError::NoMemory)
    );
    assert_eq!(
        (used(&jobs, j), mm.db.free_count()),
        (used0, free0),
        "nothing changed"
    );
    assert!(!vmos.is_committed(id, 2, 1));
    assert!(
        vmos.is_committed(id, 0, 2),
        "what was committed before stays"
    );
    // the same with the frames running out instead of the job's limit
    let mut small = mm_small();
    let mut v2 = Vmos::new();
    let j2 = job(&mut jobs, 100);
    let id2 = v2.create_in(&mut jobs, 10, j2);
    let free2 = small.db.free_count();
    assert_eq!(
        v2.commit(&mut small, &mut jobs, id2, 0, 10),
        Err(VmoError::NoMemory)
    );
    assert_eq!((used(&jobs, j2), small.db.free_count()), (0, free2));
}

fn mm_small() -> Mm<FakeMem> {
    mm(4)
}

#[test]
fn range_and_kind_errors() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let id = vmos.create(&mut jobs, 4);
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, id, 3, 2),
        Err(VmoError::OutOfRange)
    );
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, id, u64::MAX, 2),
        Err(VmoError::OutOfRange)
    );
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, 999, 0, 1),
        Err(VmoError::Gone)
    );
    assert!(
        vmos.commit(&mut mm, &mut jobs, id, 0, 0).is_ok(),
        "an empty range is fine"
    );
    assert!(
        !vmos.is_committed(id, 3, 2),
        "past the end is never committed"
    );
    assert!(!vmos.is_committed(999, 0, 1));
}

#[test]
fn destroying_credits_exactly_what_was_committed() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let j = job(&mut jobs, 10);
    let free = mm.db.free_count();
    let id = vmos.create_in(&mut jobs, 8, j);
    vmos.commit(&mut mm, &mut jobs, id, 0, 5).unwrap();
    assert_eq!(used(&jobs, j), 5);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(used(&jobs, j), 0, "credited back");
    assert_eq!(mm.db.free_count(), free, "frames back");
}

#[test]
fn a_failed_commit_over_already_committed_pages_keeps_them_and_double_credits_nothing() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let j = job(&mut jobs, 5);
    let id = vmos.create_in(&mut jobs, 8, j);
    vmos.commit(&mut mm, &mut jobs, id, 0, 3).unwrap();
    // pages 0-2 are already ours; 0..8 would need five more (total 8 > 5): refused, nothing changes
    assert_eq!(
        vmos.commit(&mut mm, &mut jobs, id, 0, 8),
        Err(VmoError::NoMemory)
    );
    assert_eq!(
        used(&jobs, j),
        3,
        "the earlier pages are still charged exactly once"
    );
    assert!(vmos.is_committed(id, 0, 3));
    let free = mm.db.free_count();
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(used(&jobs, j), 0);
    assert_eq!(mm.db.free_count(), free + 3);
}
