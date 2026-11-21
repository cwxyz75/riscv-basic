//! Exception delivery on the host (EX2): a fault goes to the process's channel, then its job's and each ancestor's; the handler resolves it once
//! (retry, pass on, kill); a level whose channel is closed or absent is skipped; a process nobody handles for ends as it always did.
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use kcore::aspace::AddressSpace;
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, ExcLevel, Quota};
use kcore::job::{JobId, Limits};
use kcore::pagetable::PageTable;
use kcore::Mm;
use ksys::exception::{self, Fault, Resolved, Target};
use ksys::{ProcState, Process, UserState};
use std::collections::BTreeMap;

mod common;
use common::FakeMem;

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
        exc_channel: None,
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
    jobs: kcore::job::Jobs,
    /// a handle in process 1 (the handler) to process 2 (the one that faults), and to the root job and a child job
    ph: u64,
    root: u64,
    child: JobId,
    jh: u64,
}

fn world() -> W {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut us = UserState::new();
    let mut jobs = kcore::job::Jobs::new();
    let child = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    us.procs
        .insert(1, Box::new(process(&mut mm, &kernel, JobId::ROOT)));
    us.procs
        .insert(2, Box::new(process(&mut mm, &kernel, child)));
    fn q(jobs: &mut kcore::job::Jobs) -> Quota<'_> {
        Quota {
            jobs,
            job: Some(JobId::ROOT),
        }
    }
    let p1 = us.procs.get_mut(&1).unwrap();
    let (ph, _) =
        ipc::process_object_create_in(&mut us.objs, &mut p1.handles, 2, false, &mut q(&mut jobs))
            .unwrap();
    let root = ipc::job_object_create(
        &mut us.objs,
        &mut p1.handles,
        JobId::ROOT,
        Rights::JOB,
        &mut q(&mut jobs),
    )
    .unwrap()
    .0;
    let jh = ipc::job_object_create(
        &mut us.objs,
        &mut p1.handles,
        child,
        Rights::JOB,
        &mut q(&mut jobs),
    )
    .unwrap()
    .0;
    W {
        us,
        jobs,
        ph: ph.0,
        root,
        child,
        jh,
    }
}

const FAULT: Fault = Fault {
    kind: 2, // ILLEGAL
    pc: 0x40_0000,
    address: 0,
    cause: 2,
};

impl W {
    fn read(&mut self, ch: u64) -> (Vec<u8>, Vec<Handle>) {
        let p1 = self.us.procs.get_mut(&1).unwrap();
        let r = ipc::channel_read_in(
            &mut self.us.objs,
            &mut p1.handles,
            Handle(ch),
            64,
            4,
            &mut self.jobs,
        )
        .unwrap();
        (r.data, r.handles)
    }
}

#[test]
fn a_fault_goes_to_the_process_channel_and_is_resolved_once() {
    let mut w = world();
    let ch =
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).unwrap();
    assert!(exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        5,
        &FAULT,
        None
    ));
    exception::stop_thread(&mut w.us, 5);
    // (the thread parks as it returns to user mode)
    let _ = w.us.suspends.get_mut(&5).unwrap().return_to_user();
    let (data, handles) = w.read(ch);
    assert_eq!(data.len(), abi::exception::MESSAGE_BYTES);
    let words: Vec<u64> = data
        .chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(
        words,
        vec![2, 5, 0x40_0000, 0, 2],
        "kind, tid, pc, address, cause"
    );
    assert_eq!(handles.len(), 2, "the thread and the exception");
    let eh = handles[1].0;
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, eh, 9),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, eh, abi::exception::RETRY),
        Ok(Resolved::Retry { tid: 5, wake: true })
    );
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, eh, abi::exception::RETRY),
        Err(Status::BadState),
        "an exception is resolved once"
    );
    assert!(!w.us.suspends.contains_key(&5), "the thread is released");
}

#[test]
fn next_reaches_the_job_then_the_ancestors_and_with_none_left_the_process_ends_as_it_always_did() {
    let mut w = world();
    let pch =
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).unwrap();
    let jch = exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Job(w.jh)).unwrap();
    let rch = exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Job(w.root)).unwrap();
    assert!(exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        5,
        &FAULT,
        None
    ));
    exception::stop_thread(&mut w.us, 5);
    let (_, h) = w.read(pch);
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, h[1].0, abi::exception::NEXT),
        Ok(Resolved::Passed)
    );
    let (_, h) = w.read(jch);
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, h[1].0, abi::exception::NEXT),
        Ok(Resolved::Passed),
        "the job's channel passes it to the root's"
    );
    let (_, h) = w.read(rch);
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, h[1].0, abi::exception::NEXT),
        Ok(Resolved::Kill {
            pid: 2,
            code: -1002
        }),
        "no level left: the usual exit code"
    );
    let _ = w.child;
}

#[test]
fn a_level_that_cannot_take_it_is_skipped_and_with_no_channel_anywhere_nothing_is_delivered() {
    let mut w = world();
    // no channel: not delivered
    assert!(!exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        5,
        &FAULT,
        None
    ));
    // a process channel whose user end closed is skipped: the job's takes it
    let pch =
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).unwrap();
    let jch = exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Job(w.jh)).unwrap();
    {
        let p1 = w.us.procs.get_mut(&1).unwrap();
        p1.handles.close(&mut w.us.objs, Handle(pch)).unwrap();
    }
    assert!(exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        5,
        &FAULT,
        None
    ));
    let (_, h) = w.read(jch);
    assert_eq!(h.len(), 2, "the job's channel got it");
    // nothing is left behind by the level that refused: the kernel's table has the channel ends and nothing else of the refused offer
    let before = w.us.ktable.objects().count();
    assert!(!exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        6,
        &FAULT,
        Some(ExcLevel::Job(JobId::ROOT))
    ));
    assert_eq!(w.us.ktable.objects().count(), before);
}

#[test]
fn a_second_channel_is_refused_while_the_first_is_open_and_replaces_it_once_it_is_closed() {
    let mut w = world();
    let ch =
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).unwrap();
    assert_eq!(
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)),
        Err(Status::BadState)
    );
    w.us.procs
        .get_mut(&1)
        .unwrap()
        .handles
        .close(&mut w.us.objs, Handle(ch))
        .unwrap();
    assert!(exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).is_ok());
    // MANAGE is needed
    let weak = {
        let p1 = w.us.procs.get_mut(&1).unwrap();
        p1.handles
            .duplicate(&mut w.us.objs, Handle(w.ph), Rights::WAIT)
            .unwrap()
    };
    assert_eq!(
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(weak.0)),
        Err(Status::AccessDenied)
    );
}

#[test]
fn closing_the_last_exception_handle_is_next_and_a_dead_process_discards_its_exceptions() {
    let mut w = world();
    let pch =
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)).unwrap();
    let jch = exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Job(w.jh)).unwrap();
    assert!(exception::deliver(
        &mut w.us,
        &mut w.jobs,
        2,
        5,
        &FAULT,
        None
    ));
    let (_, h) = w.read(pch);
    // (the kernel hands a dead exception object back to `exception_dropped`: here the object is taken out the way cleanup does it)
    let (_, e) =
        ipc::exception_of(&w.us.objs, &w.us.procs[&1].handles, h[1], Rights::MANAGE).unwrap();
    assert_eq!(
        exception::exception_dropped(&mut w.us, &mut w.jobs, &e),
        Some(Resolved::Passed)
    );
    let (_, h2) = w.read(jch);
    assert_eq!(h2.len(), 2, "it went to the job");
    // a process that has ended: nothing to resolve
    w.us.procs.get_mut(&2).unwrap().killed = Some(-9);
    assert_eq!(
        exception::exception_resume(&mut w.us, &mut w.jobs, 1, h2[1].0, abi::exception::RETRY),
        Err(Status::BadState)
    );
    assert_eq!(
        exception::exception_dropped(&mut w.us, &mut w.jobs, &e),
        None
    );
    assert_eq!(
        exception::exception_channel(&mut w.us, &mut w.jobs, 1, Target::Process(w.ph)),
        Err(Status::BadState),
        "an ended process takes no channel"
    );
}
