//! The device-domain host (IOMMU6): the ksys world with the simulated IOMMU of `iommucore::sim` brought up as the kernel does, the
//! store, and an `IoEnv` over them. Shared by `iodomain_calls.rs` and the allocation-failure sweep.
use super::*;
use abi::Handle;
use iommucore::cmdq::CmdQueue;
use iommucore::ddt::{Ddt, Format, Levels};
use iommucore::faultq::FaultQueue;
use iommucore::iopt::{Iopt, PtMode};
use iommucore::regs::{self, Ddtp};
use iommucore::sim::{Sim, SimConfig};
use iommucore::{PhysMem64, Regs};
use kcore::frame::FrameDb;
use kcore::iodomain::IoDomains;
use kcore::ipc::Quota;
use kcore::job::{IoResource, Jobs};
use ksys::iodomain::{self as kio, IoEnv, IoObj};
use std::sync::Mutex;

/// Physical memory as the IOMMU sees it (its directory, queues and tables; on the host apart from `FakeMem`, which holds the VMOs' data).
#[derive(Default)]
pub struct IoMem {
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
        // (the fake machine's own allocation, not the code under test: the allocation-failure sweep must not fail it)
        EXEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.frames
            .lock()
            .unwrap()
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
        EXEMPT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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

pub const FENCE_BOUND: u64 = 1000;
/// The frames the bring-up takes: the directory, the command and fault queues, and the quarantine root.
pub const BOOT_FRAMES: usize = 7;

/// The machine: the ksys world, the simulated IOMMU brought up as the kernel does (2.4 steps 3 to 5), the store, a clock that runs
/// only while a call waits, and the fence timeouts it reported.
pub struct Host<'m> {
    pub w: World,
    pub io: &'m IoMem,
    pub sim: Sim<'m, IoMem>,
    pub store: IoDomains,
    pub fq: FaultQueue,
    pub clock: u64,
    /// The device stalls: the commands are not processed while a call waits.
    pub stall: bool,
    pub timeouts: Vec<Option<u32>>,
    /// A device access that faults, made at the next `relax` once the commands have run, with the fault interrupt taken there (a
    /// fault while a call waits: between a create's attach and its object, say).
    pub fault_in_relax: Option<(u32, u64)>,
}

impl<'m> IoEnv for Host<'m> {
    type M = FakeMem;
    type P = IoMem;
    type R = Sim<'m, IoMem>;
    fn obj<T>(&mut self, f: impl FnOnce(IoObj<'_, FakeMem>) -> T) -> Option<T> {
        let w = &mut self.w;
        Some(f(IoObj {
            objs: &mut w.objs,
            handles: &mut w.handles,
            job: w.job,
            mm: &mut w.mm,
            vmos: &mut w.vmos,
            jobs: &mut w.jobs,
        }))
    }
    fn obj_iommu<T>(
        &mut self,
        f: impl FnOnce(&mut Kobjs, &HandleTable, &mut IoDomains, &IoMem, &mut Self::R) -> T,
    ) -> Option<T> {
        Some(f(
            &mut self.w.objs,
            &self.w.handles,
            &mut self.store,
            self.io,
            &mut self.sim,
        ))
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
            // (the device's work, not the kernel's: exempt from the allocation-failure sweep)
            EXEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.sim.process_commands();
            EXEMPT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some((did, iova)) = self.fault_in_relax.take() {
            let _ = self.sim.write(did, iova, 1);
            self.fault_interrupt();
        }
    }
    fn fence_timeout(&mut self, device: Option<u32>) {
        self.timeouts.push(device);
    }
}

impl<'m> Host<'m> {
    pub fn new(io: &'m IoMem) -> Host<'m> {
        Host::with_world(io, world_with_frames(64, 2048))
    }

    /// The IOMMU brought up over `w`, from its frames (`BOOT_FRAMES` of them, counted in `w.extra_frames`).
    pub fn with_world(io: &'m IoMem, mut w: World) -> Host<'m> {
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
        w.extra_frames += BOOT_FRAMES;
        Host {
            w,
            io,
            sim,
            store,
            fq,
            clock: 0,
            stall: false,
            timeouts: vec![],
            fault_in_relax: None,
        }
    }

    /// The call alone, as the kernel's dispatch runs it (`None`: not a call the host layer handles).
    pub fn dispatch(&mut self, n: usize, a: [u64; 6]) -> Option<Reply> {
        Some(match n {
            nr::IODOMAIN_CREATE => kio::iodomain_create(self, a),
            nr::IODOMAIN_MAP => kio::iodomain_map(self, a),
            nr::IODOMAIN_UNMAP => kio::iodomain_unmap(self, a),
            nr::IODOMAIN_DESTROY => kio::iodomain_destroy(self, a),
            nr::IODOMAIN_FAULT_READ => kio::iodomain_fault_read(self, a),
            nr::VMO_CREATE_DMA_IN => kio::vmo_create_dma_in(self, a),
            _ => return self.w.call(n, a),
        })
    }

    pub fn call(&mut self, n: usize, a: [u64; 6]) -> Reply {
        let r = self.dispatch(n, a).expect("handled");
        // the tail of every call: what died is cleaned up and the destructions it began are completed
        self.settle();
        r
    }
    pub fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("succeeds")
    }
    pub fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect_err("fails").0
    }
    pub fn narrow(&mut self, h: u64, rights: Rights) -> u64 {
        self.ok(nr::HANDLE_DUPLICATE, [h, u64::from(rights.0), 0, 0, 0, 0])
            .0
            .unwrap()
    }
    pub fn close(&mut self, h: u64) {
        self.ok(nr::HANDLE_CLOSE, [h, 0, 0, 0, 0, 0]);
    }

    /// The kernel's `cleanup_dead` and its tail: a dead domain object begins its destruction (6.3 step 1) and drops its use of its job.
    pub fn settle(&mut self) {
        loop {
            let mut dead = self.w.objs.drain_dead();
            if dead.is_empty() {
                // (the empty vector goes back too, as the kernel's `cleanup_dead` does: otherwise the next burial would allocate, KT9)
                self.w.objs.recycle(dead);
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
        self.settle_domains();
    }

    /// The domains `cleanup_keep` found dead begin their destruction; then every destruction begun is completed and the VMOs
    /// whose last holder went are freed.
    pub fn settle_domains(&mut self) {
        for d in std::mem::take(&mut self.w.io_dead) {
            kio::iodomain_closed(&mut self.store, self.io, &mut self.sim, d.domain);
            self.w.jobs.release(d.job);
        }
        kio::settle_destructions(self);
        self.w.vmos.drain_all(&mut self.w.mm, &mut self.w.jobs);
    }

    pub fn resource(&mut self) -> u64 {
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
    pub fn root_job(&mut self, rights: Rights) -> u64 {
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
    pub fn child_job(&mut self, l: [u64; 6]) -> u64 {
        let root = self.root_job(Rights::JOB);
        let bytes: Vec<u8> = l.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.w.poke(BASE, &bytes);
        self.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap()
    }
    pub fn job_of(&self, h: u64) -> JobId {
        let e = self.w.handles.get(Handle(h)).unwrap();
        match self.w.objs.get(e.obj) {
            Some(KObj::Job(j)) => *j,
            _ => panic!("not a job"),
        }
    }
    pub fn obj_of(&self, h: u64) -> ObjId {
        self.w.handles.get(Handle(h)).unwrap().obj
    }
    pub fn vmo_id(&self, h: u64) -> u64 {
        match self.w.objs.get(self.obj_of(h)) {
            Some(KObj::Vmo(id)) => *id,
            _ => panic!("not a VMO"),
        }
    }
    /// The frames of `pages` pages of a DMA VMO from page `first`.
    pub fn frames(&self, vmo: u64, first: u64, pages: u64) -> Vec<Pfn> {
        let mut out = vec![0; pages as usize];
        assert!(self.w.vmos.io_frames(self.vmo_id(vmo), first, &mut out));
        out
    }

    pub fn domain(&mut self, res: u64, device: u64) -> u64 {
        self.ok(nr::IODOMAIN_CREATE, [res, device, 0, 0, 0, 0])
            .0
            .unwrap()
    }
    pub fn dma(&mut self, res: u64, pages: u64) -> u64 {
        self.ok(nr::VMO_CREATE_DMA_IN, [res, pages, 0, 0, 0, 0])
            .0
            .unwrap()
    }
    pub fn map(&mut self, d: u64, vmo: u64, first: u64, pages: u64, access: u64) -> u64 {
        self.ok(nr::IODOMAIN_MAP, [d, vmo, first * 4096, pages, access, 0])
            .0
            .unwrap()
    }

    /// Does device `did` reach exactly `frames` from `iova` on (reads), and fault just past them?
    pub fn reaches(&mut self, did: u32, iova: u64, frames: &[Pfn]) -> bool {
        frames
            .iter()
            .enumerate()
            .all(|(i, &f)| self.sim.read(did, iova + i as u64 * 4096) == Ok(f << 12))
    }
    pub fn blocked(&mut self, did: u32, iova: u64, pages: u64) -> bool {
        (0..pages).all(|i| self.sim.read(did, iova + i * 4096).is_err())
    }

    /// The fault interrupt: the queue drained into the store, and DEVICE_FAULT raised on the domains that took a record (7.4).
    pub fn fault_interrupt(&mut self) {
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

    pub fn io_used(&self, job: JobId) -> u64 {
        self.w.jobs.io_usage(job, IoResource::MappedPages).unwrap()
    }
    pub fn kernel_bytes(&self, job: JobId) -> u64 {
        self.w.jobs.usage(job).unwrap().get(Resource::KernelBytes)
    }
}
