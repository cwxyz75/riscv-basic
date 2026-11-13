//! Device-memory and DMA VMOs (S6a): hand-derived scenarios plus a random test against an
//! independent frame-accounting formula. Invariants D1 (a physical VMO never names RAM, so the
//! frame database never sees its frames), D2 (only a DMA VMO has a physical address) and D4
//! (frames freed exactly once).
mod common;
use abi::limits::{MAX_DMA_PAGES, MAX_DMA_TOTAL_PAGES};
use common::{FakeMem, Rng, SEEDS};
use kcore::aspace::{AddressSpace, VmoError, VmoKind, Vmos};
use kcore::frame::FrameDb;
use kcore::job::JobId;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;

const BASE: u64 = 0xffff_ffd0_0000_0000;
const MMIO_PFN: u64 = 0x10001; // outside the database's range below

fn mm(frames: usize) -> Mm<FakeMem> {
    Mm {
        db: FrameDb::new(2000, frames),
        mem: FakeMem::default(),
    }
}

fn rw() -> Perms {
    Perms {
        read: true,
        write: true,
        exec: false,
        user: true,
    }
}

#[test]
fn a_physical_vmo_maps_device_frames_without_touching_the_database() {
    let mut mm = mm(100);
    let mut vmos = Vmos::new();
    let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let free_before = mm.db.free_count();
    let v = vmos.create_physical(MMIO_PFN, 2);
    assert_eq!(vmos.kind(v), Some(VmoKind::Device));
    assert_eq!(vmos.phys_addr(v), None, "D2: only DMA VMOs have an address");
    a.map_region(&vmos, BASE, 2, v, 0, rw()).unwrap();
    a.handle_fault(&mut mm, &mut vmos, BASE, kcore::aspace::Access::Write)
        .unwrap();
    a.handle_fault(&mut mm, &mut vmos, BASE + 4096, kcore::aspace::Access::Read)
        .unwrap();
    let t = a.table().translate(&mm, BASE).unwrap();
    assert_eq!(t.pfn, MMIO_PFN);
    assert_eq!(
        a.table().translate(&mm, BASE + 4096).unwrap().pfn,
        MMIO_PFN + 1
    );
    // mapping took table frames only; nothing about the device frames is in the database
    assert!(mm.db.refs(MMIO_PFN).is_none());
    // closing the VMO while mapped, then unmapping, frees nothing it does not own
    // the kernel never reads or writes it (D1)
    let mut b = [0u8; 4];
    assert_eq!(vmos.read(&mut mm, v, 0, &mut b), Err(VmoError::Device));
    assert_eq!(vmos.write(&mut mm, v, 0, &b), Err(VmoError::Device));
    vmos.destroy(&mut mm, v).unwrap();
    a.unmap_region(&mut mm, BASE).unwrap();
    a.destroy(&mut mm);
    assert_eq!(
        mm.db.free_count(),
        free_before + 1,
        "only the root table comes back"
    );
    mm.db.audit().unwrap();
}

#[test]
fn a_dma_vmo_is_contiguous_zeroed_charged_and_freed_exactly() {
    let mut mm = mm(200);
    let mut vmos = Vmos::new();
    let free = mm.db.free_count();
    // fragment the free list so contiguity is not an accident
    let hold: Vec<u64> = (0..7).map(|_| mm.db.alloc().unwrap()).collect();
    mm.db.dec(hold[3]).unwrap();
    let v = vmos.create_dma(&mut mm, 8, JobId::ROOT).unwrap();
    assert_eq!(vmos.kind(v), Some(VmoKind::Dma));
    let pa = vmos.phys_addr(v).unwrap();
    for i in 0..8 {
        let pfn = vmos.frame(v, i).unwrap();
        assert_eq!(pfn, (pa >> 12) + i, "contiguous");
        assert_eq!(mm.db.refs(pfn), Some(1));
    }
    assert!(
        !(0..8).any(|i| (pa >> 12) + i == hold[3]),
        "the hole of one frame is too small"
    );
    assert_eq!(mm.db.free_count(), free - 7 + 1 - 8);
    vmos.destroy(&mut mm, v).unwrap();
    assert_eq!(
        mm.db.free_count(),
        free - 7 + 1,
        "all eight frames came back"
    );
    for h in hold
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 3)
        .map(|(_, h)| *h)
    {
        mm.db.dec(h).unwrap();
    }
    assert_eq!(mm.db.free_count(), free);
    mm.db.audit().unwrap();
}

#[test]
fn dma_limits_and_failure_leave_nothing_behind() {
    let mut mm = mm(2000);
    let mut vmos = Vmos::new();
    assert_eq!(
        vmos.create_dma(&mut mm, 0, JobId::ROOT),
        Err(VmoError::OutOfRange)
    );
    assert_eq!(
        vmos.create_dma(&mut mm, MAX_DMA_PAGES + 1, JobId::ROOT),
        Err(VmoError::OutOfRange)
    );
    let mut ids = vec![];
    for _ in 0..MAX_DMA_TOTAL_PAGES / MAX_DMA_PAGES {
        ids.push(
            vmos.create_dma(&mut mm, MAX_DMA_PAGES, JobId::ROOT)
                .unwrap(),
        );
    }
    assert_eq!(
        vmos.create_dma(&mut mm, 1, JobId::ROOT),
        Err(VmoError::OutOfRange),
        "the system-wide DMA budget"
    );
    vmos.destroy(&mut mm, ids.pop().unwrap()).unwrap();
    assert!(
        vmos.create_dma(&mut mm, 1, JobId::ROOT).is_ok(),
        "budget comes back"
    );
    // no contiguous run left: NoMemory, and the job is not left charged
    let mut small = self::mm(10);
    let hold: Vec<u64> = (0..10).map(|_| small.db.alloc().unwrap()).collect();
    for (i, h) in hold.iter().enumerate() {
        if i % 2 == 0 {
            small.db.dec(*h).unwrap();
        }
    }
    let mut v2 = Vmos::new();
    assert_eq!(
        v2.create_dma(&mut small, 2, JobId::ROOT),
        Err(VmoError::NoMemory)
    );
    assert_eq!(small.db.free_count(), 5);
    small.db.audit().unwrap();
}

#[test]
fn the_jobs_page_charge_follows_dma_exactly() {
    use kcore::job::{Limits, Resource};
    let mut mm = mm(200);
    let mut vmos = Vmos::new();
    let job = vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 6,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let used = |v: &Vmos| v.jobs.usage(job).unwrap().get(Resource::Pages);
    assert_eq!(
        vmos.create_dma(&mut mm, 7, job),
        Err(VmoError::NoMemory),
        "over the job limit"
    );
    assert_eq!(used(&vmos), 0, "a refused creation charges nothing");
    assert_eq!(mm.db.free_count(), 200, "and takes no frames");
    let a = vmos.create_dma(&mut mm, 4, job).unwrap();
    assert_eq!(used(&vmos), 4);
    assert_eq!(vmos.create_dma(&mut mm, 3, job), Err(VmoError::NoMemory));
    assert_eq!(used(&vmos), 4);
    vmos.destroy(&mut mm, a).unwrap();
    assert_eq!(used(&vmos), 0, "destroy credits exactly");
    assert_eq!(mm.db.free_count(), 200);
}

#[test]
fn random_creation_mapping_and_destruction_balance_the_frames() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = mm(400);
        let base_free = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
        let after_root = mm.db.free_count();
        let mut live: Vec<(u64, VmoKind, u64)> = vec![]; // (id, kind, pages)
        let mut mapped: Vec<(u64, u64)> = vec![]; // (va, vmo)
        let mut next_slot = 0u64;
        for _ in 0..300 {
            match rng.below(5) {
                0 => {
                    let n = 1 + rng.below(4);
                    if let Ok(id) = vmos.create_dma(&mut mm, n, JobId::ROOT) {
                        live.push((id, VmoKind::Dma, n));
                    }
                }
                1 => {
                    let n = 1 + rng.below(3);
                    live.push((vmos.create_physical(MMIO_PFN, n), VmoKind::Device, n));
                }
                2 if !live.is_empty() => {
                    let (id, _, pages) = live[rng.below(live.len() as u64) as usize];
                    let va = BASE + next_slot * (1 << 21);
                    next_slot += 1;
                    if a.map_region(&vmos, va, pages, id, 0, rw()).is_ok() {
                        for p in 0..pages {
                            let _ = a.handle_fault(
                                &mut mm,
                                &mut vmos,
                                va + p * 4096,
                                kcore::aspace::Access::Write,
                            );
                        }
                        mapped.push((va, id));
                    }
                }
                3 if !mapped.is_empty() => {
                    let (va, _) = mapped.swap_remove(rng.below(mapped.len() as u64) as usize);
                    a.unmap_region(&mut mm, va).unwrap();
                }
                4 if !live.is_empty() => {
                    let (id, _, _) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    vmos.destroy(&mut mm, id).unwrap();
                }
                _ => {}
            }
            mm.db.audit().unwrap();
        }
        for (id, _, _) in live.drain(..) {
            vmos.destroy(&mut mm, id).unwrap();
        }
        for (va, _) in mapped.drain(..) {
            a.unmap_region(&mut mm, va).unwrap();
        }
        // everything but the address space's own tables is back
        assert!(mm.db.free_count() >= after_root, "seed {seed}: frames lost");
        a.destroy(&mut mm);
        assert_eq!(mm.db.free_count(), base_free, "seed {seed}: exact");
        assert_eq!(vmos.count(), 0);
    }
}
