//! L16: the per-hart frame cache against the frame database, as an exact oracle: every frame is in exactly one place (free in the
//! database, held by a cache, or out with a user), the counts always add up, nothing is handed out twice, and the database's audit
//! passes after the caches are flushed.
use kcore::fcache::{FrameCache, Shared, BATCH, CACHE_MAX};
use kcore::frame::{FrameDb, Pfn};
use std::collections::BTreeSet;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn caches_and_database_account_for_every_frame_exactly() {
    for seed in 1..=40u64 {
        let total = 200usize;
        let mut db = FrameDb::new(100, total);
        let mut caches = [FrameCache::new(), FrameCache::new(), FrameCache::new()];
        let mut out: Vec<BTreeSet<Pfn>> = vec![BTreeSet::new(); 3];
        let mut rng = Lcg(seed);
        for _ in 0..6000 {
            let c = (rng.next() % 3) as usize;
            if rng.next().is_multiple_of(2) {
                if let Some(p) = caches[c].alloc(&mut db) {
                    assert!(
                        out.iter().all(|s| !s.contains(&p)),
                        "frame {p} handed out twice"
                    );
                    assert_eq!(db.refs(p), Some(1), "a handed-out frame has one reference");
                    out[c].insert(p);
                } else {
                    // out of frames: the caches and the others hold everything
                    assert_eq!(db.free_count(), 0);
                }
            } else if let Some(&p) = out[c].iter().next() {
                out[c].remove(&p);
                caches[c].free_sole(p, &mut db);
            }
            let cached: usize = caches.iter().map(|c| c.len()).sum();
            let user: usize = out.iter().map(|s| s.len()).sum();
            assert_eq!(db.free_count() + cached + user, total, "seed {seed}");
            assert!(caches.iter().all(|c| c.len() <= CACHE_MAX));
        }
        for c in &mut caches {
            c.flush(&mut db);
        }
        let user: usize = out.iter().map(|s| s.len()).sum();
        assert_eq!(db.free_count() + user, total);
        for s in &out {
            for &p in s {
                db.dec(p).unwrap();
            }
        }
        assert_eq!(db.free_count(), total);
        db.audit().unwrap();
    }
}

#[test]
fn refills_and_drains_move_a_batch_in_one_take() {
    struct Count {
        db: FrameDb,
        refills: usize,
        drains: usize,
        sizes: Vec<usize>,
    }
    impl Shared for Count {
        fn refill(&mut self, out: &mut [Pfn]) -> usize {
            self.refills += 1;
            self.db.refill(out)
        }
        fn drain(&mut self, frames: &[Pfn]) {
            self.drains += 1;
            self.sizes.push(frames.len());
            self.db.drain(frames);
        }
    }
    let mut s = Count {
        db: FrameDb::new(0, 256),
        refills: 0,
        drains: 0,
        sizes: vec![],
    };
    let mut c = FrameCache::new();
    let mut got = vec![];
    for _ in 0..BATCH {
        got.push(c.alloc(&mut s).unwrap());
    }
    assert_eq!(
        s.refills, 1,
        "a batch of allocations is one take of the lock"
    );
    got.push(c.alloc(&mut s).unwrap());
    assert_eq!(s.refills, 2);
    for p in got {
        c.free_sole(p, &mut s);
    }
    assert_eq!(s.drains, 0, "17 frames fit in a cache of {CACHE_MAX}");
    c.flush(&mut s);
    s.sizes.clear();
    // fill the cache, then one more frame: exactly one batch goes back
    let mut more = vec![];
    for _ in 0..(CACHE_MAX + 1) {
        more.push(s.db.alloc().unwrap());
    }
    for p in more {
        c.free_sole(p, &mut s);
    }
    assert_eq!(s.sizes, vec![BATCH], "a full cache drains one batch");
    c.flush(&mut s);
    assert_eq!(s.db.free_count(), 256);
}

#[test]
fn a_full_cache_drains_and_an_empty_database_says_so() {
    let mut db = FrameDb::new(0, 40);
    let mut c = FrameCache::new();
    let mut held = vec![];
    while let Some(p) = c.alloc(&mut db) {
        held.push(p);
    }
    assert_eq!(held.len(), 40);
    assert!(c.alloc(&mut db).is_none());
    for p in held {
        c.free_sole(p, &mut db);
    }
    assert!(c.len() <= CACHE_MAX);
    c.flush(&mut db);
    assert_eq!(db.free_count(), 40);
    db.audit().unwrap();
}
