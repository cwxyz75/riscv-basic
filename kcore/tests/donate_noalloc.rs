//! T248: the donation graph allocates only in `reserve_thread` / `add_thread`. A random walk of donate, revoke, retarget and remove_thread over a
//! graph whose threads were added first counts zero allocations; a refused `reserve_thread` leaves the graph as it was.

use kcore::donate::Donations;
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
fn donating_revoking_retargeting_and_removing_allocate_nothing() {
    for seed in 0..40 {
        let mut rng = Rng::new(seed);
        let mut d = Donations::new();
        for t in 1..=40u64 {
            d.add_thread(t, (t % 30) as u8 + 1).unwrap();
        }
        // (what removal and re-adding cost is the table's, not the graph's operations': only the four operations are counted)
        let mut spent = 0;
        for _ in 0..2000 {
            let (a, b) = (rng.below(40) + 1, rng.below(40) + 1);
            let at = allocs();
            match rng.below(5) {
                0 | 1 => {
                    let _ = d.donate(a, b);
                }
                2 => {
                    d.revoke(a);
                }
                3 => {
                    let _ = d.retarget(a, b);
                }
                _ => {
                    d.remove_thread(a);
                    spent += allocs() - at;
                    d.add_thread(a, 7).unwrap();
                    continue;
                }
            }
            spent += allocs() - at;
        }
        assert_eq!(spent, 0, "seed {seed}");
        d.audit().unwrap();
    }
}

#[test]
fn a_refused_reservation_changes_nothing() {
    let mut d = Donations::new();
    d.add_thread(1, 5).unwrap();
    FAILING.with(|c| c.set(true));
    let mut refused = false;
    for _ in 0..1000 {
        if d.reserve_thread().is_err() {
            refused = true;
            break;
        }
        // (room was found: fill it until the table must grow)
        let t = 100 + d.thread_count() as u64;
        if d.add_thread(t, 5).is_err() {
            refused = true;
            break;
        }
    }
    FAILING.with(|c| c.set(false));
    assert!(
        refused,
        "the table grows, so a failing allocator must refuse"
    );
    d.audit().unwrap();
}
