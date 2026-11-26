//! IOMMU3: the removal paths of the device-domain store allocate nothing (IOMMU-SPEC 6.5), and the two paths that do allocate fail
//! cleanly at every allocation point.
//!
//! * `unmap`, `poll`, `complete`, `Released::settle`, `abandon`, `destroy`, `destroy_complete`, `Detached::next_record` and `finish`,
//!   `destroy_abandon`, `fault` and `fault_read` count zero allocations with hundreds of mappings live (a counting allocator, as
//!   `pager_noalloc.rs`). The VMO store's own release is not counted: it is not the store's, and IOMMU6 runs it as `vmo_close` does.
//! * `create` and `map` (with its hold, its charge and its record) fail at the n-th allocation for every n until they succeed, and each
//!   failure leaves the frames, the store, the VMO's holders and every job's charges exactly as they were (as the `allocfail` tests).
mod common;
mod iodomain_common;

use iodomain_common::*;
use iommucore::faultq::{FaultRecord, TransactionType};
use iommucore::iopt::Access;
use kcore::iodomain::*;
use kcore::job::{IoResource, JobId, Limits};
use std::alloc::{GlobalAlloc, Layout, System};

thread_local! {
    static COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// `Some(n)`: the allocation after `n` more succeeds fails, and every one after it.
    static FAIL_IN: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn refuse() -> bool {
    FAIL_IN.with(|f| match f.get() {
        Some(0) => true,
        Some(n) => {
            f.set(Some(n - 1));
            false
        }
        None => false,
    })
}

struct Counting;
// SAFETY: forwards to the system allocator; the counters are per thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        COUNT.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        COUNT.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn allocs() -> usize {
    COUNT.with(|c| c.get())
}

/// Run `f` and assert it allocated nothing.
fn none<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let at = allocs();
    let r = f();
    assert_eq!(allocs() - at, 0, "{what} allocated");
    r
}

/// Run `f` with the allocation after `n` failing.
fn failing_after<T>(n: usize, f: impl FnOnce() -> T) -> T {
    FAIL_IN.with(|c| c.set(Some(n)));
    let r = f();
    FAIL_IN.with(|c| c.set(None));
    r
}

/// The settle of 6.1 step 4, with only the store's half counted.
fn settle_counted(w: &mut World, rel: Released) {
    w.vmos.release(&mut w.mm, &mut w.jobs, rel.vmo()).unwrap();
    w.vmos.drain_all(&mut w.mm, &mut w.jobs);
    let World {
        mem,
        mm,
        counts,
        jobs,
        ..
    } = w;
    let mut f = DbFrames {
        db: &mut mm.db,
        mem,
        counts,
        fail: None,
    };
    none("Released::settle", || rel.settle(*mem, &mut f, jobs));
}

fn finish_counted(w: &mut World, mut det: Detached) {
    while let Some(rel) = none("Detached::next_record", || det.next_record()) {
        settle_counted(w, rel);
    }
    let World {
        mem,
        mm,
        counts,
        jobs,
        ..
    } = w;
    let mut f = DbFrames {
        db: &mut mm.db,
        mem,
        counts,
        fail: None,
    };
    none("Detached::finish", || det.finish(*mem, &mut f, jobs));
}

fn fault(device_id: u32, iotval: u64) -> FaultRecord {
    FaultRecord {
        cause: 21,
        ttype: TransactionType::UntranslatedWrite,
        device_id,
        pv: false,
        pid: 0,
        privileged: false,
        iotval,
        iotval2: 0,
    }
}

#[test]
fn removal_and_the_fault_path_allocate_nothing_with_hundreds_of_mappings() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let base = w.mm.db.free_count();
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let mut rng = common::Rng::new(3);
    let doms = [
        ready(&mut w, 1, job),
        ready(&mut w, 2, job),
        ready(&mut w, 3, job),
    ];
    let vmos: Vec<u64> = (0..4)
        .map(|i| {
            w.vmos
                .create_dma_in(&mut w.mm, &mut w.jobs, 16 + 60 * i, job)
                .unwrap()
        })
        .collect();
    let mut maps = vec![];
    for _ in 0..600 {
        let d = doms[rng.below(3) as usize];
        let (vi, v) = {
            let i = rng.below(4) as usize;
            (i as u64, vmos[i])
        };
        let size = 16 + 60 * vi;
        let first = rng.below(size);
        let pages = 1 + rng.below((size - first).min(9));
        let iova = map_ok(&mut w, d, v, first, pages, Access::ReadWrite);
        maps.push((d, iova, pages));
    }
    // faults, and their reads, past the ring's size
    for i in 0..40u64 {
        none("fault", || {
            w.store
                .fault(fault(1 + (i % 4) as u32, IO_BASE + i * PAGE), i, 1000)
        });
    }
    for d in doms {
        while none("fault_read", || w.store.fault_read(d))
            .unwrap()
            .is_some()
        {}
    }
    // unmap a third: unmap, poll, complete and settle allocate nothing
    for _ in 0..200 {
        let (d, iova, pages) = maps.swap_remove(rng.below(maps.len() as u64) as usize);
        let p = none("unmap", || w.store.unmap(w.mem, &mut w.sim, d, iova, pages)).unwrap();
        none("poll", || w.store.poll(&mut w.sim, p.ticket()));
        w.sim.process_commands();
        assert_eq!(
            none("poll", || w.store.poll(&mut w.sim, p.ticket())),
            Fence::Done
        );
        let rel = none("complete", || w.store.complete(&p)).unwrap().unwrap();
        settle_counted(&mut w, rel);
    }
    // a stalled device: an unmap whose fence is abandoned
    let (d, iova, pages) = maps.pop().unwrap();
    let p = none("unmap", || w.store.unmap(w.mem, &mut w.sim, d, iova, pages)).unwrap();
    assert!(none("abandon", || w.store.abandon(&p)));
    // destroy one domain to completion and abandon another's destruction; the third completes too
    let x = none("destroy", || w.store.destroy(w.mem, &mut w.sim, doms[0])).unwrap();
    let y = none("destroy", || w.store.destroy(w.mem, &mut w.sim, doms[1])).unwrap();
    none("fault", || w.store.fault(fault(1, IO_BASE), 99, 1000));
    assert!(none("destroy_abandon", || w.store.destroy_abandon(&y)));
    let z = none("destroy", || w.store.destroy(w.mem, &mut w.sim, doms[2])).unwrap();
    w.sim.process_commands();
    for t in [x, z] {
        assert_eq!(
            none("poll", || w.store.poll(&mut w.sim, t.ticket())),
            Fence::Done
        );
        let det = none("destroy_complete", || w.store.destroy_complete(w.mem, &t))
            .unwrap()
            .unwrap();
        finish_counted(&mut w, det);
    }
    let s = w.store.stats();
    assert!(s.fence_timeouts >= 2 && s.holds_leaked > 0);
    assert!(
        w.mm.db.free_count() < base,
        "what was abandoned stays leaked"
    );
    assert!(w.jobs.io_usage(job, IoResource::MappedPages).unwrap() > 0);
}

/// What failure must leave unchanged.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    free: usize,
    counts: FrameCounts,
    kernel_bytes: u64,
    io: u64,
    domains: u64,
    io_pages: u64,
    vmo_refs: Option<u32>,
    records: Option<(u64, u64, u64)>,
    usage: Option<(u64, u64)>,
}

fn snapshot(w: &World, job: JobId, d: Option<DomainId>, vmo: Option<u64>) -> Snapshot {
    Snapshot {
        free: w.mm.db.free_count(),
        counts: w.counts,
        kernel_bytes: kernel_bytes(w, job),
        io: w.jobs.io_usage(job, IoResource::MappedPages).unwrap(),
        domains: w.store.stats().domains,
        io_pages: w.store.stats().io_pages,
        vmo_refs: vmo.and_then(|v| w.vmos.refs(v)),
        records: d.and_then(|d| w.store.counts(d)),
        usage: d.and_then(|d| w.store.usage(d)),
    }
}

#[test]
fn create_fails_cleanly_at_every_allocation_point() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    // the store's slot table grows with the first domains; sweep the first create, then one into a grown table
    for did in [7u32, 8, 9] {
        let before = snapshot(&w, job, None, None);
        let mut n = 0;
        let (d, t) = loop {
            match failing_after(n, || w.create(did, job)) {
                Ok(x) => break x,
                Err(e) => {
                    assert_eq!(e, IoError::NoMemory, "allocation {n}");
                    assert_eq!(
                        snapshot(&w, job, None, None),
                        before,
                        "create failing at allocation {n}"
                    );
                    assert_eq!(w.store.domain_of(did), None);
                    assert_eq!(w.store.device_state(w.mem, did), DeviceState::Unclaimed);
                }
            }
            n += 1;
            assert!(n < 100, "create never succeeded");
        };
        assert!(n > 0, "create allocates (the domain, its allocator)");
        w.sim.process_commands();
        assert_eq!(w.store.poll(&mut w.sim, t), Fence::Done);
        assert_eq!(w.store.domain_of(did), Some(d));
    }
}

#[test]
fn map_fails_cleanly_at_every_allocation_point_the_record_among_them() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 4, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 40, job)
        .unwrap();
    for (first, pages) in [(0, 1), (3, 37), (0, 40)] {
        let before = snapshot(&w, job, Some(d), Some(v));
        let mut n = 0;
        let mut record_failed = false;
        let iova = loop {
            // the kernel's sequence, every step fallible: the hold and frame list, the charge, the record, the store
            let r = failing_after(n, || {
                let mut pfns = Vec::new();
                w.vmos
                    .retain_for_io(v, first, pages, &mut pfns)
                    .map_err(|_| "hold")?;
                let charge = match charge_map(&mut w.jobs, job, pages) {
                    Ok(c) => c,
                    Err(_) => {
                        w.vmos.release(&mut w.mm, &mut w.jobs, v).unwrap();
                        return Err("charge");
                    }
                };
                let Some(rec) = Record::new(v, first, pages, Access::ReadWrite) else {
                    charge.refund(&mut w.jobs);
                    w.vmos.release(&mut w.mm, &mut w.jobs, v).unwrap();
                    return Err("record");
                };
                let World {
                    mem,
                    sim,
                    store,
                    mm,
                    counts,
                    ..
                } = &mut w;
                let mut f = DbFrames {
                    db: &mut mm.db,
                    mem,
                    counts,
                    fail: None,
                };
                match store.map(*mem, sim, &mut f, d, rec, &pfns) {
                    Ok(m) => {
                        charge.settle(&mut w.jobs, m.tables);
                        Ok(m)
                    }
                    Err(e) => panic!("the store's half of map allocated and failed: {e:?}"),
                }
            });
            match r {
                Ok(m) => break m,
                Err(at) => {
                    record_failed |= at == "record";
                    assert_eq!(
                        snapshot(&w, job, Some(d), Some(v)),
                        before,
                        "map failing at allocation {n} ({at})"
                    );
                }
            }
            n += 1;
            assert!(n < 100, "map never succeeded");
        };
        assert!(record_failed, "the sweep reached the record's allocation");
        w.sim.process_commands();
        assert_eq!(w.store.poll(&mut w.sim, iova.ticket), Fence::Done);
        assert_eq!(
            w.sim.read(4, iova.iova).unwrap() >> 12,
            w.vmos.frame(v, first).unwrap()
        );
        unmap_ok(&mut w, d, iova.iova, pages);
        assert_eq!(
            snapshot(&w, job, Some(d), Some(v)),
            before,
            "and unmapped, all is as before"
        );
    }
    destroy_ok(&mut w, d);
    w.close_vmo(v);
    assert_eq!(kernel_bytes(&w, job), 0);
}
