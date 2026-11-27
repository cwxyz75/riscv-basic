//! T249: the sparse page index is charged to the VMO's job as kernel bytes, one `LEAF_BYTES` per leaf, exactly. A refused charge changes nothing
//! (`NoMemory`), a pager's supply moves the charge with the pages, a merge or a release credits every leaf (holes included), a shrink drops the
//! holes past the end, and a DMA run pays for its leaves with its pages.
mod common;
use common::FakeMem;
use kcore::aspace::{ChildMode, VmoError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagemap::LEAF_BYTES;
use kcore::Mm;

const PAGE: u64 = 4096;

fn world() -> (Mm<FakeMem>, Vmos, Jobs) {
    (
        Mm {
            db: FrameDb::new(3000, 300),
            mem: FakeMem::default(),
        },
        Vmos::new(),
        Jobs::new(),
    )
}

fn kb(jobs: &Jobs, job: JobId) -> u64 {
    jobs.usage(job).unwrap().get(Resource::KernelBytes)
}

#[test]
fn a_sparse_write_that_needs_a_leaf_the_job_cannot_pay_is_refused_and_changes_nothing() {
    let (mut mm, mut vmos, mut jobs) = world();
    let mut lim = Limits::UNLIMITED;
    lim.pages = 16; // 16 x 512 kernel bytes: fifteen leaves of 528 fit, the sixteenth does not
    let job = jobs.create(JobId::ROOT, lim).unwrap();
    let id = vmos.create_in(&mut jobs, 64 * 20, job);
    let mut done = 0;
    for k in 0..16u64 {
        match vmos.write(&mut mm, &mut jobs, id, k * 64 * PAGE, &[1]) {
            Ok(()) => done += 1,
            Err(e) => {
                assert_eq!(e, VmoError::NoMemory);
                break;
            }
        }
        assert_eq!(kb(&jobs, job), done * LEAF_BYTES);
    }
    assert_eq!(done, 15, "fifteen leaves fit in 8192 bytes");
    let (pages, free) = (
        jobs.usage(job).unwrap().get(Resource::Pages),
        mm.db.free_count(),
    );
    assert_eq!(
        vmos.write(&mut mm, &mut jobs, id, 15 * 64 * PAGE, &[1]),
        Err(VmoError::NoMemory)
    );
    assert_eq!(
        (
            jobs.usage(job).unwrap().get(Resource::Pages),
            kb(&jobs, job),
            mm.db.free_count(),
            vmos.committed_pages(id)
        ),
        (pages, 15 * LEAF_BYTES, free, Some(15)),
        "a refused write changed nothing"
    );
    // a page in a leaf that exists needs no new one
    vmos.write(&mut mm, &mut jobs, id, PAGE, &[2]).unwrap();
    assert_eq!(kb(&jobs, job), 15 * LEAF_BYTES);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(kb(&jobs, job), 0, "release credits every leaf");
}

#[test]
fn a_commit_refused_on_a_leaf_leaves_the_ledger_equal_to_the_leaves_held() {
    let (mut mm, mut vmos, mut jobs) = world();
    let mut lim = Limits::UNLIMITED;
    lim.pages = 40; // 20480 kernel bytes: 38 leaves
    let job = jobs.create(JobId::ROOT, lim).unwrap();
    let id = vmos.create_in(&mut jobs, 64 * 50, job);
    // commit one page in each of 45 leaves: the job pays 38 of them
    let mut failed = false;
    for k in 0..45u64 {
        if vmos.commit(&mut mm, &mut jobs, id, k * 64, 1).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    // the failing call left its own page and leaf out
    let committed = vmos.committed_pages(id).unwrap();
    assert_eq!(kb(&jobs, job), committed * LEAF_BYTES);
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(kb(&jobs, job), 0);
}

#[test]
fn a_supply_moves_the_leaf_charge_and_a_refusal_leaves_the_source_whole() {
    let (mut mm, mut vmos, mut jobs) = world();
    let rich = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let mut lim = Limits::UNLIMITED;
    lim.pages = 1; // 512 kernel bytes: not even one leaf
    let poor = jobs.create(JobId::ROOT, lim).unwrap();
    let src = vmos.create_in(&mut jobs, 4, rich);
    vmos.commit(&mut mm, &mut jobs, src, 0, 2).unwrap();
    assert_eq!(kb(&jobs, rich), LEAF_BYTES);
    let pv = vmos.create_pager_in(&mut jobs, 4, poor, 7, 1);
    assert_eq!(
        vmos.supply(&mut mm, &mut jobs, pv, 0, 1, src, 0),
        Err((0, VmoError::NoMemory))
    );
    assert_eq!(vmos.committed_pages(src), Some(2), "the source is whole");
    assert_eq!(
        (kb(&jobs, rich), kb(&jobs, poor)),
        (LEAF_BYTES, 0),
        "no charge moved"
    );
    // a pager of the rich job: the destination's job pays its leaf, the source's job is credited for its own
    let pv2 = vmos.create_pager_in(&mut jobs, 4, rich, 8, 2);
    assert_eq!(vmos.supply(&mut mm, &mut jobs, pv2, 0, 2, src, 0), Ok(2));
    assert_eq!(vmos.committed_pages(src), Some(0));
    assert_eq!(
        kb(&jobs, rich),
        LEAF_BYTES,
        "one leaf moved from the source to the pager VMO"
    );
    vmos.release(&mut mm, &mut jobs, pv2).unwrap();
    vmos.release(&mut mm, &mut jobs, pv).unwrap();
    vmos.release(&mut mm, &mut jobs, src).unwrap();
    assert_eq!((kb(&jobs, rich), kb(&jobs, poor)), (0, 0));
}

#[test]
fn a_family_with_holes_gives_every_leaf_back() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let src = vmos.create_in(&mut jobs, 200, job);
    for p in [0u64, 70, 130, 199] {
        vmos.write(&mut mm, &mut jobs, src, p * PAGE, &[p as u8 + 1])
            .unwrap();
    }
    let c = vmos
        .child_prepare(&mut jobs, src, 0, 200, job, ChildMode::Private)
        .unwrap();
    vmos.child_attach(&mut jobs, c, src, 0, 200, ChildMode::Private);
    // decommit in the child over an inherited window makes holes (and leaves)
    vmos.decommit(&mut mm, &mut jobs, c, 60, 100).unwrap();
    let want = vmos.index_bytes_of_job(job);
    assert_eq!(kb(&jobs, job), want);
    assert!(want > 0);
    // the parent goes first: the child merges what it needs
    vmos.release(&mut mm, &mut jobs, src).unwrap();
    assert_eq!(kb(&jobs, job), vmos.index_bytes_of_job(job));
    vmos.release(&mut mm, &mut jobs, c).unwrap();
    assert_eq!(kb(&jobs, job), 0);
    assert_eq!(jobs.usage(job).unwrap().get(Resource::Pages), 0);
}

#[test]
fn a_shrink_drops_the_holes_past_the_end_with_their_leaves() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let src = vmos.create_in(&mut jobs, 200, job);
    vmos.write(&mut mm, &mut jobs, src, 0, &[1]).unwrap();
    let c = vmos
        .child_prepare(&mut jobs, src, 0, 200, job, ChildMode::Private)
        .unwrap();
    vmos.child_attach(&mut jobs, c, src, 0, 200, ChildMode::Private);
    vmos.set_resizable(c);
    vmos.decommit(&mut mm, &mut jobs, c, 150, 10).unwrap(); // holes in a far leaf
    let with_holes = kb(&jobs, job);
    assert_eq!(with_holes, vmos.index_bytes_of_job(job));
    // (a child with no children of its own can shrink)
    vmos.set_size(&mut mm, &mut jobs, c, 100).unwrap();
    assert!(kb(&jobs, job) < with_holes, "the far leaf went");
    assert_eq!(kb(&jobs, job), vmos.index_bytes_of_job(job));
    vmos.release(&mut mm, &mut jobs, c).unwrap();
    vmos.release(&mut mm, &mut jobs, src).unwrap();
    assert_eq!(kb(&jobs, job), 0);
}

#[test]
fn a_dma_run_pays_for_its_leaves_with_its_pages() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_dma_in(&mut mm, &mut jobs, 3, job).unwrap();
    assert_eq!(kb(&jobs, job), LEAF_BYTES);
    assert_eq!(vmos.index_leaves(id), Some(1));
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert_eq!(kb(&jobs, job), 0);
    // a job with no room for the leaf refuses the run and keeps nothing
    let mut lim = Limits::UNLIMITED;
    lim.pages = 1;
    let poor = jobs.create(JobId::ROOT, lim).unwrap();
    let free = mm.db.free_count();
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 1, poor),
        Err(VmoError::NoMemory)
    );
    assert_eq!(
        (
            kb(&jobs, poor),
            jobs.usage(poor).unwrap().get(Resource::Pages),
            mm.db.free_count()
        ),
        (0, 0, free)
    );
}
