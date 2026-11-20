//! The buddy allocator (MM2) against a bitmap reference: random allocations and frees of mixed orders never overlap, are aligned to their
//! order, keep every free-list invariant (`audit`) after every step, and merge back completely when everything is freed. Then `FrameDb`
//! over several ranges with partial contiguous allocations and shared frames, against a plain reference-count map.
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::buddy::{Buddy, Meta};
use kcore::frame::FrameDb;
use std::collections::BTreeMap;

fn ranges_for(seed: u64) -> (u64, usize) {
    // bases that are aligned to little, some and a lot, and counts that are not powers of two
    let bases = [1, 3000, 4096, 5, 262_144, 1000];
    let counts = [97, 1000, 2048, 333, 5000, 1];
    (
        bases[(seed % 6) as usize],
        counts[((seed / 6) % 6) as usize],
    )
}

#[test]
fn random_blocks_never_overlap_stay_aligned_and_merge_back_completely() {
    for seed in 0..SEEDS {
        let (base, count) = ranges_for(seed);
        let mut rng = Rng::new(seed);
        let mut meta = vec![Meta::default(); count];
        let mut b = Buddy::new(base, count, &mut meta);
        b.audit(&meta).unwrap();
        let initial = b.free_blocks(&meta);
        assert_eq!(b.free_count(), count, "seed {seed}");
        let mut used = vec![false; count]; // the reference: which frames are allocated
        let mut live: Vec<(u64, usize)> = Vec::new();
        for step in 0..STEPS {
            let ctx = format!("seed {seed} (base {base}, count {count}) step {step}");
            if rng.chance(55) || live.is_empty() {
                let order = rng.below(7) as usize;
                let free_before = b.free_count();
                match b.alloc(&mut meta, order) {
                    Some(pfn) => {
                        assert_eq!(pfn % (1 << order), 0, "{ctx}: aligned to its order");
                        for f in pfn..pfn + (1 << order) {
                            let i = (f - base) as usize;
                            assert!(!used[i], "{ctx}: frame {f} handed out twice");
                            used[i] = true;
                            meta[i].refs = 1;
                        }
                        assert_eq!(b.free_count(), free_before - (1 << order), "{ctx}");
                        live.push((pfn, order));
                    }
                    None => {
                        // refused: then no free block of that order or larger exists
                        let any = b.free_blocks(&meta).iter().any(|&(_, o)| o >= order);
                        assert!(
                            !any,
                            "{ctx}: refused an order-{order} allocation with a block free"
                        );
                    }
                }
            } else {
                let (pfn, order) = live.swap_remove(rng.below(live.len() as u64) as usize);
                for f in pfn..pfn + (1 << order) {
                    let i = (f - base) as usize;
                    used[i] = false;
                    meta[i].refs = 0;
                }
                b.free_block(&mut meta, pfn, order);
            }
            b.audit(&meta).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let free_frames = used.iter().filter(|u| !**u).count();
            assert_eq!(
                b.free_count(),
                free_frames,
                "{ctx}: free_count against the bitmap"
            );
        }
        for (pfn, order) in live {
            for f in pfn..pfn + (1 << order) {
                meta[(f - base) as usize].refs = 0;
            }
            b.free_block(&mut meta, pfn, order);
        }
        b.audit(&meta).unwrap();
        assert_eq!(
            b.free_blocks(&meta),
            initial,
            "seed {seed}: not merged back to the initial free lists"
        );
    }
}

#[test]
fn a_range_cut_at_unaligned_edges_uses_the_largest_blocks_that_fit() {
    let mut meta = vec![Meta::default(); 11];
    let b = Buddy::new(5, 11, &mut meta); // frames 5..16
    assert_eq!(b.free_blocks(&meta), vec![(5, 0), (6, 1), (8, 3)]);
    b.audit(&meta).unwrap();
}

#[test]
fn frame_db_over_two_ranges_with_partial_runs_and_shared_frames_agrees_with_a_count_map() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let ranges = [(1000u64, 700usize), (8192, 1500)];
        let storage: &'static mut [Meta] =
            Box::leak(vec![Meta::default(); FrameDb::meta_len(&ranges)].into_boxed_slice());
        let mut db = FrameDb::with_ranges(&ranges, storage).unwrap();
        assert!(FrameDb::with_ranges(
            &ranges,
            Box::leak(vec![Meta::default(); 10].into_boxed_slice())
        )
        .is_none());
        assert_eq!(db.total(), 2200);
        let mut refs: BTreeMap<u64, u32> = BTreeMap::new(); // the reference: frame -> holders
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(6) {
                0 | 1 => {
                    let n = 1 + rng.below(40) as usize;
                    let before = db.free_count();
                    match db.alloc_contiguous(n) {
                        Some(first) => {
                            assert_eq!(db.free_count(), before - n, "{ctx}: the tail went back");
                            for f in first..first + n as u64 {
                                assert!(db.is_managed(f), "{ctx}");
                                assert_eq!(
                                    refs.insert(f, 1),
                                    None,
                                    "{ctx}: frame {f} handed out twice"
                                );
                            }
                        }
                        None => assert!(n > 1 || db.free_count() == 0 || before > 0, "{ctx}"),
                    }
                }
                2 => {
                    if let Some(f) = db.alloc() {
                        assert_eq!(refs.insert(f, 1), None, "{ctx}");
                    }
                }
                3 => {
                    if let Some((&f, _)) = refs
                        .iter()
                        .nth(rng.below(refs.len().max(1) as u64) as usize)
                    {
                        db.inc(f).unwrap();
                        *refs.get_mut(&f).unwrap() += 1;
                    }
                }
                _ => {
                    if let Some((&f, &n)) = refs
                        .iter()
                        .nth(rng.below(refs.len().max(1) as u64) as usize)
                    {
                        let freed = db.dec(f).unwrap();
                        assert_eq!(freed, n == 1, "{ctx}");
                        if n == 1 {
                            refs.remove(&f);
                        } else {
                            *refs.get_mut(&f).unwrap() -= 1;
                        }
                    }
                }
            }
            assert_eq!(db.free_count(), 2200 - refs.len(), "{ctx}: free_count");
            db.audit().unwrap_or_else(|e| panic!("{ctx}: {e}"));
        }
        for (&f, &n) in &refs {
            assert_eq!(db.refs(f), Some(n), "seed {seed}: frame {f}");
        }
        assert!(!db.is_managed(999) && !db.is_managed(1700) && !db.is_managed(9692));
        let all: Vec<(u64, u32)> = refs.iter().map(|(&f, &n)| (f, n)).collect();
        for (f, n) in all {
            for _ in 0..n {
                db.dec(f).unwrap();
            }
        }
        assert_eq!(db.free_count(), 2200, "seed {seed}");
        db.audit().unwrap();
        // fully merged again: the largest blocks the two ranges allow
        assert!(
            db.alloc_contiguous(512).is_some(),
            "seed {seed}: a whole 512-frame block is free again"
        );
    }
}
