//! Interrupt masking (`sstatus.SIE`) and the timer interrupt enable bit.

use core::arch::asm;

const SIE_BIT: usize = 1 << 1;
const STIE_BIT: usize = 1 << 5;

/// Disable interrupts, returning whether they were enabled.
pub fn save_and_disable() -> bool {
    let old: usize;
    // SAFETY: atomically clears sstatus.SIE and returns the old sstatus; changes
    // only the interrupt-enable bit.
    unsafe { asm!("csrrci {}, sstatus, 2", out(reg) old) };
    old & SIE_BIT != 0
}

/// Re-enable interrupts if `was_enabled`.
pub fn restore(was_enabled: bool) {
    if was_enabled {
        enable();
    }
}

pub fn enable() {
    // SAFETY: sets sstatus.SIE; callers only do so where a timer interrupt may be taken.
    unsafe { asm!("csrsi sstatus, 2") };
}

pub fn enabled() -> bool {
    let v: usize;
    // SAFETY: reads a CSR.
    unsafe { asm!("csrr {}, sstatus", out(reg) v) };
    v & SIE_BIT != 0
}

/// Allow the supervisor timer interrupt (it is delivered once SIE is also set).
pub fn enable_timer_interrupt() {
    // SAFETY: sets sie.STIE only.
    unsafe { asm!("csrs sie, {}", in(reg) STIE_BIT) };
}

/// Run `f` with interrupts disabled, restoring the previous state after.
pub fn without<R>(f: impl FnOnce() -> R) -> R {
    let was = save_and_disable();
    let r = f();
    restore(was);
    r
}
