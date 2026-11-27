//! The device-domain calls on the host (IOMMU6; IOMMU-SPEC 4, 5.3, 7.4 and the amendment of 2026-11-26): `iodomain_create`,
//! `iodomain_map`, `iodomain_unmap`, `iodomain_destroy`, `iodomain_fault_read`, `vmo_create_dma_in` and `job_set_io_limits`, with the
//! error rows of 4.2 to 4.7 in order, the rights split, and what a device reaches checked on the simulated IOMMU of `iommucore::sim`
//! (the independent half of the oracle: its own walker and caches, which only the invalidation commands drop).
use abi::iodomain as io;
use abi::rights::Rights;
use abi::signals;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use kcore::ipc;
use kcore::job::{JobId, Resource, UNLIMITED};
use kcore::pagetable::PhysMem;
use ksys::iodomain as kio;

mod common;
use common::iohost::*;
use common::*;

const RW: u64 = io::READ | io::WRITE;
const NONE: u64 = UNLIMITED;

#[test]
fn a_driver_maps_what_it_holds_and_the_device_reaches_exactly_that() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let sup = h.domain(res, 5);
    let drv = h.narrow(sup, Rights::IODOMAIN_DRIVER);
    let v = h.dma(res, 4);
    let iova = h.map(drv, v, 1, 2, RW);
    let f = h.frames(v, 1, 2);
    assert!(h.reaches(5, iova, &f));
    assert!(h.blocked(5, iova + 2 * 4096, 1), "nothing past the range");
    assert!(h.blocked(6, iova, 2), "another device reaches nothing");
    assert_eq!(h.io_used(JobId::ROOT), 2);
    // a device write lands in the frame
    assert!(h.sim.write(5, iova + 8, 0xabcd).is_ok());
    h.ok(nr::IODOMAIN_UNMAP, [drv, iova, 2, 0, 0, 0]);
    assert!(h.blocked(5, iova, 2), "unmap stops the device");
    assert_eq!(h.io_used(JobId::ROOT), 0);
    // the same page twice, read-only, in one domain: two mappings, both charged
    let a = h.map(drv, v, 0, 1, io::READ);
    let b = h.map(drv, v, 0, 1, io::READ);
    assert_ne!(a, b);
    assert_eq!(h.io_used(JobId::ROOT), 2);
    assert!(
        h.sim.write(5, a, 1).is_err(),
        "a read mapping is not writable"
    );
    assert!(h.timeouts.is_empty());
}

#[test]
fn iodomain_create_rows() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let v = h.dma(res, 1);
    let job_ro = h.root_job(Rights::INSPECT);
    let c = nr::IODOMAIN_CREATE;
    assert_eq!(h.err(c, [0x999, 1, 0, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(c, [v, 1, 0, 0, 0, 0]), Status::WrongType);
    assert_eq!(h.err(c, [res, 1, v, 0, 0, 0]), Status::WrongType, "job");
    let weak = h.narrow(res, Rights::DUPLICATE);
    assert_eq!(h.err(c, [weak, 1, 0, 0, 0, 0]), Status::AccessDenied);
    assert_eq!(
        h.err(c, [res, 1, job_ro, 0, 0, 0]),
        Status::AccessDenied,
        "job without MANAGE"
    );
    // the kinds rank across the handles: a BAD_HANDLE job beats a WRONG_TYPE resource, a WRONG_TYPE job beats a weak resource
    assert_eq!(h.err(c, [v, 1, 0x999, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(c, [weak, 1, v, 0, 0, 0]), Status::WrongType);
    assert_eq!(
        h.err(c, [weak, 999, v, 0, 0, 0]),
        Status::WrongType,
        "handles before the id"
    );
    for bad in [64, 1 << 32, u64::MAX] {
        assert_eq!(
            h.err(c, [res, bad, 0, 0, 0, 0]),
            Status::InvalidArgs,
            "{bad}"
        );
    }
    let d = h.domain(res, 1);
    assert_eq!(
        h.err(c, [res, 1, 0, 0, 0, 0]),
        Status::BadState,
        "a live domain"
    );
    assert_eq!(
        h.err(c, [res, 64, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "INVALID_ARGS before BAD_STATE"
    );
    // the handle carries every domain right
    let e = h.w.handles.get(Handle(d)).unwrap();
    assert_eq!(e.rights, Rights::IODOMAIN);
    assert!(!e.rights.contains(Rights::MANAGE));
    // NO_MEMORY: a job with too few kernel bytes; the device stays as it was, and nothing is charged
    let poor = h.child_job([NONE, NONE, 1, NONE, NONE, NONE]);
    let pj = h.job_of(poor);
    let before = h.kernel_bytes(pj);
    assert_eq!(h.err(c, [res, 2, poor, 0, 0, 0]), Status::NoMemory);
    assert_eq!(h.kernel_bytes(pj), before);
    assert!(h.blocked(2, 0x10_0000, 1));
    h.domain(res, 2);
    // a destroyed domain's device may be claimed again
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    h.domain(res, 1);
}

#[test]
fn a_domain_charges_its_job_and_gives_it_all_back() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let job = h.child_job([NONE; 6]);
    let j = h.job_of(job);
    let base = h.kernel_bytes(j);
    let free = h.w.mm.db.free_count();
    let d = h.ok(nr::IODOMAIN_CREATE, [res, 7, job, 0, 0, 0]).0.unwrap();
    assert!(h.kernel_bytes(j) > base, "the domain's job pays");
    let v = h
        .ok(nr::VMO_CREATE_DMA_IN, [res, 8, job, 0, 0, 0])
        .0
        .unwrap();
    for _ in 0..3 {
        h.map(d, v, 0, 8, RW);
    }
    assert_eq!(h.io_used(j), 24);
    assert_eq!(h.io_used(JobId::ROOT), 24, "and every ancestor");
    h.close(d);
    assert_eq!(h.io_used(j), 0);
    h.close(v);
    h.close(job);
    assert_eq!(h.w.mm.db.free_count(), free, "every frame is back");
    assert!(h.timeouts.is_empty());
}

#[test]
fn iodomain_map_rows() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 3);
    let v = h.dma(res, 4);
    let plain = h.ok(nr::VMO_CREATE, [4, 0, 0, 0, 0, 0]).0.unwrap();
    let m = nr::IODOMAIN_MAP;
    assert_eq!(h.err(m, [0x999, v, 0, 1, RW, 0]), Status::BadHandle);
    assert_eq!(h.err(m, [d, 0x999, 0, 1, RW, 0]), Status::BadHandle);
    assert_eq!(h.err(m, [v, v, 0, 1, RW, 0]), Status::WrongType, "domain");
    assert_eq!(h.err(m, [d, d, 0, 1, RW, 0]), Status::WrongType, "vmo");
    assert_eq!(
        h.err(m, [d, plain, 0, 1, RW, 0]),
        Status::WrongType,
        "not a DMA VMO"
    );
    // a BAD_HANDLE VMO beats a WRONG_TYPE domain
    assert_eq!(h.err(m, [v, 0x999, 0, 1, RW, 0]), Status::BadHandle);
    let no_map = h.narrow(d, Rights(Rights::IODOMAIN.0 & !Rights::IO_MAP.0));
    assert_eq!(h.err(m, [no_map, v, 0, 1, RW, 0]), Status::AccessDenied);
    let no_dma = h.narrow(v, Rights(Rights::READ.0 | Rights::WRITE.0));
    assert_eq!(
        h.err(m, [d, no_dma, 0, 1, io::READ, 0]),
        Status::AccessDenied
    );
    let ro = h.narrow(v, Rights(Rights::READ.0 | Rights::DMA.0));
    assert_eq!(
        h.err(m, [d, ro, 0, 1, RW, 0]),
        Status::AccessDenied,
        "IO4: wider than the handle"
    );
    let wo = h.narrow(v, Rights(Rights::WRITE.0 | Rights::DMA.0));
    assert_eq!(
        h.err(m, [d, wo, 0, 1, io::READ, 0]),
        Status::AccessDenied,
        "READ is needed"
    );
    // a WRONG_TYPE VMO beats a domain without IO_MAP
    assert_eq!(h.err(m, [no_map, plain, 0, 1, RW, 0]), Status::WrongType);
    assert_eq!(
        h.err(m, [no_map, v, 0, 0, RW, 0]),
        Status::AccessDenied,
        "rights before arguments"
    );
    for (off, pages, acc) in [
        (0, 0, RW),
        (100, 1, RW),
        (0, 5, RW),
        (4 * 4096, 1, RW),
        (3 * 4096, 2, RW),
        (0, u64::MAX, RW),
        (!0xfff_u64, 1, RW),
        (0, 1, 4 | RW),
        (0, 1, io::WRITE),
        (0, 1, 0),
    ] {
        assert_eq!(
            h.err(m, [d, v, off, pages, acc, 0]),
            Status::InvalidArgs,
            "{off} {pages} {acc}"
        );
    }
    // a read-only handle maps for reading
    let iova = h.map(d, ro, 0, 1, io::READ);
    assert!(h.sim.read(3, iova).is_ok());
    // NO_MEMORY: the job's limit on I/O-mapped pages (5.3); nothing is left charged or mapped
    let job = h.child_job([NONE; 6]);
    let j = h.job_of(job);
    h.ok(nr::JOB_SET_IO_LIMITS, [job, 3, NONE, 0, 0, 0]);
    let jd = h.ok(nr::IODOMAIN_CREATE, [res, 4, job, 0, 0, 0]).0.unwrap();
    let kb = h.kernel_bytes(j);
    assert_eq!(h.err(m, [jd, v, 0, 4, RW, 0]), Status::NoMemory);
    assert_eq!((h.io_used(j), h.kernel_bytes(j)), (0, kb));
    let a = h.map(jd, v, 0, 3, RW);
    assert_eq!(
        h.err(m, [jd, v, 0, 1, RW, 0]),
        Status::NoMemory,
        "the limit is reached"
    );
    h.ok(nr::IODOMAIN_UNMAP, [jd, a, 3, 0, 0, 0]);
    h.map(jd, v, 0, 1, RW);
    // BAD_STATE: the domain was destroyed (after INVALID_ARGS)
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    assert_eq!(h.err(m, [d, v, 0, 0, RW, 0]), Status::InvalidArgs);
    assert_eq!(h.err(m, [d, v, 0, 1, RW, 0]), Status::BadState);
    let drv = h.narrow(d, Rights::IODOMAIN_DRIVER);
    assert_eq!(
        h.err(m, [drv, v, 0, 1, RW, 0]),
        Status::BadState,
        "on every handle"
    );
    assert!(h.blocked(3, iova, 1), "the destruction removed the mapping");
}

#[test]
fn iodomain_unmap_rows() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 9);
    let v = h.dma(res, 4);
    let iova = h.map(d, v, 0, 4, RW);
    let u = nr::IODOMAIN_UNMAP;
    assert_eq!(h.err(u, [0x999, iova, 4, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(u, [v, iova, 4, 0, 0, 0]), Status::WrongType);
    let no_unmap = h.narrow(d, Rights(Rights::IODOMAIN.0 & !Rights::IO_UNMAP.0));
    assert_eq!(h.err(u, [no_unmap, iova, 4, 0, 0, 0]), Status::AccessDenied);
    // not exactly a live mapping: a mapping is not split or merged
    for (at, pages) in [
        (iova, 0),
        (iova + 1, 4),
        (iova, 3),
        (iova, 5),
        (iova + 4096, 3),
        (iova + 4096 * 64, 1),
        (0, 1),
        (!0xfff_u64, 1),
        (iova, u64::MAX),
    ] {
        assert_eq!(
            h.err(u, [d, at, pages, 0, 0, 0]),
            Status::InvalidArgs,
            "{at:#x} {pages}"
        );
    }
    let f = h.frames(v, 0, 4);
    assert!(h.reaches(9, iova, &f), "a refused unmap changes nothing");
    h.ok(u, [d, iova, 4, 0, 0, 0]);
    assert_eq!(
        h.err(u, [d, iova, 4, 0, 0, 0]),
        Status::InvalidArgs,
        "twice"
    );
    let again = h.map(d, v, 0, 4, RW);
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    // (OD171: a destroyed domain is BAD_STATE, before the mapping and the arguments are looked at)
    assert_eq!(h.err(u, [d, again, 4, 0, 0, 0]), Status::BadState);
    assert_eq!(h.err(u, [d, again, 0, 0, 0, 0]), Status::BadState);
    assert_eq!(h.err(u, [d, 1, 4, 0, 0, 0]), Status::BadState);
    assert_eq!(
        h.err(u, [no_unmap, again, 4, 0, 0, 0]),
        Status::AccessDenied,
        "ACCESS_DENIED before BAD_STATE"
    );
}

#[test]
fn each_domain_right_allows_its_call_alone() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 11);
    let v = h.dma(res, 2);
    let only = |h: &mut Host<'_>, r: Rights| h.narrow(d, r);
    let map_only = only(&mut h, Rights::IO_MAP);
    let unmap_only = only(&mut h, Rights::IO_UNMAP);
    let destroy_only = only(&mut h, Rights::IO_DESTROY);
    let inspect_only = only(&mut h, Rights::INSPECT);
    let iova = h.map(map_only, v, 0, 1, RW);
    for x in [unmap_only, destroy_only, inspect_only] {
        assert_eq!(
            h.err(nr::IODOMAIN_MAP, [x, v, 0, 1, RW, 0]),
            Status::AccessDenied
        );
    }
    for x in [map_only, destroy_only, inspect_only] {
        assert_eq!(
            h.err(nr::IODOMAIN_UNMAP, [x, iova, 1, 0, 0, 0]),
            Status::AccessDenied
        );
        assert_eq!(
            h.err(nr::IODOMAIN_UNMAP, [x, 0, 0, 0, 0, 0]),
            Status::AccessDenied
        );
    }
    for x in [map_only, unmap_only, inspect_only] {
        assert_eq!(
            h.err(nr::IODOMAIN_DESTROY, [x, 0, 0, 0, 0, 0]),
            Status::AccessDenied
        );
    }
    for x in [map_only, unmap_only, destroy_only] {
        assert_eq!(
            h.err(nr::IODOMAIN_FAULT_READ, [x, 0, 0, 0, 0, 0]),
            Status::AccessDenied
        );
    }
    assert_eq!(
        h.err(nr::IODOMAIN_FAULT_READ, [inspect_only, 0, 0, 0, 0, 0]),
        Status::ShouldWait
    );
    h.ok(nr::IODOMAIN_UNMAP, [unmap_only, iova, 1, 0, 0, 0]);
    // the driver's duplicate cannot be widened, passed on or duplicated (it lacks DUPLICATE)
    let drv = h.narrow(d, Rights::IODOMAIN_DRIVER);
    assert_eq!(
        h.err(
            nr::HANDLE_DUPLICATE,
            [drv, u64::from(Rights::IODOMAIN.0), 0, 0, 0, 0]
        ),
        Status::AccessDenied
    );
    assert_eq!(
        h.err(
            nr::HANDLE_DUPLICATE,
            [drv, u64::from(Rights::IODOMAIN_DRIVER.0), 0, 0, 0, 0]
        ),
        Status::AccessDenied
    );
    // the other domain calls refuse a non-domain with WRONG_TYPE
    for n in [nr::IODOMAIN_DESTROY, nr::IODOMAIN_FAULT_READ] {
        assert_eq!(h.err(n, [v, 0, 0, 0, 0, 0]), Status::WrongType);
        assert_eq!(h.err(n, [0x999, 0, 0, 0, 0, 0]), Status::BadHandle);
    }
    h.ok(nr::IODOMAIN_DESTROY, [destroy_only, 0, 0, 0, 0, 0]);
}

#[test]
fn destroy_quarantines_the_device_and_a_second_destroy_does_nothing() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 12);
    let v = h.dma(res, 3);
    let iova = h.map(d, v, 0, 3, RW);
    let id = h.vmo_id(v);
    h.close(v);
    assert!(
        h.w.vmos.pages(id).is_some(),
        "the mapping holds the VMO (5.1)"
    );
    let free = h.w.mm.db.free_count();
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    assert!(h.blocked(12, iova, 3));
    assert!(
        h.w.vmos.pages(id).is_none(),
        "the last hold went with the destruction"
    );
    assert!(
        h.w.mm.db.free_count() > free,
        "the VMO's frames and the tables are freed"
    );
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    assert_eq!(
        h.err(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]),
        Status::ShouldWait
    );
    h.close(d);
    assert!(h.timeouts.is_empty());
}

#[test]
fn closing_the_last_handle_destroys_the_domain() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let job = h.child_job([NONE; 6]);
    let free = h.w.mm.db.free_count(); // (after the job: its limits are poked into a user page)
    let j = h.job_of(job);
    let kb = h.kernel_bytes(j);
    let d = h
        .ok(nr::IODOMAIN_CREATE, [res, 13, job, 0, 0, 0])
        .0
        .unwrap();
    let drv = h.narrow(d, Rights::IODOMAIN_DRIVER);
    let v = h.dma(res, 2);
    let iova = h.map(drv, v, 0, 2, RW);
    h.close(d);
    let f = h.frames(v, 0, 2);
    assert!(h.reaches(13, iova, &f), "the driver's handle keeps it");
    h.close(drv);
    assert!(h.blocked(13, iova, 2));
    assert_eq!(h.kernel_bytes(j), kb, "the domain's charges are credited");
    h.close(v);
    h.close(job);
    assert_eq!(h.w.mm.db.free_count(), free);
    // the device can be claimed again
    let d2 = h.domain(res, 13);
    h.close(d2);
}

#[test]
fn maps_and_unmaps_far_larger_than_a_step() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 14);
    let n = 3 * kcore::iodomain::RANGE_STEP + 17;
    let v = h.dma(res, n);
    let iova = h.map(d, v, 0, n, RW);
    let f = h.frames(v, 0, n);
    assert!(h.reaches(14, iova, &f));
    let b = h.map(d, v, 5, n - 5, io::READ);
    let g = h.frames(v, 5, n - 5);
    assert!(h.reaches(14, b, &g));
    h.ok(nr::IODOMAIN_UNMAP, [d, iova, n, 0, 0, 0]);
    assert!(h.blocked(14, iova, n));
    assert!(h.reaches(14, b, &g), "the other mapping stays");
    assert_eq!(h.io_used(JobId::ROOT), n - 5);
    h.close(d);
    assert_eq!(h.io_used(JobId::ROOT), 0);
}

#[test]
fn vmo_create_dma_in_rows() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let plain = h.ok(nr::VMO_CREATE, [1, 0, 0, 0, 0, 0]).0.unwrap();
    let job_ro = h.root_job(Rights::INSPECT);
    let c = nr::VMO_CREATE_DMA_IN;
    assert_eq!(h.err(c, [0x999, 1, 0, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(c, [res, 1, 0x999, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(c, [plain, 1, 0, 0, 0, 0]), Status::WrongType);
    assert_eq!(h.err(c, [res, 1, plain, 0, 0, 0]), Status::WrongType);
    let weak = h.narrow(res, Rights::DUPLICATE);
    assert_eq!(h.err(c, [weak, 1, 0, 0, 0, 0]), Status::AccessDenied);
    assert_eq!(h.err(c, [res, 1, job_ro, 0, 0, 0]), Status::AccessDenied);
    assert_eq!(
        h.err(c, [weak, 1, plain, 0, 0, 0]),
        Status::WrongType,
        "ranked across handles"
    );
    assert_eq!(
        h.err(c, [weak, 0, 0, 0, 0, 0]),
        Status::AccessDenied,
        "handles before arguments"
    );
    assert_eq!(h.err(c, [res, 0, 0, 0, 0, 0]), Status::InvalidArgs);
    h.w.vmos.set_dma_total_cap(300);
    assert_eq!(
        h.err(c, [res, 301, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "past the backstop"
    );
    assert_eq!(h.err(c, [res, u64::MAX, 0, 0, 0, 0]), Status::InvalidArgs);
    // NO_MEMORY: the job's pages, and nothing stays charged
    let poor = h.child_job([NONE, NONE, 10, NONE, NONE, NONE]);
    let pj = h.job_of(poor);
    let used = h.w.jobs.usage(pj).unwrap();
    assert_eq!(h.err(c, [res, 11, poor, 0, 0, 0]), Status::NoMemory);
    assert_eq!(h.w.jobs.usage(pj).unwrap(), used);
    // NO_MEMORY: the frames run out part way (more than a step's worth in); every frame taken is given back
    let free = h.w.mm.db.free_count() as u64;
    h.w.vmos.set_dma_total_cap(free + 100);
    assert_eq!(h.err(c, [res, free + 1, 0, 0, 0, 0]), Status::NoMemory);
    assert_eq!(h.w.mm.db.free_count() as u64, free);
    // a VMO of several steps' pages: committed and zeroed, charged to the job named, with the VMO rights less EXECUTE, plus DMA
    let rich = h.child_job([NONE; 6]);
    let rj = h.job_of(rich);
    let before = h.w.jobs.usage(rj).unwrap().get(Resource::Pages);
    let v = h.ok(c, [res, 200, rich, 0, 0, 0]).0.unwrap();
    assert_eq!(
        h.w.jobs.usage(rj).unwrap().get(Resource::Pages),
        before + 200
    );
    let e = h.w.handles.get(Handle(v)).unwrap();
    assert_eq!(
        e.rights,
        Rights((Rights::VMO.0 & !Rights::EXECUTE.0) | Rights::DMA.0)
    );
    let f = h.frames(v, 0, 200);
    for &p in &f {
        assert!(
            (0..512).all(|i| h.w.mm.mem.read_u64((p << 12) + i * 8) == 0),
            "zeroed"
        );
    }
    h.close(v);
    assert_eq!(h.w.jobs.usage(rj).unwrap().get(Resource::Pages), before);
}

#[test]
fn job_set_io_limits_and_job_info() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let parent = h.child_job([NONE; 6]);
    let s = nr::JOB_SET_IO_LIMITS;
    assert_eq!(h.err(s, [0x999, 1, 1, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(s, [res, 1, 1, 0, 0, 0]), Status::WrongType);
    let weak = h.narrow(parent, Rights(Rights::JOB.0 & !Rights::SET_LIMITS.0));
    assert_eq!(h.err(s, [weak, 1, 1, 0, 0, 0]), Status::AccessDenied);
    // no default: unlimited until set
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 12, 0, 0, 0, 0]),
        (Some(0), Some(NONE))
    );
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 13, 0, 0, 0, 0]),
        (Some(0), Some(NONE))
    );
    h.ok(s, [parent, 8, 3, 0, 0, 0]);
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 12, 0, 0, 0, 0]),
        (Some(0), Some(8))
    );
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 13, 0, 0, 0, 0]),
        (Some(0), Some(3))
    );
    // a child inherits the smaller cap and cannot exceed it
    let bytes: Vec<u8> = [NONE; 6].iter().flat_map(|v| v.to_le_bytes()).collect();
    h.w.poke(BASE, &bytes);
    let child = h.ok(nr::JOB_CREATE, [parent, BASE, 0, 0, 0, 0]).0.unwrap();
    h.ok(s, [child, 100, NONE, 0, 0, 0]);
    assert_eq!(
        h.ok(nr::JOB_INFO, [child, 12, 0, 0, 0, 0]),
        (Some(0), Some(8))
    );
    assert_eq!(
        h.ok(nr::JOB_INFO, [child, 13, 0, 0, 0, 0]),
        (Some(0), Some(3))
    );
    let d = h
        .ok(nr::IODOMAIN_CREATE, [res, 20, child, 0, 0, 0])
        .0
        .unwrap();
    let v = h.dma(res, 9);
    assert_eq!(
        h.err(nr::IODOMAIN_MAP, [d, v, 0, 9, RW, 0]),
        Status::NoMemory
    );
    h.map(d, v, 0, 5, RW);
    assert_eq!(
        h.ok(nr::JOB_INFO, [child, 12, 0, 0, 0, 0]),
        (Some(5), Some(8))
    );
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 12, 0, 0, 0, 0]),
        (Some(5), Some(8)),
        "counted on the ancestors"
    );
    // a new limit below the usage is not an error; later charges fail
    h.ok(s, [parent, 2, 3, 0, 0, 0]);
    assert_eq!(
        h.err(nr::IODOMAIN_MAP, [d, v, 0, 1, RW, 0]),
        Status::NoMemory
    );
    assert_eq!(
        h.ok(nr::JOB_INFO, [child, 12, 0, 0, 0, 0]),
        (Some(5), Some(2))
    );
    h.close(d);
    assert_eq!(
        h.ok(nr::JOB_INFO, [parent, 12, 0, 0, 0, 0]),
        (Some(0), Some(2))
    );
}

/// 7.4: a device that faults the moment it is attached (a device a killed supervisor left running, 10.3), before the call has made
/// the domain's object: the record is kept, and DEVICE_FAULT is up when the handle is returned.
#[test]
fn a_fault_during_the_create_raises_device_fault_on_the_new_handle() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    h.fault_in_relax = Some((23, 0x5000));
    let d = h.domain(res, 23);
    assert!(
        h.fault_in_relax.is_none(),
        "the fault was taken during the create"
    );
    let sig = ipc::signals_of(&h.w.objs, &h.w.handles, Handle(d)).unwrap();
    assert_ne!(
        sig & signals::DEVICE_FAULT,
        0,
        "DEVICE_FAULT is up on the new handle"
    );
    let (_, a) = h.ok(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]);
    assert_eq!(a, Some(0x5000));
}

#[test]
fn a_device_fault_reaches_the_supervisor() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 21);
    let drv = h.narrow(d, Rights::IODOMAIN_DRIVER);
    let v = h.dma(res, 1);
    let iova = h.map(drv, v, 0, 1, io::READ);
    let sig = |h: &Host<'_>| ipc::signals_of(&h.w.objs, &h.w.handles, Handle(d)).unwrap();
    assert_eq!(sig(&h) & signals::DEVICE_FAULT, 0);
    // a write to a read mapping, and a read past it
    assert!(h.sim.write(21, iova, 7).is_err());
    assert!(h.sim.read(21, iova + 4096).is_err());
    h.fault_interrupt();
    assert_ne!(sig(&h) & signals::DEVICE_FAULT, 0);
    let (w1, a1) = h.ok(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]);
    let w1 = w1.unwrap();
    assert_eq!(a1, Some(iova));
    assert_eq!((w1 >> io::DIRECTION_SHIFT) & 0xffff, io::DIR_WRITE);
    assert_eq!(w1 >> io::DROPPED_SHIFT, 0);
    assert_ne!(w1 & 0xffff, 0, "the cause");
    assert_ne!(sig(&h) & signals::DEVICE_FAULT, 0, "one more waits");
    let (w2, a2) = h.ok(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]);
    assert_eq!(a2, Some(iova + 4096));
    assert_eq!((w2.unwrap() >> io::DIRECTION_SHIFT) & 0xffff, io::DIR_READ);
    assert_eq!(sig(&h) & signals::DEVICE_FAULT, 0, "the ring is empty");
    assert_eq!(
        h.err(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]),
        Status::ShouldWait
    );
    // the driver cannot read them
    assert_eq!(
        h.err(nr::IODOMAIN_FAULT_READ, [drv, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
}

#[test]
fn a_fence_that_never_completes_leaks_and_the_call_reports_it() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 22);
    let v = h.dma(res, 2);
    let ok = h.map(d, v, 0, 1, RW);
    h.stall = true;
    // a map whose fence fails is unmapped again and is NO_MEMORY; the roll-back's fence cannot complete either, so its frames leak
    assert_eq!(
        h.err(nr::IODOMAIN_MAP, [d, v, 4096, 1, RW, 0]),
        Status::NoMemory
    );
    assert_eq!(h.timeouts, vec![Some(22), Some(22)]);
    // an unmap whose fence fails is still OK (4.4)
    h.ok(nr::IODOMAIN_UNMAP, [d, ok, 1, 0, 0, 0]);
    assert_eq!(h.timeouts.len(), 3);
    // (the store counts the fences it abandoned: the map's own fence is not one, its roll-back's is)
    assert_eq!(h.store.stats().fence_timeouts, 2);
    assert_eq!(h.store.stats().holds_leaked, 2);
    let id = h.vmo_id(v);
    h.close(v);
    assert!(
        h.w.vmos.pages(id).is_some(),
        "a leaked mapping's hold is never dropped"
    );
}

// ---- interrupt vectors (IOMMU7; IOMMU-SPEC 8.2 to 8.5, 6.2, 10.6) -------------------------------------------------------------------

const MSI_INVALID: u16 = 262;
const MSI_MISCONFIGURED: u16 = 263;

/// The route of vector handle `x` (the kernel's name for it).
fn vref(h: &Host<'_>, x: u64) -> ipc::VectorRef {
    ipc::interrupt_kind_of(&h.w.objs, &h.w.handles, Handle(x), Rights::WAIT)
        .unwrap()
        .1
        .expect("a vector")
}

/// Where vector handle `x` is placed: the hart of its open identity.
fn hart_of(h: &Host<'_>, x: u64) -> u16 {
    let (p, ready) = h.vecs.current(&h.ids, vref(h, x)).unwrap();
    assert!(ready, "the vector's identity is open");
    p.hart
}

/// Hart `f` goes offline as `hart_set_online` takes it: its routes evacuated, then the moves settled with no lock held.
fn offline(h: &mut Host<'_>, f: u16) -> bool {
    let usable: Vec<bool> = (0..HARTS)
        .map(|g| g != f && !h.harts.offline[usize::from(g)])
        .collect();
    let asked = &mut h.harts.asked;
    let r = h.ids.evacuate(
        f,
        |g| usable[usize::from(g)],
        |c| match c {
            kcore::msiid::Evacuated::Cancelled { at, .. } => {
                asked.insert(at.hart);
            }
            kcore::msiid::Evacuated::Moved { to, .. } => {
                asked.insert(to.hart);
            }
            kcore::msiid::Evacuated::Replaced { from, to, .. } => {
                asked.insert(from.hart);
                asked.insert(to.hart);
            }
        },
    );
    if r.is_err() {
        return false;
    }
    h.harts.offline[usize::from(f)] = true;
    h.relax_harts();
    kio::settle_moves(h);
    h.settle();
    true
}

fn vector_usage(h: &mut Host<'_>, job: u64) -> (Option<u64>, Option<u64>) {
    h.ok(nr::JOB_INFO, [job, 13, 0, 0, 0, 0])
}

/// The state every vector test ends in: no route, no identity, no file frame, no charge left behind (the leak oracle of 8.3).
fn at_baseline(h: &Host<'_>, free_ids: u64, frames: usize) {
    let c = h.vecs.counts();
    assert_eq!((c.live, c.dead, c.leaked), (0, 0, 0), "{c:?}");
    assert_eq!(h.ids.free_total(), free_ids, "every identity is free again");
    assert_eq!(
        h.w.mm.db.free_count(),
        frames,
        "every file frame is free again"
    );
    assert_eq!(
        h.no_route, 0,
        "no message reached an identity with no route"
    );
}

#[test]
fn iodomain_grant_vector_rows() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 7);
    let drv = h.narrow(d, Rights::IODOMAIN_DRIVER);
    let v = h.dma(res, 1);
    let g = nr::IODOMAIN_GRANT_VECTOR;
    assert_eq!(h.err(g, [0x999, 0, 0, 0, 0, 0]), Status::BadHandle);
    assert_eq!(h.err(g, [v, 0, 0, 0, 0, 0]), Status::WrongType);
    assert_eq!(
        h.err(g, [drv, 0, 0, 0, 0, 0]),
        Status::AccessDenied,
        "a driver's handle cannot grant"
    );
    let all_but = h.narrow(d, Rights(Rights::IODOMAIN.0 & !Rights::IO_GRANT.0));
    assert_eq!(h.err(g, [all_but, 0, 0, 0, 0, 0]), Status::AccessDenied);
    for bad in [256, 1 << 16, 1 << 32, u64::MAX] {
        assert_eq!(h.err(g, [d, bad, 0, 0, 0, 0]), Status::InvalidArgs, "{bad}");
    }
    assert_eq!(
        h.err(g, [drv, 256, 0, 0, 0, 0]),
        Status::AccessDenied,
        "ACCESS_DENIED before INVALID_ARGS"
    );
    // the pair to program: the window's page k, and the data word 1
    let (x, addr) = h.grant(d, 3);
    assert_eq!(addr, kcore::iodomain::IoDomains::vector_address(3));
    assert_eq!(addr % 4096, 0);
    assert_eq!(io::MSI_DATA, 1);
    let e = h.w.handles.get(Handle(x)).unwrap();
    assert_eq!(
        e.rights,
        Rights::INTERRUPT,
        "the rights of an interrupt object"
    );
    assert_eq!(
        h.err(g, [d, 3, 0, 0, 0, 0]),
        Status::BadState,
        "granted and live"
    );
    assert_eq!(
        h.err(g, [d, 256, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "INVALID_ARGS before BAD_STATE"
    );
    let (y, _) = h.grant(d, 255);
    assert_ne!(x, y);
    // a destroyed domain grants nothing
    h.ok(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0]);
    assert_eq!(h.err(g, [d, 4, 0, 0, 0, 0]), Status::BadState);
    assert_eq!(h.err(g, [d, 256, 0, 0, 0, 0]), Status::InvalidArgs);
    // its vectors are dead: acknowledgement is BAD_STATE, and they never fire
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]),
        Status::BadState
    );
    assert_eq!(
        h.err(nr::INTERRUPT_DELIVER_HERE, [y, 0, 0, 0, 0, 0]),
        Status::BadState
    );
    assert!(
        h.signal(7, 3).is_err(),
        "the quarantined device cannot interrupt"
    );
    assert!(!h.interrupt_pending(x));
    assert_eq!(h.vecs.counts().live, 0);
    assert!(h.timeouts.is_empty());
}

#[test]
fn a_granted_vector_fires_once_per_pending_bit_and_acknowledges() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let res = h.resource();
    let d = h.domain(res, 9);
    let frames = h.w.mm.db.free_count();
    let (x, _) = h.grant(d, 0);
    assert!(!h.interrupt_pending(x));
    assert!(h.signal(9, 0).is_ok());
    assert!(h.interrupt_pending(x), "the grant: its own vector fires");
    assert_eq!((h.notices, h.fired), (1, 1));
    // while fired, a message sets pending and sends nothing
    assert!(h.signal(9, 0).is_ok());
    assert_eq!(h.notices, 1, "masked: no notice");
    // the acknowledgement finds the bit and fires again at once
    let (claims, _) = h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    assert!(claims.is_some());
    assert!(
        h.interrupt_pending(x),
        "a message while masked fires again at once"
    );
    h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    assert!(!h.interrupt_pending(x), "armed again");
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]),
        Status::BadState,
        "not fired"
    );
    assert!(h.signal(9, 0).is_ok());
    assert!(h.interrupt_pending(x));
    // the acknowledge right: a handle that can only wait cannot acknowledge or move it
    let waiter = h.narrow(x, Rights::WAIT);
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [waiter, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        h.err(nr::INTERRUPT_DELIVER_HERE, [waiter, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        h.err(nr::INTERRUPT_DELIVER_HERE, [0x999, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        h.err(nr::INTERRUPT_DELIVER_HERE, [d, 0, 0, 0, 0, 0]),
        Status::WrongType
    );
    h.close(waiter);
    h.close(x);
    at_baseline(&h, free_ids, frames);
    assert!(h.signal(9, 0).is_err(), "revoked: the entry is invalid");
    h.close(d);
}

#[test]
fn an_ungranted_or_forged_message_faults_and_is_delivered_to_nobody() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 11);
    let (x, addr) = h.grant(d, 2);
    let base = (h.notices, h.fired);
    // a page of the window whose vector is not granted: the entry is invalid
    assert_eq!(h.signal(11, 5), Err(MSI_INVALID));
    let (w, a) = h.ok(nr::IODOMAIN_FAULT_READ, [d, 0, 0, 0, 0, 0]);
    assert_eq!(w.unwrap() & 0xffff, u64::from(MSI_INVALID));
    let _ = a; // (QEMU reports an MSI fault with no address: iotval 0)
               // a data word no file holds, and a misaligned address: misconfigured
    assert_eq!(h.signal_raw(11, addr, 4096), Err(MSI_MISCONFIGURED));
    assert_eq!(h.signal_raw(11, addr + 2, 1), Err(MSI_MISCONFIGURED));
    // a real interrupt file's page, from the device: an ordinary translation fault, nothing written
    assert!(h.signal_raw(11, IMSIC_BASE, 1).is_err());
    // another device's message to this domain's window reaches nothing
    assert!(h.signal(12, 2).is_err());
    assert_eq!((h.notices, h.fired), base, "nothing was delivered");
    assert!(!h.interrupt_pending(x));
    // a data word other than 1 on a granted page: a stray bit in its own file, found and counted at the acknowledgement
    assert!(h.signal_raw(11, addr, 5).is_ok());
    assert_eq!((h.notices, h.fired), base, "a stray bit sends nothing");
    assert!(h.signal(11, 2).is_ok());
    assert!(h.interrupt_pending(x));
    let strays = |h: &Host<'_>| h.vecs.counts().strays + h.store.stats().strays;
    assert_eq!(strays(&h), 0);
    h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    assert_eq!(strays(&h), 1, "the stray bit is cleared and counted");
    // and at revoke
    assert!(h.signal_raw(11, addr, 9).is_ok());
    h.close(x);
    assert_eq!(strays(&h), 2);
    assert_eq!(h.no_route, 0);
    h.close(d);
}

#[test]
fn revoke_stops_the_vector_and_gives_everything_back() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let res = h.resource();
    let job = h.child_job([NONE; 6]);
    let j = h.job_of(job);
    let frames = h.w.mm.db.free_count();
    let d = h
        .ok(nr::IODOMAIN_CREATE, [res, 13, job, 0, 0, 0])
        .0
        .unwrap();
    let bytes = h.kernel_bytes(j);
    let (x, _) = h.grant(d, 1);
    let (y, _) = h.grant(d, 2);
    assert_eq!(vector_usage(&mut h, job).0, Some(2));
    assert!(h.kernel_bytes(j) > bytes, "a file each, charged");
    assert_eq!(h.ids.free_total(), free_ids - 2);
    assert!(h.signal(13, 1).is_ok());
    assert!(h.interrupt_pending(x));
    // a duplicate keeps it live; the last handle revokes it
    let x2 = h.narrow(x, Rights::WAIT);
    h.close(x);
    assert!(h.signal(13, 1).is_ok(), "one handle is left");
    h.close(x2);
    assert_eq!(
        h.signal(13, 1),
        Err(MSI_INVALID),
        "revoked: invalid, the cached entry dropped"
    );
    assert_eq!(vector_usage(&mut h, job).0, Some(1));
    assert_eq!(h.ids.free_total(), free_ids - 1);
    // the index can be granted again
    let (x3, _) = h.grant(d, 1);
    assert!(h.signal(13, 1).is_ok());
    assert!(h.interrupt_pending(x3));
    // the domain's destruction revokes the rest; the handles stay, dead. Before the call returns (6.3: the call alone, without
    // the tail a later call would run) the identities are retired and the charges given back
    h.dispatch(nr::IODOMAIN_DESTROY, [d, 0, 0, 0, 0, 0])
        .expect("handled")
        .expect("destroyed");
    assert_eq!(
        h.ids.free_total(),
        free_ids,
        "every identity retired before the destroy returned"
    );
    assert_eq!(
        h.vecs.counts().dead,
        0,
        "no route left to credit after the destroy"
    );
    assert_eq!(vector_usage(&mut h, job).0, Some(0));
    assert_eq!(
        h.kernel_bytes(j),
        0,
        "the domain's bytes and its files', all given back"
    );
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [x3, 0, 0, 0, 0, 0]),
        Status::BadState
    );
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [y, 0, 0, 0, 0, 0]),
        Status::BadState
    );
    h.close(x3);
    h.close(y);
    h.close(d);
    at_baseline(&h, free_ids, frames);
    assert!(h.timeouts.is_empty());
}

#[test]
fn the_vector_limit_is_hierarchical_and_a_refusal_charges_nothing() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let res = h.resource();
    let parent = h.child_job([NONE; 6]);
    h.ok(nr::JOB_SET_IO_LIMITS, [parent, NONE, 2, 0, 0, 0]);
    let bytes: Vec<u8> = [NONE; 6].iter().flat_map(|v| v.to_le_bytes()).collect();
    h.w.poke(BASE, &bytes);
    let child = h.ok(nr::JOB_CREATE, [parent, BASE, 0, 0, 0, 0]).0.unwrap();
    let a = h
        .ok(nr::IODOMAIN_CREATE, [res, 14, child, 0, 0, 0])
        .0
        .unwrap();
    let b = h
        .ok(nr::IODOMAIN_CREATE, [res, 15, parent, 0, 0, 0])
        .0
        .unwrap();
    let (x, _) = h.grant(a, 0);
    let (_y, _) = h.grant(b, 0);
    assert_eq!(
        vector_usage(&mut h, parent),
        (Some(2), Some(2)),
        "the child's vector counts on the parent"
    );
    let used = h.ids.free_total();
    assert_eq!(
        h.err(nr::IODOMAIN_GRANT_VECTOR, [a, 1, 0, 0, 0, 0]),
        Status::NoMemory
    );
    assert_eq!(vector_usage(&mut h, child).0, Some(1), "nothing charged");
    assert_eq!(h.ids.free_total(), used, "no identity taken");
    // the charge comes back when the vector's identity is free again, before the close returns
    h.close(x);
    assert_eq!(vector_usage(&mut h, parent).0, Some(1));
    let (x, _) = h.grant(a, 1);
    assert_eq!(vector_usage(&mut h, child).0, Some(1));
    h.close(x);
    h.close(_y);
    h.close(a);
    h.close(b);
    assert_eq!(vector_usage(&mut h, parent).0, Some(0));
    assert_eq!(h.ids.free_total(), free_ids);
}

#[test]
fn placement_moves_and_hart_offline_keep_every_message() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let res = h.resource();
    let frames = h.w.mm.db.free_count();
    let d = h.domain(res, 17);
    h.harts.here = 2;
    let (x, _) = h.grant(d, 0);
    // placed at the grant on the hart with the fewest routes (hart 0 has none taken here: identity counts decide)
    let first = hart_of(&h, x);
    // the first wait moves it to the hart that waits, once
    h.harts.here = 3;
    kio::first_wait_vector(&mut h, Handle(x));
    h.settle();
    assert_eq!(hart_of(&h, x), 3, "the first wait places it");
    h.harts.here = 1;
    kio::first_wait_vector(&mut h, Handle(x));
    h.settle();
    assert_eq!(hart_of(&h, x), 3, "only the first");
    assert!(h.signal(17, 0).is_ok());
    assert!(h.interrupt_pending(x));
    h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    assert_eq!(
        hart_of(&h, x),
        3,
        "an acknowledgement after placement issues no move"
    );
    // the explicit move
    h.harts.here = 1;
    h.ok(nr::INTERRUPT_DELIVER_HERE, [x, 0, 0, 0, 0, 0]);
    assert_eq!(hart_of(&h, x), 1);
    assert_eq!(
        h.ids.free_total(),
        free_ids - 1,
        "the old identity was retired"
    );
    assert!(h.signal(17, 0).is_ok());
    assert!(h.interrupt_pending(x), "delivered on the new identity");
    h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    // its hart goes offline: it moves to another, and a message right after still arrives
    assert!(offline(&mut h, 1));
    let now = hart_of(&h, x);
    assert_ne!(now, 1);
    assert_eq!(h.ids.free_total(), free_ids - 1);
    assert!(h.signal(17, 0).is_ok());
    assert!(h.interrupt_pending(x));
    h.ok(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]);
    let _ = first;
    h.close(x);
    h.close(d);
    at_baseline(&h, free_ids, frames);
}

/// 8.3 and 6.4 (review of IOMMU7): a grant whose chosen hart does not answer takes an identity on the calling hart instead and
/// succeeds (NO_MEMORY is only for the causes 8.3 lists); the unanswered identity is retired when its hart answers.
#[test]
fn a_grant_whose_hart_does_not_answer_is_placed_on_the_calling_hart() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let frames = h.w.mm.db.free_count();
    let res = h.resource();
    let d = h.domain(res, 24);
    // (hart 0 has the fewest routes and is chosen first; the caller runs on hart 3)
    h.harts.here = 3;
    h.harts.deaf = Some(0);
    let (x, _) = h.grant(d, 0);
    assert_eq!(hart_of(&h, x), 3, "placed on the calling hart");
    assert!(h.signal(24, 0).is_ok());
    assert!(h.interrupt_pending(x), "and delivered");
    h.harts.deaf = None;
    h.close(x);
    h.close(d);
    at_baseline(&h, free_ids, frames);
}

/// 8.5, hart offline (review of IOMMU7): the hart a move is going to goes offline before the move's fence. The evacuation's settle
/// skips the busy route; the fence then names an identity the route has already left, which asks for a settle, and the settle moves
/// the entry to the identity the evacuation opened. The vector ends on an online hart and is delivered.
#[test]
fn a_hart_going_offline_during_a_move_is_settled_when_the_move_fences() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let free_ids = h.ids.free_total();
    let frames = h.w.mm.db.free_count();
    let res = h.resource();
    let d = h.domain(res, 26);
    let (x, _) = h.grant(d, 0);
    let v = vref(&h, x);
    let from = hart_of(&h, x);
    let to = (from + 1) % HARTS;
    // the move begins on hart `to`: its identity, the entry rewritten, the fence queued (not yet run)
    h.harts.here = to;
    assert_eq!(
        h.vecs.move_here(&mut h.ids, &mut h.harts, v),
        Ok(kcore::iovec::Moved::Retarget)
    );
    let t = h
        .vecs
        .retarget_begin(&h.ids, &h.harts, &mut h.store, h.io, &mut h.sim, v);
    let Ok(Some(ticket)) = t else {
        panic!("the move's fence queued: {t:?}")
    };
    // hart `to` goes offline before the fence
    h.harts.here = from;
    assert!(offline(&mut h, to));
    // the fence completes, naming the identity on `to` that the route has left
    h.sim.process_commands();
    let _ = h.store.poll(&mut h.sim, ticket);
    assert_eq!(
        h.vecs.fenced(&mut h.ids, &mut h.harts, &mut h.store, v),
        Ok(true)
    );
    let (now, _) = h.vecs.current(&h.ids, v).unwrap();
    assert_ne!(
        Some(now),
        h.vecs.named(v),
        "the entry names the identity left"
    );
    assert!(h.vecs.take_resettle(), "the fence asks for a settle");
    kio::settle_moves(&mut h);
    h.settle();
    let (now, ready) = h.vecs.current(&h.ids, v).unwrap();
    assert!(ready);
    assert_eq!(
        Some(now),
        h.vecs.named(v),
        "settled: the entry names the open identity"
    );
    assert_ne!(now.hart, to, "on an online hart");
    assert!(h.signal(26, 0).is_ok());
    assert!(h.interrupt_pending(x), "delivered");
    h.close(x);
    h.close(d);
    at_baseline(&h, free_ids, frames);
}

/// IO8 and 8.4 (review of IOMMU7): a move whose fence does not complete leaks the vector with its file disabled, so a device that
/// goes on sending to it sends no notice (no flood); `interrupt_deliver_here` says the move did not happen (BAD_STATE).
#[test]
fn a_vector_leaked_by_a_fence_timeout_is_disabled() {
    let mem = IoMem::default();
    let mut h = Host::new(&mem);
    let res = h.resource();
    let d = h.domain(res, 25);
    let (x, _) = h.grant(d, 0);
    let from = hart_of(&h, x);
    h.harts.here = (from + 1) % HARTS;
    h.stall = true;
    let tables = h.store.stats().tables_leaked;
    assert_eq!(
        h.err(nr::INTERRUPT_DELIVER_HERE, [x, 0, 0, 0, 0, 0]),
        Status::BadState,
        "the move's fence did not complete"
    );
    assert_eq!(h.vecs.counts().leaked, 1);
    assert_eq!(
        h.store.stats().tables_leaked,
        tables + 1,
        "the frame that holds its file is counted as leaked"
    );
    let notices = h.notices;
    for _ in 0..4 {
        let _ = h.signal(25, 0);
    }
    assert_eq!(h.notices, notices, "a leaked vector's file sends no notice");
    assert!(!h.interrupt_pending(x));
    assert_eq!(
        h.err(nr::INTERRUPT_ACK, [x, 0, 0, 0, 0, 0]),
        Status::BadState,
        "and cannot be armed again"
    );
}
