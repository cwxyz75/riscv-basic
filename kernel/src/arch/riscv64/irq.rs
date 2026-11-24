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

/// Run `f` with interrupts disabled and **no lock**, restoring the previous state after (L20): for the scheduler's and the thread table's own calls, which take the
/// locks they need (`RQ`, `TT`, `PI`) and touch none of the object domain. The end of the outermost one (interrupts were on) is a defined point for a switch a
/// wake asked for (SCHED-SMP-SPEC X7), as the end of a critical section under the object lock is.
#[track_caller]
pub fn off<R>(f: impl FnOnce() -> R) -> R {
    let site = core::panic::Location::caller();
    let was = save_and_disable();
    let started = crate::latency::off_begin(was);
    let r = f();
    if was {
        crate::thread::preempt_point();
    }
    crate::latency::off_end(started, site);
    restore(was);
    r
}

/// Run `f` with interrupts disabled, restoring the previous state after.
#[track_caller]
pub fn without<R>(f: impl FnOnce() -> R) -> R {
    let site = core::panic::Location::caller();
    let was = save_and_disable();
    let started = crate::latency::off_begin(was);
    crate::bkl::lock();
    let r = f();
    // the outermost critical section ends here: what it touched is committed to the waiters before the lock is let go (W1', L20), and it is a defined
    // point for a switch a wake asked for (SCHED-SMP-SPEC X7)
    if crate::percpu::this().bkl_depth.get() == 1 {
        crate::user::commit_if_dirty();
        crate::thread::preempt_point();
    }
    crate::bkl::unlock();
    crate::latency::off_end(started, site);
    restore(was);
    r
}

/// T206: a window in a syscall's long loop. Interrupts are taken here (one nested trap at most: the handler only records `need_resched` and returns with
/// interrupts off, `trap.rs` `window_trap`), then the switch a wake or a tick asked for happens at this depth, as at any preempt point. Called with
/// **no lock held** (the big lock, and every lock of the hart's held set) between the `without` sections of a sweep, a drain or a teardown.
#[track_caller]
pub fn window() {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::this();
    debug_assert!(!crate::bkl::holds(), "irq::window with the big lock held");
    debug_assert!(
        me.held.with(|h| h.is_empty()),
        "irq::window with a lock held"
    );
    if enabled() {
        return; // (not a trap context: the caller's own interrupts are on and a tick is taken anyway)
    }
    me.syscall_window.store(true, Ordering::SeqCst);
    enable();
    save_and_disable();
    me.syscall_window.store(false, Ordering::SeqCst);
    crate::latency::switch_in();
    crate::bkl::lock();
    crate::thread::preempt_point();
    crate::bkl::unlock();
}
