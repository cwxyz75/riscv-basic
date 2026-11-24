//! KT9: `FMap` against `BTreeMap` (the oracle), with a counting allocator for the promise that matters: no allocation between `reserve` and the
//! inserts it covered, and none in any removal. `FBox` makes exactly one allocation.

use kcore::fbox::FBox;
use kcore::fmap::{FMap, CHUNK};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

struct Counting;
// (per thread: the test harness's own threads allocate too)
thread_local! {
    static COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
struct Allocs;
impl Allocs {
    fn load(&self, _: Ordering) -> usize {
        COUNT.with(|c| c.get())
    }
}
static ALLOCS: Allocs = Allocs;
struct Fail;
impl Fail {
    fn store(&self, v: usize, _: Ordering) {
        FAILING.with(|c| c.set(v != 0));
    }
}
static FAIL: Fail = Fail;
fn bump() {
    COUNT.with(|c| c.set(c.get() + 1));
}

// SAFETY: forwards to the system allocator; the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if FAILING.with(|c| c.get()) {
            return std::ptr::null_mut();
        }
        bump();
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        bump();
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Counting = Counting;

// (the tests share the counter: they run one at a time)
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn same(m: &FMap<u64, u64>, o: &BTreeMap<u64, u64>) {
    assert_eq!(m.len(), o.len());
    assert!(m
        .iter()
        .map(|(k, v)| (*k, *v))
        .eq(o.iter().map(|(k, v)| (*k, *v))));
    assert!(m.iter().rev().map(|(k, _)| *k).eq(o.keys().rev().copied()));
    assert_eq!(m.first_key_value(), o.first_key_value());
    assert_eq!(m.last_key_value(), o.last_key_value());
    assert!(
        m.chunk_count() <= 4 * m.len() / CHUNK + 1,
        "chunk count {} for {} entries",
        m.chunk_count(),
        m.len()
    );
}

#[test]
fn random_walks_agree_with_the_btreemap() {
    let _g = ONE_AT_A_TIME.lock().unwrap();
    for seed in 1..=300u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut m, mut o) = (FMap::<u64, u64>::new(), BTreeMap::new());
        let span = 8 + r.next() % 600;
        for step in 0..1500 {
            let k = r.next() % span;
            match r.next() % 8 {
                0..=3 => {
                    let n = 1 + (r.next() % 3) as usize;
                    m.reserve(n).unwrap();
                    assert_eq!(m.insert_reserved(k, step), o.insert(k, step));
                }
                4 | 5 => assert_eq!(m.remove(&k), o.remove(&k)),
                6 => {
                    assert_eq!(m.get(&k), o.get(&k));
                    if let Some(v) = m.get_mut(&k) {
                        *v += 1;
                        *o.get_mut(&k).unwrap() += 1;
                    }
                    assert_eq!(m.contains_key(&k), o.contains_key(&k));
                }
                _ => {
                    let (a, b) = (r.next() % span, r.next() % span);
                    let (lo, hi) = (a.min(b), a.max(b));
                    assert!(m
                        .range(lo..hi)
                        .map(|(k, _)| *k)
                        .eq(o.range(lo..hi).map(|(k, _)| *k)));
                    assert!(m
                        .range(..=hi)
                        .rev()
                        .map(|(k, _)| *k)
                        .eq(o.range(..=hi).rev().map(|(k, _)| *k)));
                    assert_eq!(m.range(..=hi).next_back(), o.range(..=hi).next_back());
                    assert!(m
                        .range(lo..)
                        .map(|(k, _)| *k)
                        .eq(o.range(lo..).map(|(k, _)| *k)));
                    assert_eq!(
                        m.range((std::ops::Bound::Excluded(lo), std::ops::Bound::Unbounded))
                            .next(),
                        o.range((std::ops::Bound::Excluded(lo), std::ops::Bound::Unbounded))
                            .next()
                    );
                }
            }
            same(&m, &o);
        }
        assert_eq!(
            m.short_inserts(),
            0,
            "seed {seed}: an insert found no reserved room"
        );
        m.clear();
        assert!(m.is_empty());
    }
}

#[test]
fn reserve_pays_for_the_inserts_and_removal_allocates_nothing() {
    let _g = ONE_AT_A_TIME.lock().unwrap();
    let mut m = FMap::<u64, u64>::new();
    let mut r = Rng(12345);
    for round in 0..4000 {
        let n = 1 + (r.next() % 4) as usize;
        m.reserve(n).unwrap();
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..n {
            m.insert_reserved(r.next() % 3000, round);
        }
        assert_eq!(
            ALLOCS.load(Ordering::Relaxed),
            before,
            "round {round}: an insert covered by `reserve` allocated"
        );
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..(r.next() % 4) {
            m.remove(&(r.next() % 3000));
        }
        assert_eq!(
            ALLOCS.load(Ordering::Relaxed),
            before,
            "a removal allocated"
        );
    }
}

#[test]
fn a_failing_allocator_makes_reserve_fail_and_changes_nothing() {
    let _g = ONE_AT_A_TIME.lock().unwrap();
    let h = std::thread::Builder::new()
        .name("fmap-fail".into())
        .spawn(|| {
            let mut m = FMap::<u64, u64>::new();
            FAIL.store(1, Ordering::Relaxed);
            let r = m.try_insert(1, 1);
            FAIL.store(0, Ordering::Relaxed);
            assert!(r.is_err(), "no memory: the insert must be refused");
            assert_eq!(m.len(), 0);
            assert!(m.get(&1).is_none());
            // and it works once there is memory again
            assert_eq!(m.try_insert(1, 2), Ok(None));
            assert_eq!(m.get(&1), Some(&2));
        })
        .unwrap();
    h.join().unwrap();
}

#[test]
fn fbox_makes_one_allocation_and_refuses_when_there_is_none() {
    let _g = ONE_AT_A_TIME.lock().unwrap();
    let before = ALLOCS.load(Ordering::Relaxed);
    let mut b = FBox::try_new([7u64; 32]).unwrap();
    assert_eq!(ALLOCS.load(Ordering::Relaxed) - before, 1);
    b[3] = 9;
    assert_eq!(b.into_inner()[3], 9);
    let h = std::thread::Builder::new()
        .name("fmap-fail".into())
        .spawn(|| {
            FAIL.store(1, Ordering::Relaxed);
            let r = FBox::try_new([1u64; 32]);
            FAIL.store(0, Ordering::Relaxed);
            assert_eq!(r.err(), Some([1u64; 32]));
        })
        .unwrap();
    h.join().unwrap();
}
