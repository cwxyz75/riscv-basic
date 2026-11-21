//! L16 part d: the address space's prepaid page reservation (KERNEL-V1-SPEC 4.7). The ledger is charged in batches by the fault path;
//! J1 holds with reservations counted as usage: a job's page usage is always the committed pages plus the prepaid ones, never above its
//! limit, and everything comes back when the address space ends.
mod common;
use common::FakeMem;
use kcore::aspace::{Access, AddressSpace, FaultError, Vmos, RESERVATION_PAGES};
use kcore::frame::FrameDb;
use kcore::job::{Jobs, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0xffff_ffd0_0000_0000;

fn setup(frames: usize) -> (Mm<FakeMem>, Vmos, Jobs, AddressSpace) {
    let mut mm = Mm {
        db: FrameDb::new(2000, frames),
        mem: FakeMem::default(),
    };
    let aspace = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    (mm, Vmos::new(), Jobs::new(), aspace)
}

fn pages_of(jobs: &Jobs, j: kcore::job::JobId) -> u64 {
    jobs.usage(j).unwrap().get(Resource::Pages)
}

fn limited(jobs: &mut Jobs, pages: u64) -> kcore::job::JobId {
    let mut l = Limits::UNLIMITED;
    l.pages = pages;
    jobs.create(kcore::job::JobId::ROOT, l).unwrap()
}

#[test]
fn usage_is_committed_plus_prepaid_at_every_step_and_the_ledger_is_asked_in_batches() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(600);
    let j = limited(&mut jobs, 1000);
    let id = vmos.create_in(&mut jobs, 200, j);
    a.map_region(&mut vmos, BASE, 200, id, 0, Perms::RW)
        .unwrap();
    let mut jumps = 0;
    let mut last = 0;
    for p in 0..190u64 {
        a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + p * 4096,
            Access::Write,
        )
        .unwrap();
        let committed = vmos.committed_pages(id).unwrap_or(0);
        let used = pages_of(&jobs, j);
        assert_eq!(used, committed + a.reserved_pages(), "J1 at step {p}");
        assert!(used <= 1000);
        if used != last {
            jumps += 1;
            last = used;
        }
    }
    let committed = vmos.committed_pages(id).unwrap();
    assert!(committed > 100, "plenty committed ({committed})");
    assert!(
        jumps < (committed / 4) as usize,
        "the ledger changed {jumps} times for {committed} pages: in batches"
    );
    assert!(a.reserved_pages() < RESERVATION_PAGES + 1);
    // the address space ends: the prepaid remainder goes back, then the VMO's pages
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    assert_eq!(
        pages_of(&jobs, j),
        committed,
        "only the VMO's pages are left charged"
    );
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(pages_of(&jobs, j), 0);
}

#[test]
fn a_job_at_its_limit_is_refused_exactly_there_not_early() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(300);
    let j = limited(&mut jobs, 20);
    let id = vmos.create_in(&mut jobs, 64, j);
    a.map_region(&mut vmos, BASE, 64, id, 0, Perms::RW).unwrap();
    let mut got = 0;
    for p in 0..64u64 {
        // (a page in each fault-around window, so each fault commits what it asks for plus neighbours already committed)
        match a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + p * 4096,
            Access::Write,
        ) {
            Ok(_) => {}
            Err(FaultError::NoMemory) => break,
            Err(e) => panic!("{e:?}"),
        }
        got = vmos.committed_pages(id).unwrap();
        assert!(pages_of(&jobs, j) <= 20, "never above the limit");
    }
    assert_eq!(
        got, 20,
        "every page the limit allows was usable: prepaying took none of them away"
    );
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(pages_of(&jobs, j), 0);
}

#[test]
fn a_vmo_of_another_job_hands_the_remainder_back_first() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(300);
    let (j1, j2) = (limited(&mut jobs, 500), limited(&mut jobs, 500));
    let v1 = vmos.create_in(&mut jobs, 64, j1);
    let v2 = vmos.create_in(&mut jobs, 64, j2);
    a.map_region(&mut vmos, BASE, 64, v1, 0, Perms::RW).unwrap();
    a.map_region(&mut vmos, BASE + (1 << 22), 64, v2, 0, Perms::RW)
        .unwrap();
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write)
        .unwrap();
    a.handle_fault(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE + (1 << 22),
        Access::Write,
    )
    .unwrap();
    let c1 = vmos.committed_pages(v1).unwrap();
    let c2 = vmos.committed_pages(v2).unwrap();
    assert_eq!(
        pages_of(&jobs, j1),
        c1,
        "the first job's prepayment went back"
    );
    assert_eq!(pages_of(&jobs, j2), c2 + a.reserved_pages());
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    assert_eq!(pages_of(&jobs, j2), c2);
}

#[test]
fn a_frame_that_cannot_be_had_keeps_the_page_prepaid_and_charges_nothing_more() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(12); // few frames: tables take most
    let j = limited(&mut jobs, 1000);
    let id = vmos.create_in(&mut jobs, 64, j);
    a.map_region(&mut vmos, BASE, 64, id, 0, Perms::RW).unwrap();
    let mut refused = 0;
    for p in 0..64u64 {
        if a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + p * 4096,
            Access::Write,
        )
        .is_err()
        {
            refused += 1;
        }
        assert_eq!(
            pages_of(&jobs, j),
            vmos.committed_pages(id).unwrap() + a.reserved_pages()
        );
    }
    assert!(refused > 0, "the frames ran out");
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(pages_of(&jobs, j), 0);
}

#[test]
fn an_unlimited_job_is_charged_page_by_page_with_nothing_prepaid() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(300);
    let id = vmos.create(&mut jobs, 40); // the root job: no limit
    a.map_region(&mut vmos, BASE, 40, id, 0, Perms::RW).unwrap();
    for p in 0..40u64 {
        a.handle_fault(
            &mut mm,
            &mut vmos,
            &mut jobs,
            BASE + p * 4096,
            Access::Write,
        )
        .unwrap();
        assert_eq!(a.reserved_pages(), 0);
        assert_eq!(
            pages_of(&jobs, kcore::job::JobId::ROOT),
            vmos.committed_pages(id).unwrap(),
            "usage is exactly what the VMO holds"
        );
    }
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(pages_of(&jobs, kcore::job::JobId::ROOT), 0);
}

#[test]
fn a_reservation_keeps_its_job_alive_until_it_is_given_back() {
    let (mut mm, mut vmos, mut jobs, mut a) = setup(300);
    let j = limited(&mut jobs, 400);
    let id = vmos.create_in(&mut jobs, 8, j);
    a.map_region(&mut vmos, BASE, 8, id, 0, Perms::RW).unwrap();
    a.handle_fault(&mut mm, &mut vmos, &mut jobs, BASE, Access::Write)
        .unwrap();
    assert!(a.reserved_pages() > 0, "a limited job prepays");
    // the creator lets go of the VMO and the job: only the mapping and the prepaid pages hold them
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    jobs.release(j);
    assert!(
        jobs.exists(j),
        "the mapping and the reservation hold the job"
    );
    a.destroy_user(&mut mm, &mut vmos, &mut jobs);
    assert!(
        !jobs.exists(j),
        "the last holder was the reservation: the job is freed with no usage left"
    );
}
