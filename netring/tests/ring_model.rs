//! H10 of NET-SPEC 12.1: the ring and slot-ownership model against a reference that tracks every slot's location.
//! Random operations from both sides and a "device"; after each one N1 holds (every slot is in exactly one place, and
//! each ring holds exactly the slots the reference says it does); malformed entries are rejected.
use netring::*;
use std::sync::atomic::{AtomicU32, Ordering};

struct Shm(Vec<AtomicU32>);
impl Shm {
    fn new() -> Shm {
        Shm((0..2 * 4096 / 4).map(|_| AtomicU32::new(0)).collect())
    }
}
impl Mem for Shm {
    fn load(&self, off: usize) -> u32 {
        self.0[off / 4].load(Ordering::Acquire)
    }
    fn store(&self, off: usize, v: u32) {
        self.0[off / 4].store(v, Ordering::Release)
    }
    fn fence(&self) {
        std::sync::atomic::fence(Ordering::SeqCst)
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rx {
    Device,
    Driver,
    InUsed,
    Stack,
    InFree,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tx {
    Free,
    Stack,
    InReady,
    Driver,
    Device,
    InDone,
}

fn run(seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mem = Shm::new();
    let ring = |id| Ring::new(&mem, id);
    let (mut rx, mut tx) = ([Rx::Device; SLOTS], [Tx::Free; SLOTS]);
    // each side's own view (5.4)
    let mut stack_rx = Pool::rx_posted(); // DeviceHas = not mine
    let mut stack_tx = Pool::tx_free();
    let mut drv_rx = Pool::rx_posted(); // DeviceHas = posted
    let mut drv_tx = Pool::tx_free(); // StackFree = the stack's (not the driver's)
    for step in 0..3000 {
        let ctx = format!("seed {seed} step {step}");
        match rng.below(8) {
            // the device finishes a posted receive: the driver holds it
            0 => {
                let start = rng.below(SLOTS);
                if let Some(s) = (0..SLOTS)
                    .map(|k| (k + start) % SLOTS)
                    .find(|&k| rx[k] == Rx::Device)
                {
                    rx[s] = Rx::Driver;
                    drv_rx
                        .mv(s as u32, Slot::DeviceHas, Slot::DriverHas)
                        .unwrap();
                }
            }
            // the driver publishes a received frame
            1 => {
                if let Some(s) = (0..SLOTS).find(|&k| rx[k] == Rx::Driver) {
                    let len = 14 + rng.below(FRAME_MAX - 13) as u16;
                    ring(RingId::RxUsed)
                        .push(Entry {
                            slot: s as u32,
                            len,
                        })
                        .unwrap();
                    rx[s] = Rx::InUsed;
                    drv_rx
                        .mv(s as u32, Slot::DriverHas, Slot::InRing(RingId::RxUsed))
                        .unwrap();
                }
            }
            // the stack takes a received frame
            2 => {
                if let Some(e) = ring(RingId::RxUsed).pop().unwrap() {
                    let e = validate(RingId::RxUsed, e).unwrap();
                    assert_eq!(
                        rx[e.slot as usize],
                        Rx::InUsed,
                        "{ctx}: the ring carried a slot that was not in it"
                    );
                    rx[e.slot as usize] = Rx::Stack;
                    stack_rx
                        .mv(e.slot, Slot::DeviceHas, Slot::StackHas)
                        .unwrap();
                }
            }
            // the stack returns a receive slot; the driver re-posts it
            3 => {
                if let Some(s) = (0..SLOTS).find(|&k| rx[k] == Rx::Stack) {
                    ring(RingId::RxFree)
                        .push(Entry {
                            slot: s as u32,
                            len: 0,
                        })
                        .unwrap();
                    rx[s] = Rx::InFree;
                    stack_rx
                        .mv(s as u32, Slot::StackHas, Slot::DeviceHas)
                        .unwrap();
                }
                if let Some(e) = ring(RingId::RxFree).pop().unwrap() {
                    let e = validate(RingId::RxFree, e).unwrap();
                    assert_eq!(rx[e.slot as usize], Rx::InFree);
                    rx[e.slot as usize] = Rx::Device;
                    drv_rx
                        .mv(e.slot, Slot::InRing(RingId::RxUsed), Slot::DeviceHas)
                        .unwrap();
                }
            }
            // the stack fills a transmit slot and publishes it
            4 => {
                if let Some(s) = (0..SLOTS).find(|&k| tx[k] == Tx::Free) {
                    stack_tx
                        .mv(s as u32, Slot::StackFree, Slot::StackHas)
                        .unwrap();
                    tx[s] = Tx::Stack;
                    if rng.below(2) == 0 {
                        let len = 14 + rng.below(FRAME_MAX - 13) as u16;
                        ring(RingId::TxReady)
                            .push(Entry {
                                slot: s as u32,
                                len,
                            })
                            .unwrap();
                        tx[s] = Tx::InReady;
                        stack_tx
                            .mv(s as u32, Slot::StackHas, Slot::DeviceHas)
                            .unwrap();
                    } else {
                        // abandoned before publishing: back to the free list
                        stack_tx
                            .mv(s as u32, Slot::StackHas, Slot::StackFree)
                            .unwrap();
                        tx[s] = Tx::Free;
                    }
                }
            }
            // the driver takes a transmit entry and hands it to the device
            5 => {
                if let Some(e) = ring(RingId::TxReady).pop().unwrap() {
                    let e = validate(RingId::TxReady, e).unwrap();
                    assert_eq!(tx[e.slot as usize], Tx::InReady);
                    tx[e.slot as usize] = Tx::Device;
                    drv_tx.mv(e.slot, Slot::StackFree, Slot::DeviceHas).unwrap();
                }
            }
            // the device finishes a transmit: the driver reports it done
            6 => {
                if let Some(s) = (0..SLOTS).find(|&k| tx[k] == Tx::Device) {
                    ring(RingId::TxDone)
                        .push(Entry {
                            slot: s as u32,
                            len: 0,
                        })
                        .unwrap();
                    tx[s] = Tx::InDone;
                    drv_tx
                        .mv(s as u32, Slot::DeviceHas, Slot::StackFree)
                        .unwrap();
                }
            }
            // the stack reclaims a finished transmit slot
            _ => {
                if let Some(e) = ring(RingId::TxDone).pop().unwrap() {
                    let e = validate(RingId::TxDone, e).unwrap();
                    assert_eq!(tx[e.slot as usize], Tx::InDone);
                    tx[e.slot as usize] = Tx::Free;
                    stack_tx
                        .mv(e.slot, Slot::DeviceHas, Slot::StackFree)
                        .unwrap();
                }
            }
        }
        // N1: every slot is somewhere; the rings hold exactly the slots the reference puts in them
        let n = |r: Rx| rx.iter().filter(|&&x| x == r).count();
        let m = |t: Tx| tx.iter().filter(|&&x| x == t).count();
        assert_eq!(ring(RingId::RxUsed).len().unwrap(), n(Rx::InUsed), "{ctx}");
        assert_eq!(ring(RingId::RxFree).len().unwrap(), n(Rx::InFree), "{ctx}");
        assert_eq!(
            ring(RingId::TxReady).len().unwrap(),
            m(Tx::InReady),
            "{ctx}"
        );
        assert_eq!(ring(RingId::TxDone).len().unwrap(), m(Tx::InDone), "{ctx}");
        assert_eq!(
            n(Rx::Device) + n(Rx::Driver) + n(Rx::InUsed) + n(Rx::Stack) + n(Rx::InFree),
            SLOTS
        );
        assert_eq!(
            m(Tx::Free)
                + m(Tx::Stack)
                + m(Tx::InReady)
                + m(Tx::Driver)
                + m(Tx::Device)
                + m(Tx::InDone),
            SLOTS
        );
        // the stack's own accounting agrees with the reference
        assert_eq!(
            stack_tx.count(Slot::StackFree),
            m(Tx::Free),
            "{ctx}: the stack's free list"
        );
    }
}

#[test]
fn random_operations_keep_every_slot_in_exactly_one_place() {
    for seed in 1..300 {
        run(seed);
    }
}

#[test]
fn malformed_entries_are_rejected() {
    use RingId::*;
    let ok = |ring, slot, len| validate(ring, Entry { slot, len });
    assert!(ok(RxUsed, 127, 14).is_ok());
    assert_eq!(ok(RxUsed, 128, 60), Err(Violation::SlotOutOfRange));
    assert_eq!(ok(RxUsed, u32::MAX, 60), Err(Violation::SlotOutOfRange));
    assert_eq!(ok(RxUsed, 3, 1515), Err(Violation::LenTooBig));
    assert_eq!(ok(RxUsed, 3, u16::MAX), Err(Violation::LenTooBig));
    assert_eq!(
        ok(RxUsed, 3, 13),
        Err(Violation::LenTooSmall),
        "a received frame shorter than an Ethernet header"
    );
    assert!(
        ok(TxDone, 3, 0).is_ok(),
        "completion entries carry no length"
    );
    // a slot in the wrong state is a violation: the same slot published twice
    let mut stack_rx = Pool::rx_posted();
    stack_rx.mv(5, Slot::DeviceHas, Slot::StackHas).unwrap();
    assert_eq!(
        stack_rx.mv(5, Slot::DeviceHas, Slot::StackHas),
        Err(Violation::WrongState)
    );
    assert_eq!(
        stack_rx.mv(200, Slot::DeviceHas, Slot::StackHas),
        Err(Violation::SlotOutOfRange)
    );
    let mut tx = Pool::tx_free();
    assert_eq!(
        tx.mv(1, Slot::DeviceHas, Slot::StackFree),
        Err(Violation::WrongState),
        "a completion for a slot the stack never sent"
    );
}

#[test]
fn a_peer_that_corrupts_its_index_is_detected_not_followed() {
    let mem = Shm::new();
    let r = Ring::new(&mem, RingId::RxUsed);
    // the producer index jumps far ahead of the consumer: more entries than the capacity
    mem.store(ctl::INDICES, 5000);
    assert_eq!(r.len(), Err(Violation::BadIndices));
    assert_eq!(r.pop(), Err(Violation::BadIndices));
    // and a producer that finds its ring full reports overflow rather than overwriting
    mem.store(ctl::INDICES, 0);
    for i in 0..CAPACITY {
        r.push(Entry {
            slot: i as u32,
            len: 60,
        })
        .unwrap();
    }
    assert_eq!(
        r.push(Entry { slot: 0, len: 60 }),
        Err(Violation::RingOverflow)
    );
    // reset empties it
    r.reset();
    assert!(r.is_empty() && r.pop().unwrap().is_none());
}

#[test]
fn indices_wrap_around_the_u32_range() {
    let mem = Shm::new();
    let r = Ring::new(&mem, RingId::TxReady);
    mem.store(ctl::INDICES + 2 * 128, u32::MAX - 2);
    mem.store(ctl::INDICES + 2 * 128 + 64, u32::MAX - 2);
    for i in 0..10u32 {
        r.push(Entry { slot: i, len: 100 }).unwrap();
    }
    for i in 0..10u32 {
        assert_eq!(r.pop().unwrap(), Some(Entry { slot: i, len: 100 }));
    }
}

#[test]
fn the_layout_constants_are_the_documented_ones() {
    assert_eq!(ctl::MAGIC_VALUE.to_le_bytes(), *b"NETR");
    assert_eq!(
        RINGS_BASE + 4 * CAPACITY * ENTRY_BYTES,
        2 * 4096,
        "four rings fill page 1 exactly"
    );
    assert!(ctl::INDICES + 4 * 128 <= 1024, "indices fit before 1024");
    assert!(ctl::STATS + 8 * stat::COUNT <= ctl::STATS_END);
    assert_eq!(HDR_LEN + FRAME_MAX, 1526);
    assert!(HDR_LEN + FRAME_MAX <= SLOT_SIZE);
}
