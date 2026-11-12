//! The big kernel lock (S5b, spec section 2) and the lock-order checker (K5).
//!
//! One lock protects all shared kernel state. It is taken by `irq::without` (disable interrupts,
//! lock, run, unlock, restore), explicitly at the entry of every trap, and released on every path
//! that leaves kernel code (the trap exit, `enter_user`, the idle loop's `wfi`). It is
//! **reentrant per hart** (a depth count) and **the lock follows the thread** (K3): the depth is
//! saved in the `KThread` that blocks or is switched out, and the context switch hands the held
//! lock to the incoming thread with *its* saved depth, so a hart that switches threads never
//! releases and re-takes it and a thread may resume on another hart.
//!
//! Until the kernel has finished booting (`enforce`), there is one hart and no concurrency, and
//! `held()` is true; after it, kernel entry points assert that the calling hart holds the lock.

use crate::percpu;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// The lock classes, in acquisition order: a hart that holds one class may take a class with a
/// *higher* number, never a lower one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Class {
    Bkl = 1,
    Heap = 2,
    Uart = 3,
}

/// Owner of the BKL: hart index + 1, or 0 when free.
static OWNER: AtomicUsize = AtomicUsize::new(0);
static ENFORCED: AtomicBool = AtomicBool::new(false);
/// Violations of the lock order seen (the checker counts instead of panicking while a test is
/// deliberately provoking one).
pub static VIOLATIONS: AtomicU32 = AtomicU32::new(0);
static PANIC_ON_VIOLATION: AtomicBool = AtomicBool::new(true);

fn me() -> usize {
    percpu::this().index.load(Ordering::Relaxed) + 1
}

/// From now on the BKL is real: entry points assert it is held.
pub fn enforce() {
    ENFORCED.store(true, Ordering::SeqCst);
}

/// Does the calling hart hold the BKL (always true before `enforce`)?
pub fn held() -> bool {
    !ENFORCED.load(Ordering::Relaxed) || OWNER.load(Ordering::Relaxed) == me()
}

// ---- the lock-order checker ----

/// Would taking `class` now break the order (the hart holds a class with a higher number)?
pub fn violates(class: Class) -> bool {
    percpu::this().locks_held.load(Ordering::Relaxed) >> (class as u32 + 1) != 0
}

fn note_acquire(class: Class) {
    if violates(class) {
        VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        if PANIC_ON_VIOLATION.load(Ordering::Relaxed) {
            panic!("lock order violation: taking {class:?} while holding a later lock");
        }
    }
    percpu::this()
        .locks_held
        .fetch_or(1 << class as u32, Ordering::Relaxed);
}

fn note_release(class: Class) {
    percpu::this()
        .locks_held
        .fetch_and(!(1 << class as u32), Ordering::Relaxed);
}

/// The heap and UART locks tell the checker when they are held.
pub fn lock_taken(class: Class) {
    note_acquire(class);
}
pub fn lock_released(class: Class) {
    note_release(class);
}

/// Test support: pretend the calling hart holds `class` while `f` runs and report whether the
/// checker flags taking `probe` then (without panicking).
pub fn order_check(held: Class, probe: Class) -> bool {
    let was = crate::irq::save_and_disable();
    PANIC_ON_VIOLATION.store(false, Ordering::SeqCst);
    let me = percpu::this();
    let before = me.locks_held.load(Ordering::Relaxed);
    me.locks_held.fetch_or(1 << held as u32, Ordering::Relaxed);
    let flagged = violates(probe);
    me.locks_held.store(before, Ordering::Relaxed);
    PANIC_ON_VIOLATION.store(true, Ordering::SeqCst);
    crate::irq::restore(was);
    flagged
}

// ---- the lock ----

/// Take the BKL. Interrupts must already be off (a handler taking it while the interrupted code
/// spun for it with interrupts on would deadlock the hart). Reentrant.
pub fn lock() {
    debug_assert!(!crate::irq::enabled(), "BKL taken with interrupts on");
    let hart = percpu::this();
    let me = me();
    if OWNER.load(Ordering::Relaxed) == me {
        hart.bkl_depth.fetch_add(1, Ordering::Relaxed);
        return;
    }
    note_acquire(Class::Bkl);
    while OWNER
        .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    hart.bkl_depth.store(1, Ordering::Relaxed);
}

/// Take the BKL only if it is free (or already ours).
pub fn try_lock() -> bool {
    debug_assert!(!crate::irq::enabled());
    let hart = percpu::this();
    let me = me();
    if OWNER.load(Ordering::Relaxed) == me {
        hart.bkl_depth.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    if OWNER
        .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        note_acquire(Class::Bkl);
        hart.bkl_depth.store(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Release one level.
pub fn unlock() {
    let hart = percpu::this();
    debug_assert_eq!(
        OWNER.load(Ordering::Relaxed),
        me(),
        "BKL released by a hart that does not hold it"
    );
    let d = hart.bkl_depth.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(d >= 1, "BKL depth underflow");
    if d == 1 {
        note_release(Class::Bkl);
        OWNER.store(0, Ordering::Release);
    }
}

/// The context switch's hand-over (K3): this hart keeps the lock, but the nesting depth now
/// belongs to the incoming thread. Returns the outgoing thread's depth (to be saved in it).
pub fn swap_depth(incoming: u32) -> u32 {
    debug_assert!(!ENFORCED.load(Ordering::Relaxed) || OWNER.load(Ordering::Relaxed) == me());
    let hart = percpu::this();
    let out = hart.bkl_depth.load(Ordering::Relaxed);
    hart.bkl_depth.store(incoming, Ordering::Relaxed);
    out
}

/// Is the BKL free (nobody holds it)? For tests, from any hart.
pub fn is_free() -> bool {
    OWNER.load(Ordering::Relaxed) == 0
}
