//! T188: the per-hart claim cells of the FP and vector units under the concurrency checkers. Run with
//!   RUSTFLAGS="--cfg loom"    cargo test -p kcore --release --test fp_claim_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test fp_claim_concurrency
//! (`make os-test` does both). Without either cfg this file is empty.
//!
//! Hart 0 runs thread T and reads its own cells without a lock (`light_target`, `quiet_return`). Hart 1, under a mutex standing for the big lock,
//! takes over thread U, which held hart 0's claim before T was loaded there (the `clear(old)` of `claim`, guarded by `claim(old) == tid`). Hart 0,
//! also under the mutex, loads T and claims (`set`). Asserted, over every interleaving: until T's claim is set hart 0 never gets a save address and
//! never a quiet return; once set, it gets exactly T's own address and a quiet return, whatever hart 1 does next.
//!
//! Mutations (by hand, `kcore/src/fp.rs`): `light_target` without the `claim == tid` compare returns U's address before T's claim (first assert);
//! `claim` without the `== tid` guard on the old hart wipes T's claim after the set (second assert).
#![cfg(any(loom, shuttle))]

use kcore::fp::{
    claim, light_target, quiet_return, ClaimCells, FpHart, Lazy, StateUnit, Tokens, UnitThread,
};
use kcore::sync::{AtomicU64, AtomicUsize, Ordering};

#[cfg(loom)]
use loom::{
    sync::{Arc, Mutex},
    thread,
};
#[cfg(shuttle)]
use shuttle::{
    sync::{Arc, Mutex},
    thread,
};

/// A register file with nothing in it: only the bookkeeping is under test.
struct Dummy;
impl StateUnit for Dummy {
    type Regs = u8;
    fn save(&mut self) -> u8 {
        0
    }
    fn load(&mut self, _: &u8) {}
}

struct Cells {
    claim: [AtomicU64; 2],
    buf: [AtomicUsize; 2],
}

impl ClaimCells for Cells {
    fn claim(&self, hart: usize) -> u64 {
        self.claim[hart].load(Ordering::Acquire)
    }
    fn buf(&self, hart: usize) -> usize {
        self.buf[hart].load(Ordering::Acquire)
    }
    fn set(&self, hart: usize, tid: u64, buf: usize) {
        self.buf[hart].store(buf, Ordering::Release);
        self.claim[hart].store(tid, Ordering::Release);
    }
    fn clear(&self, hart: usize) {
        self.claim[hart].store(0, Ordering::Release);
        self.buf[hart].store(0, Ordering::Release);
    }
}

const T: u64 = 1;
const U: u64 = 2;
const ADDR_T: usize = 0x1000;
const ADDR_U: usize = 0x2000;

fn takeover_races_the_light_path() {
    let cells = Arc::new(Cells {
        claim: [AtomicU64::new(0), AtomicU64::new(0)],
        buf: [AtomicUsize::new(0), AtomicUsize::new(0)],
    });
    // U was last loaded on hart 0
    cells.set(0, U, ADDR_U);
    let big_lock = Arc::new(Mutex::new(()));
    let (c1, l1) = (cells.clone(), big_lock.clone());
    let other = thread::spawn(move || {
        // hart 1 loads U (U's holder was hart 0): the old hart's claim goes, if it still names U
        let _g = l1.lock().unwrap();
        // (the protocol's own `claim`, not a copy of it: it clears hart 0's claim only if it still names U, and sets hart 1's)
        let mut tokens = Tokens::new();
        let mut u = UnitThread::<u8>::new();
        u.enable_with(&mut tokens, 0);
        let mut harts = [FpHart::new(), FpHart::new()];
        harts[1].return_to_user(&u, &mut Dummy);
        u.set_holder(Some(0));
        claim(&*c1, &mut u, &mut harts, U, 1);
    });
    // hart 0 is running T, whose state is not in the registers yet: the lock-free paths must say no
    assert_eq!(
        light_target(&*cells, 0, T),
        None,
        "U's buffer offered for T"
    );
    assert!(
        !quiet_return(&*cells, 0, T, Lazy::Clean),
        "a quiet return for a thread that holds no claim"
    );
    {
        // the locked load of T, then its claim
        let _g = big_lock.lock().unwrap();
        cells.set(0, T, ADDR_T);
    }
    other.join().unwrap();
    // whatever hart 1 did, T's claim stands and names T's own buffer
    assert_eq!(
        light_target(&*cells, 0, T),
        Some(ADDR_T),
        "T's claim was lost"
    );
    assert!(quiet_return(&*cells, 0, T, Lazy::Dirty));
    assert_eq!(cells.claim(1), U);
}

#[cfg(loom)]
#[test]
fn loom_takeover_races_the_light_path() {
    loom::model(takeover_races_the_light_path);
}

#[cfg(shuttle)]
#[test]
fn shuttle_takeover_races_the_light_path() {
    shuttle::check_random(takeover_races_the_light_path, 2000);
}
