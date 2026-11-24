//! T248: the PI futex allocates only in `lock`, in one reservation made before anything changes. A failing allocator makes `lock` report
//! `NoMemory` with the word, the graph and the tables as they were; once a waiter is queued, hand-over, cancel and the owner's death allocate
//! nothing (a counting allocator over a random walk).

use kcore::donate::Donations;
use kcore::pifutex::{Lock, PiError, PiFutexes, Unlock, Word};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

mod common;
use common::Rng;

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static FAILING: Cell<bool> = const { Cell::new(false) };
}
struct Counting;
// SAFETY: forwards to the system allocator (or fails, which the contract allows); the counters are per thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if FAILING.with(|c| c.get()) {
            return std::ptr::null_mut();
        }
        COUNT.with(|c| c.set(c.get() + 1));
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if FAILING.with(|c| c.get()) {
            return std::ptr::null_mut();
        }
        COUNT.with(|c| c.set(c.get() + 1));
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static A: Counting = Counting;

fn allocs() -> usize {
    COUNT.with(|c| c.get())
}

struct W(Cell<u32>);
impl Word for W {
    fn load(&self) -> u32 {
        self.0.get()
    }
    fn cas(&self, old: u32, new: u32) -> Result<u32, u32> {
        if self.0.get() == old {
            self.0.set(new);
            Ok(old)
        } else {
            Err(self.0.get())
        }
    }
}

#[test]
fn a_refused_reservation_leaves_the_word_and_the_tables_as_they_were() {
    let mut d = Donations::new();
    for t in 1..=3 {
        d.add_thread(t, 5 + t as u8).unwrap();
    }
    let mut pi = PiFutexes::new();
    let w = W(Cell::new(0));
    let key = (1, 0x1000);
    assert_eq!(
        pi.lock(&mut d, &w, key, 1, |_| true, true, &mut |_| {}),
        Ok(Lock::Acquired)
    );
    FAILING.with(|c| c.set(true));
    let r = pi.lock(&mut d, &w, key, 2, |_| true, true, &mut |_| {});
    FAILING.with(|c| c.set(false));
    assert_eq!(r, Err(PiError::NoMemory));
    assert_eq!(w.0.get(), 1, "the WAITERS bit was not set");
    assert!(pi.is_empty() && d.target(2).is_none());
    pi.audit(&d).unwrap();
    // (and with memory the same call queues)
    assert_eq!(
        pi.lock(&mut d, &w, key, 2, |_| true, true, &mut |_| {}),
        Ok(Lock::Blocked)
    );
}

#[test]
fn hand_over_cancel_and_death_allocate_nothing_once_queued() {
    for seed in 0..60 {
        let mut rng = Rng::new(seed);
        let mut d = Donations::new();
        for t in 1..=8u64 {
            d.add_thread(t, (rng.below(40) + 1) as u8).unwrap();
        }
        let mut pi = PiFutexes::new();
        let words: Vec<W> = (0..3).map(|_| W(Cell::new(0))).collect();
        let keys: Vec<(u64, u64)> = (0..3).map(|k| (1, 0x1000 + k * 8)).collect();
        for _ in 0..300 {
            let (t, k) = (rng.below(8) + 1, rng.below(3) as usize);
            match rng.below(4) {
                0 | 1 => {
                    // (lock may allocate: it is the reportable step; a queued thread is blocked and calls nothing)
                    if pi.still_waiting(t) {
                        continue;
                    }
                    let _ = pi.lock(&mut d, &words[k], keys[k], t, |_| true, true, &mut |_| {});
                }
                2 => {
                    if pi.still_waiting(t) {
                        continue;
                    }
                    let at = allocs();
                    let r = pi.unlock(&mut d, &words[k], keys[k], t, &mut |_| {});
                    assert_eq!(allocs() - at, 0, "unlock {r:?}");
                    if let Ok(Unlock::Handed { to }) = r {
                        pi.take_outcome(to);
                    }
                    pi.drain_wake_into(|_| {});
                }
                _ => {
                    let at = allocs();
                    pi.cancel(&mut d, t, &mut |_| {});
                    assert_eq!(allocs() - at, 0, "cancel");
                }
            }
            pi.audit(&d).unwrap();
            d.audit().unwrap();
        }
    }
}
