//! H8 of NET-SPEC 12.1: the supervisor against a reference that recomputes the intensity from the full history.
use supervise::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// The reference: the whole history of exits, with the decision recomputed from scratch each time.
struct Reference {
    policy: Policy,
    granted: Vec<(usize, Time)>,
    down: bool,
}

impl Reference {
    fn on_exit(&mut self, child: usize, now: Time, restartable: bool) -> Action {
        if self.down {
            return Action::Down;
        }
        if !restartable {
            self.down = true;
            return Action::Down;
        }
        let in_child = self
            .granted
            .iter()
            .filter(|(c, t)| *c == child && *t + self.policy.per_child_window > now)
            .count();
        let in_all = self
            .granted
            .iter()
            .filter(|(_, t)| *t + self.policy.overall_window > now)
            .count();
        if in_child >= self.policy.per_child || in_all >= self.policy.overall {
            self.down = true;
            return Action::Down;
        }
        self.granted.push((child, now));
        let i = in_child.min(2);
        Action::Restart {
            delay: self.policy.delays[i],
        }
    }
}

#[test]
fn random_exit_histories_agree_with_the_reference() {
    for seed in 1..2000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = Supervisor::new(Policy::NET);
        let mut r = Reference {
            policy: Policy::NET,
            granted: vec![],
            down: false,
        };
        let mut now = 0;
        for step in 0..40 {
            now += (rng.next() % (12 * SEC)) as Time;
            let child = (rng.next() % 3) as usize;
            let restartable = !rng.next().is_multiple_of(40);
            assert_eq!(
                s.on_exit(child, now, restartable),
                r.on_exit(child, now, restartable),
                "seed {seed} step {step} t={now}"
            );
            assert_eq!(s.is_down(), r.down);
        }
    }
}

#[test]
fn the_per_child_limit_is_three_in_thirty_seconds() {
    let mut s = Supervisor::new(Policy::NET);
    assert_eq!(s.on_exit(0, 0, true), Action::Restart { delay: 0 });
    assert_eq!(s.on_exit(0, SEC, true), Action::Restart { delay: 250 * MS });
    assert_eq!(s.on_exit(0, 2 * SEC, true), Action::Restart { delay: SEC });
    assert_eq!(
        s.on_exit(0, 3 * SEC, true),
        Action::Down,
        "the fourth exit within 30 s"
    );
    assert_eq!(s.down_reason(), Some(DownReason::ChildIntensity));
    assert_eq!(
        s.on_exit(1, 100 * SEC, true),
        Action::Down,
        "down stays down"
    );
}

#[test]
fn restarts_outside_the_window_do_not_count() {
    let mut s = Supervisor::new(Policy::NET);
    for i in 0..3 {
        assert!(matches!(
            s.on_exit(0, i * SEC, true),
            Action::Restart { .. }
        ));
    }
    // 30 s after the first restart it no longer counts: the exit at t = 30 s is allowed (two restarts remain in the window,
    // so the delay is the third entry)
    assert_eq!(s.on_exit(0, 30 * SEC, true), Action::Restart { delay: SEC });
    // and a quiet minute forgets everything: the delay starts again at zero
    assert_eq!(s.on_exit(0, 200 * SEC, true), Action::Restart { delay: 0 });
}

#[test]
fn the_overall_limit_is_six_in_sixty_seconds() {
    let mut s = Supervisor::new(Policy::NET);
    for i in 0..6 {
        assert!(
            matches!(
                s.on_exit(i % 3, i as Time * 5 * SEC, true),
                Action::Restart { .. }
            ),
            "restart {i}"
        );
    }
    assert_eq!(
        s.on_exit(0, 31 * SEC, true),
        Action::Down,
        "the seventh within 60 s"
    );
    assert_eq!(s.down_reason(), Some(DownReason::OverallIntensity));
}

#[test]
fn a_child_that_must_not_restart_takes_the_supervisor_down_at_once() {
    let mut s = Supervisor::new(Policy::NET);
    assert_eq!(s.on_exit(0, 0, false), Action::Down);
    assert_eq!(s.down_reason(), Some(DownReason::NoRestart));
}
