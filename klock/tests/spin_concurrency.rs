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
static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
// (the checkers' own thread-locals: one value per model thread, not per OS thread)
#[cfg(loom)]
loom::thread_local!(static ME: u32 = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
#[cfg(shuttle)]
shuttle::thread_local!(static ME: u32 = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
impl Machine for M {
    fn irq_save() -> bool {
        true
    }
    fn irq_restore(_: bool) {}
    fn hart() -> u32 {
        ME.with(|m| *m)
    }
    fn relax() {
        thread::yield_now();
    }
    fn acquired(_: Class, _: u32) {}
    fn released(_: Class, _: u32) {}
}

fn model_rounds(rounds: u32) {
    NEXT.store(0, std::sync::atomic::Ordering::Relaxed);
    let lock: Arc<Spin<AtomicU32, M>> = Arc::new(Spin::new(Class::Jobs, 0, AtomicU32::new(0)));
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let l = lock.clone();
            thread::spawn(move || {
                for _ in 0..rounds {
                    let g = l.lock();
                    // a read-modify-write that loses updates unless the lock excludes
                    let v = g.load(Ordering::Relaxed);
                    thread::yield_now();
                    g.store(v + 1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(lock.lock().load(Ordering::Relaxed), 2 * rounds);
}

#[cfg(loom)]
#[test]
fn the_lock_excludes_under_loom() {
    loom::model(|| model_rounds(1));
}

#[cfg(shuttle)]
#[test]
fn the_lock_excludes_under_shuttle() {
    shuttle::check_random(|| model_rounds(3), 2000);
}
