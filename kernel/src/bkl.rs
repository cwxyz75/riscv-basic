//! The big kernel lock (S5b, spec section 2) and the lock-order checker (K5).
//!
//! One lock protects all shared kernel state. It is taken by `irq::without` (disable interrupts,
//! lock, run, unlock, restore), explicitly at the entry of every trap, and released on every path
//! that leaves kernel code (the trap exit, `enter_user`, the idle loop's `wfi`). It is
//! **not reentrant** (a hart that holds it and asks again is a bug: `lock` asserts; the few entry points reached both ways use `ensure`) and **released at a context switch** (K3, SCHED-SMP-SPEC
//! G4): the switching thread releases it completely (`release_all`, keeping its depth on its own
//! stack) and the thread that resumes, on whatever hart, takes it back to that depth (`reacquire`).
//!
//! Until the kernel has finished booting (`enforce`), there is one hart and no concurrency, and
//! `held()` is true; after it, kernel entry points assert that the calling hart holds the lock.

use crate::percpu;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// The lock classes (`klock`, SCHED-SMP-SPEC 3.3): the big kernel lock is `Obj` (L20).
pub use klock::Class;

/// Owner of the BKL: hart index + 1, or 0 when free.
static OWNER: AtomicUsize = AtomicUsize::new(0);
/// Where the current owner took the lock (debug builds: the spin detector names it, T212).
static OWNER_SITE: core::sync::atomic::AtomicPtr<core::panic::Location<'static>> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static ENFORCED: AtomicBool = AtomicBool::new(false);
/// Violations of the lock order seen in the real run (each one also panics; the order test's own probes are counted per hart
/// instead, `PerHart::order_probe_hits`).
pub static VIOLATIONS: AtomicU32 = AtomicU32::new(0);

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
        violation(v);
    }
}

/// A violation the checker found on this hart: counted for the order test while it probes this hart (`order_check`), else counted and
/// a panic.
#[track_caller]
fn violation(v: klock::Violation) {
    let me = percpu::this();
    if me.order_probing.load(Ordering::Relaxed) {
        me.order_probe_hits.fetch_add(1, Ordering::Relaxed);
        return;
    }
    VIOLATIONS.fetch_add(1, Ordering::SeqCst);
    panic!(
        "lock order violation: taking {:?} while holding {:?}",
        v.taking, v.held
    );
}

#[track_caller]
fn note_acquire(class: Class, instance: u32) {
    let bad = percpu::this().held.with(|h| {
        let bad = h.check(class, instance);
        h.push(class, instance);
        bad
    });
    if let Err(v) = bad {
        violation(v);
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

/// Test support: pretend the calling hart holds `held` (instance 0), then really take `probe` (at `instance`; OBJ, HEAP and UART
/// have only instance 0) through the code the kernel uses, with the checker counting instead of panicking, and report whether it
/// flagged the order. (So a lock site that forgot to call the checker, or a checker that never fires, fails the test; the real
/// BKL/heap/UART take paths are exercised, not just a predicate.) The pretence starts from an empty stack, whatever the hart holds,
/// and the hart's own stack is put back afterwards; only this hart counts instead of panicking meanwhile.
#[inline(never)] // (its saved copy of the held stack stays out of the caller's frame, KT13)
pub fn order_check(held: Class, probe: Class, instance: u32) -> bool {
    let was = crate::irq::save_and_disable();
    let me = percpu::this();
    let saved = me.held.with(|h| *h);
    me.held.with(|h| {
        *h = klock::Held::new();
        h.push(held, 0);
    });
    me.order_probe_hits.store(0, Ordering::Relaxed);
    me.order_probing.store(true, Ordering::Relaxed);
    match probe {
        Class::Obj => {
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
        // (the other classes are probed by pretending to hold them; the pure pair table is in `klock/tests/order.rs`)
        other => note_acquire(other, instance),
    }
    me.order_probing.store(false, Ordering::Relaxed);
    let flagged = me.order_probe_hits.load(Ordering::Relaxed) > 0;
    me.held.with(|h| *h = saved);
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

#[track_caller]
/// Take the BKL. Interrupts must already be off (a handler taking it while the interrupted code
/// spun for it with interrupts on would deadlock the hart). The hart must not hold it already (L20).
pub fn lock() {
    debug_assert!(!crate::irq::enabled(), "BKL taken with interrupts on");
    let me = who();
    assert!(
        !LOCK.held_by(me),
        "the object lock taken by a hart that holds it (L20)"
    );
    // the order check: a path that holds a later lock (HEAP, UART, ...) and then takes this one is taking it in the wrong order
    note_acquire(Class::Obj, 0);
    #[cfg(feature = "latency-trace")]
    let asked = crate::timer::now();
    let spins = core::cell::Cell::new(0u64);
    LOCK.lock(me, &percpu::this().bkl_depth, || {
        core::hint::spin_loop();
        let mut n = spins.get();
        spin_watchdog(&mut n, "the big kernel lock", true);
        spins.set(n);
    });
    OWNER.store(me as usize + 1, Ordering::Relaxed);
    percpu::this().bkl_takes.fetch_add(1, Ordering::Relaxed);
    #[cfg(debug_assertions)]
    OWNER_SITE.store(
        core::panic::Location::caller() as *const _ as *mut _,
        Ordering::Relaxed,
    );
    #[cfg(feature = "latency-trace")]
    crate::latency::bkl_acquired(crate::timer::now().saturating_sub(asked));
}

/// Does this hart hold the lock now (for real: unlike `held`, false before `enforce` too)?
pub fn holds() -> bool {
    LOCK.held_by(who())
}

/// Run `f` under the lock: as it is if this hart holds it, else in a critical section of its own. For the few entry points that are reached both ways (from
/// a syscall's locked section and from the unlocked tail of a trap or a kernel thread); everything below them assumes the lock.
#[track_caller]
pub fn ensure<R>(f: impl FnOnce() -> R) -> R {
    if holds() {
        f()
    } else {
        crate::irq::without(f)
    }
}

/// Take the BKL only if it is free.
pub fn try_lock() -> bool {
    debug_assert!(!crate::irq::enabled());
    let me = who();
    let reentrant = LOCK.held_by(me);
    if LOCK.try_lock(me, &percpu::this().bkl_depth) {
        if !reentrant {
            note_acquire(Class::Obj, 0);
        }
        OWNER.store(me as usize + 1, Ordering::Relaxed);
        if !reentrant {
            percpu::this().bkl_takes.fetch_add(1, Ordering::Relaxed); // (an outermost take, as `lock` counts it: the spin report reads it)
        }
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
        note_release(Class::Obj, 0);
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
/// microseconds). It panics with who owns the big lock, where the owner took it and (T266, T212) whether the owner held it the whole
/// wait: at the first spin of a wait for the big lock (`big`) the owner, the owner hart's outermost takes and the clock are recorded, and
/// the report gives the takes since, the time waited and the reading (`kcore::watchdog::spin_verdict`). A wait for a `klock` spin lock
/// (`big` false) reports the big lock's owner as before, without a verdict: the owner of the lock waited on is not recorded.
fn spin_watchdog(spins: &mut u64, what: &str, big: bool) {
    if cfg!(debug_assertions) {
        *spins += 1;
        if *spins == 1 && big {
            let me = percpu::this();
            let owner = OWNER.load(Ordering::Relaxed) as u64;
            me.spin_owner.store(owner, Ordering::Relaxed);
            me.spin_owner_takes
                .store(takes_of(owner), Ordering::Relaxed);
            me.spin_at.store(crate::timer::now_ns(), Ordering::Relaxed);
        }
        if *spins == 30_000_000 {
            *spins = 0;
            spin_report(what, big);
        }
    }
}

/// Hart `owner - 1`'s outermost takes of the big lock (`owner` as `OWNER` holds it: hart + 1, 0 none; 0 for none or a hart not started).
fn takes_of(owner: u64) -> u64 {
    owner
        .checked_sub(1)
        .and_then(|h| usize::try_from(h).ok())
        .filter(|&h| h < crate::smp::started().min(percpu::MAX_HARTS))
        .map_or(0, |h| percpu::hart(h).bkl_takes.load(Ordering::Relaxed))
}

/// The deadlock detector's report (`spin_watchdog`): out of line, so the spin loop's frame stays small.
#[cold]
#[inline(never)]
fn spin_report(what: &str, big: bool) -> ! {
    let me = percpu::this();
    let start = kcore::watchdog::SpinStart {
        owner: me.spin_owner.load(Ordering::Relaxed),
        owner_takes: me.spin_owner_takes.load(Ordering::Relaxed),
        at_ns: me.spin_at.load(Ordering::Relaxed),
    };
    let owner = OWNER.load(Ordering::Relaxed) as u64;
    let verdict = if big {
        alloc::format!(
            "; owner at the start: hart+1 = {}, {}",
            start.owner,
            kcore::watchdog::spin_verdict(
                start,
                owner,
                takes_of(start.owner),
                crate::timer::now_ns()
            )
        )
    } else {
        alloc::string::String::new()
    };
    panic!(
        "hart {} has spun for {what} (last class asked for: {}) for 30 million iterations; the big kernel lock is owned by hart+1 = {}, taken at {}{verdict}",
        who(),
        me.last_class.load(Ordering::Relaxed),
        owner,
        // SAFETY: the pointer is null or a `&'static Location` stored by `lock`.
        unsafe { OWNER_SITE.load(Ordering::Relaxed).as_ref() }
            .map_or("?".into(), |l| alloc::format!("{}:{}", l.file(), l.line())),
    );
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
    fn hart() -> usize {
        percpu::this().index.load(Ordering::Relaxed)
    }
    fn relax() {
        core::hint::spin_loop();
        let mut n = percpu::this().spins.load(Ordering::Relaxed);
        spin_watchdog(&mut n, "a spin lock", false);
        percpu::this().spins.store(n, Ordering::Relaxed);
    }
    #[track_caller]
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
