//! The device-domain calls (IOMMU6; IOMMU-SPEC 4, 5.3, 6, 7.4, 11.2 and the amendment of 2026-11-26): `iodomain_create`, `iodomain_map`,
//! `iodomain_unmap`, `iodomain_destroy`, `iodomain_fault_read` and `vmo_create_dma_in`, over the machine's locks (`IoEnv`).
//!
//! Each call is a sequence of critical sections in the order 11.2 fixes, never one held across a wait: `OBJ` (the handles, with the
//! memory locks `VMO`, `JOBS` and `FRAME` inside it for the hold and the charge), then `IOMMU` (with `FRAME` inside it for table frames),
//! once per `RANGE_STEP` pages of work (OD168), then the fence waited for with **no lock held**, then `IOMMU` again (complete), then the
//! memory locks for the release (the hold dropped, the record's frames and charge settled). `IOMMU` is never held when `VMO`, `JOBS` or
//! `OBJ` is taken. The kernel and the host tests (`ksys/tests/iodomain_calls.rs`, with the simulated IOMMU) run the same code.

use crate::Reply;
use abi::iodomain as io;
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use iommucore::iopt::Access;
use iommucore::{PhysMem64, Regs, TableFrames};
use kcore::aspace::{IoHoldError, VmoError, VmoKind, Vmos};
use kcore::frame::{FrameDb, Pfn};
use kcore::handle::HandleTable;
use kcore::iodomain::{
    self as iod, Destroying, DomainId, Fence, IoDomains, IoError, IoFrames, MapFailed, MapStep,
    Mapped, Pending, Record, Released, Ticket, UnmapStep, RANGE_STEP,
};
use kcore::ipc::{self, KObj, Kobjs, Quota};
use kcore::job::{JobId, Jobs};
use kcore::pagetable::PhysMem;
use kcore::Mm;

/// What the `OBJ` phase of a call sees: the caller's handles and the memory locks inside the object lock.
pub struct IoObj<'a, M: PhysMem> {
    pub objs: &'a mut Kobjs,
    pub handles: &'a mut HandleTable,
    /// The caller's job (pays for the caller's handles and objects).
    pub job: JobId,
    pub mm: &'a mut Mm<M>,
    pub vmos: &'a mut Vmos,
    pub jobs: &'a mut Jobs,
}

/// The machine under the device-domain calls: its locks, each a closure run with exactly those held, its clock and its console.
pub trait IoEnv {
    /// The memory of the frame database (`Mm`).
    type M: PhysMem;
    /// The memory the IOMMU reads (its structures, through the direct map).
    type P: PhysMem64;
    /// The IOMMU's registers.
    type R: Regs;
    /// `OBJ`, then `VMO`, `JOBS` and `FRAME` inside it. `None` (and `f` not run) if the caller's process is gone (BAD_STATE).
    fn obj<T>(&mut self, f: impl FnOnce(IoObj<'_, Self::M>) -> T) -> Option<T>;
    /// `OBJ`, then `IOMMU` inside it (no memory lock: the fault signal is changed in the same section as the ring, 7.4). `None` as `obj`.
    fn obj_iommu<T>(
        &mut self,
        f: impl FnOnce(&mut Kobjs, &HandleTable, &mut IoDomains, &Self::P, &mut Self::R) -> T,
    ) -> Option<T>;
    /// `VMO`, `JOBS` and `FRAME`.
    fn mem<T>(
        &mut self,
        f: impl FnOnce(&mut Mm<Self::M>, &mut Vmos, &mut Jobs, &Self::P) -> T,
    ) -> T;
    /// `FRAME` only.
    fn frame<T>(&mut self, f: impl FnOnce(&mut FrameDb, &mut Self::M, &Self::P) -> T) -> T;
    /// `IOMMU`, then `FRAME` inside it (table frames).
    fn iommu<T>(
        &mut self,
        f: impl FnOnce(&mut IoDomains, &Self::P, &mut Self::R, &mut FrameDb) -> T,
    ) -> T;
    /// Is `device_id` one the tree's `iommu-map` covers (4.2)?
    fn device_ok(&self, device_id: u64) -> bool;
    /// The `time` counter, and `IOMMU_FENCE_BOUND` in its ticks (6.4).
    fn now(&self) -> u64;
    fn fence_bound(&self) -> u64;
    /// Between two looks at a fence, and between the steps of a long call (no lock is held: an interrupt window, OD168).
    fn relax(&mut self);
    /// A fence did not complete within the bound or the queue failed: count it and say so (6.4), naming the device.
    fn fence_timeout(&mut self, device: Option<u32>);
}

/// Table frames from the frame database, zeroed through the IOMMU's view of memory (inside `FRAME`).
pub struct DbFrames<'a, P: PhysMem64> {
    pub db: &'a mut FrameDb,
    pub mem: &'a P,
}

impl<P: PhysMem64> DbFrames<'_, P> {
    fn zero(&self, pa: u64, frames: u64) {
        let mut at = pa;
        let end = pa.saturating_add(frames.saturating_mul(4096));
        while at < end {
            self.mem.write(at, 0);
            at = at.saturating_add(8);
        }
    }
}

impl<P: PhysMem64> TableFrames for DbFrames<'_, P> {
    fn alloc(&mut self) -> Option<u64> {
        let pa = self.db.alloc()?.checked_mul(4096)?;
        self.zero(pa, 1);
        Some(pa)
    }
    fn free(&mut self, pa: u64) {
        let freed = self.db.dec(pa / 4096);
        debug_assert!(freed.is_ok(), "a table frame the store allocated");
    }
}

impl<P: PhysMem64> IoFrames for DbFrames<'_, P> {
    fn alloc_root(&mut self) -> Option<u64> {
        let pa = self
            .db
            .alloc_contiguous(iod::ROOT_FRAMES as usize)?
            .checked_mul(4096)?;
        self.zero(pa, iod::ROOT_FRAMES);
        Some(pa)
    }
    fn free_root(&mut self, pa: u64) {
        for i in 0..iod::ROOT_FRAMES {
            let freed = self.db.dec((pa / 4096).saturating_add(i));
            debug_assert!(freed.is_ok(), "a root the store allocated");
        }
    }
}

fn err(s: Status) -> Reply {
    Err((s, None, None))
}

fn io_status(e: IoError) -> Status {
    match e {
        IoError::InvalidArgs => Status::InvalidArgs,
        IoError::BadState => Status::BadState,
        IoError::NoMemory | IoError::NotDone => Status::NoMemory,
    }
}

/// The handle errors come in their kinds' order whichever handle has them (4.2, 4.3): every BAD_HANDLE, then WRONG_TYPE, then
/// ACCESS_DENIED.
fn first_error(errors: &[Option<Status>]) -> Option<Status> {
    let rank = |s: Status| match s {
        Status::BadHandle => 0,
        Status::WrongType => 1,
        Status::AccessDenied => 2,
        _ => 3,
    };
    errors.iter().flatten().copied().min_by_key(|&s| rank(s))
}

/// A wait for one fence, and how to queue its commands again (`Fence::Unqueued`).
enum Waiter {
    Create(DomainId, Ticket),
    Map(DomainId, Mapped),
    Unmap(Pending),
    Destroy(Destroying),
}

impl Waiter {
    fn look<P: PhysMem64, R: Regs>(&mut self, s: &mut IoDomains, mem: &P, regs: &mut R) -> Fence {
        let t = match self {
            Waiter::Create(_, t) => *t,
            Waiter::Map(_, m) => m.ticket,
            Waiter::Unmap(p) => p.ticket(),
            Waiter::Destroy(x) => x.ticket(),
        };
        match s.poll(regs, t) {
            Fence::Unqueued => match self {
                Waiter::Create(d, t) => s.requeue_create(mem, regs, *d, t),
                Waiter::Map(d, m) => s.requeue_map(mem, regs, *d, m),
                Waiter::Unmap(p) => s.requeue_unmap(mem, regs, p),
                Waiter::Destroy(x) => s.requeue_destroy(mem, regs, x),
            },
            f => f,
        }
    }

    fn domain(&self) -> DomainId {
        match self {
            Waiter::Create(d, _) | Waiter::Map(d, _) => *d,
            Waiter::Unmap(p) => p.range().0,
            Waiter::Destroy(x) => x.domain(),
        }
    }
}

/// Wait for a fence with no lock held, bounded by `IOMMU_FENCE_BOUND` (6.4); false if it did not complete (counted and reported: the
/// caller leaks what it guarded). A full queue is not a failure: the commands are queued again within the same bound.
fn wait<E: IoEnv>(e: &mut E, w: &mut Waiter) -> bool {
    let t0 = e.now();
    let bound = e.fence_bound();
    loop {
        // (the clock is read before the look: a thread held off between the look and the clock does not time out a fence that completed)
        let expired = e.now().wrapping_sub(t0) > bound;
        let f = e.iommu(|s, mem, regs, _| w.look(s, mem, regs));
        match f {
            Fence::Done => return true,
            Fence::Waiting | Fence::Unqueued if !expired => e.relax(),
            _ => {
                let d = w.domain();
                let device = e.iommu(|s, _, _, _| s.device_of(d));
                e.fence_timeout(device);
                return false;
            }
        }
    }
}

/// Mark (or unmark) the frames of `pages` pages of `vmo` from `first` as named by a device mapping (debug builds: the frame database's
/// `dec` asserts that a marked frame is never freed, IO3), `RANGE_STEP` frames per hold of the memory locks.
#[allow(unused_variables)]
fn mark<E: IoEnv>(e: &mut E, vmo: u64, first: u64, pages: u64, on: bool) {
    #[cfg(debug_assertions)]
    {
        let mut done = 0u64;
        while done < pages {
            let n = (pages.saturating_sub(done)).min(RANGE_STEP);
            e.mem(|mm, vmos, _, _| {
                let mut buf = [0 as Pfn; RANGE_STEP as usize];
                let Some(out) = buf.get_mut(..usize::try_from(n).unwrap_or(0)) else {
                    return;
                };
                if vmos.io_frames(vmo, first.saturating_add(done), out) {
                    for &pfn in out.iter() {
                        if on {
                            mm.db.io_mark(pfn);
                        } else {
                            mm.db.io_unmark(pfn);
                        }
                    }
                }
            });
            done = done.saturating_add(n);
            if done < pages {
                e.relax(); // (between steps, with no lock held: OD168)
            }
        }
    }
}

/// 6.1 step 4 for a record whose fence has completed: its frames unmarked, its hold dropped (which may free the VMO's frames), its table
/// frames freed and its charges credited. Allocates nothing.
fn release<E: IoEnv>(e: &mut E, rel: Released) {
    mark(e, rel.vmo(), rel.vmo_page(), rel.pages(), false);
    e.mem(|mm, vmos, jobs, p| {
        let released = vmos.release(mm, jobs, rel.vmo());
        debug_assert!(released.is_ok(), "a mapping holds its VMO");
        rel.settle(
            p,
            &mut DbFrames {
                db: &mut mm.db,
                mem: p,
            },
            jobs,
        );
    });
}

/// Drop the call's own use of a job (taken so that the job outlives the call's charges).
fn release_job<E: IoEnv>(e: &mut E, job: JobId) {
    e.mem(|_, _, jobs, _| jobs.release(job));
}

/// 6.3 steps 2 to 4 for a destruction whose step 1 is done: the fence waited for with no lock held, then the domain out of the store
/// (constant work under `IOMMU`), then its records released and its tables freed a step at a time with no `IOMMU` (nothing can walk them
/// after the fence), then its charge. A fence that does not complete leaks the domain (6.4).
fn finish_destroy<E: IoEnv>(e: &mut E, x: Destroying) {
    let mut w = Waiter::Destroy(x);
    if !wait(e, &mut w) {
        let Waiter::Destroy(x) = w else { return };
        e.iommu(|s, _, _, _| s.destroy_abandon(&x));
        return;
    }
    let Waiter::Destroy(x) = w else { return };
    let det = e.iommu(|s, mem, _, _| s.destroy_complete(mem, &x));
    let Ok(Some(mut det)) = det else {
        return; // (another thread completed or abandoned it)
    };
    // (the release in steps, with an interrupt window between them: OD168)
    while let Some(rel) = det.next_record() {
        release(e, rel);
        e.relax();
    }
    while !e.frame(|db, _, p| det.step(p, &mut DbFrames { db, mem: p })) {
        e.relax();
    }
    e.mem(|mm, _, jobs, p| {
        det.finish(
            p,
            &mut DbFrames {
                db: &mut mm.db,
                mem: p,
            },
            jobs,
        )
    });
    e.iommu(|s, _, _, _| s.destroy_released());
}

/// Destroy a domain now and wait for it (a create that cannot finish).
fn destroy_now<E: IoEnv>(e: &mut E, d: DomainId) {
    if let Some(x) = e.iommu(|s, mem, regs, _| s.destroy(mem, regs, d)) {
        finish_destroy(e, x);
    }
}

/// Complete every destruction whose step 1 was done under another lock (a handle close, a process's teardown: 6.3), with no lock held.
/// The kernel runs it at the tail of every call, at the end of a teardown and before a create.
pub fn drain_destructions<E: IoEnv>(e: &mut E) {
    while let Some(x) = e.iommu(|s, _, _, _| s.next_detaching()) {
        finish_destroy(e, x);
    }
}

/// `drain_destructions`, then wait (with `relax`, no lock held) until no destruction is left unfinished, also one another thread
/// took: the tail of a call that began one, so that it returns only when the domain is gone and its charges are back (6.3, 4.5). Each
/// destruction ends within its fence bound (completed, or abandoned and leaked), so the wait is bounded; it gives up after a few
/// bounds all the same, rather than spin behind destructions that others keep beginning.
pub fn settle_destructions<E: IoEnv>(e: &mut E) {
    drain_destructions(e);
    let t0 = e.now();
    let bound = e.fence_bound().saturating_mul(4);
    while e.iommu(|s, _, _, _| s.unfinished()) && e.now().saturating_sub(t0) < bound {
        e.relax();
        drain_destructions(e);
    }
}

/// The last handle of a device domain went (`needs_cleanup`, under `OBJ`; may run inside another call's critical section): 6.3 step 1,
/// which allocates nothing and does not wait. The caller drops the object's use of its job and makes sure `drain_destructions` runs
/// before it returns to user mode.
pub fn iodomain_closed<P: PhysMem64, R: Regs>(
    s: &mut IoDomains,
    mem: &P,
    regs: &mut R,
    domain: u64,
) {
    let _ = s.destroy(mem, regs, DomainId::from_raw(domain));
}

// ---- iodomain_create -------------------------------------------------------------------------------------------------------------

/// `iodomain_create(resource, device_id, job) -> domain` (4.2).
pub fn iodomain_create<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    // (an old domain of the device whose destruction is still pending is finished first)
    drain_destructions(e);
    let device_ok = e.device_ok(a[1]);
    let job = e.obj(|o| -> Result<JobId, Status> {
        let r = ipc::resource_of(o.objs, o.handles, Handle(a[0])).err();
        let j = if a[2] == 0 {
            Ok(o.job)
        } else {
            ipc::job_of(o.objs, o.handles, Handle(a[2]), Rights::MANAGE)
        };
        if let Some(s) = first_error(&[r, j.err()]) {
            return Err(s);
        }
        let job = j.map_err(|_| Status::BadHandle)?;
        if !device_ok {
            return Err(Status::InvalidArgs);
        }
        o.jobs.retain(job);
        Ok(job)
    });
    let job = match job.unwrap_or(Err(Status::BadState)) {
        Ok(j) => j,
        Err(s) => return err(s),
    };
    let r = create_in(e, a, job);
    release_job(e, job);
    r
}

fn create_in<E: IoEnv>(e: &mut E, a: [u64; 6], job: JobId) -> Reply {
    let Ok(device) = u32::try_from(a[1]) else {
        return err(Status::InvalidArgs);
    };
    if e.iommu(|s, _, _, _| s.domain_of(device).is_some()) {
        return err(Status::BadState);
    }
    if e.mem(|_, _, jobs, _| iod::charge_create(jobs, job))
        .is_err()
    {
        return err(Status::NoMemory);
    }
    let made =
        e.iommu(|s, mem, regs, db| s.create(mem, regs, &mut DbFrames { db, mem }, device, job));
    let (d, t) = match made {
        Ok(x) => x,
        Err(x) => {
            e.mem(|_, _, jobs, _| iod::uncharge_create(jobs, job));
            return err(io_status(x));
        }
    };
    // (from here the domain's record holds the charge and its use of the job: its destruction gives them back)
    if !wait(e, &mut Waiter::Create(d, t)) {
        destroy_now(e, d);
        return err(Status::NoMemory);
    }
    let made = e.obj(|o| {
        ipc::iodomain_object_create_in(
            o.objs,
            o.handles,
            d.raw(),
            job,
            &mut Quota {
                jobs: o.jobs,
                job: Some(o.job),
            },
        )
    });
    match made.unwrap_or(Err(Status::BadState)) {
        Ok((h, obj)) => {
            // (the tag and the signal in one section, OBJ then IOMMU: a fault the device took between the attach and here is in the
            // ring with no object named, and raises DEVICE_FAULT now, 7.4)
            let _ = e.obj_iommu(|objs, _, s, _, _| {
                s.set_tag(d, obj.to_words());
                if s.has_faults(d) {
                    ipc::iodomain_set_fault(objs, obj, true);
                }
            });
            Ok((Some(h.0), None))
        }
        Err(s) => {
            destroy_now(e, d);
            err(s)
        }
    }
}

// ---- iodomain_map ----------------------------------------------------------------------------------------------------------------

/// `iodomain_map(domain, vmo, vmo_offset, pages, access) -> io_address` (4.3, amendment part 1).
pub fn iodomain_map<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    let (offset, pages, access) = (a[2], a[3], a[4]);
    let writes = access & io::WRITE != 0;
    let resolved = e.obj(
        |o| -> Result<(DomainId, JobId, u64, iod::MapCharge), Status> {
            let d = ipc::iodomain_of(o.objs, o.handles, Handle(a[0]), Rights::IO_MAP);
            // the VMO: a DMA VMO (WRONG_TYPE otherwise) whose handle carries DMA, READ, and WRITE for a writable mapping (IO4)
            let v = (|| -> Result<u64, Status> {
                let entry = o.handles.get(Handle(a[1]))?;
                let id = match o.objs.get(entry.obj) {
                    Some(KObj::Vmo(id)) => *id,
                    Some(_) => return Err(Status::WrongType),
                    None => return Err(Status::BadHandle),
                };
                if o.vmos.kind(id) != Some(VmoKind::Dma) {
                    return Err(Status::WrongType);
                }
                let mut need = Rights(Rights::DMA.0 | Rights::READ.0);
                if writes {
                    need = need.union(Rights::WRITE);
                }
                if !entry.rights.contains(need) {
                    return Err(Status::AccessDenied);
                }
                Ok(id)
            })();
            if let Some(s) = first_error(&[d.err(), v.err()]) {
                return Err(s);
            }
            let (d, vmo) = (
                d.map_err(|_| Status::BadHandle)?,
                v.map_err(|_| Status::BadHandle)?,
            );
            if pages == 0
                || offset % 4096 != 0
                || access & !(io::READ | io::WRITE) != 0
                || access & io::READ == 0
            {
                return Err(Status::InvalidArgs);
            }
            let first = offset / 4096;
            if first
                .checked_add(pages)
                .is_none_or(|end| o.vmos.pages(vmo).is_none_or(|n| end > n))
            {
                return Err(Status::InvalidArgs);
            }
            if d.destroyed {
                return Err(Status::BadState);
            }
            let charge = iod::charge_map(o.jobs, d.job, pages).map_err(|_| Status::NoMemory)?;
            if let Err(x) = o.vmos.hold_for_io(vmo, first, pages) {
                charge.refund(o.jobs);
                return Err(match x {
                    IoHoldError::Gone => Status::BadHandle,
                    IoHoldError::NotDma => Status::WrongType,
                    IoHoldError::OutOfRange => Status::InvalidArgs,
                    IoHoldError::NoMemory => Status::NoMemory,
                });
            }
            // (the call's own use of the job: the handle may be closed by another thread while the map runs)
            o.jobs.retain(d.job);
            Ok((DomainId::from_raw(d.domain), d.job, vmo, charge))
        },
    );
    let (d, job, vmo, charge) = match resolved.unwrap_or(Err(Status::BadState)) {
        Ok(x) => x,
        Err(s) => return err(s),
    };
    let first = offset / 4096;
    let r = map_in(e, d, vmo, first, pages, access, charge);
    release_job(e, job);
    r
}

#[allow(clippy::too_many_arguments)]
fn map_in<E: IoEnv>(
    e: &mut E,
    d: DomainId,
    vmo: u64,
    first: u64,
    pages: u64,
    access: u64,
    charge: iod::MapCharge,
) -> Reply {
    let acc = if access & io::WRITE != 0 {
        Access::ReadWrite
    } else {
        Access::Read
    };
    // the hold is taken: the frames are marked before any leaf names them (debug builds), and the record is made with no lock held
    mark(e, vmo, first, pages, true);
    let undo_hold = |e: &mut E, charge: iod::MapCharge| {
        mark(e, vmo, first, pages, false);
        e.mem(|mm, vmos, jobs, _| {
            charge.refund(jobs);
            let released = vmos.release(mm, jobs, vmo);
            debug_assert!(released.is_ok(), "the map's hold");
        });
    };
    let Some(rec) = Record::new(vmo, first, pages, acc) else {
        undo_hold(e, charge);
        return err(Status::NoMemory);
    };
    let mut cur = match e.iommu(|s, _, _, _| s.map_begin(d, rec)) {
        Ok(c) => c,
        Err(f) => {
            let s = match f {
                MapFailed::Refused(x, _) => io_status(x),
                _ => Status::BadState, // (not reached: nothing is written before the first step)
            };
            undo_hold(e, charge);
            return err(s);
        }
    };
    let step = loop {
        let mut buf = [0 as Pfn; RANGE_STEP as usize];
        let mut n = 0usize;
        if let Some((at, k)) = cur.next_frames() {
            n = usize::try_from(k).unwrap_or(0).min(buf.len());
            let out = buf.get_mut(..n).unwrap_or(&mut []);
            if !e.mem(|_, vmos, _, _| vmos.io_frames(vmo, at, out)) {
                n = 0; // (not reached: the hold keeps the VMO and its frames)
            }
        }
        let frames = buf.get(..n).unwrap_or(&[]);
        match e
            .iommu(|s, mem, regs, db| s.map_step(mem, regs, &mut DbFrames { db, mem }, cur, frames))
        {
            MapStep::More(c) => {
                cur = c;
                e.relax(); // (between steps, with no lock held: OD168)
            }
            other => break other,
        }
    };
    match step {
        MapStep::Done(m) => {
            e.mem(|_, _, jobs, _| charge.settle(jobs, m.tables));
            let iova = m.iova;
            if wait(e, &mut Waiter::Map(d, m)) {
                return Ok((Some(iova), None));
            }
            // 4.3: a map whose fence fails is unmapped again, and the call is NO_MEMORY (6.4)
            // (BAD_STATE here: the domain's destruction began meanwhile and took the mapping, which it releases after its own fence)
            let _ = unmap_in(e, d, iova, pages);
            err(Status::NoMemory)
        }
        MapStep::Failed(MapFailed::RolledBack {
            pending,
            tables,
            error,
        }) => {
            e.mem(|_, _, jobs, _| charge.settle(jobs, tables));
            settle_pending(e, pending);
            err(io_status(error))
        }
        MapStep::Failed(MapFailed::Dropped { released, tables }) => {
            e.mem(|_, _, jobs, _| charge.settle(jobs, tables));
            release(e, released);
            err(Status::BadState)
        }
        MapStep::Failed(MapFailed::Leaked { tables }) => {
            e.mem(|_, _, jobs, _| charge.settle(jobs, tables));
            err(Status::BadState)
        }
        MapStep::Failed(MapFailed::Refused(x, _)) => {
            // (not reached: a refusal comes from `map_begin`)
            undo_hold(e, charge);
            err(io_status(x))
        }
        MapStep::More(_) => err(Status::BadState), // (not reached: the loop runs until the map ends)
    }
}

/// 6.1 steps 2 to 4 for a pending record: wait, complete, release; a fence that does not complete leaks the record (6.4). A record the
/// domain's destruction took is released by the destruction.
fn settle_pending<E: IoEnv>(e: &mut E, p: Pending) {
    let mut w = Waiter::Unmap(p);
    let done = wait(e, &mut w);
    let Waiter::Unmap(p) = w else { return };
    if !done {
        e.iommu(|s, _, _, _| s.abandon(&p));
        return;
    }
    if let Ok(Some(rel)) = e.iommu(|s, _, _, _| s.complete(&p)) {
        release(e, rel);
    }
}

// ---- iodomain_unmap --------------------------------------------------------------------------------------------------------------

/// `iodomain_unmap(domain, io_address, pages)` (4.4, amendment parts 1 and 3).
pub fn iodomain_unmap<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    let d = e.obj(|o| ipc::iodomain_of(o.objs, o.handles, Handle(a[0]), Rights::IO_UNMAP));
    let d = match d.unwrap_or(Err(Status::BadState)) {
        Ok(d) => d,
        Err(s) => return err(s),
    };
    let (io_address, pages) = (a[1], a[2]);
    // (4.4's order: INVALID_ARGS, "not exactly a live mapping", comes before BAD_STATE; a destroyed domain has no live mapping, its
    // mappings went with it, so its unmap is INVALID_ARGS and BAD_STATE is not reached: the owner's question is decision Q-IOMMU-UNMAPORDER)
    if pages == 0 || io_address % 4096 != 0 || d.destroyed {
        return err(Status::InvalidArgs);
    }
    match unmap_in(e, DomainId::from_raw(d.domain), io_address, pages) {
        Ok(()) => Ok((None, None)),
        // (a destruction begun since the handle was resolved took the mapping: not a live mapping, as above)
        Err(IoError::BadState) => err(Status::InvalidArgs),
        Err(x) => err(io_status(x)),
    }
}

fn unmap_in<E: IoEnv>(e: &mut E, d: DomainId, io_address: u64, pages: u64) -> Result<(), IoError> {
    let mut u = e.iommu(|s, _, _, _| s.unmap_begin(d, io_address, pages))?;
    loop {
        match e.iommu(|s, mem, regs, _| s.unmap_step(mem, regs, u)) {
            UnmapStep::More(n) => {
                u = n;
                e.relax(); // (between steps, with no lock held: OD168)
            }
            // OD170: the destruction took the record; what the caller asked for is guaranteed by it
            UnmapStep::Taken => return Ok(()),
            UnmapStep::Queued(p) => {
                settle_pending(e, p);
                return Ok(());
            }
        }
    }
}

// ---- iodomain_destroy ------------------------------------------------------------------------------------------------------------

/// `iodomain_destroy(domain)` (4.5): 6.3 step 1 in the same section as the object's mark (OBJ, then IOMMU), then steps 2 to 4 with no
/// lock held. A destroyed domain's destroy is OK and does nothing.
pub fn iodomain_destroy<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    let started = e.obj_iommu(
        |objs, handles, s, mem, regs| -> Result<Option<Destroying>, Status> {
            let d = ipc::iodomain_of(objs, handles, Handle(a[0]), Rights::IO_DESTROY)?;
            if !ipc::iodomain_mark_destroyed(objs, d.obj) {
                return Ok(None);
            }
            Ok(s.destroy(mem, regs, DomainId::from_raw(d.domain)))
        },
    );
    match started.unwrap_or(Err(Status::BadState)) {
        Ok(x) => {
            if let Some(x) = x {
                finish_destroy(e, x);
            }
            // (another thread may have taken this destruction, or a second destroy found it under way: done before the return)
            settle_destructions(e);
            Ok((None, None))
        }
        Err(s) => err(s),
    }
}

// ---- iodomain_fault_read ---------------------------------------------------------------------------------------------------------

/// `iodomain_fault_read(domain) -> (cause | direction << 16 | dropped << 32, io_address)` (7.4). The ring and the DEVICE_FAULT signal
/// change in one section (OBJ, then IOMMU), so a fault stored meanwhile is never hidden.
pub fn iodomain_fault_read<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    e.obj_iommu(|objs, handles, s, _, _| -> Reply {
        let d = ipc::iodomain_of(objs, handles, Handle(a[0]), Rights::INSPECT)
            .map_err(|x| (x, None, None))?;
        let read = s.fault_read(DomainId::from_raw(d.domain)).ok().flatten();
        let more = read.is_some_and(|r| r.more);
        ipc::iodomain_set_fault(objs, d.obj, more);
        let Some(r) = read else {
            return err(Status::ShouldWait);
        };
        let dir = match r.record.ttype.direction() {
            "read" => io::DIR_READ,
            "write" => io::DIR_WRITE,
            "exec" => io::DIR_EXEC,
            _ => io::DIR_NONE,
        };
        let word = u64::from(r.record.cause)
            | dir << io::DIRECTION_SHIFT
            | u64::from(r.dropped) << io::DROPPED_SHIFT;
        Ok((Some(word), Some(r.record.iotval)))
    })
    .unwrap_or(Err((Status::BadState, None, None)))
}

// ---- vmo_create_dma_in -----------------------------------------------------------------------------------------------------------

/// `vmo_create_dma_in(resource, pages, job) -> vmo` (4.7; amendment part 1 point 5: the frames are taken and zeroed `RANGE_STEP` at a
/// time under `FRAME` only, while no handle names the VMO).
pub fn vmo_create_dma_in<E: IoEnv>(e: &mut E, a: [u64; 6]) -> Reply {
    let pages = a[1];
    let begun = e.obj(|o| -> Result<(JobId, kcore::aspace::DmaBuild), Status> {
        let r = ipc::resource_of(o.objs, o.handles, Handle(a[0])).err();
        let j = if a[2] == 0 {
            Ok(o.job)
        } else {
            ipc::job_of(o.objs, o.handles, Handle(a[2]), Rights::MANAGE)
        };
        if let Some(s) = first_error(&[r, j.err()]) {
            return Err(s);
        }
        let job = j.map_err(|_| Status::BadHandle)?;
        let b = o.vmos.dma_begin(o.jobs, pages, job).map_err(|x| match x {
            VmoError::OutOfRange => Status::InvalidArgs,
            _ => Status::NoMemory,
        })?;
        o.jobs.retain(job);
        Ok((job, b))
    });
    let (job, mut b) = match begun.unwrap_or(Err(Status::BadState)) {
        Ok(x) => x,
        Err(s) => return err(s),
    };
    let built = loop {
        match e.frame(|db, mem, _| b.step_with(db, mem)) {
            Ok(false) => e.relax(), // (between steps, with no lock held: OD168)
            Ok(true) => break true,
            Err(_) => break false,
        }
    };
    let mut slot = Some(b);
    let r = e.obj(|o| -> Reply {
        let Some(b) = slot.take() else {
            return err(Status::BadState);
        };
        if !built {
            o.vmos.dma_abort(o.mm, o.jobs, b);
            return err(Status::NoMemory);
        }
        let id = o
            .vmos
            .dma_finish(o.mm, o.jobs, b)
            .map_err(|_| (Status::NoMemory, None, None))?;
        let rights = Rights((Rights::VMO.0 & !Rights::EXECUTE.0) | Rights::DMA.0);
        match ipc::vmo_object_create_with(
            o.objs,
            o.handles,
            id,
            rights,
            &mut Quota {
                jobs: o.jobs,
                job: Some(o.job),
            },
        ) {
            Ok(h) => Ok((Some(h.0), None)),
            Err(s) => {
                let _ = o.vmos.release(o.mm, o.jobs, id);
                err(s)
            }
        }
    });
    // (the caller's process went while the frames were taken: what was built is given back)
    let r = r.unwrap_or_else(|| {
        if let Some(b) = slot.take() {
            e.mem(|mm, vmos, jobs, _| vmos.dma_abort(mm, jobs, b));
        }
        err(Status::BadState)
    });
    release_job(e, job);
    r
}
