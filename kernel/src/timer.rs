//! Time: a millisecond clock from the `time` counter, and a periodic tick
//! driven by SBI timer interrupts.

use crate::{cpu, sbi};
use core::sync::atomic::{AtomicU64, Ordering};

/// Frequency of the `time` counter on QEMU's `virt` machine (10 MHz).
pub const TIMEBASE_HZ: u64 = 10_000_000;

/// Interrupt period: 10 ms, i.e. 100 ticks a second.
const TICK_PERIOD: u64 = TIMEBASE_HZ / 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Milliseconds since boot (the source for BASIC's `TIMER`).
pub fn millis() -> u64 {
    cpu::rdtime() / (TIMEBASE_HZ / 1000)
}

/// Number of timer interrupts taken so far.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Start the periodic tick. The trap vector must be installed, and interrupts
/// are only taken once `cpu::enable_interrupts` has been called.
pub fn init() {
    sbi::set_timer(cpu::rdtime() + TICK_PERIOD);
    cpu::enable_timer_interrupt();
}

/// Called from the trap handler on a supervisor timer interrupt.
pub fn on_interrupt() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    sbi::set_timer(cpu::rdtime() + TICK_PERIOD);
}
