//! `process_create` (TS3c): a new process in the Created state, charged to a job, with its page table sharing the kernel half. The kernel's two
//! machine facts, whether the kernel half of its own table still matches the snapshot and the kernel table itself, are passed in.

use crate::{ProcState, Process, UserState};
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::handle::HandleTable;
use kcore::handle::MAX_SLOTS as MAX_HANDLES;
use kcore::ipc::{self, Quota};
use kcore::job::Resource;
use kcore::pagetable::{PageTable, PhysMem};
use kcore::Mm;

/// `process_create(job_handle) -> handle` (SYSCALLS.md): `a[0]` names the job the process is charged to (MANAGE), or 0 for the caller's own.
/// `kernel_half_ok` is whether the kernel's page-table half is still as snapshotted; `kernel` its table.
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: pids are a counter far below u32::MAX; the new process's handle was made with room checked
#[inline(never)]
pub fn process_create<M: PhysMem>(
    us: &mut UserState,
    mm: &mut Mm<M>,
    vmos: &mut Vmos,
    kernel: &PageTable,
    kernel_half_ok: bool,
    me: u32,
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
    vmos.jobs
        .charge(job, Resource::Processes, 1)
        .map_err(crate::job_status)?;
    // the process's root page table is charged to its job too (one page)
    if let Err(e) = vmos.jobs.charge(job, Resource::Pages, 1) {
        vmos.jobs.credit(job, Resource::Processes, 1);
        return Err(crate::job_status(e));
    }
    vmos.jobs.retain(job);
    let give_back = |vmos: &mut Vmos| {
        vmos.jobs.credit(job, Resource::Pages, 1);
        vmos.jobs.credit(job, Resource::Processes, 1);
        vmos.jobs.release(job);
    };
    let Some(table) = PageTable::new_with_kernel_half(mm, kernel) else {
        give_back(vmos);
        return Err(Status::NoMemory);
    };
    // (pids wrap after 2^32 creations and skip the live ones: a loop of create and close must not end in an overflow)
    let pid = loop {
        let candidate = us.next_pid;
        us.next_pid = if candidate == u32::MAX {
            1
        } else {
            candidate + 1
        };
        if !us.procs.contains_key(&candidate) {
            break candidate;
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
            jobs: &mut vmos.jobs,
            job: Some(job),
        },
    ) {
        Ok(x) => x,
        Err(e) => {
            PageTable::destroy_user(table, mm);
            give_back(vmos);
            return Err(e);
        }
    };
    procs.insert(
        pid,
        Box::new(Process {
            aspace: AddressSpace::new(table),
            handles: HandleTable::new(MAX_HANDLES),
            asid: (pid & 0xffff) as u16,
            vmos: Vec::new(),
            entry: 0,
            arg: 0,
            exit_code: None,
            kernel_end: None,
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
            stacks: BTreeMap::new(),
            stack_hint: abi::user::STACK_ARENA_BASE,
            privileged: false,
        }),
    );
    Ok(handle.0)
}
