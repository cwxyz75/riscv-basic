//! Schedule perturbation (KT7, feature `chaos`): seeded random delays at the points where harts race, so that interleavings QEMU's regular
//! scheduling of harts never produces get tried. With the feature off, `point` is an empty function and the hooks compile to nothing.
//!
//! The seed comes from the boot argument `chaos=<seed>` (`-append`). Each hart has its own generator (`kcore::chaos`), seeded from the seed
//! and the hart's index, so one hart's delays do not depend on what the others did. At a hook point the generator says "spin N
//! iterations" one time in eight (N at most 2000); interrupts stay as they are.

/// Where a delay can be injected (only for the log: every point is treated alike).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Point {
    /// Just after a lock was taken (the lock is held while the delay runs: longer hold times).
    Acquired,
    /// Just after a lock was released.
    Released,
    /// Before an inter-hart interrupt is sent.
    BeforeIpi,
    /// At the entry of the scheduler's switch.
    Schedule,
    /// Before the trap handler returns to user mode.
    ReturnToUser,
}

#[cfg(not(feature = "chaos"))]
#[inline(always)]
pub fn point(_: Point) {}

#[cfg(not(feature = "chaos"))]
pub fn init() {}

#[cfg(feature = "chaos")]
mod on {
    use super::Point;
    use crate::percpu::MAX_HARTS;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use kcore::chaos::Chaos;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    /// Each hart's generator state, touched only by that hart.
    static STATE: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
    /// How many delays were injected (a diagnostic the boot prints).
    pub static DELAYS: AtomicU64 = AtomicU64::new(0);

    /// Read `chaos=<seed>` from the kernel command line and seed every hart's generator. No argument: no chaos.
    pub fn init() {
        let Some(seed) = crate::dtb::boot_arg_value("chaos=").and_then(|v| v.parse::<u64>().ok()) else {
            return;
        };
        for (hart, slot) in STATE.iter().enumerate() {
            slot.store(Chaos::new(seed, hart).to_raw(), Ordering::Relaxed);
        }
        ENABLED.store(true, Ordering::SeqCst);
        let _ = core::fmt::Write::write_fmt(&mut crate::console::Console, format_args!("kernel: chaos seed {seed}\n"));
    }

    pub fn point(_: Point) {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let hart = crate::percpu::this().index.load(Ordering::Relaxed);
        let Some(slot) = STATE.get(hart) else {
            return;
        };
        let mut g = Chaos::from_raw(slot.load(Ordering::Relaxed));
        let delay = g.next_delay();
        slot.store(g.to_raw(), Ordering::Relaxed);
        if let Some(n) = delay {
            DELAYS.fetch_add(1, Ordering::Relaxed);
            for _ in 0..n {
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(feature = "chaos")]
pub use on::{init, point};
