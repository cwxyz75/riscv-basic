//! The wall-clock model (FS-SPEC section 7). `clockd` reads a hardware RTC and the kernel's monotonic clock together
//! (`sync`) and answers `now(mono)` from them. The answer **never goes backwards**: if a later reading says the clock was
//! ahead of the truth, the surplus is slewed away at half speed (the answer advances at least half as fast as real time
//! until the surplus is gone) instead of stepping back. A forward correction is applied at once. All arithmetic is
//! saturating or in `u128`, so it holds for any value a `u64` of nanoseconds can carry.
#![no_std]
#![forbid(unsafe_code)]

/// How far to trust the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// No source: the time counts from the epoch at boot.
    Unset,
    /// Read from a hardware clock.
    FromRtc,
    /// Set by a time-synchronisation client.
    Synced,
}

#[derive(Debug, Clone, Copy)]
pub struct ClockModel {
    /// Wall nanoseconds at `mono0`, per the latest reading.
    base: u64,
    mono0: u64,
    /// Nanoseconds by which answers run ahead of `base + (mono - mono0)`, still to be slewed away.
    ahead: u64,
    /// The monotonic time `ahead` was last reduced at.
    last_mono: u64,
    quality: Quality,
}

impl ClockModel {
    /// A clock with no source: `now(m)` is `m` (counting from the epoch at boot).
    pub fn unset(mono: u64) -> ClockModel {
        ClockModel {
            base: 0,
            mono0: mono,
            ahead: 0,
            last_mono: mono,
            quality: Quality::Unset,
        }
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    fn estimate(&self, mono: u64) -> u64 {
        self.base.saturating_add(mono.saturating_sub(self.mono0))
    }

    /// Surplus left at `mono` after slewing from `last_mono`.
    fn ahead_at(&self, mono: u64) -> u64 {
        let dt = mono.saturating_sub(self.last_mono);
        self.ahead.saturating_sub(dt / 2)
    }

    /// The wall time at monotonic time `mono` (which must not go backwards between calls; if it does the answer holds).
    pub fn now(&self, mono: u64) -> u64 {
        let mono = mono.max(self.last_mono);
        self.estimate(mono).saturating_add(self.ahead_at(mono))
    }

    /// A reading: the source says it is `wall` at monotonic time `mono`. `quality` is `FromRtc` or `Synced`.
    pub fn sync(&mut self, wall: u64, mono: u64, quality: Quality) {
        let mono = mono.max(self.last_mono);
        let shown = self.now(mono);
        self.base = wall;
        self.mono0 = mono;
        self.last_mono = mono;
        self.quality = quality;
        // behind what clients have seen: keep the difference as a surplus to slew; ahead of it: step forward
        self.ahead = shown.saturating_sub(wall);
    }
}
