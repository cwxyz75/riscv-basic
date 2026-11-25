//! MM6: `decommit` and `set_size` against a byte-level reference. Random create (resizable), map (into two address spaces), write, touch
//! (a fault maps the page), decommit, grow, shrink and unmap. The model keeps, for each VMO, the byte every page was last written with
//! (`None`: never written or decommitted: it reads zero). After every operation: what the VMO reads equals the model, the job ledger's page
//! usage equals the committed pages, no translation of a decommitted page survives in either address space, and a page past the end of a
//! shrunk VMO is refused at the fault. At the end everything is returned: no frame is lost.
mod common;
use common::{FakeMem, Rng};
use kcore::aspace::{Access, AddressSpace, FaultError, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
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
        // a job with a page limit: faults spend the address spaces' prepaid pages (L16d), so the ledger is committed plus prepaid
        let mut lim = Limits::UNLIMITED;
        lim.pages = 400;
        let job = jobs.create(JobId::ROOT, lim).unwrap();
        let mut asp: Vec<AddressSpace> = (0..2)
            .map(|_| AddressSpace::new(PageTable::new(&mut mm).unwrap()))
            .collect();
        let tables = total - mm.db.free_count();
        let mut ids = vec![];
        // (vmo index, base, pages) of the offset mappings in space 0, each starting at VMO page 1
        let mut offset_maps: Vec<(usize, u64, u64)> = vec![];
        let mut models: Vec<Model> = vec![];
        for (slot, &base) in BASES.iter().enumerate() {
            let pages = 2 + rng.below(MAXP - 2);
            let id = vmos.create_in(&mut jobs, pages, job);
            vmos.set_resizable(id);
            assert!(vmos.is_resizable(id));
            for a in asp.iter_mut() {
                a.map_region(&mut vmos, base, pages, id, 0, Perms::RW)
                    .unwrap();
            }
            // and the same VMO again in the first space, from page 1, right after the first region (so a range that is not clipped to its own
            // region would reach into this one): an offset mapping and a double mapping
            if pages > 2 {
                asp[0]
                    .map_region(&mut vmos, base + pages * PAGE, pages - 1, id, 1, Perms::RW)
                    .unwrap();
                offset_maps.push((slot, base + pages * PAGE, pages - 1));
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
                    let mut p = rng.below(size.min(reglen[v]));
                    let mut ai = rng.below(2) as usize;
                    let mut va = base + p * PAGE;
                    // sometimes through the offset mapping (space 0, VMO page 1 and up)
                    if let Some(&(_, ob, on)) = offset_maps.iter().find(|m| m.0 == v) {
                        if rng.below(2) == 0 && size > 1 && on > 0 {
                            p = 1 + rng.below((size - 1).min(on));
                            ai = 0;
                            va = ob + (p - 1) * PAGE;
                        }
                    }
                    let r = asp[ai].handle_fault(&mut mm, &mut vmos, &mut jobs, va, Access::Read);
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
                        // (only inside the base region: past its end the address belongs to the offset mapping, checked below)
                        for a in asp.iter().filter(|_| p < reglen[v]) {
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
            // ML1: the two spaces' regions are exactly the VMOs' lists of mappings
            vmos.audit_maps(&asp.iter().collect::<Vec<_>>())
                .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let prepaid: u64 = asp.iter().map(|a| a.reserved_pages()).sum();
            assert_eq!(
                jobs.usage(job).unwrap().get(Resource::Pages),
                committed + prepaid,
                "{ctx}: the ledger is the committed pages plus the prepaid ones"
            );
            // T249: the index is charged exactly: one leaf of kernel bytes per leaf it holds
            assert_eq!(
                jobs.usage(job).unwrap().get(Resource::KernelBytes),
                vmos.index_bytes_of_job(job),
                "{ctx}: the ledger holds the index leaves"
            );
            // every translation is the VMO's own frame (no stale mapping), and each frame's count is the VMO's reference plus its mappings
            let mut mappers: std::collections::BTreeMap<u64, u64> = Default::default();
            for (ai, a) in asp.iter().enumerate() {
                for (i, &base) in BASES.iter().enumerate() {
                    for p in 0..reglen[i] {
                        if let Some(t) = a.table().translate(&mm, base + p * PAGE) {
                            assert_eq!(
                                Some(t.pfn),
                                vmos.frame(ids[i], p),
                                "{ctx}: space {ai} vmo {i} page {p} maps a frame the VMO does not hold"
                            );
                            *mappers.entry(t.pfn).or_default() += 1;
                        }
                    }
                }
                if ai == 0 {
                    for &(i, base, n) in &offset_maps {
                        for p in 0..n {
                            if let Some(t) = a.table().translate(&mm, base + p * PAGE) {
                                assert_eq!(
                                    Some(t.pfn),
                                    vmos.frame(ids[i], p + 1),
                                    "{ctx}: the offset mapping of vmo {i} page {}",
                                    p + 1
                                );
                                *mappers.entry(t.pfn).or_default() += 1;
                            }
                        }
                    }
                }
            }
            for (&pfn, &n) in &mappers {
                assert_eq!(
                    mm.db.refs(pfn),
                    Some(1 + n as u32),
                    "{ctx}: frame {pfn}: the VMO's reference plus {n} mappings"
                );
            }
        }
        // everything back
        for &(_, base, _) in &offset_maps {
            asp[0]
                .unmap_region(&mut mm, &mut vmos, &mut jobs, base)
                .unwrap();
        }
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
        assert_eq!(
            jobs.usage(job).unwrap().get(Resource::Pages),
            0,
            "seed {seed}: the prepaid pages went back too"
        );
        assert_eq!(
            jobs.usage(job).unwrap().get(Resource::KernelBytes),
            0,
            "seed {seed}: every leaf was credited"
        );
        mm.db.audit().unwrap();
    }
}
