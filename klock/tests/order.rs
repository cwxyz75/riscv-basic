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

/// The classes of SCHED-SMP-SPEC 3.3 that may be held twice (ascending instance), and the two that are reentrant, written out here
/// rather than read from `Class`: the table below is the oracle the checker is held to.
const TWO: [Class; 4] = [Class::As, Class::Tt, Class::Rq, Class::Kstack];
const REENTRANT: [Class; 2] = [Class::Obj, Class::Uart];

/// SCHED-SMP-SPEC 3.3 and 3.4: holding `(a, ia)`, taking `(b, ib)` is legal when `b` comes later, or it is the same class and the
/// class is reentrant (a recursive take), or the same class held two at a time and a greater instance.
fn the_table_allows((a, ia): (Class, u32), (b, ib): (Class, u32)) -> bool {
    (b as u8) > (a as u8) || (b == a && (REENTRANT.contains(&b) || (TWO.contains(&b) && ib > ia)))
}

#[test]
fn every_ordered_pair_matches_the_table() {
    // every class held, every class taken, at the instances that tell the equal-class rules apart (the same, above, below)
    let mut pairs = 0;
    for &a in &Class::ALL {
        for ia in 0..3u32 {
            let h = held_with(&[(a, ia)]);
            for &b in &Class::ALL {
                for ib in 0..3u32 {
                    let want = the_table_allows((a, ia), (b, ib));
                    assert_eq!(
                        h.check(b, ib).is_ok(),
                        want,
                        "holding {a:?} {ia}, taking {b:?} {ib}"
                    );
                    if let Err(v) = h.check(b, ib) {
                        // the report names the pair
                        assert_eq!(
                            (v.held, v.held_instance, v.taking, v.instance),
                            (a, ia, b, ib)
                        );
                    }
                    pairs += 1;
                }
            }
        }
    }
    assert_eq!(pairs, 15 * 15 * 9);
    // and the class's own answers are the table's
    for &c in &Class::ALL {
        assert_eq!(c.allows_two(), TWO.contains(&c), "{c:?} allows two");
        assert_eq!(c.reentrant(), REENTRANT.contains(&c), "{c:?} reentrant");
    }
}

#[test]
fn every_ordered_pair_under_a_held_chain_matches_the_table() {
    // two held (any legal pair), a third taken: the check is against everything held, which for a legal stack is its top
    for &a in &Class::ALL {
        for &b in &Class::ALL {
            if !the_table_allows((a, 0), (b, 1)) || a == b && REENTRANT.contains(&a) {
                continue;
            }
            let h = held_with(&[(a, 0), (b, 1)]);
            for &c in &Class::ALL {
                for ic in 0..3u32 {
                    let mut want = the_table_allows((b, 1), (c, ic));
                    if a == b && c == b && TWO.contains(&c) {
                        // "two means two": a third of the class is refused, except the address spaces and the stack locks
                        want &= Class::most_at_once(c) > 2;
                    }
                    assert_eq!(
                        h.check(c, ic).is_ok(),
                        want,
                        "holding {a:?} 0, {b:?} 1, taking {c:?} {ic}"
                    );
                }
            }
        }
    }
}

/// Run `f` and say whether it panicked.
fn panics(f: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    std::panic::catch_unwind(f).is_err()
}

#[test]
fn a_release_of_an_unheld_lock_panics_for_every_class_but_the_console() {
    // (T85 (5): the release was lenient for the reentrant classes while the big lock's hand-overs made an exact count hard; they ended
    // with L20, so only the console stays lenient: a panic breaks its lock with the line half written, T240)
    for &c in &Class::ALL {
        let lenient = c == Class::Uart;
        assert_eq!(
            panics(move || Held::new().pop(c, 0)),
            !lenient,
            "{c:?} released, never taken"
        );
        assert_eq!(
            panics(move || {
                let mut h = held_with(&[(c, 0)]);
                h.pop(c, 0);
                h.pop(c, 0);
            }),
            !lenient,
            "{c:?} released twice"
        );
        assert_eq!(
            panics(move || {
                let mut h = held_with(&[(c, 1)]);
                h.pop(c, 2);
            }),
            !lenient,
            "{c:?} released at another instance"
        );
        if REENTRANT.contains(&c) {
            // a recursive take is not pushed again, so it is released once: the second release is one too many
            assert_eq!(
                panics(move || {
                    let mut h = held_with(&[(c, 0)]);
                    h.check(c, 0).unwrap();
                    h.push(c, 0);
                    h.pop(c, 0);
                    assert!(h.is_empty());
                    h.pop(c, 0);
                }),
                !lenient,
                "{c:?} taken twice, released twice"
            );
        }
    }
    // the lenient one leaves the stack as it was
    let mut h = held_with(&[(Class::Obj, 0), (Class::Heap, 0)]);
    h.pop(Class::Uart, 0);
    assert_eq!(h.len(), 2);
}

#[test]
fn the_run_queue_class_may_be_taken_twice_in_ascending_instance_order() {
    let h = held_with(&[(Class::Rq, 1)]);
    assert!(h.check(Class::Rq, 2).is_ok(), "ascending hart index");
    assert!(h.check(Class::Rq, 1).is_err(), "the same instance again");
    assert!(h.check(Class::Rq, 0).is_err(), "descending");
    // no other class allows two
    for &c in &Class::ALL {
        if c != Class::Rq
            && c != Class::As
            && c != Class::Tt
            && c != Class::Kstack
            && !c.reentrant()
        {
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
    h.pop(Class::Obj, 0);
}

#[test]
fn a_chain_is_checked_against_everything_held() {
    // the nestings of 3.3: LEGACY, OBJ, JOBS, RQ, HEAP, UART
    let mut h = Held::new();
    for &c in &[
        Class::Obj,
        Class::Procs,
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
    assert!(h.check(Class::Obj, 0).is_err());
}

#[test]
fn releases_make_room_again() {
    let mut h = held_with(&[(Class::Obj, 0), (Class::Futex, 3)]);
    assert!(h.check(Class::Procs, 0).is_err(), "PROCS after FUTEX");
    h.pop(Class::Futex, 3);
    assert!(h.check(Class::Procs, 0).is_ok(), "PROCS after only OBJ");
    h.pop(Class::Obj, 0);
    assert!(h.is_empty());
    assert!(h.check(Class::Obj, 0).is_ok());
}

#[test]
fn a_lock_can_be_released_out_of_order() {
    let mut h = held_with(&[(Class::Procs, 0), (Class::Futex, 0), (Class::Heap, 0)]);
    h.pop(Class::Futex, 0); // a guard dropped before the later ones
    assert_eq!(h.len(), 2);
    assert!(h.holds(Class::Procs) && h.holds(Class::Heap) && !h.holds(Class::Futex));
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
    // SCHED-SMP-SPEC 3.3 numbers them in order, 1 to 15, since the amendment of 2026-11-23 (the object lock, OBJ, is class 1 and the old LEGACY slot is gone) and IOMMU-SPEC 11.1 (IOMMU between KSTACK and FRAME, IOMMU5); the kernel's masks and the INVARIANTS table rely on it
    for (i, &c) in Class::ALL.iter().enumerate() {
        assert_eq!(c as usize, i + 1, "{c:?}");
    }
}

#[test]
fn a_held_stack_knows_when_it_is_empty() {
    // (KT10: the mutant "is_empty gives true" survived the first mutation run)
    let mut h = Held::new();
    assert!(h.is_empty());
    assert_eq!(h.len(), 0);
    h.push(Class::Procs, 0);
    assert!(!h.is_empty());
    assert_eq!(h.len(), 1);
    h.pop(Class::Procs, 0);
    assert!(h.is_empty());
}

#[test]
fn address_spaces_may_be_taken_ascending_up_to_the_process_limit() {
    // (a call that names a process, or all of them, takes the address spaces of up to 64 processes in ascending order)
    let mut h = Held::new();
    for pid in 1..=64u32 {
        h.check(Class::As, pid).unwrap();
        h.push(Class::As, pid);
    }
    assert!(h.check(Class::As, 65).is_err(), "a 65th");
    let h = held_with(&[(Class::As, 5)]);
    assert!(h.check(Class::As, 4).is_err(), "descending");
    assert!(
        h.check(Class::Vmo, 0).is_ok(),
        "the memory locks come after"
    );
    let h = held_with(&[(Class::Vmo, 0)]);
    assert!(
        h.check(Class::As, 1).is_err(),
        "AS before the memory locks, never after"
    );
}

#[test]
fn the_iommu_lock_sits_between_the_kernel_stacks_and_the_frames() {
    // IOMMU-SPEC 11.1: the fault interrupt holds OBJ and takes IOMMU; a map takes table frames (FRAME, HEAP) under it; nothing
    // under it takes VMO, JOBS, an AS lock or OBJ; never two
    let h = held_with(&[(Class::Obj, 0), (Class::Iommu, 0)]);
    for c in [Class::Frame, Class::Heap, Class::Entropy, Class::Uart] {
        assert!(h.check(c, 0).is_ok(), "{c:?} under IOMMU");
    }
    for c in [
        Class::As,
        Class::Vmo,
        Class::Jobs,
        Class::Tt,
        Class::Rq,
        Class::Kstack,
    ] {
        assert!(h.check(c, 0).is_err(), "{c:?} under IOMMU");
    }
    assert!(h.check(Class::Iommu, 1).is_err(), "never two");
    let h = held_with(&[(Class::Vmo, 0), (Class::Jobs, 0), (Class::Kstack, 0)]);
    assert!(h.check(Class::Iommu, 0).is_ok());
    let h = held_with(&[(Class::Frame, 0)]);
    assert!(h.check(Class::Iommu, 0).is_err(), "FRAME then IOMMU");
}
