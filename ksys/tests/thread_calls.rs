//! `thread_create` on the host (TS3c): the error rows of SYSCALLS.md in order, the ledger on every failure, and the start it registers.
use abi::rights::Rights;
use abi::status::Status;
use abi::user::{BASE, TOP};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Quota};
use kcore::job::{JobId, Limits, Resource, SchedLimits};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;
use ksys::{ProcState, Process, UserState};
use std::collections::BTreeMap;

mod common;
use common::FakeMem;

const CODE: u64 = 0x40_0000;

fn process(mm: &mut Mm<FakeMem>, kernel: &PageTable, job: JobId) -> Process {
    Process {
        aspace: AddressSpace::new(PageTable::new_with_kernel_half(mm, kernel).unwrap()),
        handles: HandleTable::new(64),
        asid: 1,
        vmos: Vec::new(),
        entry: 0,
        arg: 0,
        exit_code: None,
        kernel_end: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 0,
        thread_objs: BTreeMap::new(),
        job,
        table_charge: BTreeMap::new(),
        stacks: BTreeMap::new(),
        stack_hint: 0,
        privileged: false,
    }
}

struct W {
    us: UserState,
    vmos: Vmos,
    mm: Mm<FakeMem>,
    /// A handle in process 1 to process 2, with every right.
    ph: u64,
}

/// Process 1 is the caller, process 2 the target, with an executable page at `CODE`.
fn world() -> W {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut us = UserState::new();
    us.procs
        .insert(1, Box::new(process(&mut mm, &kernel, JobId::ROOT)));
    us.procs
        .insert(2, Box::new(process(&mut mm, &kernel, JobId::ROOT)));
    let mut vmos = Vmos::new();
    let code = vmos.create(1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .map_region(&vmos, CODE, 1, code, 0, Perms::RX.user())
        .unwrap();
    let data = vmos.create(1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .map_region(&vmos, CODE + 0x10_000, 1, data, 0, Perms::RW.user())
        .unwrap();
    let p = us.procs.get_mut(&1).unwrap();
    let (h, _) = ipc::process_object_create_in(
        &mut us.objs,
        &mut p.handles,
        2,
        false,
        &mut Quota {
            jobs: &mut vmos.jobs,
            job: Some(JobId::ROOT),
        },
    )
    .unwrap();
    W {
        us,
        vmos,
        mm,
        ph: h.0,
    }
}

impl W {
    fn create(&mut self, a: [u64; 6]) -> Result<ksys::Spawn, Status> {
        ksys::thread_create(&mut self.us, &mut self.vmos.jobs, 1, 10, a)
    }
    fn ok_args(&self) -> [u64; 6] {
        [self.ph, CODE, BASE + 0x10_000, 7, 0, 0]
    }
    fn threads_charged(&self, job: JobId) -> u64 {
        self.vmos.jobs.usage(job).unwrap().get(Resource::Threads)
    }
}

#[test]
fn a_thread_is_created_charged_and_registered() {
    let mut w = world();
    let s = w.create(w.ok_args()).unwrap();
    assert_eq!(s.prio, 10);
    assert_eq!(w.us.procs[&2].threads_alive, 1);
    assert_eq!(w.threads_charged(JobId::ROOT), 1);
    // the start the new thread will look up, and the handle to it
    let start = &w.us.starts[&s.token];
    assert_eq!(
        (start.pid, start.entry, start.sp, start.arg),
        (2, CODE, BASE + 0x10_000, 7)
    );
    assert!(w.us.procs[&1].handles.get(abi::Handle(s.handle)).is_ok());
    // tokens are never reused
    let s2 = w.create(w.ok_args()).unwrap();
    assert_ne!(s.token, s2.token);
    let _ = &mut w.mm;
}

#[test]
fn the_error_rows_in_order() {
    let mut w = world();
    let mut a = w.ok_args();
    a[0] = 0x999;
    assert_eq!(w.create(a), Err(Status::BadHandle));
    // a VMO handle where a process is wanted
    let v = w.vmos.create(1);
    let p = w.us.procs.get_mut(&1).unwrap();
    let vh = ipc::vmo_object_create_in(
        &mut w.us.objs,
        &mut p.handles,
        v,
        &mut Quota {
            jobs: &mut w.vmos.jobs,
            job: Some(JobId::ROOT),
        },
    )
    .unwrap();
    let mut a = w.ok_args();
    a[0] = vh.0;
    assert_eq!(w.create(a), Err(Status::WrongType));
    // no MANAGE
    let weak =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(w.ph), Rights::INSPECT)
            .unwrap();
    let mut a = w.ok_args();
    a[0] = weak.0;
    assert_eq!(w.create(a), Err(Status::AccessDenied));
    // the entry must be executable, the stack aligned and inside the user range
    let mut a = w.ok_args();
    a[1] = CODE + 0x10_000;
    assert_eq!(
        w.create(a),
        Err(Status::InvalidArgs),
        "entry in a data page"
    );
    a[1] = 0x1234;
    assert_eq!(w.create(a), Err(Status::InvalidArgs), "entry in no region");
    let mut a = w.ok_args();
    a[2] += 8;
    assert_eq!(w.create(a), Err(Status::InvalidArgs), "sp not 16-aligned");
    a[2] = BASE - 16;
    assert_eq!(
        w.create(a),
        Err(Status::InvalidArgs),
        "sp below the user range"
    );
    a[2] = TOP + 16;
    assert_eq!(w.create(a), Err(Status::InvalidArgs), "sp above it");
    assert_eq!(w.us.procs[&2].threads_alive, 0);
    assert_eq!(
        w.threads_charged(JobId::ROOT),
        0,
        "nothing was charged for the refusals"
    );
}

#[test]
fn a_target_that_has_not_started_or_has_ended_is_refused() {
    let mut w = world();
    w.us.procs.get_mut(&2).unwrap().state = ProcState::Created;
    assert_eq!(w.create(w.ok_args()), Err(Status::BadState));
    let t = w.us.procs.get_mut(&2).unwrap();
    t.state = ProcState::Running;
    t.exit_code = Some(0);
    assert_eq!(w.create(w.ok_args()), Err(Status::BadState));
    let t = w.us.procs.get_mut(&2).unwrap();
    t.exit_code = None;
    t.killed = Some(1);
    assert_eq!(w.create(w.ok_args()), Err(Status::BadState));
}

#[test]
fn the_limits_refuse_with_no_memory_and_charge_nothing() {
    let mut w = world();
    w.us.procs.get_mut(&2).unwrap().threads_alive = abi::limits::MAX_THREADS_PER_PROCESS;
    assert_eq!(
        w.create(w.ok_args()),
        Err(Status::NoMemory),
        "the per-process limit"
    );
    w.us.procs.get_mut(&2).unwrap().threads_alive = 0;
    // the job's thread limit
    let tight = w
        .vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                threads: 1,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = tight;
    w.create(w.ok_args()).unwrap();
    assert_eq!(
        w.create(w.ok_args()),
        Err(Status::NoMemory),
        "the job's limit"
    );
    assert_eq!(w.threads_charged(tight), 1);
    // the kernel-stack pages are charged with the thread, and all or nothing
    let pages = w
        .vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: ksys::KSTACK_PAGES - 1,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = pages;
    assert_eq!(w.create(w.ok_args()), Err(Status::NoMemory));
    assert_eq!(
        (
            w.threads_charged(pages),
            w.vmos.jobs.usage(pages).unwrap().get(Resource::Pages)
        ),
        (0, 0),
        "all or nothing"
    );
}

#[test]
fn a_failed_thread_object_gives_the_charge_back() {
    let mut w = world();
    // the caller's own job may hold no object: the thread's handle cannot be made
    let none = w
        .vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                objects: 0,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.us.procs.get_mut(&1).unwrap().job = none;
    assert_eq!(w.create(w.ok_args()), Err(Status::NoMemory));
    assert_eq!(
        w.threads_charged(JobId::ROOT),
        0,
        "the thread slot and its stack pages were credited back"
    );
    assert_eq!(w.us.procs[&2].threads_alive, 0);
    assert!(w.us.starts.is_empty(), "no start was registered");
}

#[test]
fn the_priority_is_inherited_clamped_to_the_job_and_at_least_one() {
    let mut w = world();
    let args = w.ok_args();
    let low = w.vmos.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.vmos
        .jobs
        .set_sched(
            low,
            SchedLimits {
                max_prio: 4,
                ..w.vmos.jobs.sched(low).unwrap()
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = low;
    assert_eq!(
        ksys::thread_create(&mut w.us, &mut w.vmos.jobs, 1, 10, args)
            .unwrap()
            .prio,
        4,
        "clamped to the job's maximum"
    );
    assert_eq!(
        ksys::thread_create(&mut w.us, &mut w.vmos.jobs, 1, 2, args)
            .unwrap()
            .prio,
        2,
        "inherited below the maximum"
    );
    assert_eq!(
        ksys::thread_create(&mut w.us, &mut w.vmos.jobs, 1, 0, args)
            .unwrap()
            .prio,
        1,
        "never the idle priority"
    );
    assert_eq!(
        ksys::clamp_prio(&w.vmos.jobs, JobId::ROOT, 200),
        63.min(w.vmos.jobs.sched(JobId::ROOT).unwrap().max_prio)
    );
}

#[test]
fn thread_create_with_a_full_handle_table() {
    // (AB1: a process's table has no fixed size any more, so the guest cannot fill it: the host table has a limit of 64)
    let mut w = world();
    let mut held = Vec::new();
    {
        let ksys::UserState { objs, procs, .. } = &mut w.us;
        let p = procs.get_mut(&1).unwrap();
        while let Ok(h) = ipc::event_create(objs, &mut p.handles) {
            held.push(h);
        }
        assert!(held.len() >= 50, "the table filled up");
    }
    assert_eq!(w.create(w.ok_args()), Err(Status::NoMemory), "table full");
    assert_eq!(w.us.procs[&2].threads_alive, 0, "nothing was started");
    assert_eq!(w.threads_charged(JobId::ROOT), 0, "nothing was charged");
    // a slot given back and the call succeeds
    let ksys::UserState { objs, procs, .. } = &mut w.us;
    let p = procs.get_mut(&1).unwrap();
    p.handles.close(objs, held.pop().unwrap()).unwrap();
    assert!(w.create(w.ok_args()).is_ok());
}
