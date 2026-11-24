//! T248: the pager tables allocate only in `reserve_room` / `reserve_pager`. Once room is made for N threads, any mix of waits, supplies,
//! failures, cancels, destroys and drains counts zero allocations; a refused reservation leaves the tables as they were and a wait without
//! room reports `NoMemory` (it never allocates).

use abi::status::Status;
use kcore::pager::Pagers;
use std::alloc::{GlobalAlloc, Layout, System};

mod common;
use common::Rng;

thread_local! {
    static COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
struct Counting;
// SAFETY: forwards to the system allocator; the counters are per thread.
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

#[test]
fn waits_supplies_failures_cancels_and_destroys_allocate_nothing() {
    for seed in 0..40 {
        let mut rng = Rng::new(seed);
        let mut p = Pagers::new();
        p.reserve_room(40).unwrap();
        for id in 1..=3u64 {
            p.reserve_pager().unwrap();
            p.create(id);
        }
        let mut spent = 0;
        let mut live = [true; 3];
        for _ in 0..3000 {
            let t = rng.below(40) + 1;
            let (pager, vmo, page) = (rng.below(3) + 1, rng.below(2) + 1, rng.below(6));
            let at = allocs();
            match rng.below(7) {
                0..=2 if !p.waiting(t) => {
                    let _ = p.wait(t, pager, vmo, page);
                }
                3 => p.supply(pager, vmo, page, 2),
                4 => p.fail(pager, vmo, page, 3, Status::TimedOut),
                5 => {
                    p.cancel(t);
                }
                _ if rng.below(40) == 0 && live[(pager - 1) as usize] => {
                    live[(pager - 1) as usize] = false;
                    p.destroy(pager);
                }
                _ => {
                    p.sent(pager);
                }
            }
            p.drain_woken(&mut |_| {});
            spent += allocs() - at;
        }
        assert_eq!(spent, 0, "seed {seed}: the operations allocated");
    }
}

#[test]
fn a_refused_reservation_changes_nothing_and_a_wait_without_room_reports_no_memory() {
    let mut p = Pagers::new();
    p.reserve_room(4).unwrap();
    p.reserve_pager().unwrap();
    p.create(1);
    FAILING.with(|c| c.set(true));
    let refused = p.reserve_room(5000).is_err();
    FAILING.with(|c| c.set(false));
    assert!(refused);
    for t in 1..=4 {
        assert!(p.wait(t, 1, 1, t).is_ok());
    }
    // a fifth waiter had no room made
    FAILING.with(|c| c.set(true));
    let r = p.wait(5, 1, 1, 9);
    FAILING.with(|c| c.set(false));
    assert_eq!(r, Err(Status::NoMemory));
    assert!(!p.waiting(5));
    assert_eq!(p.waits(), 4);
}
