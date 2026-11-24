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
        jobs: &mut w.jobs,
        job: Some(JobId::ROOT),
    };

    ipc::job_object_create(&mut w.objs, &mut w.handles, JobId::ROOT, rights, &mut q)
        .unwrap()
        .0
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
    let live = w.jobs.live();
    assert_eq!(
        w.err(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]),
        Status::NoMemory
    );
    assert_eq!(
        w.jobs.live(),
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
        w.err(nr::JOB_INFO, [root, 12, 0, 0, 0, 0]),
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
fn a_job_handle_is_a_full_register_and_a_value_that_names_nothing_is_bad_handle() {
    let mut w = world();
    for n in [nr::JOB_CREATE, nr::JOB_SET_LIMITS, nr::JOB_INFO] {
        // (AB1: no width rule on a handle; the table refuses what it does not hold)
        assert_eq!(wide_argument_error(n, &[1 << 32, 0, 0, 0, 0, 0]), None);
        assert_eq!(w.err(n, [1 << 40 | 1, 0, 0, 0, 0, 0]), Status::BadHandle);
    }
}

/// SCHED-SMP-SPEC 4.5, 4.10: `job_info` 9 is the RT time run in the current window against the budget, 10 the CPU time of the job and
/// the jobs below it; a reused slot starts from nothing.
#[test]
fn job_info_reports_rt_and_cpu_time_of_the_subtree() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    let a = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    let b = w.ok(nr::JOB_CREATE, [a, BASE, 0, 0, 0, 0]).0.unwrap();
    let ids = w.jobs.live_ids();
    let (ja, jb) = (ids[1].index(), ids[2].index());
    assert_eq!(
        w.ok(nr::JOB_INFO, [a, 9, 0, 0, 0, 0]),
        (Some(0), Some(100_000_000))
    );
    let (ta, tb) = (w.job_sched.tag(ja), w.job_sched.tag(jb));
    w.job_sched.charge(ta, 30_000_000, true, 40_000_000); // 30 ms of RT time in window 0
    w.job_sched.charge(tb, 5_000_000, false, 45_000_000); // 5 ms of ordinary time in the child
    assert_eq!(w.ok(nr::JOB_INFO, [a, 9, 0, 0, 0, 0]).0, Some(30_000_000));
    assert_eq!(
        w.ok(nr::JOB_INFO, [a, 10, 0, 0, 0, 0]),
        (Some(35_000_000), Some(0)),
        "the job and the child"
    );
    assert_eq!(
        w.ok(nr::JOB_INFO, [b, 10, 0, 0, 0, 0]).0,
        Some(5_000_000),
        "the child alone"
    );
    assert_eq!(
        w.ok(nr::JOB_INFO, [b, 9, 0, 0, 0, 0]).0,
        Some(0),
        "the child ran no RT time"
    );
}

#[test]
fn a_job_slot_that_is_used_again_starts_with_no_cpu_time() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]);
    // (the next job takes slot 2: time left there by an earlier job must not show in it)
    w.job_sched.charge(2, 9_000_000, true, 9_000_000);
    let b = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(w.jobs.live_ids()[2].index(), 2);
    assert_eq!(w.ok(nr::JOB_INFO, [b, 10, 0, 0, 0, 0]).0, Some(0));
    assert_eq!(w.ok(nr::JOB_INFO, [b, 9, 0, 0, 0, 0]).0, Some(0));
}

/// PL8: `job_set_region_limit` needs SET_LIMITS, the value shows in `job_info` 11 as the effective limit (the smallest along the ancestors, at most
/// `MAX_REGIONS`), and a child inherits what its parent set.
#[test]
fn job_set_region_limit_is_a_quota_that_children_inherit() {
    let mut w = world();
    let root = root_handle(&mut w, Rights::JOB);
    w.poke(BASE, &limits([NONE; 6]));
    let parent = w.ok(nr::JOB_CREATE, [root, BASE, 0, 0, 0, 0]).0.unwrap();
    let max = abi::limits::MAX_REGIONS as u64;
    assert_eq!(
        w.ok(nr::JOB_INFO, [parent, 11, 0, 0, 0, 0]),
        (Some(max), Some(max)),
        "no quota: the backstop"
    );
    w.ok(nr::JOB_SET_REGION_LIMIT, [parent, 100, 0, 0, 0, 0]);
    let child = w.ok(nr::JOB_CREATE, [parent, BASE, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.ok(nr::JOB_INFO, [child, 11, 0, 0, 0, 0]).0,
        Some(100),
        "a child inherits"
    );
    // a child's own limit may be lower, never higher than the parent's
    w.ok(nr::JOB_SET_REGION_LIMIT, [child, 10, 0, 0, 0, 0]);
    assert_eq!(w.ok(nr::JOB_INFO, [child, 11, 0, 0, 0, 0]).0, Some(10));
    w.ok(nr::JOB_SET_REGION_LIMIT, [child, 1 << 40, 0, 0, 0, 0]);
    assert_eq!(w.ok(nr::JOB_INFO, [child, 11, 0, 0, 0, 0]).0, Some(100));
    // the rights come first
    let weak = w.narrow(child, Rights::MANAGE);
    assert_eq!(
        w.err(nr::JOB_SET_REGION_LIMIT, [weak, 5, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::JOB_SET_REGION_LIMIT, [0x999, 5, 0, 0, 0, 0]),
        Status::BadHandle
    );
}
