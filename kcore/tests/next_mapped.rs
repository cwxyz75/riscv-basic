//! `PageTable::next_mapped` against a scan with `translate`: random pages in a few distant spans, random query windows.
mod common;
use common::{FakeMem, Rng};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

#[test]
fn next_mapped_finds_exactly_what_a_scan_finds() {
    for seed in 1..=20u64 {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 500),
            mem: FakeMem::default(),
        };
        let mut pt = PageTable::new(&mut mm).unwrap();
        let spans = [0x40_0000u64, 0x4000_0000, 0x40_0000_0000 - 0x20_0000];
        let mut mapped = vec![];
        for _ in 0..30 {
            let va = spans[rng.below(3) as usize] + rng.below(1024) * 4096;
            if pt.translate(&mm, va).is_none()
                && pt
                    .map(&mut mm, va, 1000 + mapped.len() as u64, Perms::RW)
                    .is_ok()
            {
                mapped.push(va);
            }
        }
        for _ in 0..200 {
            let lo = spans[rng.below(3) as usize] - 0x10_0000 + rng.below(2048) * 4096;
            let hi = lo + 1 + rng.below(0x80_0000);
            let expect = mapped.iter().copied().filter(|&v| v >= lo && v < hi).min();
            let aligned = lo & !4095;
            let expect = expect.filter(|&v| v >= aligned);
            assert_eq!(
                pt.next_mapped(&mm, aligned, hi),
                expect,
                "seed {seed} [{lo:#x}, {hi:#x})"
            );
        }
    }
}
