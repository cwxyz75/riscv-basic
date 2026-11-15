//! The doorbell handshake of NET-SPEC 5.5 under loom and shuttle: a producer publishes an entry and signals only when the
//! consumer's waiting flag is set and the ring was empty; the consumer drains, sets the flag, **re-checks**, and only then
//! sleeps. Property: once the producer is done, either the consumer has the entry or a doorbell is pending (it will wake and
//! drain): a lost wakeup is impossible. Negative control: the consumer re-checks *before* setting the flag.
//!   RUSTFLAGS="--cfg loom"    cargo test -p netring --release --test doorbell_concurrency
//!   RUSTFLAGS="--cfg shuttle" cargo test -p netring --release --test doorbell_concurrency
#![cfg(any(loom, shuttle))]

use kcore::sync::{AtomicU32, Ordering};
use netring::{announce_waiting, must_signal, Entry, Mem, Ring, RingId, CAPACITY};

#[cfg(loom)]
use loom::{sync::Arc, thread};
#[cfg(shuttle)]
use shuttle::{sync::Arc, thread};

struct Shm(Vec<AtomicU32>);
impl Mem for Shm {
    fn load(&self, off: usize) -> u32 {
        self.0[off / 4].load(Ordering::Acquire)
    }
    fn store(&self, off: usize, v: u32) {
        self.0[off / 4].store(v, Ordering::Release)
    }
    fn fence(&self) {
        kcore::sync::fence(Ordering::SeqCst)
    }
}

/// The waiting flag lives in the control header, as in the real layout.
const WAITING: usize = netring::ctl::NET_WAITING;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Variant {
    Right,
    /// Negative control 1: re-check the ring before setting the waiting flag.
    FlagAfterRecheck,
    /// Negative control 2: the right order, but without the fences (Dekker's store-load pattern with only
    /// acquire/release ordering: both sides can read the other's old value).
    NoFences,
}

struct World {
    mem: Shm,
    event: AtomicU32,
    consumed: AtomicU32,
    sleeping: AtomicU32,
    v: Variant,
}

fn drain(w: &World) {
    let r = Ring::new(&w.mem, RingId::RxUsed);
    while let Some(_e) = r.pop().unwrap() {
        w.consumed.fetch_add(1, Ordering::SeqCst);
    }
}

fn consumer(w: Arc<World>) {
    drain(&w);
    let r = Ring::new(&w.mem, RingId::RxUsed);
    match w.v {
        Variant::Right => {
            // step 1 to 3 of 5.5 as a loop: drain, announce, re-check; only an empty re-check may sleep
            while announce_waiting(&w.mem, WAITING, RingId::RxUsed) {
                drain(&w);
            }
        }
        Variant::NoFences => {
            w.mem.store(WAITING, 1);
            if !r.is_empty() {
                w.mem.store(WAITING, 0);
                drain(&w);
            }
        }
        Variant::FlagAfterRecheck => {
            if !r.is_empty() {
                drain(&w);
            }
            thread::yield_now();
            w.mem.store(WAITING, 1);
        }
    }
    // step 4: sleep (the model just records that it decided to)
    w.sleeping.store(1, Ordering::SeqCst);
}

fn producer(w: Arc<World>) {
    producer_n(w, 1);
}

/// `n` entries published one after another, each followed by the doorbell decision (a ring that already holds an entry the
/// consumer has not seen is the case the review of S8 showed the old "only when empty" rule loses).
fn producer_n(w: Arc<World>, n: u32) {
    for i in 0..n {
        producer_one(&w, i);
    }
}

fn producer_one(w: &World, i: u32) {
    let r = Ring::new(&w.mem, RingId::RxUsed);
    let was_empty = r.is_empty();
    r.push(Entry {
        slot: 1 + i,
        len: 60,
    })
    .unwrap();
    let signal = if w.v == Variant::NoFences {
        was_empty && w.mem.load(WAITING) == 1
    } else {
        must_signal(&w.mem, WAITING, was_empty)
    };
    if signal {
        w.event.store(1, Ordering::SeqCst);
    }
}

fn model(v: Variant) {
    model_n(v, 1)
}

fn model_n(v: Variant, entries: u32) {
    let words = (2 * 4096) / 4;
    let w = Arc::new(World {
        mem: Shm((0..words).map(|_| AtomicU32::new(0)).collect()),
        event: AtomicU32::new(0),
        consumed: AtomicU32::new(0),
        sleeping: AtomicU32::new(0),
        v,
    });
    let c = {
        let w = w.clone();
        thread::spawn(move || consumer(w))
    };
    let p = {
        let w = w.clone();
        thread::spawn(move || producer_n(w, entries))
    };
    c.join().unwrap();
    p.join().unwrap();
    // the consumer decided to sleep: either it already has the entry, or a doorbell is pending
    let r = Ring::new(&w.mem, RingId::RxUsed);
    let got = w.consumed.load(Ordering::SeqCst) == entries;
    let pending_doorbell = w.event.load(Ordering::SeqCst) == 1;
    assert!(
        got || pending_doorbell || r.is_empty(),
        "lost wakeup: the entry is in the ring, the consumer sleeps and no doorbell is pending ({:?})",
        w.v
    );
    let _ = CAPACITY;
}

fn explore(v: Variant) {
    explore_n(v, 1);
}

fn explore_n(v: Variant, entries: u32) {
    #[cfg(loom)]
    loom::model(move || model_n(v, entries));
    #[cfg(shuttle)]
    shuttle::check_random(move || model_n(v, entries), 3000);
}

#[test]
fn the_handshake_loses_no_wakeup() {
    explore(Variant::Right);
}

// (shuttle explores interleavings under sequential consistency only; loom models the weaker orderings that make the fences
// necessary, so this control runs under loom)
#[cfg(loom)]
#[test]
fn leaving_out_the_fences_is_caught() {
    let caught = std::panic::catch_unwind(|| explore(Variant::NoFences)).is_err();
    assert!(caught, "the negative control must fail the model");
}

#[test]
fn the_handshake_loses_no_wakeup_with_several_entries() {
    explore_n(Variant::Right, 2);
}

#[test]
fn setting_the_flag_after_the_recheck_is_caught() {
    let caught = std::panic::catch_unwind(|| explore(Variant::FlagAfterRecheck)).is_err();
    assert!(caught, "the negative control must fail the model");
}
