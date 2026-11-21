//! MM6: `decommit` and `set_size` against a byte-level reference. Random create (resizable), map (into two address spaces), write, touch
//! (a fault maps the page), decommit, grow, shrink and unmap. The model keeps, for each VMO, the byte every page was last written with
//! (`None`: never written or decommitted: it reads zero). After every operation: what the VMO reads equals the model, the job ledger's page
//! usage equals the committed pages, no translation of a decommitted page survives in either address space, and a page past the end of a
//! shrunk VMO is refused at the fault. At the end everything is returned: no frame is lost.
mod common;
use common::{FakeMem, Rng};
use kcore::aspace::{Access, AddressSpace, FaultError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const PAGE: u64 = 4096;
const MAXP: u64 = 12;
const BASES: [u64; 3] = [
    0xffff_ffd0_0000_0000,
    0xffff_ffd0_0020_0000,
    0xffff_ffd0_0040_0000,
];

struct Model {
    /// per VMO: the byte each page was last written with
    pages: Vec<Option<u8>>,
}

fn read_page(mm: &mut Mm<FakeMem>, vmos: &mut Vmos, id: u64, page: u64) -> u8 {
    let mut b = [0u8; 1];
    vmos.read(mm, id, page * PAGE, &mut b).unwrap();
    b[0]
}

#[test]
fn decommit_and_resize_agree_with_a_byte_level_model_and_the_ledger_is_exact() {
    for seed in 1..=40u64 {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(2000, 500),
            mem: FakeMem::default(),
        };
        let total = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut jobs = Jobs::new();
        let mut asp: Vec<AddressSpace> = (0..2)
            .map(|_| AddressSpace::new(PageTable::new(&mut mm).unwrap()))
            .collect();
        let tables = total - mm.db.free_count();
        let mut ids = vec![];
        let mut models: Vec<Model> = vec![];
        for (slot, &base) in BASES.iter().enumerate() {
            let pages = 2 + rng.below(MAXP - 2);
            let id = vmos.create(&mut jobs, pages);
            vmos.set_resizable(id);
            assert!(vmos.is_resizable(id));
            for a in asp.iter_mut() {
                a.map_region(&mut vmos, base, pages, id, 0, Perms::RW)
                    .unwrap();
            }
            ids.push(id);
            models.push(Model {
                pages: vec![None; pages as usize],
            });
            let _ = slot;
        }
        let mut sizes: Vec<u64> = models.iter().map(|m| m.pages.len() as u64).collect();
        // the length each region was made with: it does not follow the VMO's size
        let reglen: Vec<u64> = sizes.clone();
        for step in 0..600 {
            let ctx = format!("seed {seed} step {step}");
            let v = rng.below(3) as usize;
            let (id, base) = (ids[v], BASES[v]);
            let size = sizes[v];
            match rng.below(7) {
                0 | 1 => {
                    // write a byte to a page
                    let p = rng.below(size);
                    let val = 1 + rng.below(250) as u8;
                    vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[val])
                        .unwrap();
                    models[v].pages[p as usize] = Some(val);
                }
                2 => {
                    // touch through an address space: the fault commits and maps a zero page
                    let p = rng.below(size.min(reglen[v]));
                    let ai = rng.below(2) as usize;
                    let r = asp[ai].handle_fault(
                        &mut mm,
                        &mut vmos,
                        &mut jobs,
                        base + p * PAGE,
                        Access::Read,
                    );
                    assert!(r.is_ok(), "{ctx}: touch {r:?}");
                    if models[v].pages[p as usize].is_none() {
                        models[v].pages[p as usize] = Some(0); // committed, reads zero
                    }
                }
                3 | 4 => {
                    // decommit a range: unmap it everywhere first (the caller's duty), then give the pages back
                    let first = rng.below(size);
                    let n = 1 + rng.below(size - first);
                    for a in asp.iter_mut() {
                        a.unmap_vmo_pages(&mut mm, id, first, n);
                    }
                    vmos.decommit(&mut mm, &mut jobs, id, first, n).unwrap();
                    for p in first..first + n {
                        models[v].pages[p as usize] = None;
                        for a in asp.iter() {
                            assert!(
                                a.table().translate(&mm, base + p * PAGE).is_none(),
                                "{ctx}: a decommitted page is still mapped"
                            );
                        }
                    }
                }
                5 => {
                    // resize
                    let new = 1 + rng.below(MAXP);
                    if new < size {
                        for a in asp.iter_mut() {
                            a.unmap_vmo_pages(&mut mm, id, new, size - new);
                        }
                    }
                    vmos.set_size(&mut mm, &mut jobs, id, new).unwrap();
                    models[v].pages.resize(new as usize, None);
                    sizes[v] = new;
                    // a touch past the new end, inside the mapping (which kept its length), is refused and maps nothing
                    if new < reglen[v] {
                        let p = new + rng.below(reglen[v] - new);
                        let r = asp[0].handle_fault(
                            &mut mm,
                            &mut vmos,
                            &mut jobs,
                            base + p * PAGE,
                            Access::Read,
                        );
                        assert_eq!(
                            r,
                            Err(FaultError::NoRegion),
                            "{ctx}: a page past the end was served"
                        );
                        assert!(asp[0].table().translate(&mm, base + p * PAGE).is_none());
                    }
                }
                _ => {
                    // a non-resizable VMO refuses to resize (checked on a fresh one)
                    let plain = vmos.create(&mut jobs, 2);
                    assert_eq!(
                        vmos.set_size(&mut mm, &mut jobs, plain, 4),
                        Err(kcore::aspace::VmoError::NotResizable)
                    );
                    vmos.release(&mut mm, &mut jobs, plain).unwrap();
                }
            }
            // after every step: the bytes, the ledger
            let mut committed = 0u64;
            for (i, m) in models.iter().enumerate() {
                for (p, expect) in m.pages.iter().enumerate() {
                    let got = read_page(&mut mm, &mut vmos, ids[i], p as u64);
                    assert_eq!(got, expect.unwrap_or(0), "{ctx}: vmo {i} page {p}");
                    if expect.is_some() {
                        committed += 1;
                    }
                }
                assert_eq!(
                    vmos.committed_pages(ids[i]),
                    Some(m.pages.iter().filter(|x| x.is_some()).count() as u64),
                    "{ctx}"
                );
            }
            assert_eq!(
                jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages),
                committed,
                "{ctx}: the ledger is exactly the committed pages"
            );
        }
        // everything back
        for (v, &base) in BASES.iter().enumerate() {
            for a in asp.iter_mut() {
                a.unmap_region(&mut mm, &mut vmos, &mut jobs, base).unwrap();
            }
            vmos.release(&mut mm, &mut jobs, ids[v]).unwrap();
        }
        for a in asp {
            a.destroy(&mut mm, &mut vmos, &mut jobs);
        }
        assert_eq!(
            mm.db.free_count(),
            total,
            "seed {seed}: no frame lost (tables were {tables})"
        );
        assert_eq!(jobs.usage(JobId::ROOT).unwrap().get(Resource::Pages), 0);
        mm.db.audit().unwrap();
    }
}
