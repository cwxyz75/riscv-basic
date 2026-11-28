//! The spinlock under loom and shuttle: mutual exclusion and visibility over the real `Spin`. Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p klock --release --test spin_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p klock --release --test spin_concurrency
#![cfg(any(loom, shuttle))]
use kcore::sync::{AtomicU32, Ordering};
use klock::{Class, Machine, Spin};

#[cfg(loom)]
use loom::{sync::Arc, thread};
#[cfg(shuttle)]
use shuttle::{sync::Arc, thread};

struct M;
// (the checkers' own thread-locals: one value per model thread, not per OS thread. Each model thread sets its own hart number when it
// starts; a counter shared by the whole process would be reset and advanced by the tests running in parallel, two threads of one
// model could then share a number and the lock's own recursion check panic or the ticket logic spin: T155)
#[cfg(loom)]
loom::thread_local!(static ME: std::cell::Cell<usize> = std::cell::Cell::new(usize::MAX));
#[cfg(shuttle)]
shuttle::thread_local!(static ME: std::cell::Cell<usize> = std::cell::Cell::new(usize::MAX));
impl Machine for M {
    fn irq_save() -> bool {
        true
    }
    fn irq_restore(_: bool) {}
    fn hart() -> usize {
        ME.with(|m| m.get())
    }
    fn relax() {
        thread::yield_now();
    }
    fn acquired(_: Class, _: u32) {}
    fn released(_: Class, _: u32) {}
}

/// Two harts each add `rounds` with a read-modify-write that loses updates unless something excludes. `locked`: under
/// the real `Spin`; not: the negative control, on a counter nothing protects (the checkers must see it lose). `one_number`:
/// the second negative control, both threads given hart number 0 (what T155's shared counter did now and then, T135).
fn model_rounds_with(rounds: u32, locked: bool, one_number: bool) {
    ME.with(|m| m.set(2)); // the model's main thread (the final read of the counter)
    let lock: Arc<Spin<AtomicU32, M>> = Arc::new(Spin::new(Class::Jobs, 0, AtomicU32::new(0)));
    let plain = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = (0..2)
        .map(|id| {
            let (l, p) = (lock.clone(), plain.clone());
            thread::spawn(move || {
                ME.with(|m| m.set(if one_number { 0 } else { id }));
                for _ in 0..rounds {
                    if locked {
                        let g = l.lock();
                        let v = g.load(Ordering::Relaxed);
                        thread::yield_now();
                        g.store(v + 1, Ordering::Relaxed);
                    } else {
                        let v = p.load(Ordering::Relaxed);
                        thread::yield_now();
                        p.store(v + 1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let total = if locked {
        lock.lock().load(Ordering::Relaxed)
    } else {
        plain.load(Ordering::Relaxed)
    };
    assert_eq!(total, 2 * rounds, "updates were lost");
}

fn explore(rounds: u32, locked: bool) {
    #[cfg(loom)]
    loom::model(move || model_rounds_with(rounds, locked, false));
    #[cfg(shuttle)]
    shuttle::check_random(move || model_rounds_with(rounds, locked, false), 2000);
}

#[test]
fn the_lock_excludes() {
    explore(if cfg!(loom) { 1 } else { 3 }, true);
}

#[test]
fn without_the_lock_the_model_loses_updates() {
    // the negative control: the same model on an unprotected counter must be caught
    assert!(std::panic::catch_unwind(|| explore(1, false)).is_err());
}

/// T135 (2026-11-17, PR 205: a panic at shuttle 0.8.1's `execution.rs:203`) was T155's shared hart number. Two model threads with
/// one number: the second's take passes `Spin::lock`'s recursion check before the first takes the ticket, then finds `held_by(0)`
/// true in `Ticket::lock` and deepens instead of drawing a ticket; both release, so `serving` passes `next` and the next ticket drawn
/// is never served. Shuttle reports that spin as "exceeded max_steps" from `execution.rs:203` (the arm for a failure of the scheduler
/// itself: a task's own panic is re-raised from another line), or the recursion check fires first. This control ties both signatures
/// to the cause: the model with one number for both threads must fail with one of them. (Shuttle only: loom would explore the endless
/// spin to its branch limit.)
#[cfg(shuttle)]
#[test]
fn two_threads_with_one_hart_number_are_caught() {
    let r = std::panic::catch_unwind(|| {
        let mut cfg = shuttle::Config::new();
        cfg.max_steps = shuttle::MaxSteps::FailAfter(20_000); // (the spin is endless: the bound only shortens the wait for it)
        shuttle::Runner::new(
            shuttle::scheduler::RandomScheduler::new_from_seed(2, 2000),
            cfg,
        )
        .run(|| model_rounds_with(3, true, true));
    });
    let msg = r
        .err()
        .and_then(|p| p.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        msg.contains("exceeded max_steps") || msg.contains("recursive take"),
        "the shared number was not caught: {msg:?}"
    );
}
