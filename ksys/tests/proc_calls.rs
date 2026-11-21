//! The calls on the process table (TS3b): `process_map`, `process_unmap`, `process_info`, `thread_info`, `process_self`, `job_root`,
//! `resource_root`, over two processes with real address spaces, and the TLB work recorded rather than done.
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::user::{BASE as USER_BASE, TOP};
use alloc_free::*;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, Quota};
use kcore::job::{JobId, Limits, Resource};
use kcore::pagetable::PageTable;
use kcore::usercopy::{copy_from_user, copy_to_user};
use kcore::Mm;
use ksys::{proc_call, Machine, PCtx, ProcState, Process, Reply};
use std::collections::BTreeMap;

mod common;
use common::FakeMem;

mod alloc_free {
    pub const R: u64 = 1;
    pub const W: u64 = 2;
    pub const X: u64 = 4;
}

#[derive(Default)]
struct Tlb {
    shootdowns: u32,
    flushes: u32,
    /// Executable pages the instruction caches were synchronised for (PL7).
    icache_pages: u64,
    /// Draws from the entropy pool (ANYWHERE).
    randoms: u32,
}
impl Machine for Tlb {
    fn shootdown(&mut self) {
        self.shootdowns += 1;
    }
    fn flush_tlb(&mut self) {
        self.flushes += 1;
    }
    fn icache_sync(&mut self, pages: u64) {
        self.icache_pages += pages;
    }
    fn random_below(&mut self, _n: u64) -> u64 {
        self.randoms += 1;
        0
    }
}

struct W {
    objs: Kobjs,
    procs: BTreeMap<u64, Box<Process>>,
    mm: Mm<FakeMem>,
    vmos: Vmos,
    jobs: kcore::job::Jobs,
    tlb: Tlb,
}

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

/// Process 1 is the caller, process 2 the target; the caller holds a MANAGE handle to the target (`ph`).
fn world() -> (W, u64) {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut procs = BTreeMap::new();
    procs.insert(1, Box::new(process(&mut mm, &kernel, JobId::ROOT)));
    procs.insert(2, Box::new(process(&mut mm, &kernel, JobId::ROOT)));
    let mut w = W {
        objs: Kobjs::new(),
        procs,
        mm,
        vmos: Vmos::new(),
        jobs: kcore::job::Jobs::new(),
        tlb: Tlb::default(),
    };
    let ph = w.handle_to(2);
    (w, ph)
}

impl W {
    fn q(jobs: &mut kcore::job::Jobs) -> Quota<'_> {
        Quota {
            jobs,
            job: Some(JobId::ROOT),
        }
    }
    /// A handle in process 1 to process `pid`, with all the rights a creator has.
    fn handle_to(&mut self, pid: u64) -> u64 {
        let p = self.procs.get_mut(&1).unwrap();
        let (h, _) = ipc::process_object_create_in(
            &mut self.objs,
            &mut p.handles,
            pid,
            false,
            &mut Self::q(&mut self.jobs),
        )
        .unwrap();
        h.0
    }
    fn vmo(&mut self, pages: u64, job: JobId) -> u64 {
        let id = self.vmos.create_in(&mut self.jobs, pages, job);
        let p = self.procs.get_mut(&1).unwrap();

        ipc::vmo_object_create_in(
            &mut self.objs,
            &mut p.handles,
            id,
            &mut Self::q(&mut self.jobs),
        )
        .unwrap()
        .0
    }
    /// Like `vmo`, with the VMO's id too (the tests that look inside the store).
    fn vmo_and_id(&mut self, pages: u64, job: JobId) -> (u64, u64) {
        let id = self.vmos.create_in(&mut self.jobs, pages, job);
        let p = self.procs.get_mut(&1).unwrap();
        let h = ipc::vmo_object_create_in(
            &mut self.objs,
            &mut p.handles,
            id,
            &mut Self::q(&mut self.jobs),
        )
        .unwrap()
        .0;
        (h, id)
    }
    fn call(&mut self, n: usize, a: [u64; 6]) -> Reply {
        let mut c = PCtx {
            objs: &mut self.objs,
            procs: &mut self.procs,
            me: 1,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            jobs: &mut self.jobs,
            machine: &mut self.tlb,
        };
        proc_call(n, a, &mut c).expect("handled")
    }
    fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("succeeds")
    }
    fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect_err("fails").0
    }
}

const VA: u64 = 0x40_0000;

#[test]
fn calls_that_are_not_these_are_left_to_the_kernel() {
    let (mut w, _) = world();
    let mut c = PCtx {
        objs: &mut w.objs,
        procs: &mut w.procs,
        me: 1,
        mm: &mut w.mm,
        vmos: &mut w.vmos,
        jobs: &mut w.jobs,
        machine: &mut w.tlb,
    };
    assert!(proc_call(nr::PROCESS_CREATE, [0; 6], &mut c).is_none());
}

#[test]
fn process_map_maps_into_the_target_and_charges_its_tables() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    let pages_before = w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, W | R]);
    let used = w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    assert_eq!(
        used - pages_before,
        4,
        "the worst-case table estimate (four pages for four pages of data) is charged to the target's job"
    );
    // the target's memory shows it, the caller's does not
    let t = w.procs.get_mut(&2).unwrap();
    copy_to_user(
        &mut t.aspace,
        &mut w.mm,
        &mut w.vmos,
        &mut w.jobs,
        VA + 8,
        b"hi",
    )
    .unwrap();
    assert_eq!(
        copy_from_user(
            &mut t.aspace,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            VA + 8,
            2
        )
        .unwrap(),
        b"hi"
    );
    let me = w.procs.get_mut(&1).unwrap();
    assert!(copy_from_user(
        &mut me.aspace,
        &mut w.mm,
        &mut w.vmos,
        &mut w.jobs,
        VA + 8,
        2
    )
    .is_err());
}

#[test]
fn process_map_rows_in_order() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    let ok_args = |ph, v| [ph, v, VA, 0, 4096, R];
    assert_eq!(w.err(nr::PROCESS_MAP, ok_args(0x999, v)), Status::BadHandle);
    assert_eq!(
        w.err(nr::PROCESS_MAP, ok_args(ph, 0x999)),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::PROCESS_MAP, ok_args(v, v)),
        Status::WrongType,
        "a VMO where a process is wanted"
    );
    assert_eq!(
        w.err(nr::PROCESS_MAP, ok_args(ph, ph)),
        Status::WrongType,
        "a process where a VMO is wanted"
    );
    // rights: MANAGE on the process; MAP|READ (+WRITE, +EXECUTE for those flags) on the VMO
    let no_manage = {
        let p = w.procs.get_mut(&1).unwrap();

        p.handles
            .duplicate(&mut w.objs, abi::Handle(ph), Rights::WAIT)
            .unwrap()
            .0
    };
    assert_eq!(
        w.err(nr::PROCESS_MAP, ok_args(no_manage, v)),
        Status::AccessDenied
    );
    let ro = {
        let p = w.procs.get_mut(&1).unwrap();

        p.handles
            .duplicate(
                &mut w.objs,
                abi::Handle(v),
                Rights(Rights::MAP.0 | Rights::READ.0),
            )
            .unwrap()
            .0
    };
    w.ok(nr::PROCESS_MAP, [ph, ro, VA, 0, 4096, R]);
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, ro, VA + 0x10_0000, 0, 4096, W]),
        Status::AccessDenied,
        "write needs WRITE on the VMO"
    );
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, ro, VA + 0x10_0000, 0, 4096, X]),
        Status::AccessDenied,
        "execute needs EXECUTE on the VMO"
    );
}

#[test]
fn process_map_argument_errors() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    for (name, a) in [
        ("unaligned address", [ph, v, VA + 1, 0, 4096, R]),
        ("unaligned offset", [ph, v, VA, 1, 4096, R]),
        ("unaligned length", [ph, v, VA, 0, 100, R]),
        ("zero length", [ph, v, VA, 0, 0, R]),
        ("unknown flag", [ph, v, VA, 0, 4096, 8]),
        ("write and execute", [ph, v, VA, 0, 4096, W | X]),
        // (address 0 is no longer "below the user range": it asks for ANYWHERE, MM7)
        ("zero length anywhere", [ph, v, 0, 0, 0, R]),
        ("unaligned length anywhere", [ph, v, 0, 0, 100, R]),
        ("above the user range", [ph, v, TOP, 0, 4096, R]),
        ("wrapping range", [ph, v, u64::MAX - 4095, 0, 8192, R]),
        (
            "past the end of the VMO",
            [ph, v, VA, 3 * 4096, 2 * 4096, R],
        ),
        ("offset overflow", [ph, v, VA, u64::MAX - 4095, 8192, R]),
    ] {
        assert_eq!(w.err(nr::PROCESS_MAP, a), Status::InvalidArgs, "{name}");
    }
    // overlapping a region that is there
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4096, R]);
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA, 0, 4096, R]),
        Status::InvalidArgs,
        "overlap"
    );
}

#[test]
fn process_map_of_a_stranger_vmo_must_be_committed_and_a_dead_target_is_refused() {
    let (mut w, ph) = world();
    let other = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let v = w.vmo(2, other);
    // charged to a job that is neither the caller's nor the target's: it must already be committed
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R]),
        Status::BadState
    );
    let id = match w
        .objs
        .get(w.procs[&1].handles.get(abi::Handle(v)).unwrap().obj)
    {
        Some(ipc::KObj::Vmo(id)) => *id,
        _ => unreachable!(),
    };
    w.vmos.commit(&mut w.mm, &mut w.jobs, id, 0, 2).unwrap();
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R]);
    // a target that has ended, or been killed
    w.procs.get_mut(&2).unwrap().exit_code = Some(0);
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA + 0x10_0000, 0, 4096, R]),
        Status::BadState
    );
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]),
        Status::BadState
    );
    let w2 = w.procs.get_mut(&2).unwrap();
    w2.exit_code = None;
    w2.killed = Some(9);
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA + 0x10_0000, 0, 4096, R]),
        Status::BadState
    );
}

#[test]
fn process_map_over_the_page_limit_charges_nothing() {
    let (mut w, ph) = world();
    let tight = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 3,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.procs.get_mut(&2).unwrap().job = tight;
    let v = w.vmo(1, JobId::ROOT);
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA, 0, 4096, R]),
        Status::NoMemory,
        "four table pages do not fit in three"
    );
    assert_eq!(w.jobs.usage(tight).unwrap().get(Resource::Pages), 0);
    assert!(w.procs[&2].table_charge.is_empty());
}

#[test]
fn process_unmap_credits_the_tables_and_asks_for_the_shootdown() {
    let (mut w, ph) = world();
    let v = w.vmo(2, JobId::ROOT);
    let before = w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R]);
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [ph, VA + 4096, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "no region starts there"
    );
    assert_eq!(
        (w.tlb.shootdowns, w.tlb.flushes),
        (0, 0),
        "a failed unmap asks for nothing"
    );
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    assert_eq!((w.tlb.shootdowns, w.tlb.flushes), (1, 1));
    assert_eq!(
        w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
        before,
        "the table charge came back"
    );
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [0x999, VA, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [v, VA, 0, 0, 0, 0]),
        Status::WrongType
    );
}

#[test]
fn process_and_thread_info_report_the_exit_code_only_when_ended() {
    let (mut w, ph) = world();
    assert_eq!(
        w.err(nr::PROCESS_INFO, [ph, 0, 0, 0, 0, 0]),
        Status::BadState,
        "still running"
    );
    let (obj, ..) = ipc::process_of(
        &w.objs,
        &w.procs[&1].handles,
        abi::Handle(ph),
        Rights::INSPECT,
    )
    .unwrap();
    ipc::process_mark_terminated(&mut w.objs, obj, -7);
    assert_eq!(
        w.ok(nr::PROCESS_INFO, [ph, 0, 0, 0, 0, 0]).0,
        Some(-7i64 as u64)
    );
    let manage_only = {
        let p = w.procs.get_mut(&1).unwrap();

        p.handles
            .duplicate(&mut w.objs, abi::Handle(ph), Rights::MANAGE)
            .unwrap()
            .0
    };
    assert_eq!(
        w.err(nr::PROCESS_INFO, [manage_only, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::PROCESS_INFO, [0x999, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let v = w.vmo(1, JobId::ROOT);
    assert_eq!(
        w.err(nr::PROCESS_INFO, [v, 0, 0, 0, 0, 0]),
        Status::WrongType
    );
    // threads: the same shape
    assert_eq!(
        w.err(nr::THREAD_INFO, [0x999, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::THREAD_INFO, [ph, 0, 0, 0, 0, 0]),
        Status::WrongType
    );
    let th = {
        let p = w.procs.get_mut(&1).unwrap();
        ipc::thread_object_create_in(&mut w.objs, &mut p.handles, 2, &mut W::q(&mut w.jobs))
            .unwrap()
    };
    let (thandle, tobj) = th;
    assert_eq!(
        w.err(nr::THREAD_INFO, [thandle.0, 0, 0, 0, 0, 0]),
        Status::BadState,
        "still running"
    );
    ipc::thread_mark_terminated(&mut w.objs, tobj, 3);
    assert_eq!(w.ok(nr::THREAD_INFO, [thandle.0, 0, 0, 0, 0, 0]).0, Some(3));
}

#[test]
fn the_root_handles_are_for_privileged_processes_only() {
    let (mut w, _) = world();
    for n in [nr::JOB_ROOT, nr::RESOURCE_ROOT] {
        assert_eq!(w.err(n, [0; 6]), Status::AccessDenied);
    }
    w.procs.get_mut(&1).unwrap().privileged = true;
    let h = w.ok(nr::JOB_ROOT, [0; 6]).0.unwrap();
    assert_eq!(
        ipc::job_of(&w.objs, &w.procs[&1].handles, abi::Handle(h), Rights::JOB).unwrap(),
        JobId::ROOT
    );
    assert!(w.ok(nr::RESOURCE_ROOT, [0; 6]).0.is_some());
}

#[test]
fn process_self_gives_views_that_cannot_be_sent_away() {
    let (mut w, _) = world();
    let h1 = w.ok(nr::PROCESS_SELF, [0; 6]).0.unwrap();
    let h2 = w.ok(nr::PROCESS_SELF, [0; 6]).0.unwrap();
    assert_ne!(h1, h2);
    for h in [h1, h2] {
        let e = w.procs[&1].handles.get(abi::Handle(h)).unwrap();
        assert!(e.rights.contains(Rights::INSPECT) && e.rights.contains(Rights::MANAGE));
        assert!(
            !e.rights.contains(Rights::TRANSFER),
            "a self handle cannot be sent away"
        );
    }
    // a full table is an error
    let mut st = None;
    for _ in 0..100 {
        if let Err((s, ..)) = w.call(nr::PROCESS_SELF, [0; 6]) {
            st = Some(s);
            break;
        }
    }
    assert_eq!(st, Some(Status::NoMemory));
}

// ---- the decisions of kill, suspend, job_kill and the scheduling calls ----

fn narrow(w: &mut W, h: u64, r: Rights) -> abi::Handle {
    let p = w.procs.get_mut(&1).unwrap();
    p.handles.duplicate(&mut w.objs, abi::Handle(h), r).unwrap()
}

fn job_handle(w: &mut W, job: JobId) -> u64 {
    let p = w.procs.get_mut(&1).unwrap();

    ipc::job_object_create(
        &mut w.objs,
        &mut p.handles,
        job,
        Rights::JOB,
        &mut W::q(&mut w.jobs),
    )
    .unwrap()
    .0
}

#[test]
fn kill_and_stop_name_their_target_and_refuse_the_ended() {
    let (mut w, ph) = world();
    let h = abi::Handle(ph);
    assert_eq!(
        ksys::kill_target(&w.objs, &w.procs, 1, abi::Handle(0x999)),
        Err(Status::BadHandle)
    );
    let v = w.vmo(1, JobId::ROOT);
    assert_eq!(
        ksys::kill_target(&w.objs, &w.procs, 1, abi::Handle(v)),
        Err(Status::WrongType)
    );
    let no_kill = narrow(&mut w, ph, Rights::INSPECT);
    assert_eq!(
        ksys::kill_target(&w.objs, &w.procs, 1, no_kill),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        ksys::stop_target(&mut w.objs, &mut w.procs, 1, no_kill, true),
        Err(Status::AccessDenied)
    );
    assert_eq!(ksys::kill_target(&w.objs, &w.procs, 1, h), Ok(2));
    // stop and resume: idempotent, and the flag follows
    assert_eq!(
        ksys::stop_target(&mut w.objs, &mut w.procs, 1, h, true),
        Ok(2)
    );
    assert_eq!(
        ksys::stop_target(&mut w.objs, &mut w.procs, 1, h, true),
        Ok(2)
    );
    assert!(w.procs[&2].stopped);
    assert_eq!(
        ksys::stop_target(&mut w.objs, &mut w.procs, 1, h, false),
        Ok(2)
    );
    assert!(!w.procs[&2].stopped);
    // a process that has ended, been killed, or is gone
    w.procs.get_mut(&2).unwrap().killed = Some(1);
    assert_eq!(
        ksys::stop_target(&mut w.objs, &mut w.procs, 1, h, true),
        Err(Status::BadState)
    );
    let (obj, ..) = ipc::process_of(&w.objs, &w.procs[&1].handles, h, Rights::INSPECT).unwrap();
    ipc::process_mark_terminated(&mut w.objs, obj, 0);
    assert_eq!(
        ksys::kill_target(&w.objs, &w.procs, 1, h),
        Err(Status::BadState)
    );
    w.procs.remove(&2);
    assert_eq!(
        ksys::kill_target(&w.objs, &w.procs, 1, h),
        Err(Status::BadState)
    );
}

#[test]
fn job_kill_closes_the_job_and_names_the_victims() {
    let (mut w, _) = world();
    let sub = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let deeper = w.jobs.create(sub, Limits::UNLIMITED).unwrap();
    w.procs.get_mut(&2).unwrap().job = deeper;
    let jh = job_handle(&mut w, sub);
    // the caller (1) is outside the job: it is not a victim and not inside
    let (v, inside) =
        ksys::job_kill_victims(&w.objs, &w.procs, &mut w.jobs, 1, abi::Handle(jh)).unwrap();
    assert_eq!(
        (v, inside),
        (vec![2], false),
        "a process in a descendant job is a victim"
    );
    let l = w.jobs.limits(sub).unwrap();
    assert_eq!(
        (l.processes, l.threads),
        (0, 0),
        "no new processes or threads"
    );
    // the caller inside: ends last, not named
    w.procs.get_mut(&1).unwrap().job = sub;
    let (v, inside) =
        ksys::job_kill_victims(&w.objs, &w.procs, &mut w.jobs, 1, abi::Handle(jh)).unwrap();
    assert_eq!((v, inside), (vec![2], true));
    let weak = narrow(&mut w, jh, Rights::MANAGE);
    assert_eq!(
        ksys::job_kill_victims(&w.objs, &w.procs, &mut w.jobs, 1, weak),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        ksys::job_kill_victims(&w.objs, &w.procs, &mut w.jobs, 1, abi::Handle(0x999)),
        Err(Status::BadHandle)
    );
}

#[test]
fn thread_target_and_set_sched_rows() {
    let (mut w, _) = world();
    let (me_tid, other) = (7u64, 9u64);
    // 0 is the calling thread
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, 0, Rights::MANAGE),
        Ok((me_tid, 1))
    );
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, 0x999, Rights::MANAGE),
        Err(Status::BadHandle)
    );
    let v = w.vmo(1, JobId::ROOT);
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, v, Rights::MANAGE),
        Err(Status::WrongType)
    );
    // a thread object of process 2 that has a live kernel thread, and one that has ended
    let (th, tobj) = {
        let p = w.procs.get_mut(&1).unwrap();
        ipc::thread_object_create_in(&mut w.objs, &mut p.handles, 2, &mut W::q(&mut w.jobs))
            .unwrap()
    };
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, th.0, Rights::MANAGE),
        Err(Status::BadState),
        "never started"
    );
    w.procs.get_mut(&2).unwrap().thread_objs.insert(other, tobj);
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, th.0, Rights::MANAGE),
        Ok((other, 2))
    );
    let weak = narrow(&mut w, th.0, Rights::INSPECT);
    assert_eq!(
        ksys::thread_target(&w.objs, &w.procs, 1, me_tid, weak.0, Rights::MANAGE),
        Err(Status::AccessDenied)
    );
    // set_sched: range of the priority and flags, then the job's maximum
    let a = |prio, flags| [th.0, prio, flags, 0, 0, 0];
    let jobs = &w.jobs;
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(0, 0)),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(64, 0)),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(5, 2)),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(5, 1)),
        Ok((other, 5, 1))
    );
    let low = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.jobs
        .set_sched(
            low,
            kcore::job::SchedLimits {
                max_prio: 5,
                ..w.jobs.sched(low).unwrap()
            },
        )
        .unwrap();
    w.procs.get_mut(&2).unwrap().job = low;
    let jobs = &w.jobs;
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(6, 0)),
        Err(Status::AccessDenied),
        "above the job's maximum"
    );
    assert_eq!(
        ksys::thread_set_sched_target(&w.objs, &w.procs, jobs, 1, me_tid, a(5, 0)),
        Ok((other, 5, 0))
    );
}

#[test]
fn job_set_sched_rows() {
    let (mut w, _) = world();
    let sub = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let jh = job_handle(&mut w, sub);
    let call = |w: &mut W, h: u64, a1: u64, a2: u64, a3: u64| {
        ksys::job_set_sched(&w.objs, &w.procs, &mut w.jobs, 1, [h, a1, a2, a3, 0, 0])
    };
    assert_eq!(call(&mut w, jh, 64, 0, 1), Err(Status::InvalidArgs));
    assert_eq!(call(&mut w, jh, 5, 0, 65537), Err(Status::InvalidArgs));
    assert_eq!(
        call(&mut w, jh, 5, 0, 0),
        Err(Status::InvalidArgs),
        "a zero weight is not a weight"
    );
    assert_eq!(call(&mut w, 0x999, 5, 0, 1), Err(Status::BadHandle));
    let weak = narrow(&mut w, jh, Rights::MANAGE).0;
    assert_eq!(
        call(&mut w, weak, 5, 0, 1),
        Err(Status::AccessDenied),
        "SET_LIMITS is needed"
    );
    assert_eq!(call(&mut w, jh, 5, 0, 100), Ok(sub));
    assert_eq!(w.jobs.sched(sub).unwrap().max_prio, 5);
    // a child cannot exceed its parent
    let child = w.jobs.create(sub, Limits::UNLIMITED).unwrap();
    let ch = job_handle(&mut w, child);
    assert_eq!(call(&mut w, ch, 6, 0, 1), Err(Status::AccessDenied));
    assert_eq!(
        call(&mut w, jh, 5, 200_000_000, 1),
        Err(Status::InvalidArgs),
        "a budget over the 100 ms window"
    );
}

// ---- MM6: vmo_decommit and vmo_set_size ----

/// Fault a page of process `pid` in (a write through its address space maps it).
fn touch(w: &mut W, pid: u64, va: u64) {
    let t = w.procs.get_mut(&pid).unwrap();
    copy_to_user(&mut t.aspace, &mut w.mm, &mut w.vmos, &mut w.jobs, va, b"x").unwrap();
}

#[test]
fn decommit_takes_the_pages_from_every_mapping_and_asks_for_the_shootdown_only_when_something_was_mapped(
) {
    let (mut w, ph) = world();
    let ph1 = w.handle_to(1);
    let (v, id) = w.vmo_and_id(4, JobId::ROOT);
    // committed but mapped nowhere: nothing to drop, no shootdown
    w.vmos.commit(&mut w.mm, &mut w.jobs, id, 0, 4).unwrap();
    let before = w.mm.db.free_count();
    w.ok(nr::VMO_DECOMMIT, [v, 0, 2 * 4096, 0, 0, 0]);
    assert_eq!(w.mm.db.free_count(), before + 2, "two frames came back");
    assert_eq!(
        (w.tlb.shootdowns, w.tlb.flushes),
        (0, 0),
        "no translation existed: no shootdown"
    );
    // mapped and touched in two processes of the VMO's own job: both lose the pages, one shootdown
    w.ok(nr::PROCESS_MAP, [ph1, v, VA, 0, 4 * 4096, W | R]);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, W | R]);
    touch(&mut w, 1, VA + 2 * 4096);
    touch(&mut w, 2, VA + 3 * 4096);
    let jobs_before = w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    w.ok(nr::VMO_DECOMMIT, [v, 2 * 4096, 2 * 4096, 0, 0, 0]);
    assert_eq!(w.tlb.shootdowns, 1);
    assert_eq!(w.tlb.flushes, 1);
    for pid in [1, 2] {
        let a = &w.procs[&pid].aspace;
        assert!(a.table().translate(&w.mm, VA + 2 * 4096).is_none());
        assert!(a.table().translate(&w.mm, VA + 3 * 4096).is_none());
    }
    assert_eq!(
        jobs_before - w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
        2,
        "the VMO's job is credited for the two pages that were committed"
    );
}

#[test]
fn a_mapping_in_another_job_stops_a_decommit_and_a_shrink_but_not_a_grow() {
    let (mut w, ph) = world();
    // process 2 lives in another job than the VMO's
    let other = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.procs.get_mut(&2).unwrap().job = other;
    let (v, id) = w.vmo_and_id(4, JobId::ROOT);
    w.vmos.set_resizable(id);
    w.vmos.commit(&mut w.mm, &mut w.jobs, id, 0, 4).unwrap();
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, R]);
    assert_eq!(
        w.err(nr::VMO_DECOMMIT, [v, 0, 4096, 0, 0, 0]),
        Status::BadState,
        "V1: a stranger's mapping"
    );
    assert_eq!(
        w.err(nr::VMO_SET_SIZE, [v, 2 * 4096, 0, 0, 0, 0]),
        Status::BadState,
        "a shrink under it"
    );
    assert_eq!(w.vmos.pages(id), Some(4), "nothing changed");
    assert_eq!(w.vmos.committed_pages(id), Some(4));
    w.ok(nr::VMO_SET_SIZE, [v, 8 * 4096, 0, 0, 0, 0]); // growing takes nothing
    assert_eq!(w.vmos.pages(id), Some(8));
    // a range the stranger does not map is fine
    w.ok(nr::VMO_DECOMMIT, [v, 5 * 4096, 4096, 0, 0, 0]);
    // once the stranger has unmapped, the pages can go
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    w.ok(nr::VMO_DECOMMIT, [v, 0, 4 * 4096, 0, 0, 0]);
    assert_eq!(w.vmos.committed_pages(id), Some(0));
}

#[test]
fn set_size_checks_resizability_before_the_size() {
    let (mut w, _) = world();
    let fixed = w.vmo(2, JobId::ROOT);
    assert_eq!(
        w.err(nr::VMO_SET_SIZE, [fixed, 100, 0, 0, 0, 0]),
        Status::AccessDenied,
        "not resizable, whatever the size"
    );
    assert_eq!(
        w.err(nr::VMO_SET_SIZE, [fixed, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    let (v, id) = w.vmo_and_id(2, JobId::ROOT);
    w.vmos.set_resizable(id);
    assert_eq!(
        w.err(nr::VMO_SET_SIZE, [v, 100, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_SET_SIZE, [v, 0, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_DECOMMIT, [v, 0, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_DECOMMIT, [v, 4096, 2 * 4096, 0, 0, 0]),
        Status::InvalidArgs,
        "past the end"
    );
}

#[test]
fn process_map_anywhere_picks_free_places_that_do_not_overlap_and_returns_them() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    let first = w
        .ok(nr::PROCESS_MAP, [ph, v, 0, 0, 2 * 4096, W | R])
        .0
        .expect("the address comes back");
    let second = w
        .ok(nr::PROCESS_MAP, [ph, v, 0, 0, 2 * 4096, R])
        .0
        .expect("and again");
    for at in [first, second] {
        assert!(
            at.is_multiple_of(4096) && (0x1_0000_0000..abi::user::STACK_ARENA_BASE).contains(&at),
            "inside the arena: {at:#x}"
        );
    }
    assert!(
        first.abs_diff(second) >= 2 * 4096,
        "two mappings do not overlap"
    );
    // the mappings are there (touching them works), and an explicit address still returns nothing
    touch(&mut w, 2, first);
    assert_eq!(w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4096, R]).0, None);
    let _ = USER_BASE;
}

// ---- MM7: ranges, protect, info ----

/// A handle in process 1 to the same VMO as `h` with only `rights`.
fn weaker(w: &mut W, h: u64, rights: Rights) -> u64 {
    let p = w.procs.get_mut(&1).unwrap();
    p.handles
        .duplicate(&mut w.objs, abi::Handle(h), rights)
        .unwrap()
        .0
}

/// A page of process 1's own memory for the info records.
fn info_buffer(w: &mut W) -> u64 {
    let me = w.handle_to(1);
    let own = w.vmo(1, JobId::ROOT);
    let buf = 0x70_0000u64;
    w.ok(nr::PROCESS_MAP, [me, own, buf, 0, 4096, W | R]);
    buf
}

fn info(w: &mut W, buf: u64, ph: u64, at: u64) -> ([u64; 6], u64) {
    let next = w
        .ok(nr::PROCESS_MAP_INFO, [ph, at, buf, 0, 0, 0])
        .0
        .unwrap();
    let mut rec = [0u64; 6];
    let t = w.procs.get_mut(&1).unwrap();
    let bytes =
        copy_from_user(&mut t.aspace, &mut w.mm, &mut w.vmos, &mut w.jobs, buf, 48).unwrap();
    for (i, r) in rec.iter_mut().enumerate() {
        *r = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
    }
    (rec, next)
}

#[test]
fn unmap_with_a_length_splits_regions_credits_and_asks_for_the_shootdown() {
    let (mut w, ph) = world();
    let v = w.vmo(8, JobId::ROOT);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 8 * 4096, W | R]);
    touch(&mut w, 2, VA + 3 * 4096);
    assert_eq!(w.procs[&2].aspace.region_count(), 1);
    // the middle: two regions are left, the hole is a hole
    w.ok(nr::PROCESS_UNMAP, [ph, VA + 2 * 4096, 4 * 4096, 0, 0, 0]);
    assert_eq!(w.procs[&2].aspace.region_count(), 2);
    assert!(
        w.procs[&2]
            .aspace
            .table()
            .translate(&w.mm, VA + 3 * 4096)
            .is_none(),
        "the touched page went"
    );
    assert_eq!(w.tlb.shootdowns, 1);
    // a range that is all hole, and one that overlaps a hole and a region, are fine
    w.ok(nr::PROCESS_UNMAP, [ph, VA + 2 * 4096, 2 * 4096, 0, 0, 0]);
    w.ok(nr::PROCESS_UNMAP, [ph, VA + 7 * 4096, 4 * 4096, 0, 0, 0]);
    assert_eq!(
        w.procs[&2].aspace.region_count(),
        2,
        "the tail went, the head stayed: {:?}",
        w.procs[&2].aspace.regions_info()
    );
    // errors: unaligned, wrapping
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [ph, VA + 1, 4096, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [ph, VA, 100, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::PROCESS_UNMAP, [ph, u64::MAX - 4095, 8192, 0, 0, 0]),
        Status::InvalidArgs
    );
    // the original form still removes the region that starts at the address, whole
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    assert_eq!(
        w.procs[&2].aspace.region_count(),
        1,
        "the head went, the one page of the tail stayed"
    );
}

#[test]
fn protect_changes_rights_in_place_within_the_ceiling_and_pays_for_what_it_must() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    let ro = weaker(&mut w, v, Rights(Rights::READ.0 | Rights::MAP.0));
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, R | W]);
    touch(&mut w, 2, VA);
    // a downgrade: write goes, the other harts are told
    w.ok(nr::PROCESS_PROTECT, [ph, VA, 4 * 4096, R, 0, 0]);
    assert_eq!(
        (w.tlb.shootdowns, w.tlb.flushes),
        (1, 1),
        "a downgrade is shot down"
    );
    let pte = w.procs[&2]
        .aspace
        .table()
        .translate(&w.mm, VA)
        .unwrap()
        .perms;
    assert!(pte.read && !pte.write, "the page table says read-only");
    // an upgrade within the ceiling needs no shootdown
    w.ok(nr::PROCESS_PROTECT, [ph, VA, 4 * 4096, R | W, 0, 0]);
    assert_eq!(w.tlb.shootdowns, 1);
    // execute needs EXECUTE on the VMO handle that mapped it (the full-rights handle has it), and makes the pages visible to fetch
    w.ok(nr::PROCESS_PROTECT, [ph, VA, 2 * 4096, R | X, 0, 0]);
    assert_eq!(
        w.tlb.icache_pages, 1,
        "one mapped page became executable (the others fault in later)"
    );
    // errors
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA, 4096, W | X, 0, 0]),
        Status::InvalidArgs,
        "write with execute"
    );
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA, 4096, 8, 0, 0]),
        Status::InvalidArgs,
        "an unknown flag"
    );
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA + 1, 4096, R, 0, 0]),
        Status::InvalidArgs,
        "unaligned"
    );
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA, 8 * 4096, R, 0, 0]),
        Status::InvalidArgs,
        "past the mapping: a hole in the range"
    );
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA, 0, R, 0, 0]),
        Status::InvalidArgs,
        "no length"
    );
    // the ceiling: a mapping made through a handle without WRITE can never become writable
    w.ok(nr::PROCESS_MAP, [ph, ro, VA + 16 * 4096, 0, 4 * 4096, R]);
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [ph, VA + 16 * 4096, 4096, R | W, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::PROCESS_PROTECT, [999, VA, 4096, R, 0, 0]),
        Status::BadHandle
    );
}

#[test]
fn map_info_walks_the_regions_and_a_protect_splits_and_merges_them() {
    let (mut w, ph) = world();
    let buf = info_buffer(&mut w);
    let v = w.vmo(8, JobId::ROOT);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 8 * 4096, R | W]);
    let (rec, next) = info(&mut w, buf, ph, 0);
    assert_eq!(rec[0], VA);
    assert_eq!(rec[1], 8 * 4096);
    assert_eq!(
        (rec[2], rec[3]),
        (0b011, 0b111),
        "perms as bits (read, write) and the ceiling (the VMO handle had every right)"
    );
    assert_eq!(rec[5], 0, "the VMO offset");
    assert_eq!(next, 0, "no region after it");
    // protect the middle: three regions, the pieces know their offsets
    w.ok(nr::PROCESS_PROTECT, [ph, VA + 2 * 4096, 3 * 4096, R, 0, 0]);
    let (a, n1) = info(&mut w, buf, ph, VA);
    assert_eq!((a[0], a[1], a[2]), (VA, 2 * 4096, 0b011));
    assert_eq!(n1, VA + 2 * 4096);
    let (b, n2) = info(&mut w, buf, ph, n1);
    assert_eq!(
        (b[0], b[1], b[2], b[5]),
        (VA + 2 * 4096, 3 * 4096, 0b001, 2 * 4096)
    );
    assert_eq!(n2, VA + 5 * 4096);
    // protect it back: the three merge into one again
    w.ok(
        nr::PROCESS_PROTECT,
        [ph, VA + 2 * 4096, 3 * 4096, R | W, 0, 0],
    );
    assert_eq!(w.procs[&2].aspace.region_count(), 1);
    // an address in a hole reports the next region; past the last, nothing
    let (c, _) = info(&mut w, buf, ph, VA + 8 * 4096);
    assert_eq!(c[1], 0, "no region at or after");
}

#[test]
fn the_tables_of_a_split_mapping_stay_charged_until_no_region_is_left_in_their_span() {
    let (mut w, ph) = world();
    let v = w.vmo(2, JobId::ROOT);
    let pages = |w: &W| w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    let base = pages(&w);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R | W]);
    let charged = pages(&w) - base;
    assert!(charged > 0);
    // the first page goes: the second stays, and so does the whole charge (its tables are still there)
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 4096, 0, 0, 0]);
    assert_eq!(
        pages(&w) - base,
        charged,
        "nothing is credited while a page of the mapping stays"
    );
    // a mapping folded into its neighbour keeps its own charge too
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4096, R | W]);
    let both = pages(&w) - base;
    w.ok(nr::PROCESS_UNMAP, [ph, VA + 4096, 4096, 0, 0, 0]);
    assert!(
        pages(&w) - base > 0,
        "the charge of the mapping that stayed is still there: {both}"
    );
    // the last page goes: everything comes back
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 4096, 0, 0, 0]);
    assert_eq!(
        pages(&w),
        base,
        "no region left: the tables' charge is credited"
    );
}

#[test]
fn the_range_calls_stay_in_the_user_half_and_anywhere_checks_its_arguments_first() {
    let (mut w, ph) = world();
    let v = w.vmo(2, JobId::ROOT);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R | W]);
    for (name, at, len) in [
        ("below the user range", 0u64, 4096u64),
        ("the kernel half", 0xffff_ffc0_0000_0000, 4096),
        ("over the top", TOP - 4096, 8192),
        ("wrapping", u64::MAX - 4095, 8192),
    ] {
        assert_eq!(
            w.err(nr::PROCESS_UNMAP, [ph, at, len, 0, 0, 0]),
            Status::InvalidArgs,
            "unmap {name}"
        );
        assert_eq!(
            w.err(nr::PROCESS_PROTECT, [ph, at, len, R, 0, 0]),
            Status::InvalidArgs,
            "protect {name}"
        );
    }
    // ANYWHERE: a bad flag, W with X, an unaligned offset, past the VMO's end: all InvalidArgs, and none takes entropy or a place
    let spent = w.tlb.randoms;
    for (name, a) in [
        ("bad flag", [ph, v, 0, 0, 4096, 8]),
        ("write with execute", [ph, v, 0, 0, 4096, W | X]),
        ("unaligned offset", [ph, v, 0, 1, 4096, R]),
        ("past the VMO", [ph, v, 0, 0, 3 * 4096, R]),
        ("bigger than the arena", [ph, v, 0, 0, 1 << 40, R]),
    ] {
        assert_eq!(
            w.err(nr::PROCESS_MAP, a),
            Status::InvalidArgs,
            "anywhere {name}"
        );
    }
    assert_eq!(
        w.tlb.randoms, spent,
        "a refused ANYWHERE does not draw from the pool"
    );
}

// ---- MM8: vmo_create_child and process_clone_region ----

fn byte(w: &mut W, pid: u64, va: u64) -> u8 {
    let t = w.procs.get_mut(&pid).unwrap();
    let b = copy_from_user(&mut t.aspace, &mut w.mm, &mut w.vmos, &mut w.jobs, va, 1).unwrap()[0];
    ksys::cow_sync(&mut w.procs, &mut w.mm, &mut w.vmos, &mut w.tlb);
    b
}

fn put(w: &mut W, pid: u64, va: u64, b: u8) {
    let t = w.procs.get_mut(&pid).unwrap();
    copy_to_user(&mut t.aspace, &mut w.mm, &mut w.vmos, &mut w.jobs, va, &[b]).unwrap();
    // (what the kernel does after every call and fault)
    ksys::cow_sync(&mut w.procs, &mut w.mm, &mut w.vmos, &mut w.tlb);
}

#[test]
fn a_snapshot_child_keeps_the_contents_as_they_were_and_the_source_write_protects_and_copies() {
    let (mut w, ph) = world();
    let ph1 = w.handle_to(1);
    let (v, id) = w.vmo_and_id(4, JobId::ROOT);
    w.ok(nr::PROCESS_MAP, [ph1, v, VA, 0, 4 * 4096, W | R]);
    put(&mut w, 1, VA, 7);
    put(&mut w, 1, VA + 4096, 8);
    let shoot = w.tlb.shootdowns;
    let (child, _) = w.ok(
        nr::VMO_CREATE_CHILD,
        [v, 0, 4 * 4096, 0, abi::child_flags::SNAPSHOT, 0],
    );
    let child = child.unwrap();
    assert_eq!(
        w.tlb.shootdowns,
        shoot + 1,
        "the source's writable translations are shot down"
    );
    assert!(
        !w.procs[&1]
            .aspace
            .table()
            .translate(&w.mm, VA)
            .unwrap()
            .perms
            .write,
        "and no longer writable"
    );
    assert_eq!(
        w.vmos.committed_pages(id),
        Some(0),
        "the pages moved to the hidden node"
    );
    // the child, mapped in the other process, sees the old bytes; a write through the source does not reach it
    w.ok(nr::PROCESS_MAP, [ph, child, VA, 0, 4 * 4096, W | R]);
    assert_eq!(byte(&mut w, 2, VA), 7);
    put(&mut w, 1, VA, 70);
    assert_eq!(byte(&mut w, 1, VA), 70, "the source sees its own write");
    assert_eq!(byte(&mut w, 2, VA), 7, "the child still sees the old page");
    // and a write through the child does not reach the source
    put(&mut w, 2, VA + 4096, 80);
    assert_eq!(byte(&mut w, 2, VA + 4096), 80);
    assert_eq!(byte(&mut w, 1, VA + 4096), 8);
    // a member gives a page back: it reads zero afterwards (and the other side is untouched); the source can too
    w.ok(nr::VMO_DECOMMIT, [child, 4096, 4096, 0, 0, 0]);
    assert_eq!(byte(&mut w, 2, VA + 4096), 0, "the child's page is gone");
    assert_eq!(
        byte(&mut w, 1, VA + 4096),
        8,
        "the source still has its own"
    );
    w.ok(nr::VMO_DECOMMIT, [v, 0, 4096, 0, 0, 0]);
    assert_eq!(byte(&mut w, 1, VA), 0);
    assert_eq!(
        byte(&mut w, 2, VA),
        7,
        "and the child still has what the snapshot showed"
    );
    // a page nobody wrote reads zero in both, and the charge is exact: frames in use are the jobs' pages (no tables charged to the job's pages here)
    assert_eq!(byte(&mut w, 1, VA + 3 * 4096), 0);
    assert_eq!(byte(&mut w, 2, VA + 3 * 4096), 0);
}

#[test]
fn a_child_is_refused_when_it_would_break_a_rule() {
    let (mut w, ph) = world();
    let ph1 = w.handle_to(1);
    let other = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let (v, id) = w.vmo_and_id(4, JobId::ROOT);
    let snap = abi::child_flags::SNAPSHOT;
    let private = abi::child_flags::PRIVATE;
    for bad_flags in [0, snap | private, 4] {
        assert_eq!(
            w.err(nr::VMO_CREATE_CHILD, [v, 0, 4096, 0, bad_flags, 0]),
            Status::InvalidArgs,
            "exactly one of SNAPSHOT or PRIVATE"
        );
    }
    assert_eq!(
        w.err(nr::VMO_CREATE_CHILD, [v, 1, 4096, 0, private, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_CREATE_CHILD, [v, 3 * 4096, 2 * 4096, 0, private, 0]),
        Status::InvalidArgs,
        "past the end"
    );
    // a job handle other than the source's job: a SNAPSHOT is refused, a PRIVATE child may be charged elsewhere
    let jh = {
        let p = w.procs.get_mut(&1).unwrap();
        let obj = w.objs.create(ipc::KObj::Job(other));
        p.handles.insert(&mut w.objs, obj, Rights::ALL).unwrap().0
    };
    assert_eq!(
        w.err(nr::VMO_CREATE_CHILD, [v, 0, 4096, jh, snap, 0]),
        Status::AccessDenied,
        "COW2: the pages never change payer"
    );
    w.ok(nr::VMO_CREATE_CHILD, [v, 0, 4096, jh, private, 0]);
    // V1: a SNAPSHOT is refused while a process of another job maps the source, and nothing changes
    w.ok(nr::PROCESS_MAP, [ph1, v, VA, 0, 4 * 4096, W | R]);
    let stranger = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.procs.get_mut(&2).unwrap().job = stranger;
    w.ok(
        nr::PROCESS_CLONE_REGION,
        [ph1, ph, VA, abi::clone_mode::SHARE, 0, 0],
    );
    assert_eq!(
        w.err(nr::VMO_CREATE_CHILD, [v, 0, 4096, 0, snap, 0]),
        Status::BadState,
        "the stranger's mapping is promised that a write never charges the VMO's job"
    );
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    w.procs.get_mut(&2).unwrap().job = JobId::ROOT;
    // a call that fails for want of a handle slot changes nothing (no hidden node, the source stays as it was)
    let vmos_before = w.vmos.count();
    let full = {
        let p = w.procs.get_mut(&1).unwrap();
        let mut n = 0;
        while p.handles.room() > 0 {
            let obj = w.objs.create(ipc::KObj::Job(JobId::ROOT));
            p.handles.insert(&mut w.objs, obj, Rights::ALL).unwrap();
            n += 1;
        }
        n
    };
    assert!(full > 0);
    assert_eq!(
        w.err(nr::VMO_CREATE_CHILD, [v, 0, 4096, 0, snap, 0]),
        Status::NoMemory
    );
    assert_eq!(w.vmos.count(), vmos_before, "no VMO was made");
    assert!(!w.vmos.has_parent(id), "the source was not restructured");
    w.vmos.audit_families().unwrap();
}

#[test]
fn process_clone_region_shares_or_snapshots_a_region_into_another_process() {
    let (mut w, ph) = world();
    let ph1 = w.handle_to(1);
    let (v, _) = w.vmo_and_id(4, JobId::ROOT);
    w.ok(nr::PROCESS_MAP, [ph1, v, VA, 0, 4 * 4096, W | R]);
    put(&mut w, 1, VA, 5);
    // SHARE: the same VMO, both see each other's writes
    w.ok(
        nr::PROCESS_CLONE_REGION,
        [ph1, ph, VA, abi::clone_mode::SHARE, 0, 0],
    );
    assert_eq!(byte(&mut w, 2, VA), 5);
    put(&mut w, 2, VA, 6);
    assert_eq!(byte(&mut w, 1, VA), 6);
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    // SNAPSHOT: each has its own from here
    w.ok(
        nr::PROCESS_CLONE_REGION,
        [ph1, ph, VA, abi::clone_mode::SNAPSHOT, 0, 0],
    );
    assert_eq!(byte(&mut w, 2, VA), 6);
    put(&mut w, 1, VA, 9);
    put(&mut w, 2, VA + 4096, 10);
    assert_eq!(byte(&mut w, 1, VA), 9);
    assert_eq!(byte(&mut w, 2, VA), 6, "the child kept the old page");
    assert_eq!(byte(&mut w, 1, VA + 4096), 0);
    assert_eq!(byte(&mut w, 2, VA + 4096), 10);
    // refusals: nothing at the address; the target already has a mapping there; a bad mode
    assert_eq!(
        w.err(
            nr::PROCESS_CLONE_REGION,
            [ph1, ph, VA + 0x10_0000, abi::clone_mode::SHARE, 0, 0]
        ),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(
            nr::PROCESS_CLONE_REGION,
            [ph1, ph, VA, abi::clone_mode::SHARE, 0, 0]
        ),
        Status::InvalidArgs,
        "the target has a region there"
    );
    assert_eq!(
        w.err(nr::PROCESS_CLONE_REGION, [ph1, ph, VA, 0, 0, 0]),
        Status::InvalidArgs
    );
    // everything goes back: the processes' regions, then the VMOs
    w.ok(nr::PROCESS_UNMAP, [ph, VA, 0, 0, 0, 0]);
    w.ok(nr::PROCESS_UNMAP, [ph1, VA, 0, 0, 0, 0]);
}
