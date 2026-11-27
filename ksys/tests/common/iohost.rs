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
use kcore::iovec::{Harts, Vectors};
use kcore::ipc::Quota;
use kcore::job::{IoResource, Jobs};
use kcore::msiid::{MsiIds, Place, Route, Slot};
use ksys::iodomain::{self as kio, IoEnv, IoObj, IrqCtx};
use std::collections::BTreeSet;
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
/// The interrupt files of the host's harts (one page each), and how many identities each has (QEMU's `riscv,num-ids`).
pub const IMSIC_BASE: u64 = 0x2800_0000;
pub const HARTS: u16 = 4;
pub const NUM_IDS: u32 = 255;

/// The harts as the vector layer sees them (IOMMU7): which one is calling, which are online, and which were asked to do their work in
/// their files (done at the next `relax` or call tail, as an inter-hart interrupt would be taken).
pub struct HostHarts {
    pub here: u16,
    pub offline: [bool; HARTS as usize],
    pub asked: BTreeSet<u16>,
    /// A hart that does not answer: its work is never done (the bounded waits give up).
    pub deaf: Option<u16>,
}

impl Harts for HostHarts {
    fn here(&self) -> Option<u16> {
        Some(self.here)
    }
    fn usable(&self, f: u16) -> bool {
        f < HARTS && !self.offline[usize::from(f)]
    }
    fn leaving(&self, f: u16) -> bool {
        !self.usable(f)
    }
    fn open_here(&mut self, _id: u16) {}
    fn ask(&mut self, f: u16) {
        // (an inter-hart interrupt in the kernel: the host's set is the fake machine's, exempt from the allocation-failure sweep)
        EXEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.asked.insert(f);
        EXEMPT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn file_page(&self, f: u16) -> u64 {
        IMSIC_BASE + u64::from(f) * 4096
    }
}
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
    /// The interrupt identities of every hart's file, the vectors' routes and the harts (IOMMU7).
    pub ids: MsiIds,
    pub vecs: Vectors,
    pub harts: HostHarts,
    /// How many of the simulated IOMMU's deliveries the harts have taken.
    pub seen: usize,
    /// Messages taken on a vector's identity, those that signalled its object, and those on an identity with no route (8.1).
    pub notices: u64,
    pub fired: u64,
    pub no_route: u64,
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
        // (the harts take their messages and do the work they were asked to: an interrupt window)
        self.relax_harts();
    }
    fn fence_timeout(&mut self, device: Option<u32>) {
        self.timeouts.push(device);
    }
    fn irq<T>(&mut self, f: impl FnOnce(IrqCtx<'_>, &IoMem) -> T) -> Option<T> {
        let w = &mut self.w;
        Some(f(
            IrqCtx {
                objs: &mut w.objs,
                caller: Some((&mut w.handles, w.job)),
                jobs: &mut w.jobs,
                ids: &mut self.ids,
                vecs: &mut self.vecs,
                harts: &mut self.harts,
            },
            self.io,
        ))
    }
    fn irq_iommu<T>(
        &mut self,
        f: impl FnOnce(IrqCtx<'_>, &mut IoDomains, &IoMem, &mut Self::R, &mut FrameDb) -> T,
    ) -> Option<T> {
        let w = &mut self.w;
        Some(f(
            IrqCtx {
                objs: &mut w.objs,
                caller: Some((&mut w.handles, w.job)),
                jobs: &mut w.jobs,
                ids: &mut self.ids,
                vecs: &mut self.vecs,
                harts: &mut self.harts,
            },
            &mut self.store,
            self.io,
            &mut self.sim,
            &mut w.mm.db,
        ))
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
            ids: MsiIds::new(usize::from(HARTS), NUM_IDS).unwrap(),
            vecs: Vectors::new(usize::from(HARTS) * NUM_IDS as usize * 2).unwrap(),
            harts: HostHarts {
                here: 0,
                offline: [false; HARTS as usize],
                asked: BTreeSet::new(),
                deaf: None,
            },
            seen: 0,
            notices: 0,
            fired: 0,
            no_route: 0,
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
            nr::IODOMAIN_GRANT_VECTOR => kio::iodomain_grant_vector(self, a),
            nr::INTERRUPT_ACK => match kio::interrupt_ack_vector(self, a) {
                Some(r) => r,
                None => return self.w.call(n, a),
            },
            // (the host has no wired sources: a handle that is no vector's is the wired path's, which the host does not run)
            nr::INTERRUPT_DELIVER_HERE => kio::interrupt_deliver_here_vector(self, a)
                .unwrap_or(Err((Status::NotSupported, None, None))),
            nr::OBJECT_WAIT => {
                kio::first_wait_vector(self, Handle(a[0]));
                return self.w.call(n, a);
            }
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
                    KObj::Interrupt(i) => {
                        if let Some(v) = i.vector {
                            kio::vector_closed(
                                &mut self.vecs,
                                &mut self.store,
                                self.io,
                                &mut self.sim,
                                v,
                            );
                        }
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
        for v in std::mem::take(&mut self.w.vec_dead) {
            kio::vector_closed(&mut self.vecs, &mut self.store, self.io, &mut self.sim, v);
        }
        kio::settle_destructions(self);
        // (IOMMU7, the kernel's `drain`: the harts' work, the moves an evacuation began, the revokes a close began, the charges given back)
        self.relax_harts();
        kio::settle_moves(self);
        kio::settle_vectors(self);
        self.w.vmos.drain_all(&mut self.w.mm, &mut self.w.jobs);
    }

    /// The harts take their messages and do their work, with no clock tick.
    pub fn relax_harts(&mut self) {
        self.take_messages();
        self.hart_work();
    }

    /// Every message the simulated IOMMU delivered since the last look, taken by the hart whose file it names (an offline hart still
    /// takes its file's).
    pub fn take_messages(&mut self) {
        while let Some(d) = self.sim.deliveries().get(self.seen).copied() {
            self.seen += 1;
            let f = d.page.wrapping_sub(IMSIC_BASE) / 4096;
            let (Ok(hart), Ok(id)) = (u16::try_from(f), u16::try_from(d.identity)) else {
                self.no_route += 1;
                continue;
            };
            if hart >= HARTS || d.page & 0xfff != 0 {
                self.no_route += 1;
                continue;
            }
            self.deliver_one(Place { hart, id });
        }
    }

    /// One message on identity `p`, as the kernel's `deliver_one`.
    pub fn deliver_one(&mut self, p: Place) {
        match self.ids.slot(p) {
            Some(
                Slot::Live {
                    route: Route::Vector(_),
                    ..
                }
                | Slot::Moved {
                    route: Route::Vector(_),
                    ..
                }
                | Slot::Retiring {
                    route: Route::Vector(_),
                    moved: true,
                },
            ) => {
                self.notices += 1;
                if self.vecs.notice(&mut self.w.objs, &self.ids, self.io, p) == Some(true) {
                    self.fired += 1;
                }
            }
            Some(Slot::Retiring { moved: false, .. }) => {}
            _ => self.no_route += 1,
        }
    }

    /// The harts asked to work in their files open their Opening identities and retire their Retiring ones (the kernel's `deliver`).
    pub fn hart_work(&mut self) {
        let mut skipped = BTreeSet::new();
        while let Some(f) = self.harts.asked.pop_first() {
            if self.harts.deaf == Some(f) {
                skipped.insert(f);
                continue;
            }
            for id in 2..=self.ids.num_ids() {
                let p = Place { hart: f, id };
                match self.ids.slot(p) {
                    Some(Slot::Opening { .. }) => {
                        if let Some(o) = self.ids.opened(p) {
                            if let Some(r) = o.retiring {
                                self.harts.asked.insert(r.hart);
                            }
                        }
                    }
                    Some(Slot::Retiring { .. }) => {
                        if let Some((Route::Vector(n), _)) = self.ids.retired(p) {
                            self.vecs.retired(&self.ids, n);
                        }
                    }
                    _ => {}
                }
            }
        }
        // (a deaf hart keeps its request, as an unanswered inter-hart interrupt stays pending)
        self.harts.asked.extend(skipped);
    }

    /// Device `did` writes the MSI-X message of vector `k` (its address and the data word), then the harts take what it sent.
    pub fn signal(&mut self, did: u32, k: u64) -> Result<iommucore::sim::WriteOutcome, u16> {
        self.signal_raw(
            did,
            IoDomains::vector_address(k as u16),
            abi::iodomain::MSI_DATA as u32,
        )
    }
    /// Device `did` writes `data` at `addr` (a message, forged or not), then the fault interrupt and the harts take what it did.
    pub fn signal_raw(
        &mut self,
        did: u32,
        addr: u64,
        data: u32,
    ) -> Result<iommucore::sim::WriteOutcome, u16> {
        let r = self.sim.write(did, addr, data);
        self.fault_interrupt();
        self.relax_harts();
        r
    }

    /// `iodomain_grant_vector`: the interrupt handle and the MSI-X address.
    pub fn grant(&mut self, d: u64, k: u64) -> (u64, u64) {
        let (h, addr) = self.ok(nr::IODOMAIN_GRANT_VECTOR, [d, k, 0, 0, 0, 0]);
        (h.unwrap(), addr.unwrap())
    }
    /// Is interrupt object `h` signalled (INTERRUPT)?
    pub fn interrupt_pending(&self, h: u64) -> bool {
        matches!(self.w.objs.get(self.obj_of(h)), Some(KObj::Interrupt(i)) if i.fired)
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
