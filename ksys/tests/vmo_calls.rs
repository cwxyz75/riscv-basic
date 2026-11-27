//! The VMO calls on the host (TS3b): the error rows of SYSCALLS.md, over the kernel's real VMO table and address-space code.
use abi::limits::{MAX_COMMIT_PAGES, MAX_MSG_BYTES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use kcore::job::{JobId, Limits};

mod common;
use common::*;

const SAME: u64 = Rights::SAME.0 as u64;

#[test]
fn vmo_create_rows() {
    let mut w = world();
    assert_eq!(
        w.err(nr::VMO_CREATE, [0, 0, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_CREATE, [abi::user::TOP + 4096, 0, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    let v = w.ok(nr::VMO_CREATE, [4 * 4096, 0, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(w.ok(nr::VMO_GET_SIZE, [v, 0, 0, 0, 0, 0]).0, Some(4 * 4096));
    // a job argument that is not a job handle
    assert_eq!(
        w.err(nr::VMO_CREATE, [4096, 0x999, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::VMO_CREATE, [4096, v, 0, 0, 0, 0]),
        Status::WrongType
    );
}

#[test]
fn vmo_create_is_charged_to_the_job_and_a_failure_frees_the_vmo() {
    let mut w = world();
    let job = w
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                objects: 1,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.job = job;
    w.ok(nr::VMO_CREATE, [4096, 0, 0, 0, 0, 0]);
    let before = w.mm.db.free_count();
    assert_eq!(
        w.err(nr::VMO_CREATE, [8 * 4096, 0, 0, 0, 0, 0]),
        Status::NoMemory,
        "no object left in the job"
    );
    assert_eq!(
        w.mm.db.free_count(),
        before,
        "and the refused VMO leaked nothing"
    );
}

#[test]
fn vmo_size_read_write_rows() {
    let mut w = world();
    let v = w.ok(nr::VMO_CREATE, [2 * 4096, 0, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.err(nr::VMO_GET_SIZE, [0x999, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    w.poke(BASE, b"vmo data");
    w.ok(nr::VMO_WRITE, [v, 4090, BASE, 8, 0, 0]);
    w.ok(nr::VMO_READ, [v, 4090, BASE + 0x100, 8, 0, 0]);
    assert_eq!(
        w.peek(BASE + 0x100, 8),
        b"vmo data",
        "across a page boundary"
    );
    // range: the end past the size, and an offset that overflows
    assert_eq!(
        w.err(nr::VMO_READ, [v, 8190, BASE, 8, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_WRITE, [v, u64::MAX - 3, BASE, 8, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_READ, [v, 0, BASE, MAX_MSG_BYTES as u64 + 1, 0, 0]),
        Status::TooBig
    );
    // a bad user buffer: nothing is written to the VMO, and a read loses nothing
    assert_eq!(
        w.err(nr::VMO_WRITE, [v, 0, 0x10, 8, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::VMO_READ, [v, 0, RO, 8, 0, 0]),
        Status::InvalidArgs
    );
    // rights and type
    let ro = w.narrow(v, Rights::READ);
    let wo = w.narrow(v, Rights::WRITE);
    assert_eq!(
        w.err(nr::VMO_WRITE, [ro, 0, BASE, 1, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::VMO_READ, [wo, 0, BASE, 1, 0, 0]),
        Status::AccessDenied
    );
    let (ch, _) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::VMO_READ, [ch, 0, BASE, 1, 0, 0]),
        Status::WrongType
    );
    assert_eq!(
        w.err(nr::VMO_GET_SIZE, [ch, 0, 0, 0, 0, 0]),
        Status::WrongType
    );
}

#[test]
fn vmo_commit_rows() {
    let mut w = world();
    let v = w.ok(nr::VMO_CREATE, [8 * 4096, 0, 0, 0, 0, 0]).0.unwrap();
    let before = w.mm.db.free_count();
    w.ok(nr::VMO_COMMIT, [v, 0, 4 * 4096, 0, 0, 0]);
    assert_eq!(
        w.mm.db.free_count(),
        before - 4,
        "four pages were taken now"
    );
    assert_eq!(
        w.err(nr::VMO_COMMIT, [v, 1, 4096, 0, 0, 0]),
        Status::InvalidArgs,
        "unaligned offset"
    );
    assert_eq!(
        w.err(nr::VMO_COMMIT, [v, 0, 100, 0, 0, 0]),
        Status::InvalidArgs,
        "unaligned length"
    );
    assert_eq!(
        w.err(nr::VMO_COMMIT, [v, 6 * 4096, 4 * 4096, 0, 0, 0]),
        Status::InvalidArgs,
        "past the end"
    );
    assert_eq!(
        w.err(
            nr::VMO_COMMIT,
            [v, 0, (MAX_COMMIT_PAGES + 1) * 4096, 0, 0, 0]
        ),
        Status::TooBig
    );
    let no_commit = w.narrow(v, Rights(Rights::READ.0 | Rights::WRITE.0));
    assert_eq!(
        w.err(nr::VMO_COMMIT, [no_commit, 0, 4096, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::VMO_COMMIT, [0x999, 0, 4096, 0, 0, 0]),
        Status::BadHandle
    );
    // out of frames: NO_MEMORY, and what was committed stays accounted
    let pages = MAX_COMMIT_PAGES;
    let mut st = None;
    for _ in 0..200 {
        let more = w
            .ok(nr::VMO_CREATE, [pages * 4096, 0, 0, 0, 0, 0])
            .0
            .unwrap();
        if let Err((s, _, _)) = w
            .call(nr::VMO_COMMIT, [more, 0, pages * 4096, 0, 0, 0])
            .unwrap()
        {
            st = Some(s);
            break;
        }
    }
    assert_eq!(st, Some(Status::NoMemory));
}

#[test]
fn physical_creation_needs_the_resource_and_the_retired_calls_are_gone() {
    let mut w = world();
    // no resource handle: a VMO handle is the wrong type, a missing one is bad
    assert_eq!(
        w.err(nr::VMO_CREATE_PHYSICAL, [0x999, 0x1000_0000, 4096, 0, 0, 0]),
        Status::BadHandle
    );
    let v = w.ok(nr::VMO_CREATE, [4096, 0, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.err(nr::VMO_CREATE_PHYSICAL, [v, 0x1000_0000, 4096, 0, 0, 0]),
        Status::WrongType
    );
    // IOMMU12 (IOMMU-SPEC 4.8): the two-argument `vmo_create_dma` and `vmo_phys_addr` are not calls of the kernel any more: no layer
    // of the syscall code takes their old numbers (the kernel's dispatch answers BAD_SYSCALL for a number nothing takes), whatever the
    // arguments, a real VMO or resource handle included
    for &(name, n) in nr::RETIRED {
        for a in [
            [v, 1, 0, 0, 0, 0],
            [0x999, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0],
        ] {
            assert!(w.call(n, a).is_none(), "{name} ({n}) is still a call");
        }
    }
    let _ = SAME;
}

#[test]
fn vmo_create_takes_bytes_and_rounds_up_to_pages() {
    let mut w = world();
    let v = w.ok(nr::VMO_CREATE, [1, 0, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.ok(nr::VMO_GET_SIZE, [v, 0, 0, 0, 0, 0]).0,
        Some(4096),
        "one byte is a page"
    );
    let v = w.ok(nr::VMO_CREATE, [4097, 0, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(w.ok(nr::VMO_GET_SIZE, [v, 0, 0, 0, 0, 0]).0, Some(8192));
    let top = abi::user::TOP;
    assert!(w.ok(nr::VMO_CREATE, [top, 0, 0, 0, 0, 0]).0.is_some());
    assert_eq!(
        w.err(nr::VMO_CREATE, [top + 1, 0, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "a byte over the limit is a page over"
    );
}
