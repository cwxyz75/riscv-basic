//! Property tests of the I/O address allocator (IOMMU2; IOMMU-SPEC 4.6): random allocate, release and complete sequences against
//! a page-by-page reference. Every allocation is the reference's lowest fit; no two live ranges overlap; a pending range is never
//! handed out; nothing is lost (completing everything frees the whole window); page 0 is never part of a window.

mod common;

use common::{Rng, SEEDS, STEPS};
use iommucore::iova::{Iova, IovaError, PageState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum S {
    Free,
    Live,
    Pending,
}

fn lowest_fit(pages: &[S], n: usize) -> Option<usize> {
    let mut run = 0;
    for (i, s) in pages.iter().enumerate() {
        if *s == S::Free {
            run += 1;
            if run == n {
                return Some(i + 1 - n);
            }
        } else {
            run = 0;
        }
    }
    None
}

#[test]
fn the_allocator_follows_the_reference() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let first = 1 + rng.below(1 << 30);
        let size = 1 + rng.below(300);
        let mut a = Iova::new(first, size).unwrap();
        let mut pages = vec![S::Free; size as usize];
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut pending: Vec<(u64, u64)> = Vec::new();
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step} window {first:#x}+{size}");
            match rng.below(6) {
                0..=2 => {
                    let bound = if rng.chance(80) { 8 } else { size + 2 };
                    let n = 1 + rng.below(bound);
                    let got = a.alloc(n);
                    let want = lowest_fit(&pages, n as usize).map(|i| first + i as u64);
                    assert_eq!(got, want, "{ctx}: alloc {n}");
                    if let Some(p) = got {
                        for i in p - first..p - first + n {
                            pages[i as usize] = S::Live;
                        }
                        live.push((p, n));
                    }
                }
                3 | 4 if !live.is_empty() => {
                    let (p, n) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    assert_eq!(a.release(p, n), Ok(()), "{ctx}");
                    for i in p - first..p - first + n {
                        pages[i as usize] = S::Pending;
                    }
                    pending.push((p, n));
                }
                5 if !pending.is_empty() => {
                    let (p, n) = pending.swap_remove(rng.below(pending.len() as u64) as usize);
                    assert_eq!(a.complete(p, n), Ok(()), "{ctx}");
                    for i in p - first..p - first + n {
                        pages[i as usize] = S::Free;
                    }
                }
                _ => {
                    // A wrong release or completion changes nothing.
                    let p = first + rng.below(size);
                    let n = 1 + rng.below(4);
                    let before = a.clone();
                    let inside = p - first + n <= size;
                    let all =
                        |s: S| inside && (p - first..p - first + n).all(|i| pages[i as usize] == s);
                    if !all(S::Live) {
                        let want = if inside {
                            IovaError::WrongState
                        } else {
                            IovaError::OutOfRange
                        };
                        assert_eq!(a.release(p, n), Err(want), "{ctx}");
                    }
                    if !all(S::Pending) {
                        let want = if inside {
                            IovaError::WrongState
                        } else {
                            IovaError::OutOfRange
                        };
                        assert_eq!(a.complete(p, n), Err(want), "{ctx}");
                    }
                    if !all(S::Live) && !all(S::Pending) {
                        assert_eq!(a, before, "{ctx}");
                    }
                }
            }
            for (i, s) in pages.iter().enumerate() {
                let want = match s {
                    S::Free => PageState::Free,
                    S::Live => PageState::Live,
                    S::Pending => PageState::Pending,
                };
                assert_eq!(a.state(first + i as u64), Some(want), "{ctx}: page {i}");
            }
            assert_eq!(a.state(first - 1), None);
            assert_eq!(a.state(first + size), None);
            assert_eq!(
                a.free_pages(),
                pages.iter().filter(|s| **s == S::Free).count() as u64,
                "{ctx}"
            );
        }
        // Everything comes back.
        for (p, n) in live.drain(..) {
            a.release(p, n).unwrap();
            pending.push((p, n));
        }
        for (p, n) in pending.drain(..) {
            a.complete(p, n).unwrap();
        }
        assert_eq!(a.free_pages(), size, "seed {seed}");
        assert_eq!(a.alloc(size), Some(first), "seed {seed}");
    }
}

#[test]
fn a_pending_range_is_never_handed_out() {
    // A window of 4 pages: take all, release the middle two; nothing of length 1 or 2 is available until they complete.
    let mut a = Iova::new(1 << 28, 4).unwrap();
    assert_eq!(a.alloc(4), Some(1 << 28));
    a.release(1 << 28, 4).unwrap();
    assert_eq!(a.alloc(1), None);
    a.complete(1 << 28, 4).unwrap();
    assert_eq!(a.alloc(1), Some(1 << 28));
    let mut a = Iova::new(1 << 28, 4).unwrap();
    let p = a.alloc(1).unwrap();
    let q = a.alloc(2).unwrap();
    assert_eq!((p, q), (1 << 28, (1 << 28) + 1));
    a.release(q, 2).unwrap();
    assert_eq!(a.alloc(2), None, "the pending pair is not reused");
    assert_eq!(a.alloc(1), Some((1 << 28) + 3));
    a.complete(q, 2).unwrap();
    assert_eq!(a.alloc(2), Some(q));
}

#[test]
fn page_zero_is_never_in_a_window_and_the_real_window_is_fast() {
    assert_eq!(Iova::new(0, 16), Err(IovaError::BadWindow));
    // An empty range is refused, not a no-op.
    let mut z = Iova::new(16, 16).unwrap();
    let p = z.alloc(1).unwrap();
    assert_eq!(z.release(p, 0), Err(IovaError::OutOfRange));
    assert_eq!(z.release(p, 1), Ok(()));
    assert_eq!(z.complete(p, 0), Err(IovaError::OutOfRange));
    assert_eq!(z.alloc(0), None);
    assert_eq!(z.alloc(17), None);
    assert_eq!(z.free_pages(), 15);
    assert_eq!(Iova::new(1, 0), Err(IovaError::BadWindow));
    assert_eq!(Iova::new(1 << 52, 1), Err(IovaError::BadWindow));
    assert_eq!(Iova::new((1 << 52) - 1, 1).map(|a| a.pages()), Ok(1));
    // IOMMU-SPEC 3.3: the DMA part of the window is IO_BASE + 256 pages to IO_BASE + 2^18 pages.
    let base = (1u64 << 40) >> 12;
    let mut a = Iova::new(base + 256, (1 << 18) - 256).unwrap();
    let mut last = 0;
    for i in 0..2000u64 {
        let p = a.alloc(1 + i % 7).unwrap();
        assert!(p > last);
        last = p;
    }
    assert_eq!(a.alloc(1 << 18), None);
    // A large run at the end after many small ones.
    assert!(a.alloc(100_000).is_some());
}
