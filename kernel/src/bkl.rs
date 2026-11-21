//! The big kernel lock (S5b, spec section 2) and the lock-order checker (K5).
//!
//! One lock protects all shared kernel state. It is taken by `irq::without` (disable interrupts,
//! lock, run, unlock, restore), explicitly at the entry of every trap, and released on every path
//! that leaves kernel code (the trap exit, `enter_user`, the idle loop's `wfi`). It is
//! **reentrant per hart** (a depth count) and **released at a context switch** (K3, SCHED-SMP-SPEC
//! G4): the switching thread releases it completely (`release_all`, keeping its depth on its own
//! stack) and the thread that resumes, on whatever hart, takes it back to that depth (`reacquire`).
//!
//! Until the kernel has finished booting (`enforce`), there is one hart and no concurrency, and
//! `held()` is true; after it, kernel entry points assert that the calling hart holds the lock.

use crate::percpu;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// The lock classes (`klock`, SCHED-SMP-SPEC 3.3): the big kernel lock is `Legacy`.
pub use klock::Class;

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
    percpu::this().held.with(|h| h.check(class, 0).is_err())
}

/// Check that using `class` now would not break the order, for a path that takes no lock but is part of the order all the same (the heap's
/// magazine fast path, MM4): a violation is counted, or a panic, exactly as `note_acquire` does it, so the checker sees every allocation and not
/// just the ones that reach the lock.
pub fn check_use(class: Class) {
    if let Err(v) = percpu::this().held.with(|h| h.check(class, 0)) {
        VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        if PANIC_ON_VIOLATION.load(Ordering::Relaxed) {
            panic!(
                "lock order violation: taking {:?} while holding {:?}",
                v.taking, v.held
            );
        }
    }
}

fn note_acquire(class: Class, instance: u32) {
    let bad = percpu::this().held.with(|h| {
        let bad = h.check(class, instance);
        h.push(class, instance);
        bad
    });
    if let Err(v) = bad {
        VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        if PANIC_ON_VIOLATION.load(Ordering::Relaxed) {
            panic!(
                "lock order violation: taking {:?} while holding {:?}",
                v.taking, v.held
            );
        }
    }
}

fn note_release(class: Class, instance: u32) {
    percpu::this().held.with(|h| h.pop(class, instance));
}

/// The heap and UART locks tell the checker when they are held.
pub fn lock_taken(class: Class) {
    note_acquire(class, 0);
}
pub fn lock_released(class: Class) {
    note_release(class, 0);
}

/// Test support: pretend the calling hart holds `held`, then really take `probe` through the
/// code the kernel uses, with the checker counting instead of panicking, and report whether it
/// flagged the order. (So a lock site that forgot to call the checker, or a checker that never
/// fires, fails the test; the real BKL/heap/UART take paths are exercised, not just a predicate.)
pub fn order_check(held: Class, probe: Class) -> bool {
    let was = crate::irq::save_and_disable();
    PANIC_ON_VIOLATION.store(false, Ordering::SeqCst);
    let me = percpu::this();
    let saved = me.held.with(|h| *h);
    me.held.with(|h| h.push(held, 0));
    let before = VIOLATIONS.load(Ordering::SeqCst);
    match probe {
        Class::Legacy => {
            lock();
            unlock();
        }
        Class::Heap => {
            // the heap's own path: `alloc` notes the class around the allocator (a small block is served from the hart's magazine with
            // no lock, and `check_use` tells the checker all the same: MM4)
            let b = alloc::vec![0u8; 64];
            core::hint::black_box(&b);
            drop(b);
        }
        Class::Uart => {
            use core::fmt::Write;
            let _ = write!(crate::console::Console, "");
            crate::console::with_uart_for_test();
        }
        // (the other classes have no lock in the kernel yet: the pure pair table covers them, `klock/tests/order.rs`)
        other => note_acquire(other, 0),
    }
    let flagged = VIOLATIONS.load(Ordering::SeqCst) > before;
    VIOLATIONS.store(before, Ordering::SeqCst);
    me.held.with(|h| *h = saved);
    PANIC_ON_VIOLATION.store(true, Ordering::SeqCst);
    crate::irq::restore(was);
    flagged
}

// ---- the lock ----
//
// The protocol (ticket order, reentrancy, the depth hand-over) is `kcore::ticket`, explored under
// loom and shuttle on the host (T38); this wraps it with the hart identity and the order checker.

static LOCK: kcore::ticket::Ticket = kcore::ticket::Ticket::new();

fn who() -> u32 {
    percpu::this().index.load(Ordering::Relaxed) as u32
}

/// Take the BKL. Interrupts must already be off (a handler taking it while the interrupted code
/// spun for it with interrupts on would deadlock the hart). Reentrant.
pub fn lock() {
    debug_assert!(!crate::irq::enabled(), "BKL taken with interrupts on");
    let me = who();
    // the order check applies to a reentrant take too: a path that holds HEAP or UART and then
    // takes a lock the hart already owns is still taking it in the wrong order
    if violates(Class::Legacy) {
        VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        if PANIC_ON_VIOLATION.load(Ordering::Relaxed) {
            panic!("lock order violation: taking the BKL while holding a later lock");
        }
    }
    if !LOCK.held_by(me) {
        note_acquire(Class::Legacy, 0);
    }
    let outermost = !LOCK.held_by(me);
    #[cfg(feature = "latency-trace")]
    let (outermost_take, asked) = (outermost, crate::timer::now());
    let spins = core::cell::Cell::new(0u64);
    LOCK.lock(me, &percpu::this().bkl_depth, || {
        core::hint::spin_loop();
        let mut n = spins.get();
        spin_watchdog(&mut n, "the big kernel lock");
        spins.set(n);
    });
    OWNER.store(me as usize + 1, Ordering::Relaxed);
    if outermost {
        percpu::this().bkl_takes.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(feature = "latency-trace")]
    if outermost_take {
        crate::latency::bkl_acquired(crate::timer::now().saturating_sub(asked));
    }
}

/// Take the BKL only if it is free (or already ours).
pub fn try_lock() -> bool {
    debug_assert!(!crate::irq::enabled());
    let me = who();
    let reentrant = LOCK.held_by(me);
    if LOCK.try_lock(me, &percpu::this().bkl_depth) {
        if !reentrant {
            note_acquire(Class::Legacy, 0);
        }
        OWNER.store(me as usize + 1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Release one level.
pub fn unlock() {
    let me = who();
    debug_assert!(
        LOCK.held_by(me),
        "BKL released by a hart that does not hold it"
    );
    // clear the mirror before the release: another hart may take the lock the moment it is out
    let outermost = percpu::this().bkl_depth.get() == 1;
    if outermost {
        note_release(Class::Legacy, 0);
        OWNER.store(0, Ordering::Relaxed);
        crate::latency::bkl_released();
    }
    LOCK.unlock(me, &percpu::this().bkl_depth);
}

/// Release the lock completely, however deeply this hart holds it, and return the depth (SCHED-SMP-SPEC G4: the lock does not
/// follow a thread across a context switch; the switching thread keeps its depth and takes it back with `reacquire` when it
/// resumes). Interrupts stay off throughout.
pub fn release_all() -> u32 {
    let depth = percpu::this().bkl_depth.get();
    for _ in 0..depth {
        unlock();
    }
    depth
}

/// Take the lock again to `depth` levels (the counterpart of `release_all`, run by the thread that resumed).
pub fn reacquire(depth: u32) {
    for _ in 0..depth {
        lock();
    }
}

/// A debug-build deadlock detector: a hart that has spun this long for one lock is stuck, not waiting (a lock is held for
/// microseconds). It panics with who owns the big lock, so a hang becomes a diagnosable failure.
fn spin_watchdog(spins: &mut u64, what: &str) {
    if cfg!(debug_assertions) {
        *spins += 1;
        if *spins == 30_000_000 {
            *spins = 0;
            panic!(
                "hart {} has spun for {what} (last class asked for: {}) for 30 million iterations; the big kernel lock is owned by hart+1 = {}",
                who(),
                percpu::this().last_class.load(Ordering::Relaxed),
                OWNER.load(Ordering::Relaxed)
            );
        }
    }
}

/// Is the BKL free (nobody holds it)? For tests, from any hart.
pub fn is_free() -> bool {
    LOCK.is_free()
}

/// The machine `klock`'s locks run on: this kernel's interrupt control, hart identity and order checker.
pub struct Machine;

impl klock::Machine for Machine {
    fn irq_save() -> bool {
        crate::irq::save_and_disable()
    }
    fn irq_restore(was: bool) {
        crate::irq::restore(was);
    }
    fn hart() -> u32 {
        who()
    }
    fn relax() {
        core::hint::spin_loop();
        let mut n = percpu::this().spins.load(Ordering::Relaxed);
        spin_watchdog(&mut n, "a spin lock");
        percpu::this().spins.store(n, Ordering::Relaxed);
    }
    fn acquired(class: Class, instance: u32) {
        percpu::this()
            .last_class
            .store(class as u32, Ordering::Relaxed);
        // (a new wait: the detector counts one wait, not everything since boot)
        percpu::this().spins.store(0, Ordering::Relaxed);
        note_acquire(class, instance);
        crate::chaos::point(crate::chaos::Point::Acquired);
    }
    fn released(class: Class, instance: u32) {
        note_release(class, instance);
        crate::chaos::point(crate::chaos::Point::Released);
    }
}
