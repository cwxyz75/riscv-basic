//! T207 PR 1: the sparse `PageMap` against a `BTreeMap` (the oracle). A random walk over pages up to 2^26 (a 256 GiB VMO), the range walks
//! (`count_frames_in`, `take_frames_in`) against the model, no allocation in any removal, the bytes held at most one leaf per touched chunk,
//! and a failing allocator: an insert that cannot get a leaf returns `NoMemory` and changes nothing.

use abi::status::Status;
use kcore::pagemap::{Entry, PageMap, LEAF_BYTES};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;

thread_local! {
    static COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BYTES: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
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
        BYTES.with(|c| c.set(c.get() + l.size() as isize));
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        BYTES.with(|c| c.set(c.get() - l.size() as isize));
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        BYTES.with(|c| c.set(c.get() + n as isize - l.size() as isize));
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static A: Counting = Counting;

fn allocs() -> usize {
    COUNT.with(|c| c.get())
}
fn bytes() -> isize {
    BYTES.with(|c| c.get())
}

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

#[derive(Clone, Copy, PartialEq, Debug)]
enum M {
    Frame(u64),
    Hole,
}

fn same(m: &PageMap, o: &BTreeMap<u64, M>, probes: &[u64]) {
    let frames = o.values().filter(|v| matches!(v, M::Frame(_))).count();
    assert_eq!(m.len(), frames);
    for &p in probes {
        let want = match o.get(&p) {
            None => Entry::None,
            Some(M::Hole) => Entry::Hole,
            Some(M::Frame(f)) => Entry::Frame(*f),
        };
        assert_eq!(m.entry(p), want, "page {p}");
        assert_eq!(m.contains(p), matches!(want, Entry::Frame(_)));
    }
    let chunks: std::collections::BTreeSet<u64> = o.keys().map(|p| p / 64).collect();
    assert_eq!(m.leaf_count(), chunks.len());
}

#[test]
fn random_walks_agree_with_the_btreemap_up_to_2_pow_26_pages() {
    for seed in 1..=60u64 {
        let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut m = PageMap::new();
        let mut o: BTreeMap<u64, M> = BTreeMap::new();
        // a few hot chunks far apart, so chunks fill, empty and refill
        let bases = [0u64, 70, 1 << 12, (1 << 20) + 5, (1 << 26) - 200];
        let pick = |r: &mut Rng| bases[(r.next() % 5) as usize] + r.next() % 150;
        let mut probes = Vec::new();
        for step in 0..1500 {
            let p = pick(&mut r);
            probes.push(p);
            match r.next() % 8 {
                0..=2 => {
                    let f = r.next() >> 8;
                    assert_eq!(m.insert(p, f), Ok(()));
                    o.insert(p, M::Frame(f));
                }
                3 => {
                    assert_eq!(m.insert_hole(p), Ok(()));
                    o.insert(p, M::Hole);
                }
                4 | 5 => {
                    let want = match o.get(&p) {
                        Some(M::Frame(f)) => Some(*f),
                        _ => None,
                    };
                    assert_eq!(m.remove(p), want);
                    if want.is_some() {
                        o.remove(&p);
                    }
                }
                6 => {
                    let want = match o.get(&p) {
                        Some(M::Frame(f)) => Some(*f),
                        _ => None,
                    };
                    if let std::collections::btree_map::Entry::Occupied(mut e) = o.entry(p) {
                        assert_eq!(m.make_hole(p), want);
                        e.insert(M::Hole);
                    } else {
                        assert_eq!(m.make_hole(p), None);
                    }
                }
                _ => {
                    // the range walks
                    let a = pick(&mut r);
                    let b = a + r.next() % 400;
                    let want = o
                        .range(a..b)
                        .filter(|(_, v)| matches!(v, M::Frame(_)))
                        .count() as u64;
                    assert_eq!(m.count_frames_in(a, b), want);
                    let from = pick(&mut r);
                    let nxt = o.range(from..).next().map(|(k, v)| {
                        (
                            *k,
                            match v {
                                M::Hole => Entry::Hole,
                                M::Frame(f) => Entry::Frame(*f),
                            },
                        )
                    });
                    assert_eq!(m.next_entry_at_or_after(from), nxt);
                }
            }
            if step % 50 == 0 {
                same(&m, &o, &probes);
            }
        }
        same(&m, &o, &probes);
        let mut seen = Vec::new();
        m.drain_pfns(|p| seen.push(p));
        seen.sort_unstable();
        let mut want: Vec<u64> = o
            .values()
            .filter_map(|v| match v {
                M::Frame(f) => Some(*f),
                _ => None,
            })
            .collect();
        want.sort_unstable();
        assert_eq!(seen, want);
        assert!(m.is_empty() && m.leaf_count() == 0);
    }
}

#[test]
fn take_frames_in_batches_in_page_order_and_leaves_holes() {
    let mut m = PageMap::new();
    for p in (0..500u64).step_by(3) {
        m.insert(p, 1000 + p).unwrap();
    }
    m.insert_hole(4).unwrap();
    let total = m.count_frames_in(10, 400);
    let mut got = Vec::new();
    let mut at = 10;
    let mut buf = [0u64; 7];
    loop {
        let (n, next) = m.take_frames_in(at, 400, &mut buf);
        got.extend_from_slice(&buf[..n]);
        if next >= 400 {
            break;
        }
        at = next;
    }
    assert_eq!(got.len() as u64, total);
    assert!(got.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(m.count_frames_in(10, 400), 0);
    assert_eq!(m.entry(4), Entry::Hole);
    assert_eq!(m.entry(9), Entry::Frame(1009));
    assert_eq!(m.entry(402), Entry::Frame(1402));
    assert_eq!(m.entry(403), Entry::None);
}

#[test]
fn removal_allocates_nothing_and_bytes_are_one_leaf_per_touched_chunk() {
    let before = bytes();
    let mut m = PageMap::new();
    // one page per chunk, 200 chunks spread over 2 GiB worth of pages
    for i in 0..200u64 {
        m.insert(i * 2600 + 3, i + 1).unwrap();
    }
    let held = (bytes() - before) as u64;
    // (the leaf, plus the map's own entry, chunks and spare room: at most twice the leaf for these small sizes)
    assert!(
        held <= 200 * LEAF_BYTES * 2 + 64 * 1024,
        "held {held} bytes for 200 leaves"
    );
    assert_eq!(m.leaf_count(), 200);
    let a = allocs();
    for i in 0..200u64 {
        assert_eq!(m.remove(i * 2600 + 3), Some(i + 1));
    }
    assert_eq!(allocs(), a, "removal allocated");
    assert_eq!((m.leaf_count(), m.len()), (0, 0));
    // a page at 2 GiB (page 2^19) costs one leaf, not an outer vector
    let b = bytes();
    m.insert(1 << 19, 7).unwrap();
    assert!(((bytes() - b) as u64) < 16 * 1024);
}

#[test]
fn a_failing_allocator_means_no_memory_and_nothing_changed() {
    let mut m = PageMap::new();
    m.insert(5, 50).unwrap();
    FAILING.with(|c| c.set(true));
    // the same chunk needs no allocation
    let same_chunk = m.insert(6, 60);
    // a new chunk does
    let new_chunk = m.insert(64 * 1000, 70);
    let new_hole = m.insert_hole(64 * 2000);
    FAILING.with(|c| c.set(false));
    assert_eq!(same_chunk, Ok(()));
    assert_eq!(new_chunk, Err(Status::NoMemory));
    assert_eq!(new_hole, Err(Status::NoMemory));
    assert_eq!((m.len(), m.leaf_count()), (2, 1));
    assert_eq!(m.entry(64 * 1000), Entry::None);
    // and it works again once memory is back
    m.insert(64 * 1000, 70).unwrap();
    assert_eq!(m.get(64 * 1000), Some(70));
}
