//! The big-lock spin report's verdict (T266, T212): did the owner hold the lock the whole time, or did it change hands while the
//! waiter spun?
use kcore::watchdog::{spin_verdict, SpinStart, SpinVerdict};

const START: SpinStart = SpinStart {
    owner: 3,
    owner_takes: 1000,
    at_ns: 5_000_000_000,
};

#[test]
fn an_owner_that_never_let_go_is_one_long_section() {
    let v = spin_verdict(START, 3, 1000, 5_750_000_000);
    assert_eq!(v, SpinVerdict::OneLongSection { waited_ms: 750 });
    assert!(v
        .to_string()
        .contains("owner takes since: 0, waited 750 ms"));
}

#[test]
fn an_owner_that_took_it_again_is_a_starved_waiter() {
    let v = spin_verdict(START, 3, 1042, 6_000_000_000);
    assert_eq!(
        v,
        SpinVerdict::Starved {
            takes: 42,
            waited_ms: 1000
        }
    );
    assert!(v
        .to_string()
        .contains("owner takes since: 42, waited 1000 ms"));
}

#[test]
fn another_owner_now_is_a_starved_waiter_even_without_takes() {
    assert!(matches!(
        spin_verdict(START, 2, 1000, 6_000_000_000),
        SpinVerdict::Starved { takes: 0, .. }
    ));
}

#[test]
fn a_clock_or_counter_read_out_of_order_does_not_underflow() {
    assert_eq!(
        spin_verdict(START, 3, 999, 4_000_000_000),
        SpinVerdict::OneLongSection { waited_ms: 0 }
    );
}
