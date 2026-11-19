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
loom::thread_local!(static ME: std::cell::Cell<u32> = std::cell::Cell::new(u32::MAX));
#[cfg(shuttle)]
shuttle::thread_local!(static ME: std::cell::Cell<u32> = std::cell::Cell::new(u32::MAX));
impl Machine for M {
    fn irq_save() -> bool {
        true
    }
    fn irq_restore(_: bool) {}
    fn hart() -> u32 {
        ME.with(|m| m.get())
    }
    fn relax() {
        thread::yield_now();
    }
    fn acquired(_: Class, _: u32) {}
    fn released(_: Class, _: u32) {}
}

/// Two harts each add `rounds` with a read-modify-write that loses updates unless something excludes. `locked`: under
/// the real `Spin`; not: the negative control, on a counter nothing protects (the checkers must see it lose).
fn model_rounds_with(rounds: u32, locked: bool) {
    ME.with(|m| m.set(2)); // the model's main thread (the final read of the counter)
    let lock: Arc<Spin<AtomicU32, M>> = Arc::new(Spin::new(Class::Jobs, 0, AtomicU32::new(0)));
    let plain = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = (0..2)
        .map(|id| {
            let (l, p) = (lock.clone(), plain.clone());
            thread::spawn(move || {
                ME.with(|m| m.set(id));
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
    loom::model(move || model_rounds_with(rounds, locked));
    #[cfg(shuttle)]
    shuttle::check_random(move || model_rounds_with(rounds, locked), 2000);
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
