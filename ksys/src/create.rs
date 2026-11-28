//! `process_create` (TS3c): a new process in the Created state, charged to a job, with its page table sharing the kernel half. The kernel's two
//! machine facts, whether the kernel half of its own table still matches the snapshot and the kernel table itself, are passed in.

use crate::{ProcState, Process, UserState};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::vec::Vec;
use kcore::aspace::AddressSpace;
use kcore::fbox::FBox;
use kcore::handle::HandleTable;
use kcore::handle::MAX_SLOTS as MAX_HANDLES;
use kcore::ipc::{self, Quota};
use kcore::job::{Jobs, Resource};
use kcore::pagetable::{PageTable, PhysMem};
use kcore::Mm;
use klock::aslock::AsCell;

/// `process_create(job_handle) -> handle` (SYSCALLS.md): `a[0]` names the job the process is charged to (MANAGE), or 0 for the caller's own.
/// `stack_hint` is where this process's `thread_spawn` stacks start looking for a gap (HD2: a random page of the stack arena's lower half, or its base).
/// `kernel_half_ok` is whether the kernel's page-table half is still as snapshotted; `kernel` its table.
#[allow(clippy::too_many_arguments)] // (the machine's parts, as the other calls take them)
#[inline(never)]
pub fn process_create<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    jobs: &mut Jobs,
    kernel: &PageTable,
    kernel_half_ok: bool,
    me: u64,
    stack_hint: u64,
    a: [u64; 6],
) -> Result<u64, Status> {
    if us.procs.len() >= us.backstops.processes || !kernel_half_ok {
        return Err(Status::NoMemory);
    }
    let job = charge_create(us, jobs, me, a[0])?;
    let Some(table) = PageTable::new_with_kernel_half(mm, kernel) else {
        give_back(jobs, job);
        return Err(Status::NoMemory);
    };
    let Some(pid) = next_pid(us) else {
        PageTable::destroy_user(table, mm);
        give_back(jobs, job);
        return Err(Status::NoMemory);
    };
    // (KT9, T177: everything that can fail for want of memory comes first, before the object exists; what follows allocates nothing. The cell comes from the
    // pool, the record and its handle table are fallible boxes, the table has room)
    let tag = us.asids.alloc();
    let record = match make_record(us, tag, job, stack_hint) {
        Ok(r) => r,
        Err(e) => {
            PageTable::destroy_user(table, mm);
            give_back(jobs, job);
            return Err(e);
        }
    };
    publish(us, mm, jobs, (me, pid, job), table, record)
}

/// The job the new process is charged to (`process_create`): the one named by the handle in `a0` (MANAGE), or the caller's own when `a0` is 0;
/// charged a process and the root page table's page, and retained. Nothing is charged on `Err`. A frame of its own (KT13).
#[inline(never)]
fn charge_create(
    us: &UserState,
    jobs: &mut Jobs,
    me: u64,
    a0: u64,
) -> Result<kcore::job::JobId, Status> {
    let creator = us.procs.get(&me).ok_or(Status::BadState)?;
    let job = if a0 == 0 {
        creator.job
    } else {
        ipc::job_of(&us.objs, &creator.handles, Handle(a0), Rights::MANAGE)?
    };
    jobs.charge(job, Resource::Processes, 1)
        .map_err(crate::job_status)?;
    // the process's root page table is charged to its job too (one page)
    if let Err(e) = jobs.charge(job, Resource::Pages, 1) {
        jobs.credit(job, Resource::Processes, 1);
        return Err(crate::job_status(e));
    }
    jobs.retain(job);
    Ok(job)
}

/// What `charge_create` charged, given back.
fn give_back(jobs: &mut Jobs, job: kcore::job::JobId) {
    jobs.credit(job, Resource::Pages, 1);
    jobs.credit(job, Resource::Processes, 1);
    jobs.release(job);
}

/// The new process's id, the counter moved past it (CNT1: a process id is never reused and the counter never wraps; at its limit, `None`, the
/// creation is refused). A pid in use is skipped: the boot-time loader and the test worlds place processes without moving the counter.
fn next_pid(us: &mut UserState) -> Option<u64> {
    let mut pid = us.next_pid;
    while us.procs.contains_key(&pid) {
        pid = pid.saturating_add(1);
    }
    us.next_pid = pid.checked_add(1)?;
    Some(pid)
}

/// The end of `process_create`: the process's object and the creator's handle to it, then the record armed with its address space and put in the
/// table (allocates nothing after the object). On `Err` everything `process_create` took goes back. A frame of its own (KT13).
#[inline(never)]
fn publish<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    jobs: &mut Jobs,
    (me, pid, job): (u64, u64, kcore::job::JobId),
    table: PageTable,
    mut record: FBox<Process>,
) -> Result<u64, Status> {
    let UserState { objs, procs, .. } = us;
    let creator = procs.get_mut(&me).ok_or(Status::BadState)?;
    let (handle, obj) = match ipc::process_object_create_in(
        objs,
        &mut creator.handles,
        pid,
        true,
        &mut Quota {
            jobs,
            job: Some(job),
        },
    ) {
        Ok(x) => x,
        Err(e) => {
            // (the cell is the record's alone: it goes back to the pool, and the rest of the record is dropped in its box, not copied out: KT13)
            let cell = alloc::sync::Arc::clone(&record.aspace);
            drop(record);
            us.as_pool.push(cell);
            PageTable::destroy_user(table, mm);
            give_back(jobs, job);
            return Err(e);
        }
    };
    // (a pooled cell is held by the pool alone, so arming cannot fail)
    let armed = AsCell::arm(&mut record.aspace, pid, AddressSpace::new_owned(table, pid));
    debug_assert!(armed.is_ok(), "a pooled address-space cell is shared");
    record.obj = Some(obj);
    procs.insert_reserved(pid, record);
    Ok(handle.0)
}

/// `process_create`'s allocations, before the object exists (KT9, T177): room in the table, a pooled address-space cell, the root table's charge
/// entry, the handle table's box, and the record's box, with the record in it. `NoMemory`: nothing is kept (the cell goes back to the pool). A frame of its own (KT13).
#[inline(never)]
fn make_record(
    us: &mut UserState,
    tag: kcore::asid::AsTag,
    job: kcore::job::JobId,
    stack_hint: u64,
) -> Result<FBox<Process>, Status> {
    us.procs.reserve(1).map_err(|_| Status::NoMemory)?;
    let cell = us.as_pool.pop().ok_or(Status::NoMemory)?;
    let mut table_charge = kcore::fmap::FMap::new();
    if table_charge.try_insert(u64::MAX, 1).is_err() {
        us.as_pool.push(cell);
        return Err(Status::NoMemory);
    }
    let Ok(handles) = FBox::try_new(HandleTable::new(MAX_HANDLES)) else {
        us.as_pool.push(cell);
        return Err(Status::NoMemory);
    };
    let Some(room) = FBox::room() else {
        us.as_pool.push(cell);
        return Err(Status::NoMemory);
    };
    // (the record is built in its box: KT13)
    Ok(room.fill_with(move || Process {
        live_tids: Default::default(),
        aspace: cell,
        handles,
        tag,
        vmos: Vec::new(),
        entry: 0,
        arg: 0,
        exit_code: None,
        kernel_end: None,
        exc_channel: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Created,
        detached: true,
        killed: None,
        stopped: false,
        threads_alive: 0,
        job,
        table_charge,
        charge_end: kcore::fmap::FMap::new(),
        stacks: kcore::fmap::FMap::new(),
        stack_hint,
        privileged: false,
        libsys: Default::default(),
    }))
}
