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
#[allow(clippy::large_stack_frames)] // KT13: audited, clippy's estimate over 1 KiB, left by T289 (see work/T289.md)
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
    // The job the new process is charged to: the one named by the handle in a0 (MANAGE), or the caller's own when a0 is 0.
    let creator = us.procs.get(&me).ok_or(Status::BadState)?;
    let job = if a[0] == 0 {
        creator.job
    } else {
        ipc::job_of(&us.objs, &creator.handles, Handle(a[0]), Rights::MANAGE)?
    };
    jobs.charge(job, Resource::Processes, 1)
        .map_err(crate::job_status)?;
    // the process's root page table is charged to its job too (one page)
    if let Err(e) = jobs.charge(job, Resource::Pages, 1) {
        jobs.credit(job, Resource::Processes, 1);
        return Err(crate::job_status(e));
    }
    jobs.retain(job);
    let give_back = |jobs: &mut Jobs| {
        jobs.credit(job, Resource::Pages, 1);
        jobs.credit(job, Resource::Processes, 1);
        jobs.release(job);
    };
    let Some(table) = PageTable::new_with_kernel_half(mm, kernel) else {
        give_back(jobs);
        return Err(Status::NoMemory);
    };
    // (CNT1: a process id is never reused and the counter never wraps; at its limit the creation is refused)
    // (a pid in use is skipped: the boot-time loader and the test worlds place processes without moving the counter)
    let mut pid = us.next_pid;
    while us.procs.contains_key(&pid) {
        pid = pid.saturating_add(1);
    }
    let Some(next) = pid.checked_add(1) else {
        PageTable::destroy_user(table, mm);
        give_back(jobs);
        return Err(Status::NoMemory);
    };
    us.next_pid = next;
    // (KT9, T177: everything that can fail for want of memory comes first, before the object exists; what follows allocates nothing. The cell comes from the
    // pool, the record is one fallible box, the table has room)
    let tag = us.asids.alloc();
    let made = (|| {
        us.procs.reserve(1).map_err(|_| Status::NoMemory)?;
        let cell = us.as_pool.pop().ok_or(Status::NoMemory)?;
        let mut table_charge = kcore::fmap::FMap::new();
        if table_charge.try_insert(u64::MAX, 1).is_err() {
            us.as_pool.push(cell);
            return Err(Status::NoMemory);
        }
        match FBox::try_new(new_process(cell, tag, job, table_charge, stack_hint)) {
            Ok(b) => Ok(b),
            Err(p) => {
                us.as_pool.push(p.aspace);
                Err(Status::NoMemory)
            }
        }
    })();
    let mut record = match made {
        Ok(r) => r,
        Err(e) => {
            PageTable::destroy_user(table, mm);
            give_back(jobs);
            return Err(e);
        }
    };
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
            us.as_pool.push(record.into_inner().aspace);
            PageTable::destroy_user(table, mm);
            give_back(jobs);
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

/// The record of a new process, in the Created state, around a vacant pooled address-space cell (armed later) and with no object yet. Its own function so that
/// the frame of the big literal is not part of `process_create`'s.
#[inline(never)]
#[allow(clippy::large_stack_frames)] // KT13: audited, clippy's estimate over 1 KiB, left by T289 (see work/T289.md)
fn new_process(
    aspace: alloc::sync::Arc<AsCell>,
    tag: kcore::asid::AsTag,
    job: kcore::job::JobId,
    table_charge: kcore::fmap::FMap<u64, u64>,
    stack_hint: u64,
) -> Process {
    Process {
        live_tids: Default::default(),
        aspace,
        handles: HandleTable::new(MAX_HANDLES),
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
    }
}
