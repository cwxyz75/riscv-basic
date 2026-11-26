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
use iommucore::cmdq::CmdQueue;
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::faultq::FaultQueue;
use iommucore::iopt::{Iopt, PtMode};
use iommucore::regs::{self, Ddtp};
use iommucore::sim::{Sim, SimConfig};
use iommucore::{PhysMem64, Regs};
use kcore::frame::{FrameDb, Pfn};
use kcore::handle::HandleTable;
use kcore::iodomain::IoDomains;
use kcore::ipc::{self, KObj, Kobjs, Quota};
use kcore::job::{IoResource, JobId, Jobs, Resource, UNLIMITED};
use kcore::object::ObjId;
use kcore::pagetable::PhysMem;
use kcore::Mm;
use ksys::iodomain::{self as kio, IoEnv, IoObj};
use ksys::Reply;
use std::collections::HashMap;
use std::sync::Mutex;

mod common;
use common::*;

/// Physical memory as the IOMMU sees it (its directory, queues and tables; on the host apart from `FakeMem`, which holds the VMOs' data).
#[derive(Default)]
struct IoMem {
    frames: Mutex<HashMap<u64, Box<[u64; 512]>>>,
}

impl PhysMem64 for IoMem {
    fn read(&self, pa: u64) -> u64 {
        self.frames
            .lock()
            .unwrap()
            .get(&(pa >> 12))
            .map_or(0, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write(&self, pa: u64, value: u64) {
        self.frames
            .lock()
            .unwrap()
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
    }
    fn fetch_or(&self, pa: u64, bits: u64) -> u64 {
        let old = self.read(pa);
        self.write(pa, old | bits);
        old
    }
    fn fetch_and(&self, pa: u64, bits: u64) -> u64 {
        let old = self.read(pa);
        self.write(pa, old & bits);
        old
    }
}

const FENCE_BOUND: u64 = 1000;

/// The machine: the ksys world, the simulated IOMMU brought up as the kernel does (2.4 steps 3 to 5), the store, a clock that runs
/// only while a call waits, and the fence timeouts it reported.
struct Host<'m> {
    w: World,
    io: &'m IoMem,
    sim: Sim<'m, IoMem>,
    store: IoDomains,
    fq: FaultQueue,
    clock: u64,
    /// The device stalls: the commands are not processed while a call waits.
    stall: bool,
    timeouts: Vec<Option<u32>>,
}

impl<'m> IoEnv for Host<'m> {
    type M = FakeMem;
    type P = IoMem;
    type R = Sim<'m, IoMem>;
    fn obj<T>(&mut self, f: impl FnOnce(IoObj<'_, FakeMem>) -> T) -> T {
        let w = &mut self.w;
        f(IoObj {
            objs: &mut w.objs,
            handles: &mut w.handles,
            job: w.job,
            mm: &mut w.mm,
            vmos: &mut w.vmos,
            jobs: &mut w.jobs,
        })
    }
    fn obj_iommu<T>(
        &mut self,
        f: impl FnOnce(&mut Kobjs, &HandleTable, &mut IoDomains, &IoMem, &mut Self::R) -> T,
    ) -> T {
        f(
            &mut self.w.objs,
            &self.w.handles,
            &mut self.store,
            self.io,
            &mut self.sim,
        )
    }
    fn mem<T>(
        &mut self,
        f: impl FnOnce(&mut Mm<FakeMem>, &mut kcore::aspace::Vmos, &mut Jobs, &IoMem) -> T,
    ) -> T {
        f(&mut self.w.mm, &mut self.w.vmos, &mut self.w.jobs, self.io)
    }
    fn frame<T>(&mut self, f: impl FnOnce(&mut FrameDb, &mut FakeMem, &IoMem) -> T) -> T {
        f(&mut self.w.mm.db, &mut self.w.mm.mem, self.io)
    }
    fn iommu<T>(
        &mut self,
        f: impl FnOnce(&mut IoDomains, &IoMem, &mut Self::R, &mut FrameDb) -> T,
    ) -> T {
        f(&mut self.store, self.io, &mut self.sim, &mut self.w.mm.db)
    }
    fn device_ok(&self, device_id: u64) -> bool {
        device_id < 64
    }
    fn now(&self) -> u64 {
        self.clock
    }
    fn fence_bound(&self) -> u64 {
        FENCE_BOUND
    }
    fn relax(&mut self) {
        self.clock += 1;
        if !self.stall {
            self.sim.process_commands();
        }
    }
    fn fence_timeout(&mut self, device: Option<u32>) {
        self.timeouts.push(device);
    }
}

impl<'m> Host<'m> {
    fn new(io: &'m IoMem) -> Host<'m> {
        let mut w = world_with_frames(64, 2048);
        let mut sim = Sim::new(
            io,
            SimConfig {
                auto_process: false,
                ..SimConfig::qemu()
            },
        );
        let db = &mut w.mm.db;
        let dir = db.alloc().unwrap() << 12;
        let cq = db.alloc().unwrap() << 12;
        let fqp = db.alloc().unwrap() << 12;
        let q = db.alloc_contiguous(4).unwrap() << 12;
        let ddt = Ddt::new(io, dir, Levels::One, Format::Extended).unwrap();
        let cmdq = CmdQueue::new(cq, 256).unwrap();
        let fq = FaultQueue::new(fqp, 128).unwrap();
        let quarantine = Iopt::new(io, q, PtMode::Sv39x4).unwrap();
        sim.write64(regs::CQB, cmdq.cqb());
        sim.write32(regs::CQCSR, regs::QCSR_EN);
        sim.write64(regs::FQB, fq.fqb());
        sim.write32(regs::FQCSR, regs::QCSR_EN);
        sim.write64(regs::DDTP, Ddtp::directory(Levels::One, dir).bits());
        let store = IoDomains::new(ddt, cmdq, &quarantine, 0x5eed_6666).unwrap();
        Host {
            w,
            io,
            sim,
            store,
            fq,
            clock: 0,
            stall: false,
            timeouts: vec![],
        }
    }

    fn call(&mut self, n: usize, a: [u64; 6]) -> Reply {
        let r = match n {
            nr::IODOMAIN_CREATE => kio::iodomain_create(self, a),
            nr::IODOMAIN_MAP => kio::iodomain_map(self, a),
            nr::IODOMAIN_UNMAP => kio::iodomain_unmap(self, a),
            nr::IODOMAIN_DESTROY => kio::iodomain_destroy(self, a),
            nr::IODOMAIN_FAULT_READ => kio::iodomain_fault_read(self, a),
            nr::VMO_CREATE_DMA_IN => kio::vmo_create_dma_in(self, a),
            _ => self.w.call(n, a).expect("handled"),
        };
        // the tail of every call: what died is cleaned up and the destructions it began are completed
        self.settle();
        r
    }
    fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("succeeds")
    }
    fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect_err("fails").0
    }
    fn narrow(&mut self, h: u64, rights: Rights) -> u64 {
        self.ok(nr::HANDLE_DUPLICATE, [h, u64::from(rights.0), 0, 0, 0, 0])
            .0
            .unwrap()
    }
    fn close(&mut self, h: u64) {
        self.ok(nr::HANDLE_CLOSE, [h, 0, 0, 0, 0, 0]);
    }

    /// The kernel's `cleanup_dead` and its tail: a dead domain object begins its destruction (6.3 step 1) and drops its use of its job.
    fn settle(&mut self) {
        loop {
            let mut dead = self.w.objs.drain_dead();
            if dead.is_empty() {
                break;
            }
            for o in dead.drain(..) {
                match o {
                    KObj::Vmo(id) => {
                        let _ = self.w.vmos.release(&mut self.w.mm, &mut self.w.jobs, id);
                    }
                    KObj::Job(id) => self.w.jobs.release(id),
                    KObj::IoDomain(d) => {
                        kio::iodomain_closed(&mut self.store, self.io, &mut self.sim, d.domain);
                        self.w.jobs.release(d.job);
                    }
                    _ => {}
                }
            }
            self.w.objs.recycle(dead);
        }
        kio::drain_destructions(self);
        self.w.vmos.drain_all(&mut self.w.mm, &mut self.w.jobs);
    }

    fn resource(&mut self) -> u64 {
        let w = &mut self.w;
        ipc::resource_object_create_in(
            &mut w.objs,
            &mut w.handles,
            &mut Quota {
                jobs: &mut w.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap()
        .0
    }
    fn root_job(&mut self, rights: Rights) -> u64 {
        let w = &mut self.w;
        ipc::job_object_create(
            &mut w.objs,
            &mut w.handles,
            JobId::ROOT,
            rights,
            &mut Quota {
                jobs: &mut w.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap()
        .0
    }
    /// A child of the root job with the six limits (`UNLIMITED` for none).
    fn child_job(&mut self, l: [u64; 6]) -> u64 {
        let root = self.root_job(Rights::JOB);
        let bytes: Vec<u8> = l.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.w.poke(BASE, &bytes);
        self.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap()
    }
    fn job_of(&self, h: u64) -> JobId {
        let e = self.w.handles.get(Handle(h)).unwrap();
        match self.w.objs.get(e.obj) {
            Some(KObj::Job(j)) => *j,
            _ => panic!("not a job"),
        }
    }
    fn obj_of(&self, h: u64) -> ObjId {
        self.w.handles.get(Handle(h)).unwrap().obj
    }
    fn vmo_id(&self, h: u64) -> u64 {
        match self.w.objs.get(self.obj_of(h)) {
            Some(KObj::Vmo(id)) => *id,
            _ => panic!("not a VMO"),
        }
    }
    /// The frames of `pages` pages of a DMA VMO from page `first`.
    fn frames(&self, vmo: u64, first: u64, pages: u64) -> Vec<Pfn> {
        let mut out = vec![0; pages as usize];
        assert!(self.w.vmos.io_frames(self.vmo_id(vmo), first, &mut out));
        out
    }

    fn domain(&mut self, res: u64, device: u64) -> u64 {
        self.ok(nr::IODOMAIN_CREATE, [res, device, 0, 0, 0, 0])
            .0
            .unwrap()
    }
    fn dma(&mut self, res: u64, pages: u64) -> u64 {
        self.ok(nr::VMO_CREATE_DMA_IN, [res, pages, 0, 0, 0, 0])
            .0
            .unwrap()
    }
    fn map(&mut self, d: u64, vmo: u64, first: u64, pages: u64, access: u64) -> u64 {
        self.ok(nr::IODOMAIN_MAP, [d, vmo, first * 4096, pages, access, 0])
            .0
            .unwrap()
    }

    /// Does device `did` reach exactly `frames` from `iova` on (reads), and fault just past them?
    fn reaches(&mut self, did: u32, iova: u64, frames: &[Pfn]) -> bool {
        frames
            .iter()
            .enumerate()
            .all(|(i, &f)| self.sim.read(did, iova + i as u64 * 4096) == Ok(f << 12))
    }
    fn blocked(&mut self, did: u32, iova: u64, pages: u64) -> bool {
        (0..pages).all(|i| self.sim.read(did, iova + i * 4096).is_err())
    }

    /// The fault interrupt: the queue drained into the store, and DEVICE_FAULT raised on the domains that took a record (7.4).
    fn fault_interrupt(&mut self) {
        loop {
            let tail = self.sim.read32(regs::FQT);
            let Some(rec) = self.fq.pop(self.io, tail) else {
                break;
            };
            let out = self.store.fault(rec, self.clock, 1000);
            if let Some(obj) = out.tag.and_then(ObjId::from_words) {
                ipc::iodomain_set_fault(&mut self.w.objs, obj, true);
            }
        }
        let head = self.fq.head();
        self.sim.write32(regs::FQH, head);
    }

    fn io_used(&self, job: JobId) -> u64 {
        self.w.jobs.io_usage(job, IoResource::MappedPages).unwrap()
    }
    fn kernel_bytes(&self, job: JobId) -> u64 {
        self.w.jobs.usage(job).unwrap().get(Resource::KernelBytes)
    }
}

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
    assert_eq!(h.err(u, [d, again, 4, 0, 0, 0]), Status::BadState);
    assert_eq!(h.err(u, [d, again, 0, 0, 0, 0]), Status::InvalidArgs);
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
