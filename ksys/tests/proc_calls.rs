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
}
impl Machine for Tlb {
    fn shootdown(&mut self) {
        self.shootdowns += 1;
    }
    fn flush_tlb(&mut self) {
        self.flushes += 1;
    }
}

struct W {
    objs: Kobjs,
    procs: BTreeMap<u32, Process>,
    mm: Mm<FakeMem>,
    vmos: Vmos,
    tlb: Tlb,
}

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

/// Process 1 is the caller, process 2 the target; the caller holds a MANAGE handle to the target (`ph`).
fn world() -> (W, u64) {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut procs = BTreeMap::new();
    procs.insert(1, process(&mut mm, &kernel, JobId::ROOT));
    procs.insert(2, process(&mut mm, &kernel, JobId::ROOT));
    let mut w = W {
        objs: Kobjs::new(),
        procs,
        mm,
        vmos: Vmos::new(),
        tlb: Tlb::default(),
    };
    let ph = w.handle_to(2);
    (w, ph)
}

impl W {
    fn q(vmos: &mut Vmos) -> Quota<'_> {
        Quota {
            jobs: &mut vmos.jobs,
            job: Some(JobId::ROOT),
        }
    }
    /// A handle in process 1 to process `pid`, with all the rights a creator has.
    fn handle_to(&mut self, pid: u32) -> u64 {
        let p = self.procs.get_mut(&1).unwrap();
        let (h, _) = ipc::process_object_create_in(
            &mut self.objs,
            &mut p.handles,
            pid,
            false,
            &mut Self::q(&mut self.vmos),
        )
        .unwrap();
        u64::from(h.0)
    }
    fn vmo(&mut self, pages: u64, job: JobId) -> u64 {
        let id = self.vmos.create_in(pages, job);
        let p = self.procs.get_mut(&1).unwrap();
        u64::from(
            ipc::vmo_object_create_in(
                &mut self.objs,
                &mut p.handles,
                id,
                &mut Self::q(&mut self.vmos),
            )
            .unwrap()
            .0,
        )
    }
    fn call(&mut self, n: usize, a: [u64; 6]) -> Reply {
        let mut c = PCtx {
            objs: &mut self.objs,
            procs: &mut self.procs,
            me: 1,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
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
        machine: &mut w.tlb,
    };
    assert!(proc_call(nr::PROCESS_CREATE, [0; 6], &mut c).is_none());
    assert!(proc_call(nr::THREAD_CREATE, [0; 6], &mut c).is_none());
}

#[test]
fn process_map_maps_into_the_target_and_charges_its_tables() {
    let (mut w, ph) = world();
    let v = w.vmo(4, JobId::ROOT);
    let pages_before = w.vmos.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    w.ok(nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, W | R]);
    let used = w.vmos.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    assert_eq!(
        used - pages_before,
        4,
        "the worst-case table estimate (four pages for four pages of data) is charged to the target's job"
    );
    // the target's memory shows it, the caller's does not
    let t = w.procs.get_mut(&2).unwrap();
    copy_to_user(&mut t.aspace, &mut w.mm, &mut w.vmos, VA + 8, b"hi").unwrap();
    assert_eq!(
        copy_from_user(&mut t.aspace, &mut w.mm, &mut w.vmos, VA + 8, 2).unwrap(),
        b"hi"
    );
    let me = w.procs.get_mut(&1).unwrap();
    assert!(copy_from_user(&mut me.aspace, &mut w.mm, &mut w.vmos, VA + 8, 2).is_err());
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
        u64::from(
            p.handles
                .duplicate(&mut w.objs, abi::Handle(ph as u32), Rights::WAIT)
                .unwrap()
                .0,
        )
    };
    assert_eq!(
        w.err(nr::PROCESS_MAP, ok_args(no_manage, v)),
        Status::AccessDenied
    );
    let ro = {
        let p = w.procs.get_mut(&1).unwrap();
        u64::from(
            p.handles
                .duplicate(
                    &mut w.objs,
                    abi::Handle(v as u32),
                    Rights(Rights::MAP.0 | Rights::READ.0),
                )
                .unwrap()
                .0,
        )
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
        (
            "below the user range",
            [ph, v, USER_BASE - 4096, 0, 4096, R],
        ),
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
    let other = w.vmos.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let v = w.vmo(2, other);
    // charged to a job that is neither the caller's nor the target's: it must already be committed
    assert_eq!(
        w.err(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, R]),
        Status::BadState
    );
    let id = match w
        .objs
        .get(w.procs[&1].handles.get(abi::Handle(v as u32)).unwrap().obj)
    {
        Some(ipc::KObj::Vmo(id)) => *id,
        _ => unreachable!(),
    };
    w.vmos.commit(&mut w.mm, id, 0, 2).unwrap();
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
        .vmos
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
    assert_eq!(w.vmos.jobs.usage(tight).unwrap().get(Resource::Pages), 0);
    assert!(w.procs[&2].table_charge.is_empty());
}

#[test]
fn process_unmap_credits_the_tables_and_asks_for_the_shootdown() {
    let (mut w, ph) = world();
    let v = w.vmo(2, JobId::ROOT);
    let before = w.vmos.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
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
        w.vmos.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
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
        abi::Handle(ph as u32),
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
        u64::from(
            p.handles
                .duplicate(&mut w.objs, abi::Handle(ph as u32), Rights::MANAGE)
                .unwrap()
                .0,
        )
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
        ipc::thread_object_create_in(&mut w.objs, &mut p.handles, 2, &mut W::q(&mut w.vmos))
            .unwrap()
    };
    let (thandle, tobj) = th;
    assert_eq!(
        w.err(nr::THREAD_INFO, [u64::from(thandle.0), 0, 0, 0, 0, 0]),
        Status::BadState,
        "still running"
    );
    ipc::thread_mark_terminated(&mut w.objs, tobj, 3);
    assert_eq!(
        w.ok(nr::THREAD_INFO, [u64::from(thandle.0), 0, 0, 0, 0, 0])
            .0,
        Some(3)
    );
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
        ipc::job_of(
            &w.objs,
            &w.procs[&1].handles,
            abi::Handle(h as u32),
            Rights::JOB
        )
        .unwrap(),
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
        let e = w.procs[&1].handles.get(abi::Handle(h as u32)).unwrap();
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
