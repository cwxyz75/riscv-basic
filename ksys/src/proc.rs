//! Processes and the calls that act across them (TS3b): the process record, and `process_map`, `process_unmap`, `process_self`,
//! `process_info`, `thread_info`, `job_root` and `resource_root` over the process table, with the machine's part (TLB shootdowns) behind
//! a small trait.

use crate::{fail, Reply};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::Handle;
use alloc::vec::Vec;
use kcore::aspace::AddressSpace;
use kcore::aspace::Vmos;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs};
use kcore::job::{JobId, Jobs, Resource};
use kcore::pagetable::{Perms, PhysMem};
use kcore::sched::Tid;
use kcore::Mm;

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;

/// What the process calls need from the machine: the translations other harts may still hold for an address space that changed.
pub trait Machine {
    /// Have every other hart drop its cached translations (a region was unmapped).
    fn shootdown(&mut self);
    /// Drop this hart's own cached translations.
    fn flush_tlb(&mut self);
    /// `pages` executable pages were just made visible (PL7): make every hart's instruction fetches agree with memory.
    fn icache_sync(&mut self, _pages: u64) {}
    /// A number below `n` from the kernel's entropy pool (HD1), for the placement of a mapping that asked for ANYWHERE; 0 where there is no pool.
    fn random_below(&mut self, _n: u64) -> u64 {
        0
    }
    /// Negative control (MM8): leave out the write-protection of a SNAPSHOT's source mappings. Never true in a real kernel.
    fn skip_cow_protect(&self) -> bool {
        false
    }
    /// CPU nanoseconds the live thread `tid` has run (AB6: `thread_info` 1 and `process_info` 1); 0 where there is no scheduler.
    fn thread_cpu_ns(&self, _tid: Tid) -> u64 {
        0
    }
    /// The same for the calling thread (`thread_info` with handle 0).
    fn current_thread_cpu_ns(&self) -> u64 {
        0
    }
    /// Negative control: leave out the shootdown that call number `call` (`VMO_DECOMMIT` or `PROCESS_PROTECT`) owes the other harts before a
    /// frame is freed or a permission is taken away. Never true in a real kernel.
    fn skip_shootdown(&self, _call: usize) -> bool {
        false
    }
}

/// A call acting on the process table: every process, the kernel objects, the memory and the ledger, and which process is calling.
pub struct PCtx<'a, M: PhysMem, X: Machine> {
    pub objs: &'a mut Kobjs,
    pub procs: &'a mut crate::ProcTable,
    /// The calling process.
    pub me: u64,
    pub mm: &'a mut Mm<M>,
    pub vmos: &'a mut Vmos,
    pub jobs: &'a mut Jobs,
    pub machine: &'a mut X,
}

/// The process table: ordered by process id, with fallible growth (KT9: `FMap`, and a process record in an `FBox`, so that making a process can say no).
pub type ProcTable = kcore::fmap::FMap<u64, kcore::fbox::FBox<Process>>;

/// Put a process in the table (the boot loader and the host tests, which have nobody to tell). Panics when there is no memory.
#[allow(clippy::expect_used)] // INVARIANT: boot and test setup only: allocation failure there is a failure to start
pub fn put_process(procs: &mut ProcTable, pid: u64, p: Process) {
    let b = kcore::fbox::FBox::try_new(p)
        .ok()
        .expect("memory for a process record");
    procs
        .try_insert(pid, b)
        .expect("memory for a process table entry");
}

pub struct Process {
    /// The user threads of this process that have started and not ended (T191): what a kill, a suspend or a teardown walks, instead of every thread
    /// in the system. Kept in step with `UserState::thread_pid` (the same threads, keyed the other way): a thread is added when it registers
    /// itself on its first run and removed when it ends or its process is torn down.
    pub live_tids: kcore::fmap::FSet<Tid>,
    /// The address space behind its `AS` lock (L17): shared by `Arc` so a syscall can hold the locks of the spaces it touches ([`klock::aslock::AsCell::hold`])
    /// while the process table is borrowed as the pure code likes.
    pub aspace: alloc::sync::Arc<klock::aslock::AsCell>,
    pub handles: HandleTable,
    /// The address-space identifier and the generation it was allocated in (L18); the allocator is `UserState::asids`.
    pub tag: kcore::asid::AsTag,
    pub vmos: Vec<u64>,
    pub entry: u64,
    pub arg: u64,
    pub exit_code: Option<i32>,
    /// The creator's end of the bootstrap channel (a handle in `UserState::ktable`), until
    /// a test or server takes it with `take_bootstrap`.
    pub kernel_end: Option<Handle>,
    /// The kernel's end of the channel exceptions of this process's threads are delivered on (EX2; a handle in `UserState::ktable`).
    pub exc_channel: Option<Handle>,
    /// Where the first thread starts the stack (0: the default `STACK_TOP`).
    pub start_sp: u64,
    /// The process's object (for TERMINATED and kill-on-close), if it has one.
    pub obj: Option<kcore::object::ObjId>,
    pub state: ProcState,
    /// Started by `process_start`: its own thread tears it down when it ends and nobody
    /// joins that thread.
    pub detached: bool,
    /// `process_kill` asked for it to end, with this exit code; its thread dies at its next
    /// return to user mode (or wakes from a wait to do so).
    pub killed: Option<i32>,
    /// `process_suspend` stopped it: every thread parks at its next return to user mode (a thread blocked in a call
    /// stays blocked, and parks when the call returns) until `process_resume` or a kill. SIG2, SYSCALLS.md.
    pub stopped: bool,
    /// User threads that exist and have not ended (counted from `thread_spawn`, before the
    /// thread first runs). The process is torn down when this reaches 0.
    pub threads_alive: usize,
    /// The job this process is charged to (it holds one use of it): its process, its threads
    /// and, through the VMOs made for it, its memory.
    pub job: JobId,
    /// Pages charged to `job` for the page-table frames this process's mappings can need (a
    /// worst-case estimate per mapped region, plus the root table), by region start; credited
    /// when the region goes or the process ends. Table frames are not VMO pages, so without this
    /// a program could spend memory the page limit never sees.
    pub table_charge: kcore::fmap::FMap<u64, u64>,
    /// Where each `process_map` charge in `table_charge` ends (its key is where it starts): the charge is credited when no region is left in
    /// that span, not when the region that started there goes (a range unmap or a protect can split a mapping: the tables of what stays
    /// must stay charged, MM7).
    pub charge_end: kcore::fmap::FMap<u64, u64>,
    /// The stacks `thread_spawn` provisioned, by thread: freed when the thread ends (or with the process). Entered at the spawn with room `reserve_thread` made.
    pub stacks: kcore::fmap::FMap<Tid, SpawnStack>,
    /// Where the next stack search in the arena starts (just above the last stack handed out).
    pub stack_hint: u64,
    /// Started by the kernel (`start`): may ask for the root job (`job_root`). Nothing a program
    /// creates is privileged.
    pub privileged: bool,
    /// Where the system library (AB7) is mapped in this process: its base and its text range (all zero when it is not).
    pub libsys: LibRange,
}

impl Process {
    /// Room, before anything changes, for one more `table_charge` and `charge_end` entry (KT9: the entries are made by `add_charge` after the mapping
    /// they account for has succeeded, and that step allocates nothing).
    pub fn reserve_charge(&mut self) -> Result<(), Status> {
        self.table_charge.reserve(1).map_err(|_| Status::NoMemory)?;
        self.charge_end.reserve(1).map_err(|_| Status::NoMemory)
    }

    /// Record `tables` pages charged for a mapping that starts at `vaddr` and ends at `end` (a key that is there already is a mapping that was split and
    /// whose start has been mapped again: its charge stays, and this one adds). Uses the room `reserve_charge` made.
    pub fn add_charge(&mut self, vaddr: u64, tables: u64, end: u64) {
        match self.table_charge.get_mut(&vaddr) {
            Some(c) => *c = c.saturating_add(tables),
            None => {
                self.table_charge.insert_reserved(vaddr, tables);
            }
        }
        match self.charge_end.get_mut(&vaddr) {
            Some(x) => *x = (*x).max(end),
            None => {
                self.charge_end.insert_reserved(vaddr, end);
            }
        }
    }
}

/// Where the system library lies in a process: the base the kernel chose, and the text range (the only place `ecall` is allowed from, AB8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibRange {
    pub base: u64,
    pub text_start: u64,
    pub text_end: u64,
}

/// What `process_start` maps into the new process before it runs (AB7: the system library). The kernel provides it; the host tests use [`NoMap`].
pub trait StartMap {
    /// Map into `aspace`; returns where, and how many bytes of address space the mapping spans (page-table frames are charged for them).
    fn map(
        &mut self,
        aspace: &mut AddressSpace,
        jobs: &mut Jobs,
    ) -> Result<(LibRange, u64), Status>;
    /// Take back what `map` mapped (the process has not run).
    fn unmap(&mut self, aspace: &mut AddressSpace, jobs: &mut Jobs, range: LibRange, span: u64);
}

/// Maps nothing.
pub struct NoMap;

impl StartMap for NoMap {
    fn map(&mut self, _: &mut AddressSpace, _: &mut Jobs) -> Result<(LibRange, u64), Status> {
        Ok((LibRange::default(), 0))
    }
    fn unmap(&mut self, _: &mut AddressSpace, _: &mut Jobs, _: LibRange, _: u64) {}
}

/// The stack a `thread_spawn` provisioned: its region in the address space and the VMO behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnStack {
    pub region: u64,
    pub vmo: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Created and possibly populated, no thread yet.
    Created,
    Running,
}

/// A chunked decommit or shrink that `vmo_range_begin` started: the VMO is busy until `vmo_range_end`.
#[derive(Debug, Clone, Copy)]
pub struct RangeCall {
    pub id: u64,
}

fn range_status(e: kcore::aspace::VmoError) -> Status {
    use kcore::aspace::VmoError;
    match e {
        VmoError::Gone => Status::BadHandle,
        VmoError::NotResizable => Status::AccessDenied,
        VmoError::HasChildren => Status::BadState,
        VmoError::NoMemory => Status::NoMemory,
        VmoError::Busy => Status::ShouldWait,
        _ => Status::InvalidArgs,
    }
}

/// `vmo_decommit` / `vmo_set_size`, first part (T207 PR 3a): resolve and check the arguments, refuse what is refused, and mark the VMO busy.
/// `Ok(None)`: nothing to take away (a growth, or an empty range); otherwise the steps follow (`vmo_range_step` until it says done, then
/// `Vmos::range_end`, whatever the steps answered).
pub fn vmo_range_begin<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    a: [u64; 6],
) -> Result<Option<RangeCall>, Status> {
    let caller = c.procs.get(&c.me).ok_or(Status::BadState)?;
    let id = ipc::vmo_of(c.objs, &caller.handles, Handle(a[0]), Rights::WRITE)?;
    let size = c.vmos.pages(id).ok_or(Status::BadHandle)?;
    let (first, count, new_size) = if n == nr::VMO_DECOMMIT {
        if c.vmos.kind(id) != Some(kcore::aspace::VmoKind::Plain) {
            return Err(Status::NotSupported);
        }
        let (offset, len) = (a[1], a[2]);
        if !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) || len == 0 {
            return Err(Status::InvalidArgs);
        }
        let first = offset / 4096;
        let count = len / 4096;
        if first.checked_add(count).is_none_or(|end| end > size) {
            return Err(Status::InvalidArgs);
        }
        (first, count, None)
    } else {
        // (a VMO that is not resizable, which every device and DMA VMO is, is refused before the size is looked at)
        if !c.vmos.is_resizable(id) {
            return Err(Status::AccessDenied);
        }
        let bytes = a[1];
        if bytes == 0 || !bytes.is_multiple_of(4096) || bytes > abi::user::TOP {
            return Err(Status::InvalidArgs);
        }
        let new = bytes / 4096;
        // growing takes nothing away; shrinking takes the tail
        (new.min(size), size.saturating_sub(new), Some(new))
    };
    // (a VMO with children cannot change size: refused before anything is unmapped)
    if new_size.is_some() && c.vmos.has_children(id) {
        return Err(Status::BadState);
    }
    if count > 0 {
        // V1 (INVARIANTS): a process of another job than the VMO's holds its mapping on the promise that touching it never fails on the
        // VMO's job and never ends it; taking pages from under such a mapping would break that (a touch would charge the VMO's job, or
        // fault past a new end). The holder's own job may take pages back; a stranger's mapping stops the call.
        let vmo_job = c.vmos.job_of(id).ok_or(Status::BadHandle)?;
        if c.vmos.maps_other_job(id, first, count, vmo_job, &|o| {
            c.procs.get(&o).map(|p| p.job)
        }) {
            return Err(Status::BadState);
        }
    }
    match c
        .vmos
        .range_begin(id, first, first.saturating_add(count), new_size)
    {
        Ok(true) => Ok(Some(RangeCall { id })),
        Ok(false) => Ok(None),
        Err(e) => Err(range_status(e)),
    }
}

/// One batch of a chunked decommit or shrink, in the three parts the kernel runs under different locks (T207 PR 3d): `vmo_range_take` (A: the frames
/// leave the VMO's index and are counted as detached), `vmo_range_unmap_owner` for each process that maps the span (B: one address space at a time),
/// `vmo_range_finish` (C: every hart drops its translations, then the frames are freed and credited).
pub struct RangeBatch {
    frames: [kcore::frame::Pfn; kcore::aspace::RANGE_STEP],
    lo: u64,
    next: u64,
    n: usize,
    /// The step took the last pages of the range.
    pub done: bool,
    err: Option<kcore::aspace::VmoError>,
    /// The greatest process id that maps the VMO or a node below it when the batch was taken: a mapping made later cannot reach a detached frame.
    bound: u64,
    /// The last process whose space was cleared (the walk goes up from it).
    cursor: u64,
    cleared: u64,
}

/// A: take the next batch out of the VMO's index. Needs the memory locks only.
pub fn vmo_range_take<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    call: RangeCall,
) -> Result<RangeBatch, Status> {
    let PCtx { vmos, jobs, .. } = c;
    let mut b = RangeBatch {
        frames: [0; kcore::aspace::RANGE_STEP],
        lo: 0,
        next: 0,
        n: 0,
        done: false,
        err: None,
        bound: 0,
        cursor: 0,
        cleared: 0,
    };
    let st = vmos
        .range_step(jobs, call.id, &mut b.frames)
        .map_err(range_status)?;
    (b.lo, b.next, b.n, b.done, b.err) = (st.lo, st.next, st.n, st.done, st.err);
    b.bound = vmos.max_mapper(call.id);
    Ok(b)
}

/// The next process (above the last one cleared) that maps the batch's span, if any.
pub fn vmo_range_next_owner<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    call: RangeCall,
    batch: &RangeBatch,
) -> Option<u64> {
    if batch.next <= batch.lo {
        return None;
    }
    c.vmos.next_mapper(call.id, batch.cursor, batch.bound)
}

/// B: clear the batch's span in the address space of process `owner` (the caller holds it, and no other), for the VMO and every node below it.
pub fn vmo_range_unmap_owner<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    call: RangeCall,
    batch: &mut RangeBatch,
    owner: u64,
) {
    let PCtx {
        procs, mm, vmos, ..
    } = c;
    batch.cursor = owner;
    let Some(p) = procs.get_mut(&owner) else {
        return; // (a process that is gone holds its own frame references; its teardown drops them)
    };
    let (first, count) = (batch.lo, batch.next.saturating_sub(batch.lo));
    let mut cleared = 0u64;
    vmos.for_each_below(call.id, 0, &mut |d, base| {
        if let Some(f) = first
            .checked_sub(base)
            .or((first.saturating_add(count) > base).then_some(0))
        {
            let k = first
                .saturating_add(count)
                .saturating_sub(base)
                .saturating_sub(f);
            cleared = cleared.saturating_add(p.aspace.held_mut().unmap_vmo_pages(mm, d, f, k));
        }
    });
    batch.cleared = batch.cleared.saturating_add(cleared);
}

/// C: other harts may still cache the translations of what was just unmapped: they drop them before any frame is freed (TLB2); then the VMO gives the
/// frames back. (When nothing was mapped there is nothing to drop: every earlier removal of a translation was shot down when it was made.) Returns
/// the status the take stopped with, if any.
pub fn vmo_range_finish<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    call: RangeCall,
    batch: &mut RangeBatch,
) -> Result<bool, Status> {
    let PCtx {
        mm,
        vmos,
        jobs,
        machine,
        ..
    } = c;
    if batch.cleared > 0 {
        if !machine.skip_shootdown(n) {
            machine.shootdown();
        }
        machine.flush_tlb();
    }
    let got = batch.frames.get(..batch.n).unwrap_or(&[]);
    vmos.range_free(mm, jobs, call.id, got);
    match batch.err {
        Some(e) => Err(range_status(e)),
        None => Ok(batch.done),
    }
}

/// One whole step (the host tests, where nothing runs between its parts).
pub fn vmo_range_step<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    call: RangeCall,
) -> Result<bool, Status> {
    let mut batch = vmo_range_take(c, call)?;
    while let Some(owner) = vmo_range_next_owner(c, call, &batch) {
        vmo_range_unmap_owner(c, call, &mut batch, owner);
    }
    vmo_range_finish(c, n, call, &mut batch)
}

/// `vmo_decommit` / `vmo_set_size` without a switch point between the steps (the host tests, where nothing else runs).
fn vmo_range_all<M: PhysMem, X: Machine>(c: &mut PCtx<'_, M, X>, n: usize, a: [u64; 6]) -> Reply {
    let call = match vmo_range_begin(c, n, a) {
        Ok(Some(call)) => call,
        Ok(None) => return Ok((None, None)),
        Err(s) => return Err(e(s)),
    };
    let r = loop {
        match vmo_range_step(c, n, call) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(s) => break Err(s),
        }
    };
    c.vmos.range_end(c.mm, c.jobs, call.id, r.is_ok());
    r.map(|()| (None, None)).map_err(e)
}

/// An unmap in chunks that `process_unmap_begin` started: the regions of `[start, end)` of process `tpid` are dying until the steps have taken them.
#[derive(Debug, Clone, Copy)]
pub struct UnmapCall {
    pub tpid: u64,
    pub start: u64,
    pub end: u64,
}

/// Mappings one unmap step clears at most, and the page-table entries it may look at (T207 PR 3b).
pub const UNMAP_STEP_PTES: u64 = 64;

/// `process_unmap`, first part: resolve and check the arguments, split the regions at the edges and mark the ones inside dying. Every failure is
/// here, before the first page goes.
pub fn process_unmap_begin<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    a: [u64; 6],
) -> Result<UnmapCall, Status> {
    let PCtx {
        objs,
        procs,
        me,
        vmos,
        jobs,
        ..
    } = c;
    let caller = procs.get(me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE)?;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    target
        .aspace
        .held_mut()
        .set_region_limit(jobs.region_limit(target.job));
    if target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    // length 0: the region that starts at `vaddr`, whole (the original call). Otherwise (MM7) every page of `[vaddr, vaddr + length)`:
    // holes are fine, regions are split at the edges.
    if a[2] == 0 {
        let end = target
            .aspace
            .held_mut()
            .unmap_region_begin(a[1])
            .map_err(|_| Status::InvalidArgs)?;
        return Ok(UnmapCall {
            tpid,
            start: a[1],
            end,
        });
    }
    if !in_user_range(a[1], a[2]) {
        return Err(Status::InvalidArgs);
    }
    target
        .aspace
        .held_mut()
        .unmap_begin(vmos, a[1], a[2])
        .map_err(|x| match x {
            kcore::aspace::RegionError::TooMany => Status::NoMemory,
            _ => Status::InvalidArgs,
        })?;
    Ok(UnmapCall {
        tpid,
        start: a[1],
        end: a[1].saturating_add(a[2]),
    })
}

/// One step of the unmap: clear a batch of mappings, have every hart drop its translations (the frames the batch freed are not reused before:
/// the memory locks are held), and say whether the call is done. A target that is gone or ending leaves its dying regions to its teardown.
pub fn process_unmap_step<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    call: UnmapCall,
) -> Result<bool, Status> {
    let PCtx {
        procs,
        mm,
        vmos,
        jobs,
        machine,
        ..
    } = c;
    let target = procs.get_mut(&call.tpid).ok_or(Status::BadState)?;
    if target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    let (done, cleared) = target.aspace.held_mut().unmap_step(
        mm,
        vmos,
        jobs,
        call.start,
        call.end,
        UNMAP_STEP_PTES,
        UNMAP_STEP_PTES.saturating_mul(kcore::aspace::SCAN_PER_PTE),
    );
    // other harts may still cache the translations of what was just unmapped (also when a step found none mapped: as the one-shot call, it asks
    // once at least)
    let _ = cleared;
    machine.shootdown();
    machine.flush_tlb();
    Ok(done)
}

/// The end of the unmap: the page-table charge of a `process_map` is credited when no region is left in the span it covered (a mapping that was
/// split keeps the charge for what stays: its tables are still there). A target that is gone has nothing to credit.
pub fn process_unmap_end<M: PhysMem, X: Machine>(c: &mut PCtx<'_, M, X>, call: UnmapCall) {
    let PCtx { procs, jobs, .. } = c;
    if let Some(target) = procs.get_mut(&call.tpid) {
        // (a cursor over the keys, not a list of them: nothing is allocated, so the call cannot fail after the unmap has been done: KT9)
        let mut from = 0u64;
        while let Some(k) = target
            .charge_end
            .range(from..)
            .find(|&(&k, &end)| !target.aspace.held_ref().has_region_in(k, end))
            .map(|(&k, _)| k)
        {
            from = k.saturating_add(1);
            target.charge_end.remove(&k);
            if let Some(tables) = target.table_charge.remove(&k) {
                jobs.credit(target.job, Resource::Pages, tables);
            }
        }
    }
}

/// `process_unmap` without a switch point between the steps (the host tests).
fn process_unmap_all<M: PhysMem, X: Machine>(c: &mut PCtx<'_, M, X>, a: [u64; 6]) -> Reply {
    let call = process_unmap_begin(c, a).map_err(e)?;
    let r = loop {
        match process_unmap_step(c, call) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(s) => break Err(s),
        }
    };
    if r.is_ok() {
        process_unmap_end(c, call);
    }
    r.map(|()| (None, None)).map_err(e)
}

/// A protect in chunks that `process_protect_begin` started: the regions of `[cursor, end)` have their new permissions, the page-table entries follow.
#[derive(Debug, Clone, Copy)]
pub struct ProtectCall {
    pub tpid: u64,
    pub cursor: u64,
    pub end: u64,
    downgraded: bool,
    exec_pages: u64,
}

/// `process_protect`, first part: every check, the split, and the new permissions of the regions. Every failure is here.
pub fn process_protect_begin<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    a: [u64; 6],
) -> Result<ProtectCall, Status> {
    let PCtx {
        objs,
        procs,
        me,
        mm,
        vmos,
        jobs,
        ..
    } = c;
    let (vaddr, length, flags) = (a[1], a[2], a[3]);
    let caller = procs.get(me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE)?;
    if flags & !(PF_R | PF_W | PF_X) != 0
        || flags & PF_W != 0 && flags & PF_X != 0
        || !in_user_range(vaddr, length)
    {
        return Err(Status::InvalidArgs);
    }
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    target
        .aspace
        .held_mut()
        .set_region_limit(jobs.region_limit(target.job));
    if target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    let perms = Perms {
        read: true,
        write: flags & PF_W != 0,
        exec: flags & PF_X != 0,
        user: true,
    };
    target
        .aspace
        .held_mut()
        .protect_begin(mm, vmos, jobs, vaddr, length, perms)
        .map_err(|x| match x {
            kcore::aspace::ProtectError::Denied => Status::AccessDenied,
            kcore::aspace::ProtectError::TooMany => Status::NoMemory,
            _ => Status::InvalidArgs,
        })?;
    Ok(ProtectCall {
        tpid,
        cursor: vaddr,
        end: vaddr.saturating_add(length),
        downgraded: false,
        exec_pages: 0,
    })
}

/// One step: bring a batch of page-table entries to the permissions of their regions. `Ok(true)` when the range is done.
pub fn process_protect_step<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    call: &mut ProtectCall,
) -> Result<bool, Status> {
    let PCtx {
        procs,
        mm,
        vmos,
        machine,
        ..
    } = c;
    let target = procs.get_mut(&call.tpid).ok_or(Status::BadState)?;
    if target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    let (done, st) = target.aspace.held_mut().protect_step(
        mm,
        vmos,
        &mut call.cursor,
        call.end,
        UNMAP_STEP_PTES,
        UNMAP_STEP_PTES.saturating_mul(kcore::aspace::SCAN_PER_PTE),
    );
    call.downgraded |= st.downgraded;
    call.exec_pages = call.exec_pages.saturating_add(st.exec_pages);
    if st.downgraded {
        // a permission taken away may be cached by every hart, and the locks drop before the next step: a SNAPSHOT's write-protect (COW1) and any
        // other reader of the table must not see a writable translation the table no longer holds
        if !machine.skip_shootdown(n) {
            machine.shootdown();
        }
        machine.flush_tlb();
    }
    Ok(done)
}

/// The end: a permission taken away may still be cached by every hart; code made visible must reach every instruction fetch (M9: both before
/// the call returns).
pub fn process_protect_end<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    call: ProtectCall,
) {
    let _ = n;
    let machine = &mut c.machine;
    if call.exec_pages > 0 {
        machine.icache_sync(call.exec_pages);
    }
}

fn process_protect_all<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    n: usize,
    a: [u64; 6],
) -> Reply {
    let mut call = process_protect_begin(c, a).map_err(e)?;
    loop {
        match process_protect_step(c, n, &mut call) {
            Ok(true) => break,
            Ok(false) => {}
            Err(s) => {
                process_protect_end(c, n, call);
                return Err(e(s));
            }
        }
    }
    process_protect_end(c, n, call);
    Ok((None, None))
}

fn bad(s: Status) -> Reply {
    fail(s)
}

fn e(s: Status) -> (Status, Option<u64>, Option<u64>) {
    (s, None, None)
}

/// Resolve a process handle and a VMO handle of the caller, checking them in the order SYSCALLS.md lists (BAD_HANDLE, WRONG_TYPE,
/// ACCESS_DENIED). Returns the target pid and the VMO id.
/// Is `[vaddr, vaddr + length)` a non-empty range inside the user half? (The range calls act only on regions, and regions are in the user half;
/// this keeps it so if a kernel-half region ever exists.)
fn in_user_range(vaddr: u64, length: u64) -> bool {
    length > 0
        && vaddr >= abi::user::BASE
        && vaddr
            .checked_add(length)
            .is_some_and(|end| end <= abi::user::TOP)
}

/// Where an ANYWHERE mapping of `bytes` goes (MM7): the mapping arena `[MAP_ARENA_BASE, the stack arena)`, at a random page-aligned place: eight
/// random positions are tried, then the first gap that fits from a random start, wrapping once; `None` when nothing fits.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the arena bounds are constants below the canonical limit; the chosen start is below `span`
pub fn place_anywhere(
    random_below: &mut dyn FnMut(u64) -> u64,
    aspace: &AddressSpace,
    bytes: u64,
) -> Option<u64> {
    const LO: u64 = abi::user::MAP_ARENA_BASE;
    let hi = abi::user::STACK_ARENA_BASE;
    if bytes > hi - LO {
        return None;
    }
    let slots = (hi - LO - bytes) / 4096 + 1;
    // eight random positions first; then the first gap that fits from a random start, wrapping once
    for _ in 0..8 {
        let at = LO + random_below(slots) * 4096;
        if !aspace.has_region_in(at, at + bytes) {
            return Some(at);
        }
    }
    let from = LO + random_below(slots) * 4096;
    aspace
        .find_gap(LO, hi, bytes, from)
        .or_else(|| aspace.find_gap(LO, hi, bytes, LO))
}

fn resolve_map_args(
    objs: &Kobjs,
    caller: &Process,
    ph: Handle,
    vh: Handle,
    flags: u64,
) -> Result<(u64, u64, Perms), Status> {
    let pe = caller.handles.get(ph)?;
    let ve = caller.handles.get(vh)?;
    let tpid = match objs.get(pe.obj) {
        Some(ipc::KObj::Process(p)) => p.pid,
        _ => return Err(Status::WrongType),
    };
    let vid = match objs.get(ve.obj) {
        Some(ipc::KObj::Vmo(id)) => *id,
        _ => return Err(Status::WrongType),
    };
    let mut need = Rights(Rights::MAP.0 | Rights::READ.0);
    if flags & PF_W != 0 {
        need = need.union(Rights::WRITE);
    }
    if flags & PF_X != 0 {
        need = need.union(Rights::EXECUTE);
    }
    if !pe.rights.contains(Rights::MANAGE) || !ve.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    // what the mapping may later be raised to: read always, write and execute as the VMO handle's rights allow
    let max = Perms {
        read: true,
        write: ve.rights.contains(Rights::WRITE),
        exec: ve.rights.contains(Rights::EXECUTE),
        user: true,
    };
    Ok((tpid, vid, max))
}

/// Drop what the copy log makes stale in every address space and have every hart drop its translations (COW1): after any call or fault
/// that may have made a page of a VMO family visible or replaced it. One atomic load when nothing is pending.
pub fn cow_sync<M: PhysMem, X: Machine>(
    procs: &mut crate::ProcTable,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    machine: &mut X,
) {
    if !kcore::aspace::COPY_PENDING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let cleared =
        kcore::aspace::invalidate_copies(procs.values_mut().map(|p| p.aspace.held_mut()), mm, vmos);
    if cleared > 0 {
        machine.shootdown();
        machine.flush_tlb();
    }
}

/// A SNAPSHOT moved the pages of VMO `vmo` to a node it shares with the child: take the write permission off every translation of it, and
/// have every hart drop what it cached, before anything runs (COW1; the negative control `fault-skip-cow-protect` leaves it out).
fn snapshot_protect<M: PhysMem, X: Machine>(
    procs: &mut crate::ProcTable,
    mm: &mut Mm<M>,
    machine: &mut X,
    vmo: u64,
) {
    if machine.skip_cow_protect() {
        return;
    }
    let mut changed = 0u64;
    for p in procs.values_mut() {
        changed = changed.saturating_add(p.aspace.held_mut().write_protect_vmo(mm, vmo));
    }
    if changed > 0 {
        machine.shootdown();
        machine.flush_tlb();
    }
}

/// `None`: `n` is not one of these calls.
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
pub fn proc_call<M: PhysMem, X: Machine>(
    n: usize,
    a: [u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Option<Reply> {
    if n == nr::PROCESS_UNMAP {
        return Some(process_unmap_all(c, a));
    }
    if n == nr::PROCESS_PROTECT {
        return Some(process_protect_all(c, n, a));
    }
    if n == nr::VMO_DECOMMIT || n == nr::VMO_SET_SIZE {
        // (the steps the kernel runs with a switch point between them, run here one after another)
        return Some(vmo_range_all(c, n, a));
    }
    let PCtx {
        objs,
        procs,
        me,
        mm,
        vmos,
        jobs,
        machine,
    } = c;
    let me = *me;
    let r: Reply = match n {
        nr::PROCESS_MAP => (|| {
            let (ph, vh) = (Handle(a[0]), Handle(a[1]));
            let (vaddr, offset, length, flags) = (a[2], a[3], a[4], a[5]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (tpid, vid, max_perms) =
                resolve_map_args(objs, caller, ph, vh, flags).map_err(e)?;
            let caller_job = caller.job;
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            target
                .aspace
                .held_mut()
                .set_region_limit(jobs.region_limit(target.job));
            if target.exit_code.is_some() || target.killed.is_some() {
                return bad(Status::BadState);
            }
            let aligned = |x: u64| x.is_multiple_of(4096);
            // ANYWHERE (MM7): the flag asks the kernel to choose a free place (`vaddr` must be 0), at random among those that fit in the
            // mapping arena. Every other argument is checked first (with the arena's start standing in for the address), so a refused
            // call costs no entropy.
            // (HD5: the reserved bits are named and refused, before any other argument is looked at beyond the handles)
            if flags & (abi::map_flags::SHADOW_STACK | abi::map_flags::RESERVED_9) != 0 {
                return bad(Status::NotSupported);
            }
            let anywhere = flags & abi::map_flags::ANYWHERE != 0;
            let flags = flags & !abi::map_flags::ANYWHERE;
            if anywhere && vaddr != 0 {
                return bad(Status::InvalidArgs);
            }
            let probe = if anywhere { abi::user::BASE } else { vaddr };
            let in_user = probe >= abi::user::BASE
                && probe
                    .checked_add(length)
                    .is_some_and(|end| end <= abi::user::TOP);
            let vmo_pages = vmos.pages(vid).ok_or(e(Status::BadHandle))?;
            let inside_vmo = offset
                .checked_add(length)
                .is_some_and(|end| end <= vmo_pages * 4096);
            if !aligned(probe)
                || !aligned(offset)
                || !aligned(length)
                || length == 0
                || flags & !(PF_R | PF_W | PF_X) != 0
                || (flags & PF_W != 0 && flags & PF_X != 0)
                || !in_user
                || !inside_vmo
            {
                return bad(Status::InvalidArgs);
            }
            let vaddr = if anywhere {
                match place_anywhere(
                    &mut |n| machine.random_below(n),
                    target.aspace.held_ref(),
                    length,
                ) {
                    Some(at) => at,
                    None => return bad(Status::NoMemory),
                }
            } else {
                vaddr
            };
            // A VMO charged to a job that is neither the caller's nor the address space owner's must already be committed over the
            // range (slice V, INVARIANTS V1): its pages are paid for, so a touch can neither fail nor charge a stranger, and a
            // process cannot be killed by a VMO it was handed. A parent mapping its own VMO, or one charged to the child's job,
            // into a child stays lazy.
            let vmo_job = vmos.job_of(vid);
            if vmo_job != Some(caller_job)
                && vmo_job != Some(target.job)
                && !vmos.is_committed(vid, offset / 4096, length / 4096)
            {
                return bad(Status::BadState);
            }
            // (a stranger's mapping of pages being taken away would outlive the promise above: wait for the call to end)
            if vmo_job != Some(target.job) && vmos.is_busy(vid) {
                return bad(Status::ShouldWait);
            }
            let perms = Perms {
                read: true,
                write: flags & PF_W != 0,
                exec: flags & PF_X != 0,
                user: true,
            };
            // Page tables for this region, worst case: one leaf table per 512 pages it spans (plus one for each end not aligned to
            // them) and two interior ones.
            let tables = length / 4096 / 512 + 4;
            // (KT9: the room for the charge's records before anything is charged or mapped)
            target.reserve_charge().map_err(e)?;
            jobs.charge(target.job, Resource::Pages, tables)
                .map_err(|x| e(crate::job_status(x)))?;
            if let Err(x) = target.aspace.held_mut().map_region_max(
                vmos,
                vaddr,
                length / 4096,
                vid,
                offset / 4096,
                perms,
                max_perms,
            ) {
                jobs.credit(target.job, Resource::Pages, tables);
                return Err(match x {
                    kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                });
            }
            // (a key that is already there is a mapping that was split and whose start has been mapped again: its charge stays, and this one adds)
            let end = vaddr + length;
            target.add_charge(vaddr, tables, end);
            // a mapping that touches one of the same VMO with the same permissions becomes part of it (fewer regions)
            target
                .aspace
                .held_mut()
                .merge_near(mm, vmos, jobs, vaddr, vaddr + length);
            Ok((anywhere.then_some(vaddr), None))
        })(),
        nr::VMO_CREATE_CHILD => (|| {
            use kcore::aspace::{ChildMode, VmoError};
            let (offset, bytes, job_handle, flags) = (a[1], a[2], a[3], a[4]);
            let mode = match flags {
                abi::child_flags::SNAPSHOT => ChildMode::Snapshot,
                abi::child_flags::PRIVATE => ChildMode::Private,
                _ => return bad(Status::InvalidArgs),
            };
            if !offset.is_multiple_of(4096) || !bytes.is_multiple_of(4096) || bytes == 0 {
                return bad(Status::InvalidArgs);
            }
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let id = ipc::vmo_of(objs, &caller.handles, Handle(a[0]), Rights::READ).map_err(e)?;
            let caller_job = caller.job;
            let job = if job_handle == 0 {
                caller_job
            } else {
                ipc::job_of(objs, &caller.handles, Handle(job_handle), Rights::MANAGE).map_err(e)?
            };
            if mode == ChildMode::Snapshot {
                // the pages never change payer (COW2)
                let vmo_job = vmos.job_of(id).ok_or(e(Status::BadHandle))?;
                if vmo_job != job {
                    return bad(Status::AccessDenied);
                }
                // V1: a process of another job maps the source on the promise that a write never fails on the VMO's job; a SNAPSHOT
                // would make its next write a copy charged to that job
                if vmos.maps_other_job(id, 0, u64::MAX, vmo_job, &|o| procs.get(&o).map(|p| p.job))
                {
                    return bad(Status::BadState);
                }
            }
            // everything that can fail comes before the source changes: the handle's room, the quota, the bookkeeping
            vmos.reserve_n(2).map_err(e)?;
            let child = match vmos.child_prepare(jobs, id, offset / 4096, bytes / 4096, job, mode) {
                Ok(c) => c,
                Err(VmoError::Gone) => return bad(Status::BadHandle),
                Err(VmoError::JobMismatch) => return bad(Status::AccessDenied),
                Err(VmoError::TooDeep) => return bad(Status::BadState),
                Err(VmoError::NotCopyable) => return bad(Status::NotSupported),
                Err(VmoError::NoMemory) => return bad(Status::NoMemory),
                Err(VmoError::Busy) => return bad(Status::ShouldWait),
                Err(_) => return bad(Status::InvalidArgs),
            };
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            let handle = match ipc::vmo_object_create_in(
                objs,
                &mut caller.handles,
                child,
                &mut ipc::Quota {
                    jobs,
                    job: Some(caller_job),
                },
            ) {
                Ok(h) => h,
                Err(x) => {
                    let _ = vmos.release(mm, jobs, child);
                    return Err(e(x));
                }
            };
            vmos.child_attach(jobs, child, id, offset / 4096, bytes / 4096, mode);
            if mode == ChildMode::Snapshot {
                snapshot_protect(procs, mm, &mut **machine, id);
            }
            Ok((Some(handle.0), None))
        })(),
        nr::PROCESS_CLONE_REGION => (|| {
            use kcore::aspace::{ChildMode, VmoError};
            let (vaddr, mode) = (a[2], a[3]);
            if (mode != abi::clone_mode::SHARE && mode != abi::clone_mode::SNAPSHOT)
                || !vaddr.is_multiple_of(4096)
            {
                return bad(Status::InvalidArgs);
            }
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, spid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE).map_err(e)?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[1]), Rights::MANAGE).map_err(e)?;
            let caller_job = caller.job;
            let source = procs.get(&spid).ok_or(e(Status::BadState))?;
            let target = procs.get(&tpid).ok_or(e(Status::BadState))?;
            if source.exit_code.is_some()
                || source.killed.is_some()
                || target.exit_code.is_some()
                || target.killed.is_some()
            {
                return bad(Status::BadState);
            }
            let region = source
                .aspace
                .held_ref()
                .region_at_or_after(vaddr)
                .filter(|r| r.start == vaddr)
                .ok_or(e(Status::InvalidArgs))?;
            let length = region.pages * 4096;
            let end = vaddr.checked_add(length).ok_or(e(Status::InvalidArgs))?;
            if target.aspace.held_ref().has_region_in(vaddr, end) {
                return bad(Status::InvalidArgs);
            }
            // (the mapping below cannot fail for want of a region slot)
            if target.aspace.held_ref().region_count() as u64
                >= jobs
                    .region_limit(target.job)
                    .min(abi::limits::MAX_REGIONS as u64)
            {
                return bad(Status::NoMemory);
            }
            let target_job = target.job;
            let vid = region.vmo;
            let vmo_job = vmos.job_of(vid).ok_or(e(Status::BadHandle))?;
            let snapshot = mode == abi::clone_mode::SNAPSHOT;
            if snapshot {
                // a fork child runs in its parent's job: the copy is charged to the source's job, which must be the target's
                if vmo_job != target_job {
                    return bad(Status::AccessDenied);
                }
                // (V1, as `vmo_create_child`)
                if vmos.maps_other_job(vid, 0, u64::MAX, vmo_job, &|o| procs.get(&o).map(|p| p.job))
                {
                    return bad(Status::BadState);
                }
            } else if vmo_job != caller_job
                && vmo_job != target_job
                && !vmos.is_committed(vid, region.vmo_page, region.pages)
            {
                // (V1, as `process_map`: a stranger's VMO must already be paid for)
                return bad(Status::BadState);
            }
            if vmo_job != target_job && vmos.is_busy(vid) {
                return bad(Status::ShouldWait);
            }
            let tables = region.pages / 512 + 4;
            jobs.charge(target_job, Resource::Pages, tables)
                .map_err(|x| e(crate::job_status(x)))?;
            let undo = |jobs: &mut Jobs| jobs.credit(target_job, Resource::Pages, tables);
            let (map_vmo, map_page, child) = if snapshot {
                if let Err(x) = vmos.reserve_n(2) {
                    undo(jobs);
                    return bad(x);
                }
                match vmos.child_prepare(
                    jobs,
                    vid,
                    region.vmo_page,
                    region.pages,
                    vmo_job,
                    ChildMode::Snapshot,
                ) {
                    Ok(c) => (c, 0, Some(c)),
                    Err(x) => {
                        undo(jobs);
                        return bad(match x {
                            VmoError::TooDeep => Status::BadState,
                            VmoError::JobMismatch => Status::AccessDenied,
                            VmoError::Gone => Status::BadHandle,
                            VmoError::NotCopyable => Status::NotSupported,
                            VmoError::Busy => Status::ShouldWait,
                            VmoError::NoMemory => Status::NoMemory,
                            _ => Status::InvalidArgs,
                        });
                    }
                }
            } else {
                (vid, region.vmo_page, None)
            };
            let target = procs.get_mut(&tpid).ok_or(e(Status::BadState))?;
            target
                .aspace
                .held_mut()
                .set_region_limit(jobs.region_limit(target.job));
            // (KT9: the room for the charge's records before the mapping; with none the call undoes what it did and says NO_MEMORY)
            if let Err(x) = target.reserve_charge() {
                if let Some(c) = child {
                    let _ = vmos.release(mm, jobs, c);
                }
                undo(jobs);
                return Err(e(x));
            }
            if let Err(x) = target.aspace.held_mut().map_region_max(
                vmos,
                vaddr,
                region.pages,
                map_vmo,
                map_page,
                region.perms,
                region.max_perms,
            ) {
                if let Some(c) = child {
                    let _ = vmos.release(mm, jobs, c);
                }
                undo(jobs);
                return Err(match x {
                    kcore::aspace::RegionError::TooMany => e(Status::NoMemory),
                    _ => e(Status::InvalidArgs),
                });
            }
            target.add_charge(vaddr, tables, end);
            target
                .aspace
                .held_mut()
                .merge_near(mm, vmos, jobs, vaddr, end);
            if let Some(c) = child {
                // the child is part of the family only now (nothing could fail since `child_prepare`); the region holds its own reference
                vmos.child_attach(
                    jobs,
                    c,
                    vid,
                    region.vmo_page,
                    region.pages,
                    ChildMode::Snapshot,
                );
                let _ = vmos.release(mm, jobs, c);
                snapshot_protect(procs, mm, &mut **machine, vid);
            }
            Ok((None, None))
        })(),
        nr::PROCESS_MAP_INFO => (|| {
            let (vaddr, buf) = (a[1], a[2]);
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, tpid, _, _) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            let target = procs.get(&tpid).ok_or(e(Status::BadState))?;
            let bits =
                |p: Perms| u64::from(p.read) | u64::from(p.write) << 1 | u64::from(p.exec) << 2;
            // the region at or after `vaddr`; none: a zeroed record and 0
            let (record, next) = match target.aspace.held_ref().region_at_or_after(vaddr) {
                Some(r) => {
                    let len = r.pages * 4096;
                    let next = target
                        .aspace
                        .held_ref()
                        .region_at_or_after(r.start + len)
                        .map_or(0, |n| n.start);
                    (
                        [
                            r.start,
                            len,
                            bits(r.perms),
                            bits(r.max_perms),
                            r.vmo,
                            r.vmo_page * 4096,
                        ],
                        next,
                    )
                }
                None => ([0; 6], 0),
            };
            let mut words = [[0u8; 8]; 6];
            for (out, w) in words.iter_mut().zip(record.iter()) {
                *out = w.to_le_bytes();
            }
            let bytes: [u8; 48] = words.concat().try_into().unwrap_or([0; 48]);
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            kcore::usercopy::copy_to_user(caller.aspace.held_mut(), mm, vmos, jobs, buf, &bytes)
                .map_err(|_| e(Status::InvalidArgs))?;
            Ok((Some(next), None))
        })(),
        // `process_info(process, which)`: 0 the exit code of an ended process (BAD_STATE while it runs), 1 the CPU nanoseconds its threads have
        // run, ended and live (needs INSPECT, works while it runs; AB6)
        nr::PROCESS_INFO if a[1] == 1 => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (pid, terminated, ended) =
                ipc::process_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)
                    .map_err(e)?;
            let live = if terminated {
                0
            } else {
                procs.get(&pid).map_or(0, |p| {
                    p.live_tids
                        .iter()
                        .fold(0u64, |s, &t| s.saturating_add(machine.thread_cpu_ns(t)))
                })
            };
            Ok((Some(ended.saturating_add(live)), None))
        })(),
        nr::PROCESS_INFO if a[1] != 0 => fail(Status::InvalidArgs),
        nr::PROCESS_INFO => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (_, _, terminated, code) =
                ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            if !terminated {
                return bad(Status::BadState);
            }
            Ok((Some(code as i64 as u64), None))
        })(),
        // `thread_info(thread, which)`: 0 the exit code of an ended thread, 1 its CPU nanoseconds (works while it runs; AB6)
        // (handle 0 names the calling thread, as in `thread_set_sched`: it needs no right)
        nr::THREAD_INFO if a[1] == 1 && a[0] == 0 => {
            Ok((Some(machine.current_thread_cpu_ns()), None))
        }
        nr::THREAD_INFO if a[1] == 1 => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (obj, pid, terminated, final_ns) =
                ipc::thread_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)
                    .map_err(e)?;
            if terminated {
                return Ok((Some(final_ns), None));
            }
            let tid = match objs.get(obj) {
                Some(ipc::KObj::Thread(t)) => t.tid,
                _ => None,
            }
            .filter(|t| procs.get(&pid).is_some_and(|p| p.live_tids.contains(t)));
            Ok((Some(tid.map_or(0, |t| machine.thread_cpu_ns(t))), None))
        })(),
        nr::THREAD_INFO if a[1] != 0 => fail(Status::InvalidArgs),
        nr::THREAD_INFO => (|| {
            let caller = procs.get(&me).ok_or(e(Status::BadState))?;
            let (terminated, code) =
                ipc::thread_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT).map_err(e)?;
            if !terminated {
                return bad(Status::BadState);
            }
            Ok((Some(code as i64 as u64), None))
        })(),
        nr::RESOURCE_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            match ipc::resource_object_create_in(
                objs,
                &mut caller.handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => Err(e(x)),
            }
        })(),
        nr::DEBUG_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            match ipc::debug_object_create_in(
                objs,
                &mut caller.handles,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => Err(e(x)),
            }
        })(),
        nr::JOB_ROOT => (|| {
            let caller = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            if !caller.privileged {
                return bad(Status::AccessDenied);
            }
            let job = caller.job;
            jobs.retain(JobId::ROOT);
            match ipc::job_object_create(
                objs,
                &mut caller.handles,
                JobId::ROOT,
                Rights::JOB,
                &mut ipc::Quota {
                    jobs,
                    job: Some(job),
                },
            ) {
                Ok(h) => Ok((Some(h.0), None)),
                Err(x) => {
                    jobs.release(JobId::ROOT);
                    Err(e(x))
                }
            }
        })(),
        nr::PROCESS_SELF => (|| {
            let p = procs.get_mut(&me).ok_or(e(Status::BadState))?;
            let rights =
                Rights(Rights::MANAGE.0 | Rights::INSPECT.0 | Rights::WAIT.0 | Rights::DUPLICATE.0);
            if p.handles.room() < 1 || p.handles.reserve(1).is_err() {
                return bad(Status::NoMemory);
            }
            // A process its parent created has a kill-on-close object (the parent's handle to it); the child's own view must not be
            // another reference to *that* object, or the parent closing its handle would never destroy it and the child would not
            // be killed (review 2026-11-14). Such a child gets a separate view object; the view is only ever held by the child.
            let own = p.obj.filter(|&o| objs.is_alive(o));
            let kill_on_close = matches!(own.and_then(|o| objs.get(o)), Some(ipc::KObj::Process(po)) if po.kill_on_close);
            match own.filter(|_| !kill_on_close) {
                Some(obj) => {
                    // another view of the same process object
                    objs.retain(obj);
                    let h = p
                        .handles
                        .insert(objs, obj, rights)
                        .expect("room was checked");
                    Ok((Some(h.0), None))
                }
                None => {
                    let job = p.job;
                    let (h, obj) = ipc::process_object_create_in(
                        objs,
                        &mut p.handles,
                        me,
                        false,
                        &mut ipc::Quota {
                            jobs,
                            job: Some(job),
                        },
                    )
                    .map_err(e)?;
                    // narrow the default rights: a self handle cannot be sent away (a table whose last free slot is on its last
                    // generation refuses the replace: give the new object back)
                    let Ok(h) = p.handles.replace(objs, h, rights) else {
                        let _ = p.handles.close(objs, h);
                        return bad(Status::NoMemory);
                    };
                    if own.is_none() {
                        p.obj = Some(obj);
                    }
                    Ok((Some(h.0), None))
                }
            }
        })(),
        _ => return None,
    };
    cow_sync(procs, mm, vmos, &mut **machine);
    Some(r)
}
