//! IOMMU6: the stepped work of the device-domain store (IOMMU-SPEC amendment 2026-11-26, OD168 and OD170). A map or an unmap far
//! larger than a step is done `RANGE_STEP` pages per hold of `IOMMU`, and between two holds anything else may run: another map, an
//! unmap, a destruction (which never waits for the map: the map gives way, BAD_STATE), its completion. The simulated IOMMU is the
//! oracle for what the device reaches; the frame database and the job ledger for what comes back.
mod common;
mod iodomain_common;

use iodomain_common::*;
use iommucore::iopt::Access;
use kcore::frame::Pfn;
use kcore::iodomain::*;
use kcore::job::{IoResource, JobId, Limits};

/// A map begun: its cursor, the frames of its pages (the hold is taken), and its charge.
struct Begun {
    cur: MapCursor,
    first: u64,
    pfns: Vec<Pfn>,
    charge: MapCharge,
}

fn begin(w: &mut World, d: DomainId, v: u64, first: u64, pages: u64) -> Begun {
    let job = w.store.job_of(d).unwrap();
    let mut pfns = vec![];
    w.vmos.retain_for_io(v, first, pages, &mut pfns).unwrap();
    let charge = charge_map(&mut w.jobs, job, pages).unwrap();
    let rec = Record::new(v, first, pages, Access::ReadWrite).unwrap();
    let cur = w.store.map_begin(d, rec).unwrap();
    Begun {
        cur,
        first,
        pfns,
        charge,
    }
}

/// One hold of `IOMMU` for the map.
fn step(w: &mut World, cur: MapCursor, first: u64, pfns: &[Pfn]) -> MapStep {
    let frames: Vec<Pfn> = cur
        .next_frames()
        .map(|(at, k)| pfns[(at - first) as usize..(at - first + k) as usize].to_vec())
        .unwrap_or_default();
    let World {
        mem,
        sim,
        store,
        mm,
        counts,
        ..
    } = w;
    let mut f = DbFrames {
        db: &mut mm.db,
        mem,
        counts,
        fail: None,
    };
    store.map_step(*mem, sim, &mut f, cur, &frames)
}

fn wait(w: &mut World, t: Ticket) {
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, t), Fence::Done);
}

fn setup(mem: &IoMem, pages: u64) -> (World<'_>, DomainId, u64, JobId) {
    let mut w = World::new(mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 4, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, pages, job)
        .unwrap();
    (w, d, v, job)
}

#[test]
fn a_big_map_takes_a_hold_per_step_and_other_work_runs_between() {
    let mem = IoMem::new();
    let n = 5 * RANGE_STEP + 3;
    let (mut w, d, v, job) = setup(&mem, n + 4);
    let base = w.mm.db.free_count();
    let mut b = begin(&mut w, d, v, 0, n);
    let iova = b.cur.io_address();
    let mut steps = 0;
    let m = loop {
        steps += 1;
        let at = b.cur.next_frames();
        assert!(
            at.is_some_and(|(_, k)| k <= RANGE_STEP),
            "at most a step's pages per hold"
        );
        match step(&mut w, b.cur, b.first, &b.pfns) {
            MapStep::More(c) => b.cur = c,
            MapStep::Done(m) => break m,
            MapStep::Failed(f) => panic!("{f:?}"),
        }
        if steps == 2 {
            // between two holds: the range is reserved (another map is placed past it), and it is not a mapping yet (an unmap of it is
            // refused)
            let other = map_ok(&mut w, d, v, n, 4, Access::Read);
            assert!(
                other >= iova + n * PAGE,
                "the reserved range is not handed out"
            );
            assert_eq!(w.unmap(d, iova, n).err(), Some(IoError::InvalidArgs));
            unmap_ok(&mut w, d, other, 4);
        }
    };
    assert_eq!(steps, n.div_ceil(RANGE_STEP));
    assert_eq!(m.iova, iova);
    b.charge.settle(&mut w.jobs, m.tables);
    wait(&mut w, m.ticket);
    for i in 0..n {
        assert_eq!(w.sim.read(4, iova + i * PAGE), Ok(b.pfns[i as usize] << 12));
    }
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(n));
    // and the unmap goes a step at a time too
    let mut u = w.store.unmap_begin(d, iova, n).unwrap();
    let mut holds = 0;
    let p = loop {
        holds += 1;
        match w.store.unmap_step(w.mem, &mut w.sim, u) {
            UnmapStep::More(x) => u = x,
            UnmapStep::Queued(p) => break p,
            UnmapStep::Taken => panic!("no destruction"),
        }
    };
    assert_eq!(holds, n.div_ceil(RANGE_STEP));
    wait(&mut w, p.ticket());
    let rel = w.store.complete(&p).unwrap().unwrap();
    w.settle(rel);
    for i in 0..n {
        assert!(w.sim.read(4, iova + i * PAGE).is_err());
    }
    assert_eq!(w.mm.db.free_count(), base, "the tables are freed");
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
    destroy_ok(&mut w, d);
}

#[test]
fn a_destruction_begun_during_a_map_makes_it_roll_back_with_bad_state() {
    let mem = IoMem::new();
    let n = 3 * RANGE_STEP;
    let (mut w, d, v, job) = setup(&mem, n);
    let kb = kernel_bytes(&w, job);
    let mut b = begin(&mut w, d, v, 0, n);
    let iova = b.cur.io_address();
    match step(&mut w, b.cur, b.first, &b.pfns) {
        MapStep::More(c) => b.cur = c,
        x => panic!("{x:?}"),
    }
    // the destruction's step 1 does not wait for the map
    let x = w.destroy(d).unwrap();
    let (pending, tables, error) = loop {
        match step(&mut w, b.cur, b.first, &b.pfns) {
            MapStep::More(c) => {
                assert!(
                    c.next_frames().is_none(),
                    "a failing map writes nothing more"
                );
                b.cur = c;
            }
            MapStep::Failed(MapFailed::RolledBack {
                pending,
                tables,
                error,
            }) => break (pending, tables, error),
            x => panic!("{x:?}"),
        }
    };
    assert_eq!(error, IoError::BadState);
    b.charge.settle(&mut w.jobs, tables);
    for i in 0..n {
        assert!(
            w.sim.read(4, iova + i * PAGE).is_err(),
            "nothing is left mapped"
        );
    }
    // the destruction completes and takes the record; the map's own completion finds it gone
    wait(&mut w, x.ticket());
    wait(&mut w, pending.ticket());
    let mut det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    let mut got = vec![];
    while let Some(rel) = det.next_record() {
        got.push((rel.vmo(), rel.pages()));
        w.settle(rel);
    }
    w.finish(det);
    assert_eq!(got, vec![(v, n)]);
    assert!(w.store.complete(&pending).unwrap().is_none());
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
    assert_eq!(w.vmos.refs(v), Some(1), "the map's hold was dropped once");
    w.close_vmo(v);
    assert!(
        kernel_bytes(&w, job) < kb,
        "the domain's bytes are credited"
    );
}

#[test]
fn a_destruction_completed_during_a_map_hands_the_record_back() {
    let mem = IoMem::new();
    let n = 2 * RANGE_STEP + 1;
    let (mut w, d, v, job) = setup(&mem, n);
    let base = w.mm.db.free_count();
    let mut b = begin(&mut w, d, v, 0, n);
    match step(&mut w, b.cur, b.first, &b.pfns) {
        MapStep::More(c) => b.cur = c,
        x => panic!("{x:?}"),
    }
    let got = destroy_ok(&mut w, d);
    assert!(
        got.is_empty(),
        "the unpublished map is not the destruction's"
    );
    match step(&mut w, b.cur, b.first, &b.pfns) {
        MapStep::Failed(MapFailed::Dropped { released, tables }) => {
            b.charge.settle(&mut w.jobs, tables);
            assert_eq!((released.vmo(), released.pages()), (v, n));
            w.settle(released);
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
    assert_eq!(w.vmos.refs(v), Some(1));
    w.close_vmo(v);
    assert_eq!(
        kernel_bytes(&w, job),
        0,
        "every charge is credited once (J2)"
    );
    assert!(w.mm.db.free_count() > base);
}

#[test]
fn a_destruction_during_an_unmap_takes_the_record_and_the_unmap_is_ok() {
    let mem = IoMem::new();
    let n = 4 * RANGE_STEP;
    let (mut w, d, v, job) = setup(&mem, n);
    let iova = map_ok(&mut w, d, v, 0, n, Access::ReadWrite);
    let u = w.store.unmap_begin(d, iova, n).unwrap();
    let u = match w.store.unmap_step(w.mem, &mut w.sim, u) {
        UnmapStep::More(u) => u,
        x => panic!("{x:?}"),
    };
    let x = w.destroy(d).unwrap();
    // OD170: the unmap is OK; the destruction's fence covers the leaves it had not cleared
    assert!(matches!(
        w.store.unmap_step(w.mem, &mut w.sim, u),
        UnmapStep::Taken
    ));
    wait(&mut w, x.ticket());
    for i in 0..n {
        assert!(w.sim.read(4, iova + i * PAGE).is_err());
    }
    let mut det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    let mut got = vec![];
    while let Some(rel) = det.next_record() {
        got.push((rel.vmo(), rel.pages()));
        w.settle(rel);
    }
    // the tables are freed a step at a time with nothing else held
    let mut steps = 0;
    {
        let World {
            mem, mm, counts, ..
        } = &mut w;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail: None,
        };
        while !det.step(*mem, &mut f) {
            steps += 1;
        }
    }
    assert!(steps < 4);
    w.finish(det);
    assert_eq!(
        got,
        vec![(v, n)],
        "the record comes back once, from the destruction"
    );
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
    assert_eq!(w.vmos.refs(v), Some(1));
    w.close_vmo(v);
    assert_eq!(kernel_bytes(&w, job), 0);
}
