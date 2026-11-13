//! The interrupt object's state machine (S6a-2): idle, fired-until-acked, destroyed. The
//! source-to-object map and the PLIC itself live in the kernel; this is the object.
use abi::rights::Rights;
use abi::signals;
use abi::status::Status;
use kcore::handle::HandleTable;
use kcore::ipc::*;
use kcore::job::Jobs;

fn world() -> (Kobjs, HandleTable) {
    (Kobjs::new(), HandleTable::new(16))
}

fn make(reg: &mut Kobjs, t: &mut HandleTable, irq: u32) -> (abi::Handle, kcore::object::ObjId) {
    let mut jobs = Jobs::new();
    let mut q = Quota {
        jobs: &mut jobs,
        job: None,
    };
    interrupt_object_create_in(reg, t, irq, &mut q).unwrap()
}

#[test]
fn fire_then_ack_cycle_with_claim_counting() {
    let (mut reg, mut t) = world();
    let (h, obj) = make(&mut reg, &mut t, 3);
    assert_eq!(t.get(h).unwrap().rights, Rights::INTERRUPT);
    assert_eq!(signals_of(&reg, &t, h).unwrap() & signals::INTERRUPT, 0);
    assert_eq!(
        interrupt_ack(&mut reg, obj),
        Err(Status::BadState),
        "nothing to ack"
    );
    assert!(interrupt_fire(&mut reg, obj, 3), "the first fire is new");
    assert_eq!(
        signals_of(&reg, &t, h).unwrap() & signals::INTERRUPT,
        signals::INTERRUPT
    );
    assert!(
        !interrupt_fire(&mut reg, obj, 3),
        "firing again while pending is not new"
    );
    assert_eq!(
        interrupt_ack(&mut reg, obj),
        Ok((3, 2)),
        "two claims counted"
    );
    assert_eq!(signals_of(&reg, &t, h).unwrap() & signals::INTERRUPT, 0);
    assert_eq!(
        interrupt_ack(&mut reg, obj),
        Err(Status::BadState),
        "ack is not repeatable"
    );
    assert!(interrupt_fire(&mut reg, obj, 3));
    assert_eq!(interrupt_ack(&mut reg, obj), Ok((3, 3)));
}

#[test]
fn a_fire_for_another_source_or_a_dead_object_does_nothing() {
    let (mut reg, mut t) = world();
    let (h, obj) = make(&mut reg, &mut t, 2);
    assert!(!interrupt_fire(&mut reg, obj, 5), "wrong source");
    assert_eq!(signals_of(&reg, &t, h).unwrap() & signals::INTERRUPT, 0);
    t.close(&mut reg, h).unwrap();
    reg.sweep(&[]);
    assert!(
        !interrupt_fire(&mut reg, obj, 2),
        "a destroyed object is gone"
    );
    assert_eq!(interrupt_ack(&mut reg, obj), Err(Status::BadHandle));
}

#[test]
fn rights_and_types_are_checked() {
    let (mut reg, mut t) = world();
    let (h, _) = make(&mut reg, &mut t, 1);
    let weak = t.duplicate(&mut reg, h, Rights::WAIT).unwrap();
    assert_eq!(
        interrupt_of(&reg, &t, weak, Rights::MANAGE).err(),
        Some(Status::AccessDenied)
    );
    assert!(interrupt_of(&reg, &t, weak, Rights::WAIT).is_ok());
    let (res, _) = (resource_object(&mut reg, &mut t), ());
    assert_eq!(
        interrupt_of(&reg, &t, res, Rights::NONE).err(),
        Some(Status::WrongType)
    );
}

fn resource_object(reg: &mut Kobjs, t: &mut HandleTable) -> abi::Handle {
    let mut jobs = Jobs::new();
    let mut q = Quota {
        jobs: &mut jobs,
        job: None,
    };
    resource_object_create_in(reg, t, &mut q).unwrap()
}
