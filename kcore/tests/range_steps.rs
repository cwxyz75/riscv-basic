//! T207 PR 3a: a decommit or shrink goes a batch at a time (`range_begin`, `range_step`, `range_free`, `range_end`). Between two steps anything else
//! may run: the ledger is exact after every step, the VMO is busy to the calls that would race the cursor, a fault ahead of the cursor is taken
//! away later and one behind it stays, and what the one-shot call did the stepped call does.
mod common;
use common::FakeMem;
use kcore::aspace::{ChildMode, VmoError, Vmos, RANGE_STEP};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagemap::LEAF_BYTES;
use kcore::Mm;

const PAGE: u64 = 4096;

fn world() -> (Mm<FakeMem>, Vmos, Jobs) {
    (
        Mm {
            db: FrameDb::new(6000, 3000),
            mem: FakeMem::default(),
        },
        Vmos::new(),
        Jobs::new(),
    )
}

fn used(jobs: &Jobs, job: JobId, r: Resource) -> u64 {
    jobs.usage(job).unwrap().get(r)
}

/// The two ledger lines every step keeps exact: pages charged are frames held, kernel bytes are leaves held.
fn exact(vmos: &Vmos, jobs: &Jobs, job: JobId) {
    assert_eq!(used(jobs, job, Resource::Pages), vmos.frames_held());
    assert_eq!(
        used(jobs, job, Resource::KernelBytes),
        vmos.index_bytes_of_job(job)
    );
}

/// Run the steps of a begun call, checking the ledger after each; `between` runs after every step but the last. Returns the steps taken.
fn run(
    mm: &mut Mm<FakeMem>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    job: JobId,
    id: u64,
    mut between: impl FnMut(&mut Mm<FakeMem>, &mut Vmos, &mut Jobs),
) -> usize {
    let mut batch = [0u64; RANGE_STEP];
    let mut steps = 0;
    loop {
        let st = vmos.range_step(jobs, id, &mut batch).unwrap();
        assert!(st.n <= RANGE_STEP && st.next >= st.lo);
        vmos.range_free(mm, jobs, id, &batch[..st.n]);
        steps += 1;
        exact(vmos, jobs, job);
        assert!(st.err.is_none());
        if st.done {
            vmos.range_end(id, true);
            return steps;
        }
        between(mm, vmos, jobs);
    }
}

#[test]
fn a_big_decommit_takes_many_steps_and_the_ledger_is_exact_after_each() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 600, job);
    for p in 0..600u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    assert_eq!(vmos.range_begin(id, 100, 500, None), Ok(true));
    let steps = run(&mut mm, &mut vmos, &mut jobs, job, id, |_, _, _| {});
    assert_eq!(steps, 400usize.div_ceil(RANGE_STEP));
    assert_eq!(used(&jobs, job, Resource::Pages), 200);
    assert!(!vmos.is_busy(id));
    let mut b = [9u8];
    vmos.read(&mut mm, id, 300 * PAGE, &mut b).unwrap();
    assert_eq!(b, [0], "a decommitted page reads zeros");
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    vmos.drain_all(&mut mm, &mut jobs);
    assert_eq!(used(&jobs, job, Resource::Pages), 0);
    assert_eq!(used(&jobs, job, Resource::KernelBytes), 0);
}

#[test]
fn a_busy_vmo_refuses_what_would_race_the_cursor_and_the_end_frees_it() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 300, job);
    vmos.set_resizable(id);
    for p in 0..300u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    assert_eq!(vmos.range_begin(id, 0, 300, None), Ok(true));
    assert!(vmos.is_busy(id));
    assert_eq!(vmos.range_begin(id, 0, 10, None), Err(VmoError::Busy));
    assert_eq!(vmos.range_begin(id, 10, 300, Some(10)), Err(VmoError::Busy));
    assert_eq!(
        vmos.child_prepare(&mut jobs, id, 0, 10, job, ChildMode::Private),
        Err(VmoError::Busy)
    );
    // (a refusal changed nothing: the size is still 300)
    assert_eq!(vmos.pages(id), Some(300));
    // one step, then the caller ends the call (an error elsewhere): the VMO is free and what went stays gone
    let mut batch = [0u64; RANGE_STEP];
    let st = vmos.range_step(&mut jobs, id, &mut batch).unwrap();
    vmos.range_free(&mut mm, &mut jobs, id, &batch[..st.n]);
    vmos.range_end(id, true);
    assert!(!vmos.is_busy(id));
    exact(&vmos, &jobs, job);
    assert_eq!(used(&jobs, job, Resource::Pages), 300 - RANGE_STEP as u64);
    assert_eq!(vmos.range_begin(id, 0, 10, None), Ok(true));
    vmos.range_end(id, true);
}

#[test]
fn a_fault_ahead_of_the_cursor_goes_with_the_rest_and_one_behind_stays() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 400, job);
    for p in 0..200u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    assert_eq!(vmos.range_begin(id, 0, 400, None), Ok(true));
    let mut n = 0;
    run(&mut mm, &mut vmos, &mut jobs, job, id, |mm, vmos, jobs| {
        n += 1;
        if n == 1 {
            // (the cursor is at 64: page 0 is behind it, page 390 far ahead)
            vmos.write(mm, jobs, id, 0, &[7]).unwrap();
            vmos.write(mm, jobs, id, 390 * PAGE, &[8]).unwrap();
        }
    });
    let mut b = [0u8];
    vmos.read(&mut mm, id, 0, &mut b).unwrap();
    assert_eq!(b, [7], "behind the cursor the refault stays");
    vmos.read(&mut mm, id, 390 * PAGE, &mut b).unwrap();
    assert_eq!(b, [0], "ahead of it the page was taken with the range");
    assert_eq!(used(&jobs, job, Resource::Pages), 1);
}

#[test]
fn a_sparse_range_gives_every_leaf_back_exactly() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 64 * 40, job);
    for k in 0..40u64 {
        vmos.write(&mut mm, &mut jobs, id, k * 64 * PAGE, &[1])
            .unwrap();
    }
    assert_eq!(used(&jobs, job, Resource::KernelBytes), 40 * LEAF_BYTES);
    assert_eq!(vmos.range_begin(id, 64 * 5, 64 * 35, None), Ok(true));
    let steps = run(&mut mm, &mut vmos, &mut jobs, job, id, |_, _, _| {});
    assert!(steps >= 30 / RANGE_STEP.min(30));
    assert_eq!(used(&jobs, job, Resource::KernelBytes), 10 * LEAF_BYTES);
    assert_eq!(used(&jobs, job, Resource::Pages), 10);
}

#[test]
fn a_child_window_decommit_in_steps_makes_holes_that_read_zeros() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let src = vmos.create_in(&mut jobs, 300, job);
    for p in 0..300u64 {
        vmos.write(&mut mm, &mut jobs, src, p * PAGE, &[5]).unwrap();
    }
    let c = vmos
        .child_prepare(&mut jobs, src, 0, 300, job, ChildMode::Private)
        .unwrap();
    vmos.child_attach(&mut jobs, c, src, 0, 300, ChildMode::Private);
    assert_eq!(vmos.range_begin(c, 10, 290, None), Ok(true));
    let steps = run(&mut mm, &mut vmos, &mut jobs, job, c, |_, _, _| {});
    assert!(steps >= 280 / RANGE_STEP);
    let mut b = [1u8];
    vmos.read(&mut mm, c, 100 * PAGE, &mut b).unwrap();
    assert_eq!(b, [0], "a hole shadows the parent's page");
    vmos.read(&mut mm, c, 5 * PAGE, &mut b).unwrap();
    assert_eq!(b, [5], "outside the range the parent still shows");
    // the parent's pages are untouched
    vmos.read(&mut mm, src, 100 * PAGE, &mut b).unwrap();
    assert_eq!(b, [5]);
}

#[test]
fn a_hole_the_job_cannot_pay_stops_the_call_with_what_it_did_and_the_end_frees_the_vmo() {
    let (mut mm, mut vmos, mut jobs) = world();
    let mut lim = Limits::UNLIMITED;
    lim.pages = 20; // 20 x 512 kernel bytes: nineteen leaves of 528 fit
    let job = jobs.create(JobId::ROOT, lim).unwrap();
    let src = vmos.create_in(&mut jobs, 64 * 30, job);
    vmos.write(&mut mm, &mut jobs, src, 0, &[1]).unwrap();
    let c = vmos
        .child_prepare(&mut jobs, src, 0, 64 * 30, job, ChildMode::Private)
        .unwrap();
    vmos.child_attach(&mut jobs, c, src, 0, 64 * 30, ChildMode::Private);
    assert_eq!(vmos.range_begin(c, 0, 64 * 30, None), Ok(true));
    let mut batch = [0u64; RANGE_STEP];
    let refused_at = loop {
        let st = vmos.range_step(&mut jobs, c, &mut batch).unwrap();
        vmos.range_free(&mut mm, &mut jobs, c, &batch[..st.n]);
        exact(&vmos, &jobs, job);
        if st.err.is_some() {
            assert_eq!(st.err, Some(VmoError::NoMemory));
            break st.next;
        }
        assert!(!st.done, "thirty leaves of holes cannot fit");
    };
    vmos.range_end(c, true);
    assert!(!vmos.is_busy(c));
    assert!(refused_at < 64 * 30, "the pages before it were done");
    // a retry goes on from where the holes stop
    assert_eq!(vmos.range_begin(c, 0, 64 * 30, None), Ok(true));
    vmos.range_end(c, true);
}

#[test]
fn a_shrink_in_steps_sets_the_size_at_once_and_clears_the_tail() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 400, job);
    vmos.set_resizable(id);
    for p in 0..400u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    assert_eq!(vmos.range_begin(id, 100, 400, Some(100)), Ok(true));
    assert_eq!(vmos.pages(id), Some(100), "the new size holds at once");
    let mut b = [0u8];
    assert!(vmos.read(&mut mm, id, 200 * PAGE, &mut b).is_err());
    run(&mut mm, &mut vmos, &mut jobs, job, id, |_, _, _| {});
    assert_eq!(used(&jobs, job, Resource::Pages), 100);
    // growing: no busy, the size changes
    assert_eq!(vmos.range_begin(id, 100, 100, Some(300)), Ok(false));
    assert_eq!(vmos.pages(id), Some(300));
    assert!(!vmos.is_busy(id));
}

#[test]
fn the_stepped_call_equals_the_one_shot_call() {
    for (first, end) in [(0u64, 700u64), (3, 4), (60, 200), (64, 128), (699, 700)] {
        let mut out = Vec::new();
        for stepped in [false, true] {
            let (mut mm, mut vmos, mut jobs) = world();
            let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
            let id = vmos.create_in(&mut jobs, 700, job);
            for p in (0..700u64).step_by(3) {
                vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
            }
            if stepped {
                assert_eq!(vmos.range_begin(id, first, end, None), Ok(true));
                run(&mut mm, &mut vmos, &mut jobs, job, id, |_, _, _| {});
            } else {
                vmos.decommit(&mut mm, &mut jobs, id, first, end - first)
                    .unwrap();
            }
            out.push((
                used(&jobs, job, Resource::Pages),
                used(&jobs, job, Resource::KernelBytes),
                mm.db.free_count(),
            ));
        }
        assert_eq!(out[0], out[1], "range {first}..{end}");
    }
}

#[test]
fn a_shrink_that_stops_short_gives_the_size_back() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 300, job);
    vmos.set_resizable(id);
    for p in 0..300u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    assert_eq!(vmos.range_begin(id, 100, 300, Some(100)), Ok(true));
    let mut batch = [0u64; RANGE_STEP];
    let st = vmos.range_step(&mut jobs, id, &mut batch).unwrap();
    vmos.range_free(&mut mm, &mut jobs, id, &batch[..st.n]);
    vmos.range_end(id, false); // (the next step found no memory for its locks)
    assert_eq!(vmos.pages(id), Some(300), "no tail is left past the size");
    exact(&vmos, &jobs, job);
    // a repeat finishes the job
    vmos.set_size(&mut mm, &mut jobs, id, 100).unwrap();
    assert_eq!(vmos.pages(id), Some(100));
    assert_eq!(used(&jobs, job, Resource::Pages), 100);
    vmos.set_size(&mut mm, &mut jobs, id, 300).unwrap();
    let mut b = [9u8];
    vmos.read(&mut mm, id, 250 * PAGE, &mut b).unwrap();
    assert_eq!(b, [0]);
}

#[test]
fn the_last_close_of_a_big_vmo_drains_in_batches_with_the_ledger_exact_at_each() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let id = vmos.create_in(&mut jobs, 400, job);
    for p in 0..400u64 {
        vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[1]).unwrap();
    }
    let free = mm.db.free_count();
    vmos.release(&mut mm, &mut jobs, id).unwrap();
    assert!(!vmos.exists(id), "the id is invalid at once");
    assert_eq!(vmos.count(), 0);
    // nothing was credited yet, and the frames are still held: the ledger says exactly that
    assert_eq!(used(&jobs, job, Resource::Pages), 400);
    exact(&vmos, &jobs, job);
    let mut batches = 0;
    while vmos.drain_dying(&mut mm, &mut jobs, RANGE_STEP) {
        batches += 1;
        exact(&vmos, &jobs, job);
        assert!(used(&jobs, job, Resource::Pages) < 400);
    }
    assert!(batches >= 5, "{batches} batches");
    exact(&vmos, &jobs, job);
    assert_eq!(used(&jobs, job, Resource::Pages), 0);
    assert_eq!(used(&jobs, job, Resource::KernelBytes), 0);
    assert!(mm.db.free_count() > free);
}

#[test]
fn a_dying_vmos_slot_is_not_reused_until_it_is_empty_and_an_exhausted_one_is_retired() {
    let (mut mm, mut vmos, mut jobs) = world();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    vmos.set_first_generation(u32::MAX);
    let big = vmos.create_in(&mut jobs, 200, job);
    for p in 0..200u64 {
        vmos.write(&mut mm, &mut jobs, big, p * PAGE, &[1]).unwrap();
    }
    vmos.release(&mut mm, &mut jobs, big).unwrap();
    // a new VMO must not land on the dying slot
    let other = vmos.create_in(&mut jobs, 1, job);
    assert_ne!(other & 0xffff_ffff, big & 0xffff_ffff);
    vmos.drain_all(&mut mm, &mut jobs);
    // the exhausted slot is retired, not wrapped: the next one is still not it
    let third = vmos.create_in(&mut jobs, 1, job);
    assert_ne!(third & 0xffff_ffff, big & 0xffff_ffff);
    assert!(!vmos.exists(big));
}
