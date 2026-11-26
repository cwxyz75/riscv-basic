//! A fake machine for the ksys tests: the kernel's real address space, usercopy and job ledger over a Vec-backed PhysMem.
#![allow(dead_code)]
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::{FrameDb, Pfn};
use kcore::handle::HandleTable;
use kcore::ipc::Kobjs;
use kcore::ipc::{self, KObj};
use kcore::job::JobId;
use kcore::job::Resource;
use kcore::object::{ObjId, Object};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;
use ksys::{ipc_call, job_call, vmo_call, Ctx, Faults, Reply};
use std::collections::HashMap;

pub mod iohost;

/// Physical memory as a map from frame number to 512 words, created on demand (the kernel's direct map, on the host).
#[derive(Default)]
pub struct FakeMem {
    frames: HashMap<Pfn, Box<[u64; 512]>>,
    /// Every frame read or written since the audit last looked (T176 4: each must be one the frame database manages, so a wild physical access cannot pass).
    touched: std::cell::RefCell<std::collections::HashSet<Pfn>>,
}

impl FakeMem {
    pub fn take_touched(&self) -> Vec<Pfn> {
        self.touched.borrow_mut().drain().collect()
    }
}

/// Non-zero while the fake machine allocates for itself.
pub static EXEMPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl PhysMem for FakeMem {
    fn read_u64(&self, pa: u64) -> u64 {
        self.touched.borrow_mut().insert(pa >> 12);
        self.frames
            .get(&(pa >> 12))
            .map_or(0xdead_beef_dead_beef, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write_u64(&mut self, pa: u64, value: u64) {
        // (the fake machine's own allocation of a frame's words is not the code under test: the allocation-failure sweep must not fail it)
        EXEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.touched.borrow_mut().insert(pa >> 12);
        self.frames
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
        EXEMPT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A user page range mapped read-write (16 pages, lazily backed) and one mapped read-only page.
pub const BASE: u64 = 0x1_0000;
pub const RO: u64 = 0x20_0000;

pub struct World {
    pub objs: Kobjs,
    pub handles: HandleTable,
    pub mm: Mm<FakeMem>,
    pub vmos: Vmos,
    pub jobs: kcore::job::Jobs,
    pub aspace: AddressSpace,
    pub job: JobId,
    pub faults: Faults,
    pub job_sched: kcore::jobsched::JobSchedTable,
}

pub fn world_with(handles: usize) -> World {
    world_with_frames(handles, 400)
}

/// `world_with` with `frames` managed frames (a small number makes the commits run out of memory: the rollback paths).
pub fn world_with_frames(handles: usize, frames: usize) -> World {
    let mut mm = Mm {
        db: FrameDb::new(3000, frames),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut aspace = AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap());
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let rw = vmos.create(&mut jobs, 16);
    aspace
        .map_region(&mut vmos, BASE, 16, rw, 0, Perms::RW.user())
        .unwrap();
    let ro = vmos.create(&mut jobs, 1);
    aspace
        .map_region(&mut vmos, RO, 1, ro, 0, Perms::R.user())
        .unwrap();
    World {
        objs: Kobjs::new(),
        handles: HandleTable::new(handles),
        mm,
        vmos,
        jobs,
        aspace,
        job: JobId::ROOT,
        faults: Faults::default(),
        job_sched: kcore::jobsched::JobSchedTable::new(),
    }
}

/// The device windows of QEMU `virt`'s tree minus what the kernel drives (the host tests' fixed table: the RTC page and the eight virtio slots).
pub const WINDOWS: [(u64, u64); 2] = [(0x10_1000, 0x10_2000), (0x1000_1000, 0x1000_9000)];

pub fn world() -> World {
    world_with(64)
}

impl World {
    pub fn call(&mut self, n: usize, a: [u64; 6]) -> Option<Reply> {
        self.call_split(n, a, None, false).0
    }
    /// The call as the kernel runs a big message call: its bytes copied in beforehand (`staged`), its copy-out left to the caller (`defer_out`).
    pub fn call_split(
        &mut self,
        n: usize,
        a: [u64; 6],
        staged: Option<ksys::Staged>,
        defer_out: bool,
    ) -> (Option<Reply>, Option<ksys::Deferred>) {
        let mut c = Ctx {
            objs: &mut self.objs,
            handles: &mut self.handles,
            job: self.job,
            aspace: &mut self.aspace,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            jobs: &mut self.jobs,
            faults: self.faults,
            harts_online: 4,
            job_sched: &self.job_sched,
            clock: || 0,
            windows: &WINDOWS,
            staged,
            defer_out,
            deferred: None,
        };
        let r = ipc_call(n, a, &mut c)
            .or_else(|| vmo_call(n, a, &mut c))
            .or_else(|| job_call(n, a, &mut c));
        (r, c.deferred.take())
    }
    pub fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("handled").expect("succeeds")
    }
    pub fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect("handled").expect_err("fails").0
    }
    pub fn pair(&mut self, n: usize) -> (u64, u64) {
        let (x, y) = self.ok(n, [0; 6]);
        (x.unwrap(), y.unwrap())
    }
    pub fn poke(&mut self, at: u64, d: &[u8]) {
        kcore::usercopy::copy_to_user(
            &mut self.aspace,
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            at,
            d,
        )
        .unwrap();
    }
    pub fn peek(&mut self, at: u64, n: usize) -> Vec<u8> {
        kcore::usercopy::copy_from_user(
            &mut self.aspace,
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            at,
            n,
        )
        .unwrap()
    }
    /// A duplicate of `h` with exactly `rights`.
    pub fn narrow(&mut self, h: u64, rights: Rights) -> u64 {
        self.ok(nr::HANDLE_DUPLICATE, [h, u64::from(rights.0), 0, 0, 0, 0])
            .0
            .unwrap()
    }
}

// ---- the audits the fuzzer and the allocation-failure sweep share ----

/// What the kernel does after a call: free what died (a VMO's pages, a job's ledger record) and sweep what nothing can reach.
pub fn cleanup(w: &mut World) {
    let d = cleanup_keep(w);
    w.objs.recycle_dirty(d);
}

/// `cleanup`, handing back the log of touched objects (the waits' commit reads it).
pub fn cleanup_keep(w: &mut World) -> kcore::object::Dirty {
    for round in 0..3 {
        let mut dead = w.objs.drain_dead();
        if dead.is_empty() && round > 0 {
            break;
        }
        for o in dead.drain(..) {
            match o {
                KObj::Vmo(id) => {
                    let _ = w.vmos.release(&mut w.mm, &mut w.jobs, id);
                }
                KObj::Job(id) => w.jobs.release(id),
                _ => {}
            }
        }
        w.objs.recycle(dead);
        if round == 0 && w.objs.needs_sweep() {
            let roots: Vec<ObjId> = w.handles.objects().collect();
            w.objs.sweep(&roots);
        }
    }
    w.objs.take_dirty()
}

/// Every audit; the first problem found.
pub fn audit(w: &mut World) -> Result<(), String> {
    w.mm.db.audit()?;
    for pfn in w.mm.mem.take_touched() {
        if !w.mm.db.is_managed(pfn) {
            return Err(format!(
                "frame {pfn:#x} was read or written but the frame database does not manage it"
            ));
        }
    }
    for o in w.handles.objects() {
        if !w.objs.is_alive(o) {
            return Err(format!("a handle names the dead object {o:?}"));
        }
    }
    // references: handles plus what live objects hold
    let ids = w.objs.live_ids();
    let mut counts: Vec<(ObjId, u32)> = ids.iter().map(|&i| (i, 0)).collect();
    let bump = |counts: &mut Vec<(ObjId, u32)>, o: ObjId| {
        if let Some(c) = counts.iter_mut().find(|(i, _)| *i == o) {
            c.1 += 1;
        }
    };
    for o in w.handles.objects() {
        bump(&mut counts, o);
    }
    for &id in &ids {
        let mut held = Vec::new();
        if let Some(obj) = w.objs.get(id) {
            obj.held(&mut held);
            if let Some(p) = obj.peer() {
                if !w.objs.is_alive(p) {
                    // (a peer end that has been destroyed is forgotten by the survivor: it must not still be named)
                    return Err(format!("{id:?} names a dead peer {p:?}"));
                }
                if w.objs.get(p).and_then(|x| x.peer()) != Some(id) {
                    return Err(format!(
                        "{id:?} names the peer {p:?}, which does not name it back"
                    ));
                }
            }
        }
        for h in held {
            bump(&mut counts, h);
        }
    }
    for (id, n) in &counts {
        let refs = w.objs.refs(*id).unwrap_or(0);
        if refs != *n {
            return Err(format!(
                "{id:?}: reference count {refs}, but {n} handles and live objects name it"
            ));
        }
    }
    // the ledger: a job's usage, less the credits of destroyed objects that are still owed (the audit does not apply them: a call that forgot to settle must not pass),
    // is exactly what live objects charged to it or below hold
    let owed: Vec<(JobId, usize)> = w.objs.pending_credits().to_vec();
    let mut charged: Vec<(JobId, usize)> = Vec::new();
    let mut jobs = vec![JobId::ROOT, w.job];
    for &id in &ids {
        if let Some(KObj::Job(j)) = w.objs.get(id) {
            jobs.push(*j);
        }
        if let Some(j) = w.objs.job_of(id) {
            let bytes = w.objs.get(id).map_or(0, |o| o.charged_bytes());
            charged.push((j, bytes));
            jobs.push(j);
        }
    }
    jobs.sort_by_key(|j| format!("{j:?}"));
    jobs.dedup();
    for j in jobs {
        let below = |c: &&(JobId, usize)| w.jobs.is_within(c.0, j);
        let held = charged.iter().filter(below).count() as u64;
        let bytes = charged
            .iter()
            .filter(below)
            .map(|c| c.1 as u64)
            .sum::<u64>();
        let u = w.jobs.usage(j);
        let (used, used_bytes) = u.map_or((0, 0), |u| {
            (u.get(Resource::Objects), u.get(Resource::IpcBytes))
        });
        let owing = owed.iter().filter(|c| w.jobs.is_within(c.0, j));
        let (owed_objects, owed_bytes) =
            owing.fold((0u64, 0u64), |a, c| (a.0 + 1, a.1 + c.1 as u64));
        let (used, used_bytes) = (
            used.saturating_sub(owed_objects),
            used_bytes.saturating_sub(owed_bytes),
        );
        if used != held {
            return Err(format!("job {j:?}: the ledger says {used} objects, {held} are alive and charged to it or below"));
        }
        if used_bytes != bytes {
            return Err(format!(
                "job {j:?}: the ledger says {used_bytes} queued bytes, the objects hold {bytes}"
            ));
        }
    }
    Ok(())
}

/// The page-table frames of the user half of the world's address space (Sv39: the root, and every table below an entry that is valid and has no
/// read, write or execute bit), found by reading the tables as the hardware would.
pub fn table_frames(w: &World) -> usize {
    fn below(w: &World, table: Pfn, level: u32, user_half_only: bool) -> usize {
        let mut n = 1;
        let entries = if user_half_only { 256 } else { 512 };
        for i in 0..entries {
            let pte = w.mm.mem.read_u64((table << 12) + i * 8);
            let valid = pte & 1 != 0;
            let leaf = pte & 0b1110 != 0;
            if valid && !leaf && level > 0 {
                n += below(w, pte >> 10, level - 1, false);
            }
        }
        n
    }
    below(w, w.aspace.table().root(), 2, true)
}

/// T176 (4): at the end of a sequence every allocated frame is accounted for: the two page-table roots (the kernel's, which has nothing below it here,
/// and the space's, with the tables under its user half) and the pages the VMOs still hold.
pub fn frame_accounting(w: &World) -> Result<(), String> {
    let allocated = w.mm.db.total() - w.mm.db.free_count();
    let tables = 1 + table_frames(w);
    let pages = w.vmos.frames_held() as usize;
    if allocated != tables + pages {
        return Err(format!(
            "{allocated} frames are allocated, but the page tables hold {tables} and the VMOs {pages}"
        ));
    }
    Ok(())
}

/// After every handle is closed and the sweep has run, nothing may be left.
pub fn baseline(w: &mut World) -> Result<(), String> {
    w.handles.drain(&mut w.objs);
    cleanup(w);
    w.objs.sweep(&[]);
    cleanup(w);
    ipc::settle(&mut w.objs, &mut w.jobs);
    if w.objs.live() != 0 {
        return Err(format!(
            "{} objects alive after every handle was closed and the sweep ran",
            w.objs.live()
        ));
    }
    for r in [Resource::Objects, Resource::IpcBytes] {
        let used = w.jobs.usage(JobId::ROOT).map_or(0, |u| u.get(r));
        if used != 0 {
            return Err(format!(
                "the root job still holds {used} of {r:?} after everything was closed"
            ));
        }
    }
    if w.objs.meter() != 0 {
        return Err(format!(
            "the object meter reads {} after everything was closed",
            w.objs.meter()
        ));
    }
    frame_accounting(w)
}
