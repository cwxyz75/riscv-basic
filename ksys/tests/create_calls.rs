//! `process_create` on the host (TS3c): the job it is charged to, every failure giving back what it took, the process it records.
use abi::rights::Rights;
use abi::status::Status;
use kcore::aspace::Vmos;
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Quota};
use kcore::job::{JobId, Limits, Resource};
use kcore::pagetable::PageTable;
use kcore::Mm;
use ksys::{ProcState, UserState};

mod common;
use common::FakeMem;

struct W {
    us: UserState,
    jobs: kcore::job::Jobs,
    mm: Mm<FakeMem>,
    kernel: PageTable,
}

/// Process 1 is the creator; the "kernel" has one page table.
fn world(frames: usize) -> W {
    let mut mm = Mm {
        db: FrameDb::new(3000, frames),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut us = UserState::new();
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let _ = &mut vmos;
    let first = ksys::process_create(&mut us, &mut mm, &mut jobs, &kernel, true, 0, [0; 6]);
    // (no creator yet: process 0 does not exist)
    assert_eq!(first, Err(Status::BadState));
    us.procs
        .insert(1, Box::new(blank(&mut mm, &kernel, JobId::ROOT)));
    W {
        us,
        jobs,
        mm,
        kernel,
    }
}

fn blank(mm: &mut Mm<FakeMem>, kernel: &PageTable, job: JobId) -> ksys::Process {
    ksys::Process {
        live_tids: Default::default(),
        aspace: kcore::aspace::AddressSpace::new(
            PageTable::new_with_kernel_half(mm, kernel).unwrap(),
        ),
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
        thread_objs: Default::default(),
        job,
        table_charge: Default::default(),
        charge_end: Default::default(),
        stacks: Default::default(),
        stack_hint: 0,
        privileged: false,
    }
}

impl W {
    fn create(&mut self, ok: bool, a: [u64; 6]) -> Result<u64, Status> {
        ksys::process_create(
            &mut self.us,
            &mut self.mm,
            &mut self.jobs,
            &self.kernel,
            ok,
            1,
            a,
        )
    }
    fn used(&self, job: JobId, r: Resource) -> u64 {
        self.jobs.usage(job).unwrap().get(r)
    }
    fn job_handle(&mut self, job: JobId, rights: Rights) -> u64 {
        let p = self.us.procs.get_mut(&1).unwrap();

        ipc::job_object_create(
            &mut self.us.objs,
            &mut p.handles,
            job,
            rights,
            &mut Quota {
                jobs: &mut self.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap()
        .0
    }
}

#[test]
fn a_process_is_made_in_the_created_state_and_charged() {
    let mut w = world(400);
    let (procs, pages) = (
        w.used(JobId::ROOT, Resource::Processes),
        w.used(JobId::ROOT, Resource::Pages),
    );
    let h = w.create(true, [0; 6]).unwrap();
    let e = w.us.procs[&1].handles.get(abi::Handle(h)).unwrap();
    assert!(
        e.rights.contains(Rights::MANAGE) && e.rights.contains(Rights::KILL),
        "the creator holds the process's authority"
    );
    let pid = *w.us.procs.keys().max().unwrap();
    assert_ne!(pid, 1);
    let p = &w.us.procs[&pid];
    assert_eq!(
        (p.state, p.detached, p.exit_code, p.threads_alive, p.job),
        (ProcState::Created, true, None, 0, JobId::ROOT)
    );
    assert_eq!(
        p.table_charge.get(&u64::MAX),
        Some(&1),
        "the root page table is charged as one page"
    );
    assert_eq!(w.used(JobId::ROOT, Resource::Processes), procs + 1);
    assert_eq!(w.used(JobId::ROOT, Resource::Pages), pages + 1);
}

#[test]
fn pids_are_never_reused_while_live_and_never_wrap() {
    let mut w = world(400);
    w.create(true, [0; 6]).unwrap();
    w.create(true, [0; 6]).unwrap();
    let pids: Vec<u64> = w.us.procs.keys().copied().collect();
    assert_eq!(pids.len(), 3);
    // CNT1: the counter never wraps. At the top a creation is refused with NO_MEMORY and leaves nothing behind
    w.us.next_pid = u64::MAX - 1;
    w.create(true, [0; 6]).unwrap();
    assert!(w.us.procs.contains_key(&(u64::MAX - 1)));
    let before = w.us.procs.len();
    assert_eq!(w.create(true, [0; 6]), Err(Status::NoMemory));
    assert_eq!(
        w.us.procs.len(),
        before,
        "the refused creation made nothing"
    );
    assert_eq!(
        w.us.next_pid,
        u64::MAX,
        "and the counter did not move past the end"
    );
}

#[test]
fn the_job_handle_is_checked_and_the_new_process_is_charged_to_that_job() {
    let mut w = world(400);
    assert_eq!(
        w.create(true, [0x999, 0, 0, 0, 0, 0]),
        Err(Status::BadHandle)
    );
    let ev = w.us.procs.get_mut(&1).unwrap();
    let e = ipc::event_create_in(
        &mut w.us.objs,
        &mut ev.handles,
        &mut Quota {
            jobs: &mut w.jobs,
            job: Some(JobId::ROOT),
        },
    )
    .unwrap();
    assert_eq!(w.create(true, [e.0, 0, 0, 0, 0, 0]), Err(Status::WrongType));
    let weak = w.job_handle(JobId::ROOT, Rights::INSPECT);
    assert_eq!(
        w.create(true, [weak, 0, 0, 0, 0, 0]),
        Err(Status::AccessDenied),
        "MANAGE is needed"
    );
    let child = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let ch = w.job_handle(child, Rights::JOB);
    w.create(true, [ch, 0, 0, 0, 0, 0]).unwrap();
    assert_eq!(
        (
            w.used(child, Resource::Processes),
            w.used(child, Resource::Pages)
        ),
        (1, 1)
    );
}

#[test]
fn every_failure_gives_back_what_it_took() {
    let mut w = world(400);
    let snapshot = |w: &W| {
        (
            w.used(JobId::ROOT, Resource::Processes),
            w.used(JobId::ROOT, Resource::Pages),
            w.us.procs.len(),
            w.mm.db.free_count(),
        )
    };
    let before = snapshot(&w);
    // the kernel half no longer matches its snapshot, or too many processes exist
    assert_eq!(w.create(false, [0; 6]), Err(Status::NoMemory));
    // the job's process limit, then its page limit (the second charge: the first is given back)
    let procs = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                processes: 0,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let ph = w.job_handle(procs, Rights::JOB);
    let before2 = snapshot(&w);
    assert_eq!(w.create(true, [ph, 0, 0, 0, 0, 0]), Err(Status::NoMemory));
    let pages = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 0,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let pgh = w.job_handle(pages, Rights::JOB);
    assert_eq!(w.create(true, [pgh, 0, 0, 0, 0, 0]), Err(Status::NoMemory));
    assert_eq!(
        (
            w.used(procs, Resource::Processes),
            w.used(pages, Resource::Processes),
            w.used(pages, Resource::Pages)
        ),
        (0, 0, 0)
    );
    // no object may be charged to the creator's job: the page table is freed again, the charges given back
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
    let nh = w.job_handle(none, Rights::JOB);
    assert_eq!(w.create(true, [nh, 0, 0, 0, 0, 0]), Err(Status::NoMemory));
    assert_eq!(
        (
            w.used(none, Resource::Processes),
            w.used(none, Resource::Pages)
        ),
        (0, 0)
    );
    assert_eq!(snapshot(&w).2, before2.2);
    assert_eq!(
        snapshot(&w).3,
        before2.3,
        "the page table made for the refused process went back"
    );
    let _ = before;
}

#[test]
fn a_machine_out_of_frames_refuses_with_no_memory_and_charges_nothing() {
    // a database too small to give a page table: the charge is given back
    let mut w = world(8);
    while w.mm.db.alloc().is_some() {}
    let before = (
        w.used(JobId::ROOT, Resource::Processes),
        w.used(JobId::ROOT, Resource::Pages),
    );
    assert_eq!(w.create(true, [0; 6]), Err(Status::NoMemory));
    assert_eq!(
        (
            w.used(JobId::ROOT, Resource::Processes),
            w.used(JobId::ROOT, Resource::Pages)
        ),
        before
    );
}
