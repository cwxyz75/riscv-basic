//! The object constructors check the handle table's room before they create anything: with exactly one slot left they succeed, with none they
//! refuse with NO_MEMORY and leave nothing behind (KT10: the mutants "room() < 1 gives room() == 1" in `vmo_object_create_with` and
//! `thread_object_create_in` survived the first mutation run, because no test built a table with exactly one free slot).
use abi::rights::Rights;
use abi::status::Status;
use kcore::handle::HandleTable;
use kcore::ipc::{thread_object_create_in, vmo_object_create_with, Kobjs, Quota};
use kcore::job::Jobs;

#[test]
fn a_vmo_object_takes_the_last_free_slot_and_is_refused_when_there_is_none() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(2);
    let mut jobs = Jobs::new();
    let mut q = Quota {
        jobs: &mut jobs,
        job: None,
    };
    assert!(vmo_object_create_with(&mut reg, &mut t, 1, Rights::READ, &mut q).is_ok());
    assert_eq!(t.room(), 1);
    assert!(
        vmo_object_create_with(&mut reg, &mut t, 2, Rights::READ, &mut q).is_ok(),
        "one free slot is enough"
    );
    assert_eq!(t.room(), 0);
    let live = reg.live();
    assert_eq!(
        vmo_object_create_with(&mut reg, &mut t, 3, Rights::READ, &mut q),
        Err(Status::NoMemory)
    );
    assert_eq!(reg.live(), live, "a refusal makes nothing");
}

#[test]
fn a_thread_object_takes_the_last_free_slot_and_is_refused_when_there_is_none() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(2);
    let mut jobs = Jobs::new();
    let mut q = Quota {
        jobs: &mut jobs,
        job: None,
    };
    assert!(thread_object_create_in(&mut reg, &mut t, 7, &mut q).is_ok());
    assert!(
        thread_object_create_in(&mut reg, &mut t, 7, &mut q).is_ok(),
        "one free slot is enough"
    );
    let live = reg.live();
    assert_eq!(
        thread_object_create_in(&mut reg, &mut t, 7, &mut q).err(),
        Some(Status::NoMemory)
    );
    assert_eq!(reg.live(), live, "a refusal makes nothing");
}
