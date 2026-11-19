//! The job calls on the host (TS3b): `job_create`, `job_set_limits`, `job_info`, with the error rows of SYSCALLS.md.
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use kcore::ipc::{self, Quota};
use kcore::job::{JobId, Limits, UNLIMITED};
use ksys::wide_argument_error;

mod common;
use common::*;

/// A handle to the root job with `rights`.
fn root_handle(w: &mut World, rights: Rights) -> u64 {
    let mut q = Quota {
        jobs: &mut w.vmos.jobs,
        job: Some(JobId::ROOT),
    };
    u64::from(
        ipc::job_object_create(&mut w.objs, &mut w.handles, JobId::ROOT, rights, &mut q)
            .unwrap()
            .0,
    )
}

fn limits(l: [u64; 6]) -> Vec<u8> {
    l.iter().flat_map(|v| v.to_le_bytes()).collect()
}

const NONE: u64 = UNLIMITED;

#[test]
fn job_create_makes_a_child_with_the_limits() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([4, 8, 100, 3, 50, 4096]));
    let child = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    // the limits read back, and a fresh job uses nothing
    for (which, limit) in [(0, 4), (1, 8), (2, 100), (3, 3), (4, 50), (5, 4096)] {
        let (used, lim) = w.ok(nr::JOB_INFO, [child, which, 0, 0, 0, 0]);
        assert_eq!(lim, Some(limit), "limit {which}");
        assert!(used.is_some());
    }
    assert_eq!(
        w.ok(nr::JOB_INFO, [child, 7, 0, 0, 0, 0]),
        (Some(4), Some(4)),
        "the processors online, in both registers"
    );
    assert!(
        w.ok(nr::JOB_INFO, [child, 6, 0, 0, 0, 0]).0.is_some(),
        "the scheduling limits"
    );
}

#[test]
fn job_create_rows() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    assert_eq!(
        w.err(nr::JOB_CREATE, [0x999, BASE, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let (ch, _) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::JOB_CREATE, [ch, BASE, 0, 0, 0, 0]),
        Status::WrongType
    );
    let inspect_only = w.narrow(root, Rights::INSPECT);
    assert_eq!(
        w.err(nr::JOB_CREATE, [inspect_only, BASE, 0, 0, 0, 0]),
        Status::AccessDenied,
        "MANAGE is needed"
    );
    assert_eq!(
        w.err(nr::JOB_CREATE, [root, 0x10, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "the limits cannot be read"
    );
    // a parent at its limit on jobs
    w.poke(BASE, &limits([NONE, NONE, NONE, 1, NONE, NONE]));
    let one = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    w.poke(BASE, &limits([NONE; 6]));
    w.ok(nr::JOB_CREATE, [one, BASE, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::JOB_CREATE, [one, BASE, 0, 0, 0, 0]),
        Status::NoMemory,
        "the job allows one child job"
    );
    // and a child can never have more than its ancestor allows, whatever its own limits say
    let (used, _) = w.ok(nr::JOB_INFO, [one, 3, 0, 0, 0, 0]);
    assert_eq!(used, Some(1));
}

#[test]
fn job_create_failure_leaks_no_job() {
    let mut w = world();
    let tiny = w
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
    w.job = tiny;
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    // the caller's job may hold no object at all: the new job's handle cannot be charged
    let live = w.vmos.jobs.live();
    assert_eq!(
        w.err(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]),
        Status::NoMemory
    );
    assert_eq!(
        w.vmos.jobs.live(),
        live,
        "the job made for the refused call is released again"
    );
}

#[test]
fn job_set_limits_needs_set_limits_and_checks_rights_before_the_pointer() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    let child = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.err(nr::JOB_SET_LIMITS, [0x999, BASE, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let (ch, _) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::JOB_SET_LIMITS, [ch, BASE, 0, 0, 0, 0]),
        Status::WrongType
    );
    let manage_only = w.narrow(child, Rights::MANAGE);
    assert_eq!(
        w.err(nr::JOB_SET_LIMITS, [manage_only, 0x10, 0, 0, 0, 0]),
        Status::AccessDenied,
        "rights first, the bad pointer is not touched"
    );
    assert_eq!(
        w.err(nr::JOB_SET_LIMITS, [child, 0x10, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    w.poke(BASE, &limits([7, NONE, NONE, NONE, NONE, NONE]));
    w.ok(nr::JOB_SET_LIMITS, [child, BASE, 0, 0, 0, 0]);
    assert_eq!(w.ok(nr::JOB_INFO, [child, 0, 0, 0, 0, 0]).1, Some(7));
}

#[test]
fn job_info_rows() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    assert_eq!(
        w.err(nr::JOB_INFO, [0x999, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let (ch, _) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(w.err(nr::JOB_INFO, [ch, 0, 0, 0, 0, 0]), Status::WrongType);
    let manage_only = w.narrow(root, Rights::MANAGE);
    assert_eq!(
        w.err(nr::JOB_INFO, [manage_only, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::JOB_INFO, [root, 8, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    // the usage counts what was made: a channel is two objects, charged to the job
    let before = w.ok(nr::JOB_INFO, [root, 4, 0, 0, 0, 0]).0.unwrap();
    w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.ok(nr::JOB_INFO, [root, 4, 0, 0, 0, 0]).0.unwrap(),
        before + 2
    );
}

#[test]
fn the_wide_job_handles_are_refused() {
    for n in [nr::JOB_CREATE, nr::JOB_SET_LIMITS, nr::JOB_INFO] {
        assert_eq!(
            wide_argument_error(n, &[1 << 32, 0, 0, 0, 0, 0]),
            Some(Status::BadHandle)
        );
    }
}
