//! `process_start` on the host (TS3c): the error rows in order, the channel end that moves into the new process as its handle 1, and nothing
//! changed on any failure.
use abi::rights::Rights;
use abi::status::Status;
use abi::user::{BASE, TOP};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Quota};
use kcore::job::{JobId, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;
use ksys::{ProcState, Process, UserState};

mod common;
use common::FakeMem;

const CODE: u64 = 0x40_0000;

fn process(mm: &mut Mm<FakeMem>, kernel: &PageTable, job: JobId) -> Process {
    Process {
        live_tids: Default::default(),
        aspace: std::sync::Arc::new(klock::aslock::AsCell::new(
            1,
            AddressSpace::new(PageTable::new_with_kernel_half(mm, kernel).unwrap()),
        )),
        handles: HandleTable::new(64),
        tag: kcore::asid::AsTag { asid: 1, gen: 1 },
        vmos: Vec::new(),
        entry: 0,
        arg: 0,
        exit_code: None,
        kernel_end: None,
        exc_channel: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 0,
        job,
        table_charge: kcore::fmap::FMap::new(),
        charge_end: kcore::fmap::FMap::new(),
        stacks: Default::default(),
        stack_hint: 0,
        privileged: false,
        libsys: Default::default(),
    }
}

struct W {
    us: UserState,
    jobs: kcore::job::Jobs,
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
    ksys::put_process(&mut us.procs, 1, process(&mut mm, &kernel, JobId::ROOT));
    ksys::put_process(&mut us.procs, 2, process(&mut mm, &kernel, JobId::ROOT));
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let code = vmos.create(&mut jobs, 1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .held_mut()
        .map_region(&mut vmos, CODE, 1, code, 0, Perms::RX.user())
        .unwrap();
    let data = vmos.create(&mut jobs, 1);
    us.procs
        .get_mut(&2)
        .unwrap()
        .aspace
        .held_mut()
        .map_region(&mut vmos, CODE + 0x10_000, 1, data, 0, Perms::RW.user())
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
    us.procs.get_mut(&2).unwrap().state = ProcState::Created;
    W { us, jobs, ph: h.0 }
}

impl W {
    /// A bootstrap channel end in process 1 (both ends are its own), with the rights a creator holds.
    fn channel(&mut self) -> u64 {
        let p = self.us.procs.get_mut(&1).unwrap();
        let (a, _b) = ipc::channel_create_in(
            &mut self.us.objs,
            &mut p.handles,
            &mut Quota {
                jobs: &mut self.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap();
        a.0
    }
    fn start(
        &mut self,
        ph: u64,
        entry: u64,
        sp: u64,
        bh: u64,
        arg: u64,
    ) -> Result<(usize, u8), Status> {
        ksys::process_start(
            &mut self.us,
            &mut self.jobs,
            &mut ksys::NoMap,
            1,
            [ph, entry, sp, bh, arg, 0],
        )
        .map(|s| (s.token, s.prio))
    }
    fn threads(&self, job: JobId) -> u64 {
        self.jobs.usage(job).unwrap().get(Resource::Threads)
    }
}

const SP: u64 = BASE + 0x10_000;

#[test]
fn the_channel_end_moves_in_as_handle_one_and_the_process_runs() {
    let mut w = world();
    let bh = w.channel();
    let (token, prio) = w.start(w.ph, CODE, SP, bh, 99).unwrap();
    assert_eq!(prio, 10);
    let t = &w.us.procs[&2];
    assert_eq!(
        (t.state, t.threads_alive, t.entry, t.arg, t.start_sp),
        (ProcState::Running, 1, CODE, 99, SP)
    );
    assert!(
        t.handles.get(abi::BOOTSTRAP_HANDLE).is_ok(),
        "handle 1 of the new process"
    );
    assert!(
        w.us.procs[&1].handles.get(abi::Handle(bh)).is_err(),
        "the caller no longer holds the end"
    );
    let st = &w.us.starts[&token];
    assert_eq!(
        (st.pid, st.entry, st.sp, st.arg, st.obj),
        (2, CODE, SP, 99, None)
    );
    assert_eq!(w.threads(JobId::ROOT), 1);
    // a process is started once
    let bh2 = w.channel();
    assert_eq!(w.start(w.ph, CODE, SP, bh2, 0), Err(Status::BadState));
}

#[test]
fn the_error_rows_in_order() {
    let mut w = world();
    let bh = w.channel();
    assert_eq!(w.start(0x999, CODE, SP, bh, 0), Err(Status::BadHandle));
    assert_eq!(w.start(w.ph, CODE, SP, 0x999, 0), Err(Status::BadHandle));
    // a channel where a process is wanted, a process where a channel is wanted
    assert_eq!(w.start(bh, CODE, SP, bh, 0), Err(Status::WrongType));
    assert_eq!(w.start(w.ph, CODE, SP, w.ph, 0), Err(Status::WrongType));
    // rights: MANAGE on the process, TRANSFER on the channel end
    let weak_p =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(w.ph), Rights::INSPECT)
            .unwrap();
    assert_eq!(
        w.start(weak_p.0, CODE, SP, bh, 0),
        Err(Status::AccessDenied)
    );
    let weak_c =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(bh), Rights::READ)
            .unwrap();
    assert_eq!(
        w.start(w.ph, CODE, SP, weak_c.0, 0),
        Err(Status::AccessDenied)
    );
    // the entry and the stack
    assert_eq!(
        w.start(w.ph, CODE + 0x10_000, SP, bh, 0),
        Err(Status::InvalidArgs),
        "entry in a data page"
    );
    assert_eq!(
        w.start(w.ph, CODE, SP + 8, bh, 0),
        Err(Status::InvalidArgs),
        "sp unaligned"
    );
    assert_eq!(
        w.start(w.ph, CODE, TOP + 16, bh, 0),
        Err(Status::InvalidArgs),
        "sp above the range"
    );
    // a process that has been started, ended or killed
    w.us.procs.get_mut(&2).unwrap().killed = Some(1);
    assert_eq!(w.start(w.ph, CODE, SP, bh, 0), Err(Status::BadState));
}

#[test]
fn every_refusal_changes_nothing() {
    let mut w = world();
    let bh = w.channel();
    let snapshot = |w: &W| {
        (
            w.threads(JobId::ROOT),
            w.us.procs[&1].handles.len(),
            w.us.procs[&2].handles.len(),
            w.us.procs[&2].state,
            w.us.starts.len(),
        )
    };
    let before = snapshot(&w);
    // the job's thread limit
    let tight = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                threads: 0,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.us.procs.get_mut(&2).unwrap().job = tight;
    assert_eq!(w.start(w.ph, CODE, SP, bh, 0), Err(Status::NoMemory));
    assert_eq!((w.threads(tight), w.threads(JobId::ROOT)), (0, before.0));
    // the system-wide thread limit
    w.us.procs.get_mut(&2).unwrap().job = JobId::ROOT;
    w.us.procs.get_mut(&1).unwrap().threads_alive = abi::limits::MAX_USER_THREADS;
    assert_eq!(w.start(w.ph, CODE, SP, bh, 0), Err(Status::NoMemory));
    w.us.procs.get_mut(&1).unwrap().threads_alive = 0;
    assert_eq!(
        snapshot(&w),
        before,
        "the channel end is still the caller's and the process is as it was"
    );
    // and the retry succeeds: handle 1 was not taken
    w.start(w.ph, CODE, SP, bh, 0).unwrap();
}

/// AB7: a stand-in for the kernel's mapper of the system library.
struct FakeLib {
    maps: u32,
    unmaps: u32,
    fail: Option<Status>,
}

impl ksys::StartMap for FakeLib {
    fn map(
        &mut self,
        _: &mut kcore::aspace::AddressSpace,
        _: &mut kcore::job::Jobs,
    ) -> Result<(ksys::LibRange, u64), Status> {
        if let Some(e) = self.fail {
            return Err(e);
        }
        self.maps += 1;
        Ok((
            ksys::LibRange {
                base: 0x4_0000_0000,
                text_start: 0x4_0000_0000,
                text_end: 0x4_0000_3000,
            },
            0x5000,
        ))
    }
    fn unmap(
        &mut self,
        _: &mut kcore::aspace::AddressSpace,
        _: &mut kcore::job::Jobs,
        _: ksys::LibRange,
        _: u64,
    ) {
        self.unmaps += 1;
    }
}

#[test]
fn the_system_library_is_mapped_recorded_charged_and_passed_in_a2() {
    let mut w = world();
    let bh = w.channel();
    let pages_before = w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages);
    let mut lib = FakeLib {
        maps: 0,
        unmaps: 0,
        fail: None,
    };
    let token = ksys::process_start(
        &mut w.us,
        &mut w.jobs,
        &mut lib,
        1,
        [w.ph, CODE, SP, bh, 0, 0],
    )
    .unwrap()
    .token;
    assert_eq!((lib.maps, lib.unmaps), (1, 0));
    let t = &w.us.procs[&2];
    assert_eq!(t.libsys.base, 0x4_0000_0000);
    assert_eq!(t.libsys.text_end, 0x4_0000_3000);
    assert_eq!(
        w.us.starts[&token].a2, 0x4_0000_0000,
        "the first thread gets the base in a2"
    );
    // the page tables for the span are charged like a `process_map`'s, and recorded to be credited with the process
    assert_eq!(t.table_charge[&0x4_0000_0000], 4);
    assert_eq!(
        w.jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
        pages_before + 4 + ksys::KSTACK_PAGES
    );
}

#[test]
fn a_library_that_cannot_be_mapped_leaves_the_process_as_it_was() {
    let mut w = world();
    let bh = w.channel();
    let mut lib = FakeLib {
        maps: 0,
        unmaps: 0,
        fail: Some(Status::NoMemory),
    };
    let r = ksys::process_start(
        &mut w.us,
        &mut w.jobs,
        &mut lib,
        1,
        [w.ph, CODE, SP, bh, 0, 0],
    );
    assert_eq!(r.err(), Some(Status::NoMemory));
    assert_eq!(w.us.procs[&2].state, ProcState::Created);
    assert!(
        w.us.procs[&1].handles.get(abi::Handle(bh)).is_ok(),
        "the caller still holds the end"
    );
    assert_eq!(w.threads(JobId::ROOT), 0, "no thread is charged");
}
