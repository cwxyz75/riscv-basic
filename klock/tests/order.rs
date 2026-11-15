//! The lock-order checker's table (SCHED-SMP-SPEC 3.3, 3.4): every ordered pair of classes, every triple, the
//! equal-class rules, reentrancy and the depth limit.
use klock::{Class, Held, MAX_HELD};

fn held_with(classes: &[(Class, u32)]) -> Held {
    let mut h = Held::new();
    for &(c, i) in classes {
        h.check(c, i).expect("the setup order is legal");
        h.push(c, i);
    }
    h
}

#[test]
fn every_pair_of_classes_follows_the_hierarchy() {
    for &a in &Class::ALL {
        let h = held_with(&[(a, 0)]);
        for &b in &Class::ALL {
            let ok = h.check(b, 0).is_ok();
            // later classes are fine; the same class only if reentrant (a recursive take); an earlier class never
            let want = b > a || (b == a && a.reentrant());
            assert_eq!(ok, want, "holding {a:?}, taking {b:?}");
        }
    }
}

#[test]
fn the_run_queue_class_may_be_taken_twice_in_ascending_instance_order() {
    let h = held_with(&[(Class::Rq, 1)]);
    assert!(h.check(Class::Rq, 2).is_ok(), "ascending hart index");
    assert!(h.check(Class::Rq, 1).is_err(), "the same instance again");
    assert!(h.check(Class::Rq, 0).is_err(), "descending");
    // no other class allows two
    for &c in &Class::ALL {
        if c != Class::Rq && !c.reentrant() {
            let h = held_with(&[(c, 1)]);
            assert!(h.check(c, 2).is_err(), "{c:?} never two");
        }
    }
}

#[test]
fn two_run_queues_means_two() {
    let mut h = Held::new();
    h.check(Class::Rq, 0).unwrap();
    h.push(Class::Rq, 0);
    h.check(Class::Rq, 1).unwrap();
    h.push(Class::Rq, 1);
    assert!(
        h.check(Class::Rq, 2).is_err(),
        "a third run-queue lock is refused"
    );
}

#[test]
#[should_panic(expected = "does not hold")]
fn releasing_a_lock_that_is_not_held_is_a_bug() {
    let mut h = Held::new();
    h.pop(Class::Vmo, 0);
}

#[test]
fn a_chain_is_checked_against_everything_held() {
    // the nestings of 3.3: LEGACY, OBJ, JOBS, RQ, HEAP, UART
    let mut h = Held::new();
    for &c in &[
        Class::Legacy,
        Class::Obj,
        Class::Jobs,
        Class::Rq,
        Class::Heap,
        Class::Uart,
    ] {
        h.check(c, 0).unwrap();
        h.push(c, 0);
    }
    // anything earlier than the top is refused, and so is anything not above it
    for &c in &Class::ALL {
        if c != Class::Uart {
            assert!(h.check(c, 0).is_err(), "{c:?} under a held UART");
        }
    }
    assert!(
        h.check(Class::Uart, 0).is_ok(),
        "the console line is reentrant (a panic prints under it)"
    );
    // a recursive take of the legacy lock under a later one is the wrong order (HEAP then BKL)
    assert!(h.check(Class::Legacy, 0).is_err());
}

#[test]
fn releases_make_room_again() {
    let mut h = held_with(&[(Class::Obj, 0), (Class::Futex, 3)]);
    assert!(h.check(Class::Procs, 0).is_err(), "PROCS after FUTEX");
    h.pop(Class::Futex, 3);
    assert!(h.check(Class::Procs, 0).is_ok(), "PROCS after only OBJ");
    h.pop(Class::Obj, 0);
    assert!(h.is_empty());
    assert!(h.check(Class::Legacy, 0).is_ok());
}

#[test]
fn a_lock_can_be_released_out_of_order() {
    let mut h = held_with(&[(Class::Obj, 0), (Class::Futex, 0), (Class::Heap, 0)]);
    h.pop(Class::Futex, 0); // a guard dropped before the later ones
    assert_eq!(h.len(), 2);
    assert!(h.holds(Class::Obj) && h.holds(Class::Heap) && !h.holds(Class::Futex));
    assert!(
        h.check(Class::Futex, 0).is_err(),
        "FUTEX under HEAP is still wrong"
    );
}

#[test]
#[should_panic(expected = "locks held")]
fn more_than_eight_locks_is_a_bug() {
    let mut h = Held::new();
    for i in 0..(MAX_HELD as u32 + 1) {
        // (a made-up chain of RQ instances: legal by the table, too deep for the checker)
        h.push(Class::Rq, i);
    }
}

#[test]
fn the_class_numbers_are_the_documented_ones() {
    // SCHED-SMP-SPEC 3.3 numbers them 1 to 14; the kernel's masks and the INVARIANTS table rely on it
    for (i, &c) in Class::ALL.iter().enumerate() {
        assert_eq!(c as usize, i + 1, "{c:?}");
    }
}
