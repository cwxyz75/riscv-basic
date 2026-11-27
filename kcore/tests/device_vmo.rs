//! Device-memory and DMA VMOs (S6a): hand-derived scenarios plus a random test against an
//! independent frame-accounting formula. Invariants D1 (a physical VMO never names RAM, so the
//! frame database never sees its frames), D2 (only a DMA VMO has a physical address) and D4
//! (frames freed exactly once).
//!
//! IOMMU3 adds the device mapping as a third holder of a DMA VMO (M8, D4): `Vmos::retain_for_io` takes a hold like a region's, and
//! `create_dma_in` makes the new kind (not contiguous, no per-object limit, charged to a named job), whose large releases go back in
//! batches.
mod common;
mod iodomain_common;
use abi::limits::{MAX_DMA_PAGES, MAX_DMA_TOTAL_PAGES};
use common::{FakeMem, Rng, SEEDS};
use kcore::aspace::{AddressSpace, IoHoldError, VmoError, VmoKind, Vmos};
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
    let mut jobs = kcore::job::Jobs::new();
    let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
    let free_before = mm.db.free_count();
    let v = vmos.create_physical(&mut jobs, MMIO_PFN, 2);
    assert_eq!(vmos.kind(v), Some(VmoKind::Device));
    assert_eq!(vmos.phys_addr(v), None, "D2: only DMA VMOs have an address");
    a.map_region(&mut vmos, BASE, 2, v, 0, rw()).unwrap();
    a.handle_fault(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE,
        kcore::aspace::Access::Write,
    )
    .unwrap();
    a.handle_fault(
        &mut mm,
        &mut vmos,
        &mut jobs,
        BASE + 4096,
        kcore::aspace::Access::Read,
    )
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
    assert_eq!(
        vmos.write(&mut mm, &mut jobs, v, 0, &b),
        Err(VmoError::Device)
    );
    vmos.release(&mut mm, &mut jobs, v).unwrap();
    a.unmap_region(&mut mm, &mut vmos, &mut jobs, BASE).unwrap();
    a.destroy(&mut mm, &mut vmos, &mut jobs);
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
    let mut jobs = kcore::job::Jobs::new();
    let free = mm.db.free_count();
    // fragment the free list so contiguity is not an accident
    let hold: Vec<u64> = (0..7).map(|_| mm.db.alloc().unwrap()).collect();
    mm.db.dec(hold[3]).unwrap();
    let v = vmos.create_dma(&mut mm, &mut jobs, 8, JobId::ROOT).unwrap();
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
    vmos.release(&mut mm, &mut jobs, v).unwrap();
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
    let mut jobs = kcore::job::Jobs::new();
    assert_eq!(
        vmos.create_dma(&mut mm, &mut jobs, 0, JobId::ROOT),
        Err(VmoError::OutOfRange)
    );
    assert_eq!(
        vmos.create_dma(&mut mm, &mut jobs, MAX_DMA_PAGES + 1, JobId::ROOT),
        Err(VmoError::OutOfRange)
    );
    let mut ids = vec![];
    for _ in 0..MAX_DMA_TOTAL_PAGES / MAX_DMA_PAGES {
        ids.push(
            vmos.create_dma(&mut mm, &mut jobs, MAX_DMA_PAGES, JobId::ROOT)
                .unwrap(),
        );
    }
    assert_eq!(
        vmos.create_dma(&mut mm, &mut jobs, 1, JobId::ROOT),
        Err(VmoError::OutOfRange),
        "the system-wide DMA budget"
    );
    vmos.release(&mut mm, &mut jobs, ids.pop().unwrap())
        .unwrap();
    assert!(
        vmos.create_dma(&mut mm, &mut jobs, 1, JobId::ROOT).is_ok(),
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
        v2.create_dma(&mut small, &mut jobs, 2, JobId::ROOT),
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
    let mut jobs = kcore::job::Jobs::new();
    let job = jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 6,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    let used = |j: &kcore::job::Jobs| j.usage(job).unwrap().get(Resource::Pages);
    assert_eq!(
        vmos.create_dma(&mut mm, &mut jobs, 7, job),
        Err(VmoError::NoMemory),
        "over the job limit"
    );
    assert_eq!(used(&jobs), 0, "a refused creation charges nothing");
    assert_eq!(mm.db.free_count(), 200, "and takes no frames");
    let a = vmos.create_dma(&mut mm, &mut jobs, 4, job).unwrap();
    assert_eq!(used(&jobs), 4);
    assert_eq!(
        vmos.create_dma(&mut mm, &mut jobs, 3, job),
        Err(VmoError::NoMemory)
    );
    assert_eq!(used(&jobs), 4);
    vmos.release(&mut mm, &mut jobs, a).unwrap();
    assert_eq!(used(&jobs), 0, "destroy credits exactly");
    assert_eq!(mm.db.free_count(), 200);
}

#[test]
fn random_creation_mapping_and_destruction_balance_the_frames() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut mm = mm(1200);
        let base_free = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut jobs = kcore::job::Jobs::new();
        let mut a = AddressSpace::new(PageTable::new(&mut mm).unwrap());
        let after_root = mm.db.free_count();
        let mut live: Vec<(u64, VmoKind, u64)> = vec![]; // (id, kind, pages)
        let mut mapped: Vec<(u64, u64)> = vec![]; // (va, vmo)
        let mut device: Vec<(u64, Vec<u64>)> = vec![]; // (vmo, the frames a device mapping holds)
        let mut next_slot = 0u64;
        for _ in 0..300 {
            match rng.below(8) {
                0 => {
                    let n = 1 + rng.below(4);
                    if let Ok(id) = vmos.create_dma(&mut mm, &mut jobs, n, JobId::ROOT) {
                        live.push((id, VmoKind::Dma, n));
                    }
                }
                5 => {
                    // the new kind, now and then larger than one release batch
                    let n = if rng.chance(25) {
                        65 + rng.below(70)
                    } else {
                        1 + rng.below(6)
                    };
                    if let Ok(id) = vmos.create_dma_in(&mut mm, &mut jobs, n, JobId::ROOT) {
                        live.push((id, VmoKind::Dma, n));
                    }
                }
                6 if !live.is_empty() => {
                    // a device mapping of a range: a hold, and the frame list it will map
                    let (id, kind, pages) = live[rng.below(live.len() as u64) as usize];
                    let first = rng.below(pages);
                    let n = 1 + rng.below(pages - first);
                    let mut pfns = vec![];
                    let r = vmos.retain_for_io(id, first, n, &mut pfns);
                    if kind == VmoKind::Dma {
                        r.unwrap();
                        assert_eq!(pfns.len() as u64, n);
                        for (i, pfn) in pfns.iter().enumerate() {
                            assert_eq!(vmos.frame(id, first + i as u64), Some(*pfn));
                        }
                        device.push((id, pfns));
                    } else {
                        assert_eq!(
                            r,
                            Err(IoHoldError::NotDma),
                            "only a DMA VMO is device-mapped"
                        );
                        assert!(pfns.is_empty());
                    }
                }
                7 if !device.is_empty() => {
                    // the unmap's fence completed: the hold goes
                    let (id, _) = device.swap_remove(rng.below(device.len() as u64) as usize);
                    vmos.release(&mut mm, &mut jobs, id).unwrap();
                }
                1 => {
                    let n = 1 + rng.below(3);
                    live.push((
                        vmos.create_physical(&mut jobs, MMIO_PFN, n),
                        VmoKind::Device,
                        n,
                    ));
                }
                2 if !live.is_empty() => {
                    let (id, _, pages) = live[rng.below(live.len() as u64) as usize];
                    let va = BASE + next_slot * (1 << 21);
                    next_slot += 1;
                    if a.map_region(&mut vmos, va, pages, id, 0, rw()).is_ok() {
                        for p in 0..pages {
                            let _ = a.handle_fault(
                                &mut mm,
                                &mut vmos,
                                &mut jobs,
                                va + p * 4096,
                                kcore::aspace::Access::Write,
                            );
                        }
                        mapped.push((va, id));
                    }
                }
                3 if !mapped.is_empty() => {
                    let (va, _) = mapped.swap_remove(rng.below(mapped.len() as u64) as usize);
                    a.unmap_region(&mut mm, &mut vmos, &mut jobs, va).unwrap();
                }
                4 if !live.is_empty() => {
                    let (id, _, _) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    vmos.release(&mut mm, &mut jobs, id).unwrap();
                }
                _ => {}
            }
            if rng.chance(30) {
                vmos.drain_all(&mut mm, &mut jobs);
            }
            mm.db.audit().unwrap();
            // D4 and M8: a frame a device mapping holds stays allocated, and its VMO alive, whatever closed
            for (id, pfns) in &device {
                assert!(
                    vmos.exists(*id),
                    "seed {seed}: a device-held VMO was destroyed"
                );
                for pfn in pfns {
                    assert!(
                        mm.db.refs(*pfn).is_some(),
                        "seed {seed}: a device-held frame is free"
                    );
                }
            }
        }
        for (id, _, _) in live.drain(..) {
            vmos.release(&mut mm, &mut jobs, id).unwrap();
        }
        for (id, _) in device.drain(..) {
            vmos.release(&mut mm, &mut jobs, id).unwrap();
        }
        for (va, _) in mapped.drain(..) {
            a.unmap_region(&mut mm, &mut vmos, &mut jobs, va).unwrap();
        }
        vmos.drain_all(&mut mm, &mut jobs);
        // everything but the address space's own tables is back
        assert!(mm.db.free_count() >= after_root, "seed {seed}: frames lost");
        a.destroy(&mut mm, &mut vmos, &mut jobs);
        assert_eq!(mm.db.free_count(), base_free, "seed {seed}: exact");
        assert_eq!(vmos.count(), 0);
    }
}

#[test]
fn a_dma_in_vmo_is_zeroed_charged_fixed_and_not_promised_contiguous() {
    use kcore::job::{Limits, Resource};
    use kcore::pagetable::PhysMem;
    let mut mm = mm(300);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let free = mm.db.free_count();
    // dirty frames, every other one held back: what the new kind gets is scattered and was written before
    let hold: Vec<u64> = (0..40).map(|_| mm.db.alloc().unwrap()).collect();
    for (i, h) in hold.iter().enumerate() {
        mm.mem.write_u64(h << 12, 0x1234);
        if i % 2 == 0 {
            mm.db.dec(*h).unwrap();
        }
    }
    let v = vmos.create_dma_in(&mut mm, &mut jobs, 20, job).unwrap();
    assert_eq!(vmos.kind(v), Some(VmoKind::Dma));
    let pfns: Vec<u64> = (0..20).map(|i| vmos.frame(v, i).unwrap()).collect();
    for p in &pfns {
        assert_eq!(mm.db.refs(*p), Some(1));
        for w in 0..512 {
            assert_eq!(mm.mem.read_u64((p << 12) + w * 8), 0, "zeroed");
        }
    }
    assert!(
        pfns.windows(2).any(|w| w[1] != w[0] + 1),
        "not contiguous (the scattered frames were used)"
    );
    assert_eq!(
        vmos.phys_addr(v),
        None,
        "D2: no first address for frames that are not contiguous"
    );
    // the kind decides, not the frames that happened to come: one page is "contiguous" and still has no address
    let one = vmos.create_dma_in(&mut mm, &mut jobs, 1, job).unwrap();
    assert_eq!(
        vmos.phys_addr(one),
        None,
        "D2, IO6: no physical address for the new kind"
    );
    vmos.release(&mut mm, &mut jobs, one).unwrap();
    let used = |j: &kcore::job::Jobs, r| j.usage(job).unwrap().get(r);
    assert_eq!(used(&jobs, Resource::Pages), 20, "charged to the named job");
    assert_eq!(
        used(&jobs, Resource::KernelBytes),
        kcore::pagemap::LEAF_BYTES
    );
    // fixed: no decommit, no resize
    assert_eq!(
        vmos.decommit(&mut mm, &mut jobs, v, 0, 1),
        Err(VmoError::Fixed)
    );
    assert!(vmos.set_size(&mut mm, &mut jobs, v, 30).is_err());
    assert_eq!(vmos.frame(v, 0), Some(pfns[0]));
    vmos.release(&mut mm, &mut jobs, v).unwrap();
    assert_eq!(
        (
            used(&jobs, Resource::Pages),
            used(&jobs, Resource::KernelBytes)
        ),
        (0, 0)
    );
    for (i, h) in hold.iter().enumerate() {
        if i % 2 == 1 {
            mm.db.dec(*h).unwrap();
        }
    }
    assert_eq!(mm.db.free_count(), free);
    // the limits: zero pages, the backstop, the job, the frames; each refusal leaves nothing
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 0, job),
        Err(VmoError::OutOfRange)
    );
    vmos.set_dma_total_cap(100);
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 101, job),
        Err(VmoError::OutOfRange)
    );
    let a = vmos.create_dma_in(&mut mm, &mut jobs, 90, job).unwrap();
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 11, job),
        Err(VmoError::NoMemory),
        "the backstop's room"
    );
    let small = jobs
        .create(
            JobId::ROOT,
            Limits {
                pages: 5,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 6, small),
        Err(VmoError::NoMemory),
        "the job's limit"
    );
    assert_eq!(jobs.usage(small).unwrap().get(Resource::Pages), 0);
    vmos.release(&mut mm, &mut jobs, a).unwrap();
    vmos.drain_all(&mut mm, &mut jobs);
    vmos.set_dma_total_cap(1000);
    let before = mm.db.free_count();
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, before as u64 + 1, job),
        Err(VmoError::NoMemory),
        "the frames"
    );
    assert_eq!(
        mm.db.free_count(),
        before,
        "a failure gives every frame back"
    );
    assert_eq!(
        (
            used(&jobs, Resource::Pages),
            used(&jobs, Resource::KernelBytes)
        ),
        (0, 0)
    );
    mm.db.audit().unwrap();
}

#[test]
fn a_large_dma_vmo_goes_back_in_batches_and_then_restores_the_backstop() {
    let mut mm = mm(600);
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let free = mm.db.free_count();
    vmos.set_dma_total_cap(300);
    let v = vmos
        .create_dma_in(&mut mm, &mut jobs, 250, JobId::ROOT)
        .unwrap();
    // a device mapping holds it past its handle
    let mut pfns = vec![];
    vmos.retain_for_io(v, 0, 250, &mut pfns).unwrap();
    vmos.release(&mut mm, &mut jobs, v).unwrap();
    assert!(vmos.exists(v), "M8: the device mapping keeps it");
    assert_eq!(mm.db.free_count(), free - 250);
    assert_eq!(
        vmos.create_dma_in(&mut mm, &mut jobs, 51, JobId::ROOT),
        Err(VmoError::NoMemory)
    );
    // the unmap completes: the last holder goes, and the frames go back in steps
    vmos.release(&mut mm, &mut jobs, v).unwrap();
    assert!(!vmos.exists(v));
    vmos.drain_all(&mut mm, &mut jobs);
    assert_eq!(mm.db.free_count(), free, "every frame back, once (D4)");
    let w = vmos
        .create_dma_in(&mut mm, &mut jobs, 300, JobId::ROOT)
        .expect("the backstop is whole again");
    vmos.release(&mut mm, &mut jobs, w).unwrap();
    vmos.drain_all(&mut mm, &mut jobs);
    assert_eq!(mm.db.free_count(), free);
    mm.db.audit().unwrap();
}

#[test]
fn a_dma_vmo_closed_while_device_mapped_is_freed_at_unmap_completion_not_before() {
    use iodomain_common::*;
    use iommucore::iopt::Access;
    use kcore::iodomain::Fence;
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let base = w.mm.db.free_count();
    let job = w
        .jobs
        .create(JobId::ROOT, kcore::job::Limits::UNLIMITED)
        .unwrap();
    let d = ready(&mut w, 2, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 3, job)
        .unwrap();
    let iova = map_ok(&mut w, d, v, 0, 3, Access::ReadWrite);
    let pfns: Vec<u64> = (0..3).map(|i| w.vmos.frame(v, i).unwrap()).collect();
    w.close_vmo(v);
    assert!(w.vmos.exists(v), "the device mapping holds the VMO");
    for p in &pfns {
        assert_eq!(w.mm.db.refs(*p), Some(1));
    }
    assert_eq!(
        w.sim.read(2, iova).unwrap() >> 12,
        pfns[0],
        "the device still reaches it"
    );
    let p = w.unmap(d, iova, 3).unwrap();
    assert!(
        w.vmos.exists(v),
        "an unmap whose fence has not completed frees nothing"
    );
    assert_eq!(
        w.store.complete(&p).unwrap_err(),
        kcore::iodomain::IoError::NotDone
    );
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Done);
    let rel = w.store.complete(&p).unwrap().unwrap();
    assert!(w.vmos.exists(v), "completed, the hold not yet dropped");
    w.settle(rel);
    assert!(!w.vmos.exists(v), "freed at completion");
    for p in &pfns {
        assert_eq!(w.mm.db.refs(*p), None, "D4: each frame freed, once");
    }
    destroy_ok(&mut w, d);
    assert_eq!(w.mm.db.free_count(), base);
}
