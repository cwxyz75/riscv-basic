//! Interrupt masking (`sstatus.SIE`), the timer interrupt enable bit, and `without`, the kernel's
//! critical section: interrupts off *and the big kernel lock held* (S5b; see `bkl.rs`).

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

#[track_caller]
pub fn enable() {
    crate::latency::interrupts_on();
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
#[track_caller]
pub fn without<R>(f: impl FnOnce() -> R) -> R {
    let site = core::panic::Location::caller();
    let was = save_and_disable();
    let started = crate::latency::off_begin(was);
    crate::bkl::lock();
    let r = f();
    // the outermost critical section ends here: a defined point for a switch a wake asked for (SCHED-SMP-SPEC X7)
    if crate::percpu::this().bkl_depth.get() == 1 {
        crate::thread::preempt_point();
    }
    crate::bkl::unlock();
    crate::latency::off_end(started, site);
    restore(was);
    r
}
