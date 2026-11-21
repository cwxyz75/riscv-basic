//! EX1: the suspend count against a model of the machine around it. A thread is running user code, inside a call (possibly blocked) or parked at
//! the return to user mode; random `thread_suspend`, `thread_resume`, traps, calls that block, waits that end and kills. After every step:
//! SUSPENDED is up exactly when the count is above 0 and the thread has stopped (parked, or blocked in a call); a parked thread has a count
//! above 0; a resume to 0 leaves nothing parked (no lost wake: the machine that acts on the answer wakes the thread); a thread whose count is
//! 0 runs.
use kcore::suspend::{AtReturn, Resumed, Suspend, MAX_SUSPEND};

mod common;
use common::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    User,
    /// in a call that is running (not blocked)
    InCall,
    /// in a call, blocked
    Blocked,
    Parked,
}

#[test]
fn the_suspend_count_never_loses_a_wake_and_raises_the_signal_exactly_when_stopped() {
    for seed in 1..=300u64 {
        let mut rng = Rng::new(seed);
        let mut s = Suspend::new();
        let mut at = Where::User;
        let mut signal = false;
        for step in 0..300 {
            let ctx = format!("seed {seed} step {step} at {at:?}");
            match rng.below(7) {
                0 | 1 => {
                    // thread_suspend: a thread blocked in a call has stopped at once
                    let blocked = at == Where::Blocked;
                    let raise = s.suspend(blocked).unwrap();
                    if raise {
                        assert!(!signal, "{ctx}: raised twice");
                        signal = true;
                    }
                }
                2 | 3 => match s.resume() {
                    None => assert_eq!(s.count(), 0, "{ctx}: a resume with a count"),
                    Some(Resumed::Still) => assert!(s.count() > 0),
                    Some(Resumed::Running { clear, wake }) => {
                        assert_eq!(s.count(), 0);
                        if clear {
                            assert!(signal, "{ctx}: cleared what was not up");
                            signal = false;
                        }
                        if wake {
                            assert_eq!(
                                at,
                                Where::Parked,
                                "{ctx}: a wake for a thread that is not parked"
                            );
                            at = Where::User;
                        }
                    }
                },
                4 => match at {
                    // a trap and its return to user mode
                    Where::User | Where::InCall => match s.return_to_user() {
                        AtReturn::Run => at = Where::User,
                        AtReturn::Park { raise } => {
                            if raise {
                                assert!(!signal, "{ctx}: raised twice");
                                signal = true;
                            }
                            at = Where::Parked;
                        }
                    },
                    _ => {}
                },
                5 => match at {
                    // a call that blocks
                    Where::User => at = Where::InCall,
                    Where::InCall => {
                        if s.blocks() {
                            assert!(!signal);
                            signal = true;
                        }
                        at = Where::Blocked;
                    }
                    // the wait ends: the call returns, through the return to user mode
                    Where::Blocked => match s.return_to_user() {
                        AtReturn::Run => at = Where::User,
                        AtReturn::Park { raise } => {
                            if raise {
                                signal = true;
                            }
                            at = Where::Parked;
                        }
                    },
                    Where::Parked => {}
                },
                _ => {
                    // killed while parked or blocked: it leaves without a wake
                    if at == Where::Parked && rng.below(8) == 0 {
                        s.unparked();
                        at = Where::User;
                        // (the thread ends: the model starts a new one)
                        s = Suspend::new();
                        signal = false;
                    }
                }
            }
            assert_eq!(
                s.stopped(),
                signal,
                "{ctx}: the signal is the stopped state"
            );
            if signal {
                assert!(s.count() > 0, "{ctx}: stopped with a count of 0");
            }
            if at == Where::Parked {
                assert!(
                    s.count() > 0 && s.parked() && signal,
                    "{ctx}: parked without a count"
                );
            }
            if s.count() == 0 {
                assert!(
                    at != Where::Parked,
                    "{ctx}: a thread with count 0 is parked: a lost wake"
                );
                assert!(!signal);
            }
            if at == Where::Blocked && s.count() > 0 {
                // (a blocked thread with a suspend counts as stopped: it was either raised by the suspend or by `blocks`)
                assert!(signal, "{ctx}: blocked with a count and not stopped");
            }
        }
    }
    // the limit
    let mut s = Suspend::new();
    for _ in 0..MAX_SUSPEND {
        assert!(s.suspend(false).is_some());
    }
    assert_eq!(s.suspend(false), None, "the count has a limit");
    assert_eq!(s.count(), MAX_SUSPEND);
}
