//! L16: the per-hart frame caches under shuttle. Three model threads (harts), each with its own cache over one database behind a mutex
//! (`FRAME`): random allocs and frees, with refills and drains racing. Properties: no frame is held by two threads, and when every
//! cache is flushed the database has every frame back and passes its audit.
//!
//! Run by `make os-test` with `RUSTFLAGS="--cfg shuttle"`; without the cfg this file is empty.
#![cfg(shuttle)]

use kcore::fcache::{FrameCache, Shared};
use kcore::frame::{FrameDb, Pfn};
use shuttle::rand::Rng;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;
use std::collections::BTreeSet;

struct Locked(Arc<Mutex<FrameDb>>);
impl Shared for Locked {
    fn refill(&mut self, out: &mut [Pfn]) -> usize {
        self.0.lock().unwrap().refill(out)
    }
    fn drain(&mut self, frames: &[Pfn]) {
        self.0.lock().unwrap().drain(frames)
    }
}

fn run() {
    let total = 96usize;
    let db = Arc::new(Mutex::new(FrameDb::new(0, total)));
    let owned = Arc::new(Mutex::new(BTreeSet::<Pfn>::new()));
    let mut hs = vec![];
    for _ in 0..3 {
        let (db, owned) = (db.clone(), owned.clone());
        hs.push(thread::spawn(move || {
            let mut shared = Locked(db);
            let mut cache = FrameCache::new();
            let mut mine = vec![];
            let mut rng = shuttle::rand::thread_rng();
            for _ in 0..60 {
                if rng.gen_bool(0.55) {
                    if let Some(p) = cache.alloc(&mut shared) {
                        assert!(owned.lock().unwrap().insert(p), "frame {p} held twice");
                        mine.push(p);
                    }
                } else if let Some(p) = mine.pop() {
                    assert!(owned.lock().unwrap().remove(&p));
                    cache.free_sole(p, &mut shared);
                }
            }
            for p in mine {
                assert!(owned.lock().unwrap().remove(&p));
                cache.free_sole(p, &mut shared);
            }
            cache.flush(&mut shared);
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let db = db.lock().unwrap();
    assert_eq!(db.free_count(), total);
    db.audit().unwrap();
}

#[test]
fn caches_over_one_locked_database_lose_and_duplicate_nothing() {
    shuttle::check_random(run, 300);
}
