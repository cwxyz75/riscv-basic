//! A supervisor's decisions as pure logic (NET-SPEC 9.2): given that a child exited at time `now`, restart it (after
//! what delay) or give up. The restart *intensity* rule: more than `per_child` restarts of one child within
//! `per_child_window`, or more than `overall` restarts of all children within `overall_window`, and the supervisor is
//! `Down` for good. The restart delay grows with the number of recent restarts of that child, so a crash loop does not
//! spin. `no_std` plus `alloc`, no `unsafe`; time is an opaque nanosecond count the caller passes in.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

pub type Time = u64;
pub const MS: Time = 1_000_000;
pub const SEC: Time = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub per_child: usize,
    pub per_child_window: Time,
    pub overall: usize,
    pub overall_window: Time,
    /// The delay before restart number `n` within the window (`n` counts from 0; beyond the table, the last entry).
    pub delays: [Time; 3],
}

impl Policy {
    /// The network stack's: 3 restarts in 30 s per child, 6 in 60 s overall, delays 0, 250 ms, 1 s.
    pub const NET: Policy = Policy {
        per_child: 3,
        per_child_window: 30 * SEC,
        overall: 6,
        overall_window: 60 * SEC,
        delays: [0, 250 * MS, SEC],
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Start the child again after `delay`.
    Restart { delay: Time },
    /// The restart intensity was exceeded (or the child may not be restarted): the supervisor stops restarting.
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownReason {
    ChildIntensity,
    OverallIntensity,
    NoRestart,
}

/// The restart history of a set of children.
pub struct Supervisor {
    policy: Policy,
    /// `(child, time)` of every restart granted, oldest first.
    restarts: Vec<(usize, Time)>,
    down: Option<DownReason>,
}

impl Supervisor {
    pub fn new(policy: Policy) -> Supervisor {
        Supervisor {
            policy,
            restarts: Vec::new(),
            down: None,
        }
    }

    pub fn is_down(&self) -> bool {
        self.down.is_some()
    }

    pub fn down_reason(&self) -> Option<DownReason> {
        self.down
    }

    /// Restarts of `child` granted within the window ending at `now`.
    pub fn recent(&self, child: usize, now: Time) -> usize {
        self.restarts
            .iter()
            .filter(|&&(c, t)| c == child && now.saturating_sub(t) < self.policy.per_child_window)
            .count()
    }

    /// Restarts of any child within the overall window ending at `now`.
    pub fn recent_overall(&self, now: Time) -> usize {
        self.restarts
            .iter()
            .filter(|&&(_, t)| now.saturating_sub(t) < self.policy.overall_window)
            .count()
    }

    /// `child` exited at `now`. `restartable` false: its exit code says it must not be started again (a driver that
    /// found the device unusable): the supervisor is down at once. Once down, always `Down`.
    pub fn on_exit(&mut self, child: usize, now: Time, restartable: bool) -> Action {
        if self.down.is_some() {
            return Action::Down;
        }
        if !restartable {
            self.down = Some(DownReason::NoRestart);
            return Action::Down;
        }
        let own = self.recent(child, now);
        if own >= self.policy.per_child {
            self.down = Some(DownReason::ChildIntensity);
            return Action::Down;
        }
        if self.recent_overall(now) >= self.policy.overall {
            self.down = Some(DownReason::OverallIntensity);
            return Action::Down;
        }
        // forget history older than the longest window
        let keep = self.policy.per_child_window.max(self.policy.overall_window);
        self.restarts.retain(|&(_, t)| now.saturating_sub(t) < keep);
        self.restarts.push((child, now));
        let delay = self.policy.delays[own.min(self.policy.delays.len() - 1)];
        Action::Restart { delay }
    }
}
