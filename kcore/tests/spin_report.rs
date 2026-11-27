//! The big-lock spin report's verdict (T266, T212): did the owner hold the lock the whole wait, did it pass through other harts, or was
//! FIFO order broken? (The lock is a ticket lock: once a hart has its ticket, nobody queued behind it is served first.)
use kcore::watchdog::{spin_verdict, SpinStart, SpinVerdict};

const START: SpinStart = SpinStart {
    owner: 3,
    owner_takes: 1000,
    at_ns: 5_000_000_000,
};

#[test]
fn an_owner_that_never_let_go_is_one_hold() {
    let v = spin_verdict(START, 3, 1000, 5_750_000_000);
    assert_eq!(v, SpinVerdict::OneHold { waited_ms: 750 });
    assert!(v
        .to_string()
        .contains("owner takes since: 0, waited 750 ms: one hold"));
}

#[test]
fn another_owner_now_is_a_lock_that_passed_through_the_harts_ahead() {
    let v = spin_verdict(START, 2, 1000, 6_000_000_000);
    assert_eq!(v, SpinVerdict::Passed { waited_ms: 1000 });
    assert!(v.to_string().contains("passed through"));
}

#[test]
fn a_start_owner_served_again_is_a_broken_order() {
    for owner_now in [3, 2] {
        let v = spin_verdict(START, owner_now, 1042, 6_000_000_000);
        assert_eq!(
            v,
            SpinVerdict::ServedAgain {
                takes: 42,
                waited_ms: 1000
            }
        );
        assert!(v
            .to_string()
            .contains("owner takes since: 42, waited 1000 ms"));
    }
}

#[test]
fn no_owner_at_the_start_has_no_reading() {
    let start = SpinStart { owner: 0, ..START };
    assert_eq!(
        spin_verdict(start, 0, 0, 6_000_000_000),
        SpinVerdict::Unknown { waited_ms: 1000 }
    );
}

#[test]
fn a_clock_or_counter_read_out_of_order_does_not_underflow() {
    assert_eq!(
        spin_verdict(START, 3, 999, 4_000_000_000),
        SpinVerdict::OneHold { waited_ms: 0 }
    );
}
