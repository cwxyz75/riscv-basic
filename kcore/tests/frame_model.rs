//! The frame database against a trivially simple reference model: a map from
//! frame number to reference count, and the set of free frames.
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::frame::{FrameDb, FrameError};
use std::collections::BTreeMap;

#[test]
fn random_operations_agree_with_the_model() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let (base, count) = (100 + rng.below(50), 1 + rng.below(40) as usize);
        let mut db = FrameDb::new(base, count);
        let mut model: BTreeMap<u64, u32> = BTreeMap::new(); // allocated frames and their counts
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            match rng.below(5) {
                0 | 1 => match db.alloc() {
                    Some(pfn) => {
                        assert!(
                            (base..base + count as u64).contains(&pfn),
                            "{ctx}: alloc outside range"
                        );
                        assert!(
                            model.insert(pfn, 1).is_none(),
                            "{ctx}: alloc returned a frame in use"
                        );
                    }
                    None => assert_eq!(model.len(), count, "{ctx}: alloc failed with frames free"),
                },
                2 => {
                    let pfn = base + rng.below(count as u64 + 4) - 2;
                    let result = db.inc(pfn);
                    match model.get_mut(&pfn) {
                        Some(n) => {
                            assert_eq!(result, Ok(()), "{ctx}");
                            *n += 1;
                        }
                        None if (base..base + count as u64).contains(&pfn) => {
                            assert_eq!(result, Err(FrameError::NotAllocated), "{ctx}")
                        }
                        None => assert_eq!(result, Err(FrameError::NotManaged), "{ctx}"),
                    }
                }
                _ => {
                    let pfn = base + rng.below(count as u64 + 4) - 2;
                    let result = db.dec(pfn);
                    match model.get_mut(&pfn) {
                        Some(n) => {
                            *n -= 1;
                            assert_eq!(result, Ok(*n == 0), "{ctx}: dec result");
                            if *n == 0 {
                                model.remove(&pfn);
                            }
                        }
                        None if (base..base + count as u64).contains(&pfn) => {
                            assert_eq!(result, Err(FrameError::NotAllocated), "{ctx}")
                        }
                        None => assert_eq!(result, Err(FrameError::NotManaged), "{ctx}"),
                    }
                }
            }
            assert_eq!(db.free_count(), count - model.len(), "{ctx}: free count");
            for (&pfn, &n) in &model {
                assert_eq!(db.refs(pfn), Some(n), "{ctx}: refs of {pfn}");
            }
            db.audit().unwrap_or_else(|e| panic!("{ctx}: audit: {e}"));
        }
        // Give everything back: the baseline is restored.
        for (pfn, n) in model {
            for _ in 0..n {
                db.dec(pfn).unwrap();
            }
        }
        assert_eq!(db.free_count(), count, "seed {seed}: not back to baseline");
    }
}

#[test]
fn exhaustion_and_reuse() {
    let mut db = FrameDb::new(0, 3);
    let a = db.alloc().unwrap();
    let b = db.alloc().unwrap();
    let c = db.alloc().unwrap();
    assert_eq!(db.alloc(), None);
    assert_eq!(db.dec(b), Ok(true));
    assert_eq!(db.alloc(), Some(b), "a freed frame is handed out again");
    assert_eq!(db.dec(b), Ok(true));
    assert_eq!(
        db.dec(b),
        Err(FrameError::NotAllocated),
        "double free is an error, not a panic"
    );
    let _ = (a, c);
}

#[test]
fn zero_frames_is_fine() {
    let mut db = FrameDb::new(10, 0);
    assert_eq!(db.alloc(), None);
    assert_eq!(db.free_count(), 0);
    db.audit().unwrap();
}
