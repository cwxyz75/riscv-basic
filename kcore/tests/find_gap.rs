//! `AddressSpace::find_gap` (the stack arena search of `thread_spawn`) against a naive page-by-page scan.
mod common;
use common::{FakeMem, Rng, SEEDS};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const LO: u64 = 0x30_0000_0000;
const PAGES: u64 = 64; // a small arena: the same logic, small enough to scan

#[test]
fn find_gap_agrees_with_a_scan() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 300),
            mem: FakeMem::default(),
        };
        let mut vmos = Vmos::new();
        let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
        let hi = LO + PAGES * 4096;
        let mut used = [false; PAGES as usize];
        let v = vmos.create(8);
        let perms = Perms {
            read: true,
            write: true,
            exec: false,
            user: true,
        };
        for _ in 0..200 {
            if rng.below(3) != 0 {
                // map where the model says there is room (using the function under test would be circular: pick a
                // random page and size, and let overlap fail)
                let first = rng.below(PAGES);
                let n = 1 + rng.below(8).min(PAGES - first - 1);
                let free = (first..first + n).all(|p| !used[p as usize]);
                let r = a.map_region(&mut vmos, LO + first * 4096, n, v, 0, perms);
                assert_eq!(r.is_ok(), free, "seed {seed}: mapping {first}+{n}");
                if r.is_ok() {
                    for p in first..first + n {
                        used[p as usize] = true;
                    }
                }
            } else {
                // unmap one region at random
                let start = rng.below(PAGES);
                if a.unmap_region(&mut mm, &mut vmos, LO + start * 4096)
                    .is_ok()
                {
                    // find its extent in the model: pages from `start` while used and not the start of another
                    // region is not recoverable from `used`, so rebuild the model from the address space instead
                    for p in 0..PAGES {
                        used[p as usize] = a.region_perms_at(LO + p * 4096).is_some();
                    }
                }
            }
            for _ in 0..4 {
                let need = 1 + rng.below(10);
                let from = rng.below(PAGES + 4);
                let got = a.find_gap(LO, hi, need * 4096, LO + from * 4096);
                let want = (from.min(PAGES)..)
                    .take_while(|p| p + need <= PAGES)
                    .find(|&p| (p..p + need).all(|q| !used[q as usize]))
                    .map(|p| LO + p * 4096);
                assert_eq!(got, want, "seed {seed}: need {need} from {from}");
            }
        }
    }
}
