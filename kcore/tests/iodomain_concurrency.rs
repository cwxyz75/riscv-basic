//! IOMMU3: the device-domain store's operations, run as the kernel will run them, split into their phases under the locks of the
//! lock order (VMO, JOBS, IOMMU, FRAME: INVARIANTS.md), on two kernel harts and a device, with shuttle exploring the schedules. Run with
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test iodomain_concurrency --target-dir target/shuttle
//! (`make os-models` does). Without the cfg this file is empty.
//!
//! The scenarios are those of the ticket: unmap against map of the same VMO, unmap against destroy, destroy against a fault interrupt,
//! and a VMO's last-handle close against a map. The device is a third thread that runs the command queue and, between commands,
//! touches every page of the window it might reach. The property (IO3, M8): **no frame is free while the device can reach it** (through
//! its tables or its translation cache): a frame is freed only after the fence of the invalidation that covered it completed. At the end
//! every frame, charge and VMO is back (M3).
#![cfg(shuttle)]

mod common;
mod iodomain_common;

use iodomain_common::*;
use iommucore::faultq::{FaultRecord, TransactionType};
use iommucore::iopt::Access;
use iommucore::sim::Sim;
use kcore::aspace::Vmos;
use kcore::iodomain::*;
use kcore::job::{IoResource, JobId, Jobs, Limits};
use kcore::Mm;
use shuttle::sync::atomic::{AtomicBool, Ordering};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

const DID: u32 = 3;
const PROBES: u64 = 8;

/// The state under the IOMMU lock: the store and (standing in for the hardware's registers) the simulator.
struct Iommu {
    sim: Sim<'static, IoMem>,
    store: IoDomains,
}

/// The state under the FRAME lock.
struct Frames {
    mm: Mm<common::FakeMem>,
    counts: FrameCounts,
}

struct Kernel {
    mem: &'static IoMem,
    vmos: Mutex<Vmos>,
    jobs: Mutex<Jobs>,
    iommu: Mutex<Iommu>,
    frames: Mutex<Frames>,
    stop: AtomicBool,
}

fn frames<'a>(mem: &'a IoMem, f: &'a mut Frames) -> DbFrames<'a> {
    DbFrames {
        db: &mut f.mm.db,
        mem,
        counts: &mut f.counts,
        fail: None,
    }
}

/// One memory for every schedule of a test (the simulator borrows it for `'static`), cleared before each. Per OS thread: the tests run
/// in parallel, each on its own.
fn memory() -> &'static IoMem {
    thread_local! {
        static MEM: &'static IoMem = Box::leak(Box::new(IoMem::new()));
    }
    let m = MEM.with(|m| *m);
    m.clear();
    m
}

struct Setup {
    k: Arc<Kernel>,
    d: DomainId,
    job: JobId,
    vmo: u64,
    /// A mapping of the VMO's first two pages, live at the start.
    iova: u64,
    base_free: usize,
}

/// A domain for `DID`, a four-page DMA VMO with one handle, and a live mapping of its first two pages.
fn setup() -> Setup {
    let mem = memory();
    let mut w = World::with_frames(mem, false, 512, false);
    let base_free = w.mm.db.free_count();
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, DID, job);
    let vmo = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 4, job)
        .unwrap();
    let iova = map_ok(&mut w, d, vmo, 0, 2, Access::ReadWrite);
    let World {
        sim,
        store,
        mm,
        vmos,
        jobs,
        counts,
        ..
    } = w;
    Setup {
        k: Arc::new(Kernel {
            mem,
            vmos: Mutex::new(vmos),
            jobs: Mutex::new(jobs),
            iommu: Mutex::new(Iommu { sim, store }),
            frames: Mutex::new(Frames { mm, counts }),
            stop: AtomicBool::new(false),
        }),
        d,
        job,
        vmo,
        iova,
        base_free,
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// the kernel's sequences, each phase under its own lock

/// Drop a hold on a VMO (a handle's or a device mapping's): VMO, JOBS, FRAME.
fn release_vmo(k: &Kernel, vmo: u64) {
    let mut v = k.vmos.lock().unwrap();
    let mut j = k.jobs.lock().unwrap();
    let mut f = k.frames.lock().unwrap();
    v.release(&mut f.mm, &mut j, vmo).unwrap();
    v.drain_all(&mut f.mm, &mut j);
}

/// 6.1 step 4, with no store lock held across: the hold (VMO, then the VMO store's own release), then the record (JOBS, FRAME).
fn settle(k: &Kernel, rel: Released) {
    release_vmo(k, rel.vmo());
    let mut j = k.jobs.lock().unwrap();
    let mut f = k.frames.lock().unwrap();
    let mut fr = frames(k.mem, &mut f);
    rel.settle(k.mem, &mut fr, &mut j);
}

/// Wait for a fence, polling with the IOMMU lock taken and dropped (the kernel's wait holds no lock).
fn wait(k: &Kernel, t: Ticket) {
    loop {
        let f = {
            let mut g = k.iommu.lock().unwrap();
            let Iommu { sim, store } = &mut *g;
            store.poll(sim, t)
        };
        match f {
            Fence::Done => return,
            Fence::Waiting => thread::yield_now(),
            Fence::Failed | Fence::Unqueued => {
                panic!("the device never stalls and the queue never fills here: {f:?}")
            }
        }
    }
}

/// The rest of an unmap after its phase A: wait, complete (IOMMU), settle (no store lock).
fn finish_unmap(k: &Kernel, p: Pending) {
    wait(k, p.ticket());
    thread::yield_now();
    let r = k.iommu.lock().unwrap().store.complete(&p).unwrap();
    if let Some(rel) = r {
        settle(k, rel);
    }
}

fn unmap(k: &Kernel, d: DomainId, iova: u64, pages: u64) -> bool {
    let p = {
        let mut g = k.iommu.lock().unwrap();
        let Iommu { sim, store } = &mut *g;
        store.unmap(k.mem, sim, d, iova, pages)
    };
    match p {
        Ok(p) => {
            finish_unmap(k, p);
            true
        }
        Err(e) => {
            assert_eq!(
                e,
                IoError::BadState,
                "only a destroyed domain refuses a live mapping's unmap"
            );
            false
        }
    }
}

/// `iodomain_map`: the hold and frame list (VMO), the charge (JOBS), the record (no lock), the store (IOMMU, FRAME), the wait. The I/O
/// address, or `None` if refused (the VMO gone, the domain being destroyed).
fn map(k: &Kernel, d: DomainId, vmo: u64, first: u64, pages: u64) -> Option<u64> {
    let job = k.iommu.lock().unwrap().store.job_of(d)?;
    thread::yield_now();
    let mut pfns = vec![];
    if k.vmos
        .lock()
        .unwrap()
        .retain_for_io(vmo, first, pages, &mut pfns)
        .is_err()
    {
        return None;
    }
    thread::yield_now();
    let charge = charge_map(&mut k.jobs.lock().unwrap(), job, pages).unwrap();
    let rec = Record::new(vmo, first, pages, Access::ReadWrite).unwrap();
    thread::yield_now();
    let r = {
        let mut g = k.iommu.lock().unwrap();
        let mut f = k.frames.lock().unwrap();
        let Iommu { sim, store } = &mut *g;
        let mut fr = frames(k.mem, &mut f);
        store.map(k.mem, sim, &mut fr, d, rec, &pfns)
    };
    match r {
        Ok(m) => {
            charge.settle(&mut k.jobs.lock().unwrap(), m.tables);
            wait(k, m.ticket);
            Some(m.iova)
        }
        Err(MapFailed::Refused(e, rec)) => {
            assert_eq!(e, IoError::BadState);
            charge.refund(&mut k.jobs.lock().unwrap());
            release_vmo(k, rec.vmo());
            None
        }
        Err(MapFailed::RolledBack { .. }) => panic!("no table allocation fails here"),
    }
}

/// `iodomain_destroy` (or the last handle): phase 1 (IOMMU), the wait, the completion (IOMMU), each record settled, then the domain's
/// frames and charge (JOBS, FRAME). False if it was already being destroyed.
fn destroy(k: &Kernel, d: DomainId) -> bool {
    let x = {
        let mut g = k.iommu.lock().unwrap();
        let Iommu { sim, store } = &mut *g;
        store.destroy(k.mem, sim, d)
    };
    let Some(x) = x else { return false };
    wait(k, x.ticket());
    thread::yield_now();
    let mut det = k
        .iommu
        .lock()
        .unwrap()
        .store
        .destroy_complete(k.mem, &x)
        .unwrap()
        .expect("completes once");
    while let Some(rel) = det.next_record() {
        settle(k, rel);
        thread::yield_now();
    }
    let mut j = k.jobs.lock().unwrap();
    let mut f = k.frames.lock().unwrap();
    let mut fr = frames(k.mem, &mut f);
    det.finish(k.mem, &mut fr, &mut j);
    true
}

/// The device: run the queue now and then, and between runs touch every page of the window it might reach. A frame it reaches is
/// never a free one.
fn device(k: Arc<Kernel>) {
    while !k.stop.load(Ordering::SeqCst) {
        {
            let mut g = k.iommu.lock().unwrap();
            if shuttle::rand::Rng::gen::<bool>(&mut shuttle::rand::thread_rng()) {
                g.sim.process_commands();
            }
            let f = k.frames.lock().unwrap();
            for i in 0..PROBES {
                let iova = IO_BASE + (IO_VECTOR_PAGES + i) * PAGE;
                if let Ok(pa) = g.sim.read(DID, iova) {
                    assert!(
                        f.mm.db.refs(pa >> 12).is_some(),
                        "IO3: the device reached frame {:#x} at {iova:#x}, which is free",
                        pa >> 12
                    );
                }
            }
        }
        thread::yield_now();
    }
}

/// Stop the device, finish everything still standing on one thread, and check the baseline (M3).
fn finish(s: Setup, dev: thread::JoinHandle<()>, vmo_open: bool) {
    s.k.stop.store(true, Ordering::SeqCst);
    dev.join().unwrap();
    let k = &s.k;
    {
        let mut g = k.iommu.lock().unwrap();
        g.sim.process_commands();
    }
    if k.iommu.lock().unwrap().store.job_of(s.d).is_some() {
        thread::spawn({
            let k = k.clone();
            let d = s.d;
            move || {
                destroy(&k, d);
            }
        });
        // (the destruction waits for the queue: run it)
        loop {
            let alive = k.iommu.lock().unwrap().store.job_of(s.d).is_some();
            k.iommu.lock().unwrap().sim.process_commands();
            if !alive {
                break;
            }
            thread::yield_now();
        }
    }
    if vmo_open {
        release_vmo(k, s.vmo);
    }
    // (a destroy spawned above may still be finishing its records)
    loop {
        k.iommu.lock().unwrap().sim.process_commands();
        let done = k.vmos.lock().unwrap().count() == 0
            && k.jobs
                .lock()
                .unwrap()
                .usage(s.job)
                .unwrap()
                .get(kcore::job::Resource::KernelBytes)
                == 0;
        if done {
            break;
        }
        thread::yield_now();
    }
    let f = k.frames.lock().unwrap();
    assert_eq!(f.mm.db.free_count(), s.base_free, "M3: every frame back");
    assert_eq!(
        k.jobs
            .lock()
            .unwrap()
            .io_usage(s.job, IoResource::MappedPages),
        Some(0)
    );
    f.mm.db.audit().unwrap();
}

fn spawn_device(s: &Setup) -> thread::JoinHandle<()> {
    let k = s.k.clone();
    thread::spawn(move || device(k))
}

// ---------------------------------------------------------------------------------------------------------------------------
// the scenarios

const SCHEDULES: usize = 1500;

#[test]
fn shuttle_unmap_against_a_map_of_the_same_vmo() {
    shuttle::check_random(
        || {
            let s = setup();
            let dev = spawn_device(&s);
            let (k, d, v, iova) = (s.k.clone(), s.d, s.vmo, s.iova);
            let t = thread::spawn(move || assert!(unmap(&k, d, iova, 2)));
            let k = s.k.clone();
            let mapped = map(&k, d, v, 1, 3).expect("the domain and VMO are alive");
            t.join().unwrap();
            assert!(unmap(&k, d, mapped, 3));
            finish(s, dev, true);
        },
        SCHEDULES,
    );
}

/// The supervisor destroys while a driver maps (OD164's rights split: two owners of one domain). The map is refused (the domain is
/// being destroyed) and gives its hold and charge back, or it lands first and the destruction returns its record. The domain's job
/// stays alive throughout: the kernel's domain object holds its own use of it (IOMMU-SPEC 4.5), as this test's creator does.
#[test]
fn shuttle_destroy_against_a_map() {
    shuttle::check_random(
        || {
            let s = setup();
            let dev = spawn_device(&s);
            let (k, d) = (s.k.clone(), s.d);
            let t = thread::spawn(move || assert!(destroy(&k, d)));
            let k = s.k.clone();
            let mapped = map(&k, s.d, s.vmo, 2, 2);
            t.join().unwrap();
            assert_eq!(k.iommu.lock().unwrap().store.job_of(s.d), None);
            if let Some(iova) = mapped {
                assert!(!unmap(&k, s.d, iova, 2), "the destruction took the mapping");
            }
            assert_eq!(
                k.vmos.lock().unwrap().refs(s.vmo),
                Some(1),
                "every device hold is gone: only the handle is left"
            );
            assert_eq!(
                k.jobs
                    .lock()
                    .unwrap()
                    .io_usage(s.job, IoResource::MappedPages),
                Some(0),
                "J2"
            );
            finish(s, dev, true);
        },
        SCHEDULES,
    );
}

#[test]
fn shuttle_unmap_against_destroy() {
    shuttle::check_random(
        || {
            let s = setup();
            let dev = spawn_device(&s);
            let (k, d, iova) = (s.k.clone(), s.d, s.iova);
            let t = thread::spawn(move || {
                unmap(&k, d, iova, 2);
            });
            let k = s.k.clone();
            assert!(destroy(&k, d));
            t.join().unwrap();
            assert_eq!(k.iommu.lock().unwrap().store.job_of(d), None);
            finish(s, dev, true);
        },
        SCHEDULES,
    );
}

#[test]
fn shuttle_destroy_against_a_fault_interrupt() {
    shuttle::check_random(
        || {
            let s = setup();
            let dev = spawn_device(&s);
            let (k, d) = (s.k.clone(), s.d);
            let t = thread::spawn(move || assert!(destroy(&k, d)));
            let k = s.k.clone();
            let mut stored = 0;
            for i in 0..4u64 {
                let rec = FaultRecord {
                    cause: 13,
                    ttype: TransactionType::UntranslatedRead,
                    device_id: DID,
                    pv: false,
                    pid: 0,
                    privileged: false,
                    iotval: IO_BASE + i * PAGE,
                    iotval2: 0,
                };
                let o = k.iommu.lock().unwrap().store.fault(rec, i, 1000);
                if let Some(x) = o.domain {
                    assert_eq!(x, d, "a fault finds only its device's domain");
                    stored += 1;
                }
                thread::yield_now();
                match k.iommu.lock().unwrap().store.fault_read(d) {
                    Ok(Some(r)) => assert_eq!(r.record.device_id, DID),
                    Ok(None) | Err(IoError::BadState) => {}
                    Err(e) => panic!("fault_read: {e:?}"),
                }
            }
            t.join().unwrap();
            assert!(stored <= 4);
            assert_eq!(k.iommu.lock().unwrap().store.stats().faults, 4);
            assert_eq!(
                k.iommu.lock().unwrap().store.fault_read(d).unwrap_err(),
                IoError::BadState
            );
            finish(s, dev, true);
        },
        SCHEDULES,
    );
}

#[test]
fn shuttle_a_vmos_last_handle_close_against_a_map() {
    shuttle::check_random(
        || {
            let s = setup();
            let dev = spawn_device(&s);
            // the setup's mapping goes first, so the handle is the last holder but for the racing map
            assert!(unmap(&s.k, s.d, s.iova, 2));
            let (k, v) = (s.k.clone(), s.vmo);
            let t = thread::spawn(move || release_vmo(&k, v));
            let k = s.k.clone();
            let mapped = map(&k, s.d, s.vmo, 0, 4);
            t.join().unwrap();
            if let Some(iova) = mapped {
                assert!(
                    k.vmos.lock().unwrap().exists(s.vmo),
                    "M8: the device mapping holds the VMO"
                );
                assert!(unmap(&k, s.d, iova, 4));
            }
            assert!(
                !k.vmos.lock().unwrap().exists(s.vmo),
                "freed with its last holder"
            );
            finish(s, dev, false);
        },
        SCHEDULES,
    );
}

/// The negative control: an unmap that drops the mapping's hold before its fence (the VMO's handle already closed, so the frames are
/// freed at once) while the device's translation cache still holds the page. The device's probe must catch it.
#[test]
#[should_panic(expected = "IO3: the device reached frame")]
fn shuttle_negative_control_a_hold_dropped_before_the_fence_is_caught() {
    shuttle::check_random(
        || {
            let s = setup();
            {
                let mut g = s.k.iommu.lock().unwrap();
                assert!(
                    g.sim.read(DID, s.iova).is_ok(),
                    "the device caches the page"
                );
            }
            release_vmo(&s.k, s.vmo);
            let dev = spawn_device(&s);
            let k = s.k.clone();
            let p = {
                let mut g = k.iommu.lock().unwrap();
                let Iommu { sim, store } = &mut *g;
                store.unmap(k.mem, sim, s.d, s.iova, 2).unwrap()
            };
            release_vmo(&k, s.vmo); // the bug: before the fence
            thread::yield_now();
            wait(&k, p.ticket());
            let rel = k.iommu.lock().unwrap().store.complete(&p).unwrap().unwrap();
            {
                let mut j = k.jobs.lock().unwrap();
                let mut f = k.frames.lock().unwrap();
                let mut fr = frames(k.mem, &mut f);
                rel.settle(k.mem, &mut fr, &mut j);
            }
            finish(s, dev, false);
        },
        SCHEDULES,
    );
}
