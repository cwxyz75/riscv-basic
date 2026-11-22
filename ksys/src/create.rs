//! `process_create` (TS3c): a new process in the Created state, charged to a job, with its page table sharing the kernel half. The kernel's two
//! machine facts, whether the kernel half of its own table still matches the snapshot and the kernel table itself, are passed in.

use crate::{ProcState, Process, UserState};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use kcore::aspace::AddressSpace;
use kcore::handle::HandleTable;
use kcore::handle::MAX_SLOTS as MAX_HANDLES;
use kcore::ipc::{self, Quota};
use kcore::job::{Jobs, Resource};
use kcore::pagetable::{PageTable, PhysMem};
use kcore::Mm;

/// `process_create(job_handle) -> handle` (SYSCALLS.md): `a[0]` names the job the process is charged to (MANAGE), or 0 for the caller's own.
/// `kernel_half_ok` is whether the kernel's page-table half is still as snapshotted; `kernel` its table.
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: the new process's handle was made with room checked
#[inline(never)]
pub fn process_create<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    jobs: &mut Jobs,
    kernel: &PageTable,
    kernel_half_ok: bool,
    me: u64,
    a: [u64; 6],
) -> Result<u64, Status> {
    if us.procs.len() >= abi::limits::MAX_PROCESSES || !kernel_half_ok {
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
    let tag = us.asids.alloc();
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
            PageTable::destroy_user(table, mm);
            give_back(jobs);
            return Err(e);
        }
    };
    procs.insert(
        pid,
        Box::new(Process {
            live_tids: Default::default(),
            aspace: alloc::sync::Arc::new(klock::aslock::AsCell::new(
                pid,
                AddressSpace::new(table),
            )),
            handles: HandleTable::new(MAX_HANDLES),
            tag,
            vmos: Vec::new(),
            entry: 0,
            arg: 0,
            exit_code: None,
            kernel_end: None,
            exc_channel: None,
            start_sp: 0,
            obj: Some(obj),
            state: ProcState::Created,
            detached: true,
            killed: None,
            stopped: false,
            threads_alive: 0,
            thread_objs: BTreeMap::new(),
            job,
            table_charge: BTreeMap::from([(u64::MAX, 1)]),
            charge_end: BTreeMap::new(),
            stacks: BTreeMap::new(),
            stack_hint: abi::user::STACK_ARENA_BASE,
            privileged: false,
        }),
    );
    Ok(handle.0)
}
