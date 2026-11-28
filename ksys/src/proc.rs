//! Processes and the calls that act across them (TS3b): the process record, and `process_map`, `process_unmap`, `process_self`,
//! `process_info`, `thread_info`, `job_root` and `resource_root` over the process table, with the machine's part (TLB shootdowns) behind
//! a small trait.

use crate::Reply;
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
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under (the boot loader's and the host tests' records)
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
    pub handles: kcore::fbox::FBox<HandleTable>,
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

/// The pages `vmo_range_begin` takes away from VMO `id` (the first, how many) and, for `vmo_set_size`, the new size, checked. A frame of its own
/// (KT13).
#[inline(never)]
fn range_window(
    vmos: &Vmos,
    id: u64,
    n: usize,
    a: &[u64; 6],
) -> Result<(u64, u64, Option<u64>), Status> {
    let size = vmos.pages(id).ok_or(Status::BadHandle)?;
    if n == nr::VMO_DECOMMIT {
        if vmos.kind(id) != Some(kcore::aspace::VmoKind::Plain) {
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
        Ok((first, count, None))
    } else {
        // (a VMO that is not resizable, which every device and DMA VMO is, is refused before the size is looked at)
        if !vmos.is_resizable(id) {
            return Err(Status::AccessDenied);
        }
        let bytes = a[1];
        if bytes == 0 || !bytes.is_multiple_of(4096) || bytes > abi::user::TOP {
            return Err(Status::InvalidArgs);
        }
        let new = bytes / 4096;
        // growing takes nothing away; shrinking takes the tail
        Ok((new.min(size), size.saturating_sub(new), Some(new)))
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
    let (first, count, new_size) = range_window(c.vmos, id, n, &a)?;
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

impl RangeBatch {
    /// An empty batch, for `vmo_range_take` to fill (the caller keeps it in one place: its frames are half a KiB, KT13). A constant, not a
    /// function: nothing builds one in a frame at run time.
    pub const EMPTY: RangeBatch = RangeBatch {
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

    /// Make the batch empty again, in place (as `EMPTY`).
    fn clear(&mut self) {
        self.frames.fill(0);
        (self.lo, self.next, self.n, self.done, self.err) = (0, 0, 0, false, None);
        (self.bound, self.cursor, self.cleared) = (0, 0, 0);
    }
}

/// A: take the next batch out of the VMO's index into `b` (whatever it held before is replaced). Needs the memory locks only.
pub fn vmo_range_take<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    call: RangeCall,
    b: &mut RangeBatch,
) -> Result<(), Status> {
    let PCtx { vmos, jobs, .. } = c;
    b.clear();
    let st = vmos
        .range_step(jobs, call.id, &mut b.frames)
        .map_err(range_status)?;
    (b.lo, b.next, b.n, b.done, b.err) = (st.lo, st.next, st.n, st.done, st.err);
    b.bound = vmos.max_mapper(call.id);
    Ok(())
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
    n: usize,
    call: RangeCall,
    batch: &mut RangeBatch,
    owner: u64,
) {
    let PCtx {
        procs,
        mm,
        vmos,
        machine,
        ..
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
    if cleared > 0 {
        // the translations go from every hart before the locks drop (SMP5): a sibling that faults the page in meanwhile must not leave another hart
        // writing the frame this clear took away
        if !machine.skip_shootdown(n) {
            machine.shootdown();
        }
        machine.flush_tlb();
    }
}

/// C: the VMO gives the frames back (B shot the translations down in each section, so no hart caches one: TLB2). Returns
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
    let _ = (n, &machine);
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
    let mut batch = RangeBatch::EMPTY;
    vmo_range_take(c, call, &mut batch)?;
    while let Some(owner) = vmo_range_next_owner(c, call, &batch) {
        vmo_range_unmap_owner(c, n, call, &mut batch, owner);
    }
    vmo_range_finish(c, n, call, &mut batch)
}

/// `vmo_decommit` / `vmo_set_size` without a switch point between the steps (the host tests, where nothing else runs).
#[inline(never)] // (KT13: its batch and cursors would otherwise sit in the frame of `proc_call`, and so on the stack of every call)
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

/// The process a call on `a0` (a process handle with MANAGE) acts on: its id. A frame of its own (KT13).
fn managed_target(
    objs: &kcore::ipc::Kobjs,
    procs: &ProcTable,
    me: u64,
    a0: u64,
) -> Result<u64, Status> {
    target_with(objs, procs, me, a0, Rights::MANAGE)
}

/// The process the caller's handle `a0` names, which must carry `rights`: its id. A frame of its own (KT13).
#[inline(never)]
fn target_with(
    objs: &kcore::ipc::Kobjs,
    procs: &ProcTable,
    me: u64,
    a0: u64,
    rights: Rights,
) -> Result<u64, Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (_, tpid, _, _) = ipc::process_of(objs, &caller.handles, Handle(a0), rights)?;
    Ok(tpid)
}

/// The target of an unmap or a protect, before its regions change: its space takes its job's region limit (PL8), and a process that is ending is
/// BAD_STATE. A frame of its own (KT13).
#[inline(never)]
fn ready_target(procs: &mut ProcTable, jobs: &Jobs, tpid: u64) -> Result<(), Status> {
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    target
        .aspace
        .held_mut()
        .set_region_limit(jobs.region_limit(target.job));
    if target.exit_code.is_some() || target.killed.is_some() {
        return Err(Status::BadState);
    }
    Ok(())
}

/// `process_unmap`, first part: resolve and check the arguments, split the regions at the edges and mark the ones inside dying. Every failure is
/// here, before the first page goes.
pub fn process_unmap_begin<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    a: [u64; 6],
) -> Result<UnmapCall, Status> {
    let tpid = managed_target(c.objs, c.procs, c.me, a[0])?;
    ready_target(c.procs, c.jobs, tpid)?;
    let PCtx { procs, vmos, .. } = c;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
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
#[inline(never)] // (KT13: its batch and cursors would otherwise sit in the frame of `proc_call`, and so on the stack of every call)
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
    let (vaddr, length, flags) = (a[1], a[2], a[3]);
    let tpid = managed_target(c.objs, c.procs, c.me, a[0])?;
    if flags & !(PF_R | PF_W | PF_X) != 0
        || flags & PF_W != 0 && flags & PF_X != 0
        || !in_user_range(vaddr, length)
    {
        return Err(Status::InvalidArgs);
    }
    ready_target(c.procs, c.jobs, tpid)?;
    let PCtx {
        procs,
        mm,
        vmos,
        jobs,
        ..
    } = c;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
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

#[inline(never)] // (KT13: its batch and cursors would otherwise sit in the frame of `proc_call`, and so on the stack of every call)
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

/// `None`: `n` is not one of these calls. Each call is a frame of its own (KT13): this one only routes.
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
    proc_call_in(n, &a, c)
}

/// `proc_call`'s calls that run in one section (a frame of its own: KT13).
#[inline(never)]
fn proc_call_in<M: PhysMem, X: Machine>(
    n: usize,
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Option<Reply> {
    let r = match n {
        nr::PROCESS_MAP => process_map(a, c),
        nr::VMO_CREATE_CHILD => vmo_create_child(a, c),
        nr::PROCESS_CLONE_REGION => process_clone_region(a, c),
        nr::PROCESS_MAP_INFO => process_map_info(a, c),
        // `process_info(process, which)`: 0 the exit code of an ended process (BAD_STATE while it runs), 1 the CPU nanoseconds its threads have
        // run, ended and live (needs INSPECT, works while it runs; AB6)
        nr::PROCESS_INFO if a[1] == 1 => process_cpu(a, c),
        nr::PROCESS_INFO if a[1] != 0 => Err(Status::InvalidArgs),
        nr::PROCESS_INFO => process_info(a, c),
        // `thread_info(thread, which)`: 0 the exit code of an ended thread, 1 its CPU nanoseconds (works while it runs; AB6)
        // (handle 0 names the calling thread, as in `thread_set_sched`: it needs no right)
        nr::THREAD_INFO if a[1] == 1 && a[0] == 0 => {
            Ok((Some(c.machine.current_thread_cpu_ns()), None))
        }
        nr::THREAD_INFO if a[1] == 1 => thread_cpu(a, c),
        nr::THREAD_INFO if a[1] != 0 => Err(Status::InvalidArgs),
        nr::THREAD_INFO => thread_info(a, c),
        nr::RESOURCE_ROOT => resource_root(c),
        nr::DEBUG_ROOT => debug_root(c),
        nr::JOB_ROOT => job_root(c),
        nr::PROCESS_SELF => process_self(c),
        _ => return None,
    };

    cow_sync(c.procs, c.mm, c.vmos, &mut *c.machine);
    Some(r.map_err(e))
}

/// `proc_call`'s `nr::PROCESS_MAP` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_map<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let (tpid, vid, max_perms, caller_job) = map_handles(c.objs, c.procs, c.me, a)?;
    ready_target(c.procs, c.jobs, tpid)?;
    let (anywhere, perms) = map_flags(c.vmos, vid, a)?;
    let PCtx {
        procs,
        mm,
        vmos,
        jobs,
        machine,
        ..
    } = c;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    let length = a[4];
    let vaddr = if anywhere {
        map_place(target, &mut **machine, length)?
    } else {
        a[2]
    };
    map_commit(
        target,
        mm,
        vmos,
        jobs,
        caller_job,
        (vid, vaddr, a[3], length),
        (perms, max_perms),
    )?;
    Ok((anywhere.then_some(vaddr), None))
}

/// `process_map`'s handles: the target process, the VMO and the most the mapping may be raised to, and the caller's job.
#[inline(never)]
fn map_handles(
    objs: &Kobjs,
    procs: &ProcTable,
    me: u64,
    a: &[u64; 6],
) -> Result<(u64, u64, Perms, JobId), Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (tpid, vid, max_perms) = resolve_map_args(objs, caller, Handle(a[0]), Handle(a[1]), a[5])?;
    Ok((tpid, vid, max_perms, caller.job))
}

/// Where an ANYWHERE mapping of `length` bytes goes in `target` (at random among the places that fit in the mapping arena: MM7). NO_MEMORY:
/// none fits.
#[inline(never)]
fn map_place<X: Machine>(target: &Process, machine: &mut X, length: u64) -> Result<u64, Status> {
    place_anywhere(
        &mut |n| machine.random_below(n),
        target.aspace.held_ref(),
        length,
    )
    .ok_or(Status::NoMemory)
}

/// `process_map`'s checks of its flags and its range (after the handles and the target): whether the kernel places the mapping (ANYWHERE), and
/// its permissions. A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the VMO's size in bytes is below the user limit
#[inline(never)]
fn map_flags(vmos: &Vmos, vid: u64, a: &[u64; 6]) -> Result<(bool, Perms), Status> {
    let (vaddr, offset, length, flags) = (a[2], a[3], a[4], a[5]);
    let aligned = |x: u64| x.is_multiple_of(4096);
    // ANYWHERE (MM7): the flag asks the kernel to choose a free place (`vaddr` must be 0), at random among those that fit in the
    // mapping arena. Every other argument is checked first (with the arena's start standing in for the address), so a refused
    // call costs no entropy.
    // (HD5: the reserved bits are named and refused, before any other argument is looked at beyond the handles)
    if flags & (abi::map_flags::SHADOW_STACK | abi::map_flags::RESERVED_9) != 0 {
        return Err(Status::NotSupported);
    }
    let anywhere = flags & abi::map_flags::ANYWHERE != 0;
    let flags = flags & !abi::map_flags::ANYWHERE;
    if anywhere && vaddr != 0 {
        return Err(Status::InvalidArgs);
    }
    let probe = if anywhere { abi::user::BASE } else { vaddr };
    let in_user = probe >= abi::user::BASE
        && probe
            .checked_add(length)
            .is_some_and(|end| end <= abi::user::TOP);
    let vmo_pages = vmos.pages(vid).ok_or(Status::BadHandle)?;
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
        return Err(Status::InvalidArgs);
    }
    Ok((
        anywhere,
        Perms {
            read: true,
            write: flags & PF_W != 0,
            exec: flags & PF_X != 0,
            user: true,
        },
    ))
}

/// `process_map`'s mapping of `length` bytes of VMO `vid` from `offset` at `vaddr` in `target`, with the page-table charge, after its last checks
/// (V1: a stranger's VMO is committed and not being shrunk). A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: vaddr + length and offset + length were checked against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4
#[inline(never)]
fn map_commit<M: PhysMem>(
    target: &mut Process,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    caller_job: JobId,
    (vid, vaddr, offset, length): (u64, u64, u64, u64),
    (perms, max_perms): (Perms, Perms),
) -> Result<(), Status> {
    // A VMO charged to a job that is neither the caller's nor the address space owner's must already be committed over the
    // range (slice V, INVARIANTS V1): its pages are paid for, so a touch can neither fail nor charge a stranger, and a
    // process cannot be killed by a VMO it was handed. A parent mapping its own VMO, or one charged to the child's job,
    // into a child stays lazy.
    let vmo_job = vmos.job_of(vid);
    if vmo_job != Some(caller_job)
        && vmo_job != Some(target.job)
        && !vmos.is_committed(vid, offset / 4096, length / 4096)
    {
        return Err(Status::BadState);
    }
    // (a stranger's mapping of pages being taken away would outlive the promise above: wait for the call to end)
    if vmo_job != Some(target.job) && vmos.is_busy(vid) {
        return Err(Status::ShouldWait);
    }
    // Page tables for this region, worst case: one leaf table per 512 pages it spans (plus one for each end not aligned to
    // them) and two interior ones.
    let tables = length / 4096 / 512 + 4;
    // (KT9: the room for the charge's records before anything is charged or mapped)
    target.reserve_charge()?;
    jobs.charge(target.job, Resource::Pages, tables)
        .map_err(crate::job_status)?;
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
            kcore::aspace::RegionError::TooMany => Status::NoMemory,
            _ => Status::InvalidArgs,
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
    Ok(())
}

/// `proc_call`'s `nr::VMO_CREATE_CHILD` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn vmo_create_child<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    use kcore::aspace::{ChildMode, VmoError};
    let (offset, bytes, job_handle, flags) = (a[1], a[2], a[3], a[4]);
    let mode = match flags {
        abi::child_flags::SNAPSHOT => ChildMode::Snapshot,
        abi::child_flags::PRIVATE => ChildMode::Private,
        _ => return Err(Status::InvalidArgs),
    };
    if !offset.is_multiple_of(4096) || !bytes.is_multiple_of(4096) || bytes == 0 {
        return Err(Status::InvalidArgs);
    }
    let (id, job, caller_job) =
        child_source(c.objs, c.procs, c.vmos, c.me, (a[0], job_handle), mode)?;
    // everything that can fail comes before the source changes: the handle's room, the quota, the bookkeeping
    c.vmos.reserve_n(2)?;
    let child = c
        .vmos
        .child_prepare(c.jobs, id, offset / 4096, bytes / 4096, job, mode)
        .map_err(|x| match x {
            VmoError::Gone => Status::BadHandle,
            VmoError::JobMismatch => Status::AccessDenied,
            VmoError::TooDeep => Status::BadState,
            VmoError::NotCopyable => Status::NotSupported,
            VmoError::NoMemory => Status::NoMemory,
            VmoError::Busy => Status::ShouldWait,
            _ => Status::InvalidArgs,
        })?;
    let handle = child_publish(
        c,
        caller_job,
        child,
        (id, offset / 4096, bytes / 4096),
        mode,
    )?;
    Ok((Some(handle), None))
}

/// The end of `vmo_create_child`: the child's object and the caller's handle to it (without them the child goes again), then the child attached
/// to its source (nothing can fail since `child_prepare`), and a SNAPSHOT's source write-protected (COW1). A frame of its own (KT13).
#[inline(never)]
fn child_publish<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    caller_job: JobId,
    child: u64,
    (id, first, pages): (u64, u64, u64),
    mode: kcore::aspace::ChildMode,
) -> Result<u64, Status> {
    let PCtx {
        objs,
        procs,
        mm,
        vmos,
        jobs,
        machine,
        me,
    } = c;
    let caller = procs.get_mut(me).ok_or(Status::BadState)?;
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
            return Err(x);
        }
    };
    vmos.child_attach(jobs, child, id, first, pages, mode);
    if mode == kcore::aspace::ChildMode::Snapshot {
        snapshot_protect(procs, mm, &mut **machine, id);
    }
    Ok(handle.0)
}

/// `vmo_create_child`'s source VMO (READ) and the job the child is charged to (MANAGE; the caller's own for handle 0), with the caller's job. A
/// SNAPSHOT's pages never change payer (COW2), and are refused while a process of another job maps the source (V1: that mapping was made on the
/// promise that a write never fails on the VMO's job; a SNAPSHOT would make its next write a copy charged to that job). A frame of its own (KT13).
#[inline(never)]
fn child_source(
    objs: &Kobjs,
    procs: &ProcTable,
    vmos: &Vmos,
    me: u64,
    (vh, job_handle): (u64, u64),
    mode: kcore::aspace::ChildMode,
) -> Result<(u64, JobId, JobId), Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let id = ipc::vmo_of(objs, &caller.handles, Handle(vh), Rights::READ)?;
    let caller_job = caller.job;
    let job = if job_handle == 0 {
        caller_job
    } else {
        ipc::job_of(objs, &caller.handles, Handle(job_handle), Rights::MANAGE)?
    };
    if mode == kcore::aspace::ChildMode::Snapshot {
        let vmo_job = vmos.job_of(id).ok_or(Status::BadHandle)?;
        if vmo_job != job {
            return Err(Status::AccessDenied);
        }
        if vmos.maps_other_job(id, 0, u64::MAX, vmo_job, &|o| procs.get(&o).map(|p| p.job)) {
            return Err(Status::BadState);
        }
    }
    Ok((id, job, caller_job))
}

/// `proc_call`'s `nr::PROCESS_CLONE_REGION` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_clone_region<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let (vaddr, mode) = (a[2], a[3]);
    if (mode != abi::clone_mode::SHARE && mode != abi::clone_mode::SNAPSHOT)
        || !vaddr.is_multiple_of(4096)
    {
        return Err(Status::InvalidArgs);
    }
    let snapshot = mode == abi::clone_mode::SNAPSHOT;
    let cl = clone_check(c.objs, c.procs, c.vmos, c.jobs, c.me, a, snapshot)?;
    clone_do(c, &cl)?;
    Ok((None, None))
}

/// A `process_clone_region` call, checked (`clone_check`).
struct CloneCall {
    tpid: u64,
    vaddr: u64,
    region: kcore::aspace::RegionInfo,
    end: u64,
    vmo_job: JobId,
    target_job: JobId,
    snapshot: bool,
}

/// `process_clone_region` after its checks: the page-table charge, the VMO to map (`clone_source`), the mapping (`clone_map`) and a SNAPSHOT
/// child's attach (`clone_attach`). A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: the page-table estimate is pages / 512 + 4
#[inline(never)]
fn clone_do<M: PhysMem, X: Machine>(c: &mut PCtx<'_, M, X>, cl: &CloneCall) -> Result<(), Status> {
    let tables = cl.region.pages / 512 + 4;
    c.jobs
        .charge(cl.target_job, Resource::Pages, tables)
        .map_err(crate::job_status)?;
    let (map_vmo, map_page, child) =
        match clone_source(c.vmos, c.jobs, &cl.region, cl.vmo_job, cl.snapshot) {
            Ok(x) => x,
            Err(x) => {
                c.jobs.credit(cl.target_job, Resource::Pages, tables);
                return Err(x);
            }
        };
    clone_map(
        c,
        cl.tpid,
        &cl.region,
        (cl.vaddr, cl.end, tables, cl.target_job),
        (map_vmo, map_page, child),
    )?;
    if let Some(ch) = child {
        clone_attach(c, ch, &cl.region);
    }
    Ok(())
}

/// A SNAPSHOT clone's child joins the family only now (nothing could fail since `child_prepare`); the region holds its own reference, and the
/// source is write-protected (COW1).
#[inline(never)]
fn clone_attach<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    ch: u64,
    region: &kcore::aspace::RegionInfo,
) {
    let PCtx {
        procs,
        mm,
        vmos,
        jobs,
        machine,
        ..
    } = c;
    vmos.child_attach(
        jobs,
        ch,
        region.vmo,
        region.vmo_page,
        region.pages,
        kcore::aspace::ChildMode::Snapshot,
    );
    let _ = vmos.release(mm, jobs, ch);
    snapshot_protect(procs, mm, &mut **machine, region.vmo);
}

/// `process_clone_region`'s checks, before anything is charged: the source's region that starts at `a[2]` (the target has no region in its
/// range and room for one more), the VMO's job and the target's (V1, as `vmo_create_child` and `process_map`). The target, the region, its end,
/// and the two jobs. A frame of its own (KT13).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's page count was checked when it was mapped
#[inline(never)]
fn clone_check(
    objs: &Kobjs,
    procs: &ProcTable,
    vmos: &Vmos,
    jobs: &Jobs,
    me: u64,
    a: &[u64; 6],
    snapshot: bool,
) -> Result<CloneCall, Status> {
    let vaddr = a[2];
    let (spid, tpid, caller_job) = clone_procs(objs, procs, me, a)?;
    let (region, end, target_job) = clone_region_of(procs, jobs, (spid, tpid), vaddr)?;
    let vmo_job = clone_v1(procs, vmos, &region, (caller_job, target_job), snapshot)?;
    Ok(CloneCall {
        tpid,
        vaddr,
        region,
        end,
        vmo_job,
        target_job,
        snapshot,
    })
}

/// The source's region that starts at `vaddr` (both processes running; the target has no region in its range and room for one more): the
/// region, its end, and the target's job.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's page count was checked when it was mapped
#[inline(never)]
fn clone_region_of(
    procs: &ProcTable,
    jobs: &Jobs,
    (spid, tpid): (u64, u64),
    vaddr: u64,
) -> Result<(kcore::aspace::RegionInfo, u64, JobId), Status> {
    let source = procs.get(&spid).ok_or(Status::BadState)?;
    let target = procs.get(&tpid).ok_or(Status::BadState)?;
    if source.exit_code.is_some()
        || source.killed.is_some()
        || target.exit_code.is_some()
        || target.killed.is_some()
    {
        return Err(Status::BadState);
    }
    let region = source
        .aspace
        .held_ref()
        .region_at_or_after(vaddr)
        .filter(|r| r.start == vaddr)
        .ok_or(Status::InvalidArgs)?;
    let end = target_room(target, jobs, vaddr, region.pages)?;
    Ok((region, end, target.job))
}

/// The target of a clone of `pages` pages at `vaddr`: no region of its own in the range, and room for one more (so that the mapping cannot fail
/// for want of a region slot). The range's end.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's page count was checked when it was mapped
#[inline(never)]
fn target_room(target: &Process, jobs: &Jobs, vaddr: u64, pages: u64) -> Result<u64, Status> {
    let end = vaddr.checked_add(pages * 4096).ok_or(Status::InvalidArgs)?;
    if target.aspace.held_ref().has_region_in(vaddr, end) {
        return Err(Status::InvalidArgs);
    }
    if target.aspace.held_ref().region_count() as u64
        >= jobs
            .region_limit(target.job)
            .min(abi::limits::MAX_REGIONS as u64)
    {
        return Err(Status::NoMemory);
    }
    Ok(end)
}

/// The source and the target of `process_clone_region` (both handles need MANAGE), and the caller's job.
#[inline(never)]
fn clone_procs(
    objs: &Kobjs,
    procs: &ProcTable,
    me: u64,
    a: &[u64; 6],
) -> Result<(u64, u64, JobId), Status> {
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (_, spid, _, _) = ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::MANAGE)?;
    let (_, tpid, _, _) = ipc::process_of(objs, &caller.handles, Handle(a[1]), Rights::MANAGE)?;
    Ok((spid, tpid, caller.job))
}

/// `clone_check`'s V1 checks of the region's VMO (as `vmo_create_child` for a SNAPSHOT, as `process_map` for a SHARE): the VMO's job.
#[inline(never)]
fn clone_v1(
    procs: &ProcTable,
    vmos: &Vmos,
    region: &kcore::aspace::RegionInfo,
    (caller_job, target_job): (JobId, JobId),
    snapshot: bool,
) -> Result<JobId, Status> {
    let vid = region.vmo;
    let vmo_job = vmos.job_of(vid).ok_or(Status::BadHandle)?;
    if snapshot {
        // a fork child runs in its parent's job: the copy is charged to the source's job, which must be the target's
        if vmo_job != target_job {
            return Err(Status::AccessDenied);
        }
        // (V1, as `vmo_create_child`)
        if vmos.maps_other_job(vid, 0, u64::MAX, vmo_job, &|o| procs.get(&o).map(|p| p.job)) {
            return Err(Status::BadState);
        }
    } else if vmo_job != caller_job
        && vmo_job != target_job
        && !vmos.is_committed(vid, region.vmo_page, region.pages)
    {
        // (V1, as `process_map`: a stranger's VMO must already be paid for)
        return Err(Status::BadState);
    }
    if vmo_job != target_job && vmos.is_busy(vid) {
        return Err(Status::ShouldWait);
    }
    Ok(vmo_job)
}

/// What `process_clone_region` maps: the source's VMO window as it is (SHARE), or a SNAPSHOT child of it, prepared (not yet attached: the
/// call can still fail). (The VMO, its first page, and the child.) A frame of its own (KT13).
#[inline(never)]
fn clone_source(
    vmos: &mut Vmos,
    jobs: &mut Jobs,
    region: &kcore::aspace::RegionInfo,
    vmo_job: JobId,
    snapshot: bool,
) -> Result<(u64, u64, Option<u64>), Status> {
    use kcore::aspace::VmoError;
    if !snapshot {
        return Ok((region.vmo, region.vmo_page, None));
    }
    vmos.reserve_n(2)?;
    match vmos.child_prepare(
        jobs,
        region.vmo,
        region.vmo_page,
        region.pages,
        vmo_job,
        kcore::aspace::ChildMode::Snapshot,
    ) {
        Ok(c) => Ok((c, 0, Some(c))),
        Err(x) => Err(match x {
            VmoError::TooDeep => Status::BadState,
            VmoError::JobMismatch => Status::AccessDenied,
            VmoError::Gone => Status::BadHandle,
            VmoError::NotCopyable => Status::NotSupported,
            VmoError::Busy => Status::ShouldWait,
            VmoError::NoMemory => Status::NoMemory,
            _ => Status::InvalidArgs,
        }),
    }
}

/// `process_clone_region`'s mapping in the target, with the page-table charge's records (KT9: their room before the mapping); on a failure the
/// child (if any) goes and the charge is credited. A frame of its own (KT13).
#[inline(never)]
fn clone_map<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
    tpid: u64,
    region: &kcore::aspace::RegionInfo,
    (vaddr, end, tables, target_job): (u64, u64, u64, JobId),
    (map_vmo, map_page, child): (u64, u64, Option<u64>),
) -> Result<(), Status> {
    let PCtx {
        procs,
        mm,
        vmos,
        jobs,
        ..
    } = c;
    let target = procs.get_mut(&tpid).ok_or(Status::BadState)?;
    target
        .aspace
        .held_mut()
        .set_region_limit(jobs.region_limit(target.job));
    let undo = |mm: &mut Mm<M>, vmos: &mut Vmos, jobs: &mut Jobs| {
        if let Some(c) = child {
            let _ = vmos.release(mm, jobs, c);
        }
        jobs.credit(target_job, Resource::Pages, tables);
    };
    if let Err(x) = target.reserve_charge() {
        undo(mm, vmos, jobs);
        return Err(x);
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
        undo(mm, vmos, jobs);
        return Err(match x {
            kcore::aspace::RegionError::TooMany => Status::NoMemory,
            _ => Status::InvalidArgs,
        });
    }
    target.add_charge(vaddr, tables, end);
    target
        .aspace
        .held_mut()
        .merge_near(mm, vmos, jobs, vaddr, end);
    Ok(())
}

/// `proc_call`'s `nr::PROCESS_MAP_INFO` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_map_info<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        mm,
        vmos,
        jobs,
        me,
        ..
    } = c;
    let me = *me;
    let (vaddr, buf) = (a[1], a[2]);
    let tpid = target_with(objs, procs, me, a[0], Rights::INSPECT)?;
    let target = procs.get(&tpid).ok_or(Status::BadState)?;
    let mut bytes = [0u8; 48];
    let next = map_record(target.aspace.held_ref(), vaddr, &mut bytes);
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    kcore::usercopy::copy_to_user(caller.aspace.held_mut(), mm, vmos, jobs, buf, &bytes)
        .map_err(|_| Status::InvalidArgs)?;
    Ok((Some(next), None))
}

/// `process_map_info`'s record of the region at or after `vaddr` in `aspace` (none: a zeroed record), as six little-endian words in `bytes`
/// (no allocation: `concat` would make a vector, which cannot fail here, T177); and the start of the region after it (0: none). A frame of its
/// own (KT13).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a region's pages were checked when it was mapped
#[inline(never)]
fn map_record(aspace: &AddressSpace, vaddr: u64, bytes: &mut [u8; 48]) -> u64 {
    let bits = |p: Perms| u64::from(p.read) | u64::from(p.write) << 1 | u64::from(p.exec) << 2;
    let (record, next) = match aspace.region_at_or_after(vaddr) {
        Some(r) => {
            let len = r.pages * 4096;
            let next = aspace
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
    for (out, w) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(record.iter()) {
        *out = w.to_le_bytes();
    }
    next
}

/// `proc_call`'s `nr::PROCESS_INFO if a[1] == 1` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_cpu<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        machine,
        me,
        ..
    } = c;
    let me = *me;
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (pid, terminated, ended) =
        ipc::process_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)?;
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
}

/// `proc_call`'s `nr::PROCESS_INFO` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_info<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs, procs, me, ..
    } = c;
    let me = *me;
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (_, _, terminated, code) =
        ipc::process_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)?;
    if !terminated {
        return Err(Status::BadState);
    }
    Ok((Some(code as i64 as u64), None))
}

/// `proc_call`'s `nr::THREAD_INFO if a[1] == 1` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn thread_cpu<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        machine,
        me,
        ..
    } = c;
    let me = *me;
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (obj, pid, terminated, final_ns) =
        ipc::thread_cpu_info(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)?;
    if terminated {
        return Ok((Some(final_ns), None));
    }
    let tid = match objs.get(obj) {
        Some(ipc::KObj::Thread(t)) => t.tid,
        _ => None,
    }
    .filter(|t| procs.get(&pid).is_some_and(|p| p.live_tids.contains(t)));
    Ok((Some(tid.map_or(0, |t| machine.thread_cpu_ns(t))), None))
}

/// `proc_call`'s `nr::THREAD_INFO` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn thread_info<M: PhysMem, X: Machine>(
    a: &[u64; 6],
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs, procs, me, ..
    } = c;
    let me = *me;
    let caller = procs.get(&me).ok_or(Status::BadState)?;
    let (terminated, code) = ipc::thread_of(objs, &caller.handles, Handle(a[0]), Rights::INSPECT)?;
    if !terminated {
        return Err(Status::BadState);
    }
    Ok((Some(code as i64 as u64), None))
}

/// `proc_call`'s `nr::RESOURCE_ROOT` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn resource_root<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        jobs,
        me,
        ..
    } = c;
    let me = *me;
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    if !caller.privileged {
        return Err(Status::AccessDenied);
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
        Err(x) => Err(x),
    }
}

/// `proc_call`'s `nr::DEBUG_ROOT` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn debug_root<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        jobs,
        me,
        ..
    } = c;
    let me = *me;
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    if !caller.privileged {
        return Err(Status::AccessDenied);
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
        Err(x) => Err(x),
    }
}

/// `proc_call`'s `nr::JOB_ROOT` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn job_root<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        jobs,
        me,
        ..
    } = c;
    let me = *me;
    let caller = procs.get_mut(&me).ok_or(Status::BadState)?;
    if !caller.privileged {
        return Err(Status::AccessDenied);
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
            Err(x)
        }
    }
}

/// `proc_call`'s `nr::PROCESS_SELF` (a frame of its own: KT13).
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)]
// INVARIANT: vaddr + length and offset + length were checked with checked_add against the user limit and the VMO's size; the page-table estimate is length / 4096 / 512 + 4; room in the handle table was checked before the insert of another view of the process
#[inline(never)]
fn process_self<M: PhysMem, X: Machine>(
    c: &mut PCtx<'_, M, X>,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let PCtx {
        objs,
        procs,
        jobs,
        me,
        ..
    } = c;
    let me = *me;
    let p = procs.get_mut(&me).ok_or(Status::BadState)?;
    let rights =
        Rights(Rights::MANAGE.0 | Rights::INSPECT.0 | Rights::WAIT.0 | Rights::DUPLICATE.0);
    if p.handles.room() < 1 || p.handles.reserve(1).is_err() {
        return Err(Status::NoMemory);
    }
    // A process its parent created has a kill-on-close object (the parent's handle to it); the child's own view must not be
    // another reference to *that* object, or the parent closing its handle would never destroy it and the child would not
    // be killed (review 2026-11-14). Such a child gets a separate view object; the view is only ever held by the child.
    let own = p.obj.filter(|&o| objs.is_alive(o));
    let kill_on_close =
        matches!(own.and_then(|o| objs.get(o)), Some(ipc::KObj::Process(po)) if po.kill_on_close);
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
        None => self_view(objs, p, jobs, (me, rights), own.is_none()).map(|h| (Some(h), None)),
    }
}

/// `process_self`'s separate view object of process `me` (`p`), with `rights` (`first`: the process has no object of its own yet, and this one
/// becomes it). A frame of its own (KT13).
#[inline(never)]
fn self_view(
    objs: &mut Kobjs,
    p: &mut Process,
    jobs: &mut Jobs,
    (me, rights): (u64, Rights),
    first: bool,
) -> Result<u64, Status> {
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
    )?;
    // narrow the default rights: a self handle cannot be sent away (a table whose last free slot is on its last
    // generation refuses the replace: give the new object back)
    let Ok(h) = p.handles.replace(objs, h, rights) else {
        let _ = p.handles.close(objs, h);
        return Err(Status::NoMemory);
    };
    if first {
        p.obj = Some(obj);
    }
    Ok(h.0)
}
