//! The order of the stores that change a structure the IOMMU may read at any moment (IOMMU2; IOMMU-SPEC 3.1 and 8.5, A16):
//! a device context's `tc` is written last when the context becomes valid and first when it becomes invalid; an MSI entry is
//! written second doubleword first when it becomes valid, valid bit first when it becomes invalid, with one store when only the
//! notice moves, and is made invalid first when its file changes. A recording memory logs every store.

use std::cell::RefCell;

use iommucore::ddt::{Ddt, DeviceContext, Format, Levels};
use iommucore::iopt::{Iopt, PtMode};
use iommucore::msi::{MsiPte, MsiTable, MsiWindow};
use iommucore::sim::{FakeFrames, FakeMem};
use iommucore::PhysMem64;

/// A memory that records every store (address, value) in order.
struct Recording<'m> {
    mem: &'m FakeMem,
    log: RefCell<Vec<(u64, u64)>>,
}

impl<'m> Recording<'m> {
    fn new(mem: &'m FakeMem) -> Self {
        Recording {
            mem,
            log: RefCell::new(Vec::new()),
        }
    }
    /// The stores since the last call, as offsets from `base` (stores outside `base .. base + len` are dropped).
    fn take(&self, base: u64, len: u64) -> Vec<(u64, u64)> {
        self.log
            .take()
            .into_iter()
            .filter(|(a, _)| (base..base + len).contains(a))
            .map(|(a, v)| (a - base, v))
            .collect()
    }
}

impl PhysMem64 for Recording<'_> {
    fn read(&self, pa: u64) -> u64 {
        self.mem.read(pa)
    }
    fn write(&self, pa: u64, value: u64) {
        self.log.borrow_mut().push((pa, value));
        self.mem.write(pa, value);
    }
    fn fetch_or(&self, pa: u64, bits: u64) -> u64 {
        let old = self.mem.fetch_or(pa, bits);
        self.log.borrow_mut().push((pa, old | bits));
        old
    }
    fn fetch_and(&self, pa: u64, bits: u64) -> u64 {
        let old = self.mem.fetch_and(pa, bits);
        self.log.borrow_mut().push((pa, old & bits));
        old
    }
}

#[test]
fn a_device_contexts_tc_is_written_last_when_valid_and_first_when_not() {
    for format in [Format::Base, Format::Extended] {
        let mem = FakeMem::new();
        let rec = Recording::new(&mem);
        let mut frames = FakeFrames::new(&mem, 0x1_0000_0000, 64);
        let t = Iopt::new(&mem, 0x8000_0000, PtMode::Sv39x4).unwrap();
        let mut d = Ddt::new(&rec, 0x8400_0000, Levels::One, format).unwrap();
        let did = 8u32;
        let size = format.context_bytes();
        let at = 0x8400_0000 + u64::from(did) * size;
        rec.take(0, 0);

        let check_valid = |what: &str, log: Vec<(u64, u64)>| {
            assert!(!log.is_empty(), "{format:?} {what}");
            let (last, tc) = *log.last().unwrap();
            assert_eq!(
                last, 0,
                "{format:?} {what}: tc is not the last store: {log:x?}"
            );
            assert_eq!(tc & 1, 1, "{format:?} {what}");
            assert!(
                log[..log.len() - 1].iter().all(|(o, _)| *o != 0),
                "{format:?} {what}: {log:x?}"
            );
        };
        // Invalid to valid (deny), valid to valid (translate), valid to invalid.
        d.set(
            &rec,
            &mut frames,
            did,
            &DeviceContext::deny(t.context_root(3).unwrap()),
        )
        .unwrap();
        check_valid("deny", rec.take(at, size));
        d.set(
            &rec,
            &mut frames,
            did,
            &DeviceContext::translate(t.context_root(4).unwrap(), None),
        )
        .unwrap();
        check_valid("translate", rec.take(at, size));
        d.clear(&rec, &mut frames, did);
        let log = rec.take(at, size);
        assert_eq!(
            log.first(),
            Some(&(0, 0)),
            "{format:?} clear: tc is not the first store: {log:x?}"
        );
        // An invalid context written with `set` takes the same path.
        d.set(
            &rec,
            &mut frames,
            did,
            &DeviceContext::deny(t.context_root(3).unwrap()),
        )
        .unwrap();
        rec.take(0, 0);
        d.set(&rec, &mut frames, did, &DeviceContext::invalid())
            .unwrap();
        let log = rec.take(at, size);
        assert_eq!(
            log.first(),
            Some(&(0, 0)),
            "{format:?} set invalid: {log:x?}"
        );
        assert!(d.get(&mem, did).is_none_or(|w| w[0] & 1 == 0));
    }
}

#[test]
fn an_msi_entry_is_written_in_an_order_the_iommu_can_read_at_any_moment() {
    const TABLE: u64 = 0x8200_0000;
    const FILES: [u64; 2] = [0x8300_0000, 0x8300_0200];
    let mem = FakeMem::new();
    let rec = Recording::new(&mem);
    let table = MsiTable::new(&rec, TABLE, MsiWindow::new(1 << 40, 8).unwrap()).unwrap();
    let at = TABLE + 4 * 16;
    rec.take(0, 0);
    let a = MsiPte::mrif(FILES[0], 0x2800_0000, 77).unwrap();
    let moved = MsiPte::mrif(FILES[0], 0x2800_1000, 78).unwrap();
    let other = MsiPte::mrif(FILES[1], 0x2800_0000, 77).unwrap();

    // A new entry: the notice doubleword, then the one with the valid bit.
    table.set(&rec, 4, a).unwrap();
    assert_eq!(rec.take(at, 16), [(8, a.words()[1]), (0, a.words()[0])]);
    // A move of the notice only: one store.
    table.set(&rec, 4, moved).unwrap();
    assert_eq!(rec.take(at, 16), [(8, moved.words()[1])]);
    // A change of file: invalid first, then as a new entry.
    table.set(&rec, 4, other).unwrap();
    assert_eq!(
        rec.take(at, 16),
        [(0, 0), (8, other.words()[1]), (0, other.words()[0])]
    );
    // Made invalid: the valid bit goes first.
    table.clear(&rec, 4).unwrap();
    let log = rec.take(at, 16);
    assert_eq!(log.first(), Some(&(0, 0)), "{log:x?}");
    assert_eq!(table.get(&mem, 4), Ok(MsiPte::invalid()));
}
