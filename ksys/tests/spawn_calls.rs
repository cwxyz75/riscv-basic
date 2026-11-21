//! `thread_spawn` on the host (TS3c): the error rows of SYSCALLS.md in order, the stack it maps, the ledger on every failure, and the start it registers.
use abi::rights::Rights;
use abi::spawn as sp;
use abi::status::Status;
use abi::user::{BASE, STACK_ARENA_BASE, STACK_ARENA_END, TOP};
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
/// The caller's record, in a read-write page of process 1.
const REC: u64 = 0x50_0000;

fn process(mm: &mut Mm<FakeMem>, kernel: &PageTable, job: JobId) -> Process {
    Process {
        live_tids: Default::default(),
        aspace: AddressSpace::new(PageTable::new_with_kernel_half(mm, kernel).unwrap()),
        handles: HandleTable::new(64),
        tag: kcore::asid::AsTag { asid: 1, gen: 1 },
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
        charge_end: BTreeMap::new(),
        stacks: BTreeMap::new(),
        stack_hint: 0,
        privileged: false,
    }
}

struct W {
    us: UserState,
    vmos: Vmos,
    jobs: kcore::job::Jobs,
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
    let mut jobs = kcore::job::Jobs::new();
    let code = vmos.create(&mut jobs, 1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .map_region(&mut vmos, CODE, 1, code, 0, Perms::RX.user())
        .unwrap();
    let data = vmos.create(&mut jobs, 1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .map_region(&mut vmos, CODE + 0x10_000, 1, data, 0, Perms::RW.user())
        .unwrap();
    let rec = vmos.create(&mut jobs, 1);
    us.procs
        .get_mut(&1)
        .unwrap()
        .aspace
        .map_region(&mut vmos, REC, 1, rec, 0, Perms::RW.user())
        .unwrap();
    let p = us.procs.get_mut(&1).unwrap();
    let (h, _) = ipc::process_object_create_in(
        &mut us.objs,
        &mut p.handles,
        2,
        false,
        &mut Quota {
            jobs: &mut jobs,
            job: Some(JobId::ROOT),
        },
    )
    .unwrap();
    W {
        us,
        vmos,
        jobs,
        mm,
        ph: h.0,
    }
}

impl W {
    fn record(&mut self, w: [u64; 8]) {
        let bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
        let p = self.us.procs.get_mut(&1).unwrap();
        kcore::usercopy::copy_to_user(
            &mut p.aspace,
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            REC,
            &bytes,
        )
        .unwrap();
    }
    /// A record: entry, arg, tp, stack bytes, guard bytes, prio | flags << 32, caller sp, reserved.
    fn spawn(&mut self, rec: [u64; 8]) -> Result<ksys::Spawn, Status> {
        self.record(rec);
        ksys::thread_spawn(
            &mut self.us,
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            1,
            10,
            [self.ph, REC, 0, 0, 0, 0],
        )
    }
    fn plain(&self) -> [u64; 8] {
        [CODE, 7, 0x1234, 0, 0, 0, 0, 0]
    }
    fn pages_used(&self, job: JobId) -> u64 {
        self.jobs.usage(job).unwrap().get(Resource::Pages)
    }
    fn threads_charged(&self, job: JobId) -> u64 {
        self.jobs.usage(job).unwrap().get(Resource::Threads)
    }
}

#[test]
fn a_thread_gets_a_stack_between_guards_in_the_arena() {
    let mut w = world();
    let s = w.spawn(w.plain()).unwrap();
    let st = &w.us.starts[&s.token];
    let stack = st.stack.expect("the kernel provisioned a stack");
    assert!((STACK_ARENA_BASE..STACK_ARENA_END).contains(&stack.region));
    assert_eq!(
        st.sp,
        stack.region + sp::DEFAULT_STACK,
        "the thread starts at the top of the default stack"
    );
    assert_eq!(
        (st.tp, st.arg, st.entry, st.spawned, st.rr, st.pid),
        (0x1234, 7, CODE, true, false, 2)
    );
    assert!(
        w.us.procs[&1].handles.get(abi::Handle(s.handle)).is_ok(),
        "a joinable thread has a handle"
    );
    let t = &w.us.procs[&2];
    assert_eq!(t.threads_alive, 1);
    assert!(t.table_charge.contains_key(&stack.region));
    assert_eq!(t.stack_hint, stack.region + sp::DEFAULT_STACK);
    // the guard below the stack and the guard above are not mapped: the next stack is not flush
    assert!(t.aspace.region_perms_at(stack.region - 4096).is_none());
    assert!(t
        .aspace
        .region_perms_at(stack.region + sp::DEFAULT_STACK)
        .is_none());
    let s2 = w.spawn(w.plain()).unwrap();
    let r2 = w.us.starts[&s2.token].stack.unwrap().region;
    assert!(
        r2 >= stack.region + sp::DEFAULT_STACK + 4096,
        "a guard page lies between two stacks"
    );
}

#[test]
fn flags_detached_round_robin_and_a_stack_of_the_callers_own() {
    let mut w = world();
    let mut r = w.plain();
    r[5] = u64::from(sp::DETACHED | sp::ROUND_ROBIN) << 32;
    let s = w.spawn(r).unwrap();
    assert_eq!(s.handle, 0, "a detached thread has no handle");
    assert!(w.us.starts[&s.token].rr && w.us.starts[&s.token].obj.is_none());
    let mut r = w.plain();
    r[5] = u64::from(sp::CALLER_STACK) << 32;
    r[6] = BASE + 0x10_000;
    let s = w.spawn(r).unwrap();
    let st = &w.us.starts[&s.token];
    assert!(
        st.stack.is_none(),
        "nothing provisioned for a caller's stack"
    );
    assert_eq!(st.sp, BASE + 0x10_000);
}

#[test]
fn the_error_rows_in_order() {
    let mut w = world();
    // the record is read first: a bad pointer beats a bad handle
    let r = w.plain();
    w.record(r);
    assert_eq!(
        ksys::thread_spawn(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            10,
            [0x999, 0x10, 0, 0, 0, 0]
        ),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        ksys::thread_spawn(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            10,
            [0x999, REC, 0, 0, 0, 0]
        ),
        Err(Status::BadHandle)
    );
    let weak =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(w.ph), Rights::INSPECT)
            .unwrap();
    w.record(r);
    assert_eq!(
        ksys::thread_spawn(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            10,
            [weak.0, REC, 0, 0, 0, 0]
        ),
        Err(Status::AccessDenied)
    );
    // a priority above the job's maximum is refused, not clamped
    let low = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.jobs
        .set_sched(
            low,
            SchedLimits {
                max_prio: 4,
                ..w.jobs.sched(low).unwrap()
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = low;
    let mut r2 = w.plain();
    r2[5] = 5;
    assert_eq!(w.spawn(r2), Err(Status::AccessDenied));
    r2[5] = 4;
    assert_eq!(w.spawn(r2).unwrap().prio, 4);
    w.us.procs.get_mut(&2).unwrap().job = JobId::ROOT;
    // state
    w.us.procs.get_mut(&2).unwrap().killed = Some(1);
    assert_eq!(w.spawn(w.plain()), Err(Status::BadState));
    w.us.procs.get_mut(&2).unwrap().killed = None;
    for (name, mutate) in [
        ("entry not executable", (0usize, CODE + 0x10_000)),
        ("reserved", (7, 1)),
        ("priority over 63", (5, 64)),
        ("unknown flag", (5, 8u64 << 32)),
        ("stack too big", (3, sp::MAX_STACK + 1)),
        ("guard too big", (4, sp::MAX_STACK + 1)),
        ("a stack pointer without CALLER_STACK", (6, 16)),
    ] {
        let mut r = w.plain();
        r[mutate.0] = mutate.1;
        assert_eq!(w.spawn(r), Err(Status::InvalidArgs), "{name}");
    }
    // CALLER_STACK: sp aligned and in range, and no sizes
    let own = u64::from(sp::CALLER_STACK) << 32;
    for (name, sp_v, bytes) in [
        ("unaligned", BASE + 8, 0),
        ("below the range", BASE - 16, 0),
        ("above it", TOP + 16, 0),
        ("with a size", BASE + 0x10_000, 4096),
    ] {
        let mut r = w.plain();
        r[5] = own;
        r[6] = sp_v;
        r[3] = bytes;
        assert_eq!(w.spawn(r), Err(Status::InvalidArgs), "{name}");
    }
}

#[test]
fn every_failure_after_the_checks_gives_back_what_it_took() {
    let mut w = world();
    let r = w.plain();
    w.record(r); // (the first write commits the record's page, which is charged: measure after it)
    let (threads, pages) = (w.threads_charged(JobId::ROOT), w.pages_used(JobId::ROOT));
    // the table pages of the stack do not fit
    let tight = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: ksys::KSTACK_PAGES + 2,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = tight;
    assert_eq!(w.spawn(w.plain()), Err(Status::NoMemory));
    assert_eq!(
        (w.threads_charged(tight), w.pages_used(tight)),
        (0, 0),
        "the thread charge and the page charge both went back"
    );
    // the caller's job has no room for the thread's object
    w.us.procs.get_mut(&2).unwrap().job = JobId::ROOT;
    let none = w
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
    assert_eq!(w.spawn(w.plain()), Err(Status::NoMemory));
    w.us.procs.get_mut(&1).unwrap().job = JobId::ROOT;
    assert_eq!(
        (w.threads_charged(JobId::ROOT), w.pages_used(JobId::ROOT)),
        (threads, pages)
    );
    assert_eq!(w.us.procs[&2].threads_alive, 0);
    assert!(w.us.starts.is_empty() && w.us.procs[&2].stacks.is_empty());
    // the per-process and per-job thread limits
    w.us.procs.get_mut(&2).unwrap().threads_alive = abi::limits::MAX_THREADS_PER_PROCESS;
    assert_eq!(w.spawn(w.plain()), Err(Status::NoMemory));
}

#[test]
fn the_priority_is_inherited_clamped_to_the_job_and_at_least_one() {
    // (moved from the retired `thread_create` tests, AB5: with priority 0 in the record the thread takes its creator's base priority)
    let mut w = world();
    let low = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.jobs
        .set_sched(
            low,
            SchedLimits {
                max_prio: 4,
                ..w.jobs.sched(low).unwrap()
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = low;
    let mut prio_of = |creator: u8| {
        let rec = w.plain();
        w.record(rec);
        ksys::thread_spawn(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            creator,
            [w.ph, REC, 0, 0, 0, 0],
        )
        .unwrap()
        .prio
    };
    assert_eq!(prio_of(10), 4, "clamped to the job's maximum");
    assert_eq!(prio_of(2), 2, "inherited below the maximum");
    assert_eq!(prio_of(0), 1, "never the idle priority");
    assert_eq!(
        ksys::clamp_prio(&w.jobs, JobId::ROOT, 200),
        63.min(w.jobs.sched(JobId::ROOT).unwrap().max_prio)
    );
}

#[test]
fn a_full_handle_table_refuses_with_no_memory_and_charges_nothing() {
    // (AB1: a process's table has no fixed size any more, so the guest cannot fill it: the host table has a limit of 64)
    let mut w = world();
    let mut held = Vec::new();
    {
        let ksys::UserState { objs, procs, .. } = &mut w.us;
        let p = procs.get_mut(&1).unwrap();
        while let Ok(h) = kcore::ipc::event_create(objs, &mut p.handles) {
            held.push(h);
        }
        assert!(held.len() >= 50, "the table filled up");
    }
    let plain = w.plain();
    assert_eq!(w.spawn(plain).map(|_| ()), Err(Status::NoMemory));
    assert_eq!(w.us.procs[&2].threads_alive, 0, "nothing was started");
    assert_eq!(w.threads_charged(JobId::ROOT), 0, "nothing was charged");
    let ksys::UserState { objs, procs, .. } = &mut w.us;
    let p = procs.get_mut(&1).unwrap();
    p.handles.close(objs, held.pop().unwrap()).unwrap();
    assert!(w.spawn(plain).is_ok());
}
