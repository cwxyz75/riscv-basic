//! IOMMU7: interrupt vectors run as the kernel runs them, each phase under its locks of the lock order (OBJ, then IOMMU, then FRAME:
//! INVARIANTS.md), on two harts that take their own file's messages and do their own work in it, a kernel thread, and a device that
//! sends its vector's message and runs the command queue; shuttle explores the schedules. Run with
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test iodomain_vectors_concurrency --target-dir target/shuttle
//! (`make os-models` does). Without the cfg this file is empty.
//!
//! The scenarios are those of the ticket: a revoke against a message in flight, a move against an acknowledgement, and a domain's
//! destruction against the notice interrupt. The property (IO10, 8.1): **a message never lands on a free identity**. An identity is
//! free only after the fence that covered every entry naming it completed and its hart retired it; the device checks, at the moment
//! the IOMMU sends each notice, that its identity is not free. At the end every identity, file frame and charge is back (M3), and the
//! negative control (the old identity of a move retired before the fence) is caught.
#![cfg(shuttle)]

mod common;
mod iodomain_common;

use iodomain_common::*;
use kcore::handle::HandleTable;
use kcore::iodomain::*;
use kcore::iovec::{Harts, Moved, RState, Vectors};
use kcore::ipc::{self, KObj, Kobjs, Quota, VectorRef};
use kcore::job::{IoResource, JobId, Jobs};
use kcore::msiid::{MsiIds, Place, Route, Slot};
use kcore::object::ObjId;
use shuttle::sync::atomic::{AtomicBool, Ordering};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;
use std::collections::BTreeSet;

const DID: u32 = 4;
const HARTS: u16 = 2;
const NUM_IDS: u32 = 15;
const IMSIC: u64 = 0x2800_0000;
const K: u16 = 3;
const MESSAGES: usize = 3;

struct HState {
    here: u16,
    asked: BTreeSet<u16>,
}

impl Harts for HState {
    fn here(&self) -> Option<u16> {
        Some(self.here)
    }
    fn usable(&self, f: u16) -> bool {
        f < HARTS
    }
    fn leaving(&self, _f: u16) -> bool {
        false
    }
    fn open_here(&mut self, _id: u16) {}
    fn ask(&mut self, f: u16) {
        self.asked.insert(f);
    }
    fn file_page(&self, f: u16) -> u64 {
        IMSIC + u64::from(f) * 4096
    }
}

/// The state under the object lock (with `JOBS` inside it, as object quotas are).
struct Obj {
    objs: Kobjs,
    table: HandleTable,
    ids: MsiIds,
    vecs: Vectors,
    harts: HState,
    jobs: Jobs,
}

struct Iommu {
    sim: iommucore::sim::Sim<'static, IoMem>,
    store: IoDomains,
}

struct Frames {
    mm: kcore::Mm<common::FakeMem>,
    counts: FrameCounts,
}

struct Kernel {
    mem: &'static IoMem,
    obj: Mutex<Obj>,
    iommu: Mutex<Iommu>,
    frames: Mutex<Frames>,
    /// Each hart's file: the identities a message is pending on (the IMSIC sets the bit; a leaf, as the hardware is).
    files: Mutex<[BTreeSet<u16>; HARTS as usize]>,
    /// The identities that are free, as of the last object-lock section (a leaf): what the device checks a notice against.
    free: Mutex<BTreeSet<Place>>,
    stop: AtomicBool,
}

fn memory() -> &'static IoMem {
    thread_local! {
        static MEM: &'static IoMem = Box::leak(Box::new(IoMem::new()));
    }
    let m = MEM.with(|m| *m);
    m.clear();
    m
}

impl Kernel {
    /// Publish the free identities (at the end of every section that can change them, before its locks go).
    fn publish(&self, o: &Obj) {
        let mut f = self.free.lock().unwrap();
        f.clear();
        for hart in 0..HARTS {
            for id in 2..=NUM_IDS as u16 {
                let p = Place { hart, id };
                if o.ids.slot(p) == Some(Slot::Free) {
                    f.insert(p);
                }
            }
        }
    }

    fn with_obj<T>(&self, here: u16, f: impl FnOnce(&mut Obj) -> T) -> T {
        let mut o = self.obj.lock().unwrap();
        o.harts.here = here;
        let r = f(&mut o);
        self.publish(&o);
        r
    }

    fn with_obj_iommu<T>(
        &self,
        here: u16,
        f: impl FnOnce(&mut Obj, &mut Iommu, &mut Frames) -> T,
    ) -> T {
        let mut o = self.obj.lock().unwrap();
        let mut i = self.iommu.lock().unwrap();
        let mut fr = self.frames.lock().unwrap();
        o.harts.here = here;
        let r = f(&mut o, &mut i, &mut fr);
        self.publish(&o);
        r
    }

    /// Wait (no lock held) for the fence the store holds for vector `k` of `d`.
    fn wait_vector(&self, d: DomainId, k: u16) {
        loop {
            let done = {
                let mut g = self.iommu.lock().unwrap();
                let Iommu { sim, store } = &mut *g;
                match store.vector_ticket(d, k) {
                    None => true,
                    Some(t) => store.poll(sim, t) == Fence::Done,
                }
            };
            if done {
                return;
            }
            thread::yield_now();
        }
    }
}

/// Hart `f` takes the messages pending in its file, then does the work it was asked to (open, retire): one object-lock section, as
/// the kernel's `deliver` is. A retiring identity's pending message is taken first (8.1).
fn hart_step(k: &Kernel, f: u16) {
    k.with_obj(f, |o| {
        let pending: Vec<u16> = std::mem::take(&mut k.files.lock().unwrap()[usize::from(f)])
            .into_iter()
            .collect();
        for id in pending {
            take_one(o, k.mem, Place { hart: f, id });
        }
        if !o.harts.asked.remove(&f) {
            return;
        }
        for id in 2..=o.ids.num_ids() {
            let p = Place { hart: f, id };
            match o.ids.slot(p) {
                Some(Slot::Opening { .. }) => {
                    let _ = o.ids.opened(p);
                }
                Some(Slot::Retiring { .. }) => {
                    // (read and clear the pending bit, delivering it to the route, then close)
                    if k.files.lock().unwrap()[usize::from(f)].remove(&id) {
                        take_one(o, k.mem, p);
                    }
                    if let Some((Route::Vector(n), _)) = o.ids.retired(p) {
                        o.vecs.retired(&o.ids, n);
                    }
                }
                _ => {}
            }
        }
        while let Some(job) = o.vecs.take_spent() {
            credit_vector(&mut o.jobs, job);
        }
    });
}

fn take_one(o: &mut Obj, mem: &IoMem, p: Place) {
    match o.ids.slot(p) {
        Some(
            Slot::Live {
                route: Route::Vector(_),
                ..
            }
            | Slot::Moved {
                route: Route::Vector(_),
                ..
            }
            | Slot::Retiring {
                route: Route::Vector(_),
                moved: true,
            },
        ) => {
            let _ = o.vecs.notice(&mut o.objs, &o.ids, mem, p);
        }
        Some(Slot::Retiring { moved: false, .. }) => {}
        s => panic!("IO10: a message taken on {p:?}, which is {s:?}"),
    }
}

/// The device: runs the command queue and sends its vector's message (data 1) `MESSAGES` times; every notice the IOMMU sends is
/// checked against the free identities at that moment and lands in its hart's file.
fn device(k: Arc<Kernel>) {
    let mut seen = 0;
    let mut sent = 0;
    while !k.stop.load(Ordering::Acquire) {
        {
            let mut g = k.iommu.lock().unwrap();
            // (the message first: it may meet an entry the IOMMU still holds from before a rewrite whose invalidation is queued)
            if sent < MESSAGES {
                let _ = g.sim.write(DID, IoDomains::vector_address(K), 1);
                sent += 1;
            }
            while let Some(d) = g.sim.deliveries().get(seen).copied() {
                seen += 1;
                let p = Place {
                    hart: ((d.page - IMSIC) / 4096) as u16,
                    id: d.identity as u16,
                };
                assert!(
                    !k.free.lock().unwrap().contains(&p),
                    "IO10: the IOMMU sent a notice to {p:?}, a free identity"
                );
                k.files.lock().unwrap()[usize::from(p.hart)].insert(p.id);
            }
        }
        thread::yield_now();
        k.iommu.lock().unwrap().sim.process_commands();
        thread::yield_now();
    }
}

fn hart(k: Arc<Kernel>, f: u16) {
    while !k.stop.load(Ordering::Acquire) {
        hart_step(&k, f);
        thread::yield_now();
    }
}

struct Setup {
    k: Arc<Kernel>,
    d: DomainId,
    v: VectorRef,
    obj: ObjId,
    h: abi::Handle,
    base_free: usize,
    /// Free frames with the domain made and no vector yet.
    dom_free: usize,
    free_ids: u64,
}

/// A domain for `DID` and vector `K` granted (placed on hart 0, the caller), its object armed.
fn setup() -> Setup {
    let mem = memory();
    let mut w = World::with_frames(mem, false, 512, false);
    let base_free = w.mm.db.free_count();
    let d = ready(&mut w, DID, JobId::ROOT);
    let dom_free = w.mm.db.free_count();
    let ids = MsiIds::new(usize::from(HARTS), NUM_IDS).unwrap();
    let free_ids = ids.free_total();
    let World {
        sim,
        store,
        mm,
        jobs,
        counts,
        ..
    } = w;
    let k = Arc::new(Kernel {
        mem,
        obj: Mutex::new(Obj {
            objs: Kobjs::new(),
            table: HandleTable::new(1000),
            ids,
            vecs: Vectors::new(64).unwrap(),
            harts: HState {
                here: 0,
                asked: BTreeSet::new(),
            },
            jobs,
        }),
        iommu: Mutex::new(Iommu { sim, store }),
        frames: Mutex::new(Frames { mm, counts }),
        files: Mutex::new(Default::default()),
        free: Mutex::new(BTreeSet::new()),
        stop: AtomicBool::new(false),
    });
    let v = k.with_obj(0, |o| {
        charge_vector(&mut o.jobs, JobId::ROOT).unwrap();
        o.vecs
            .grant_begin(&mut o.ids, &mut o.harts, d, K, JobId::ROOT)
            .unwrap()
    });
    let t = k.with_obj_iommu(0, |o, i, f| {
        let Obj {
            ids, vecs, harts, ..
        } = o;
        let Iommu { sim, store } = i;
        let mut fr = DbFrames {
            db: &mut f.mm.db,
            mem,
            counts: &mut f.counts,
            fail: None,
        };
        vecs.grant_write(ids, harts, store, mem, sim, &mut fr, v)
    });
    assert!(t.is_ok(), "placed on the calling hart: open at once");
    k.iommu.lock().unwrap().sim.process_commands();
    k.wait_vector(d, K);
    let (obj, h) = k.with_obj_iommu(0, |o, i, _| {
        assert_eq!(
            o.vecs.fenced(&mut o.ids, &mut o.harts, &mut i.store, v),
            Ok(true)
        );
        let (h, obj) = ipc::vector_object_create_in(
            &mut o.objs,
            &mut o.table,
            v,
            &mut Quota {
                jobs: &mut o.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap();
        assert!(o.vecs.attach(&mut o.objs, mem, v, obj));
        (obj, h)
    });
    Setup {
        k,
        d,
        v,
        obj,
        h,
        base_free,
        dom_free,
        free_ids,
    }
}

fn spawn_machine(s: &Setup) -> Vec<thread::JoinHandle<()>> {
    let mut out = vec![];
    let k = s.k.clone();
    out.push(thread::spawn(move || device(k)));
    for f in 0..HARTS {
        let k = s.k.clone();
        out.push(thread::spawn(move || hart(k, f)));
    }
    out
}

/// Stop the machine, take what is left (the queue run, every hart's file and work), and check the counts.
fn finish(s: Setup, machine: Vec<thread::JoinHandle<()>>, live: u64, destroyed: bool) {
    let k = &s.k;
    k.stop.store(true, Ordering::Release);
    for t in machine {
        t.join().unwrap();
    }
    for _ in 0..4 {
        k.iommu.lock().unwrap().sim.process_commands();
        for f in 0..HARTS {
            k.obj.lock().unwrap().harts.asked.insert(f);
            hart_step(k, f);
        }
    }
    let o = k.obj.lock().unwrap();
    let c = o.vecs.counts();
    assert_eq!((c.dead, c.leaked), (0, 0), "{c:?}");
    assert_eq!(
        o.ids.free_total(),
        s.free_ids - live,
        "every identity but the live vector's is free"
    );
    assert_eq!(
        o.jobs.io_usage(JobId::ROOT, IoResource::Vectors),
        Some(live),
        "J1: the vector charge"
    );
    if live == 0 {
        let want = if destroyed { s.base_free } else { s.dom_free };
        assert_eq!(
            k.frames.lock().unwrap().mm.db.free_count(),
            want,
            "M3: the file frame is back (and the domain's, if destroyed)"
        );
    }
}

/// Revoke steps: the last handle (OBJ, then IOMMU: enable off, entry invalid, invalidation and fence queued), the wait with no lock,
/// then the file and the identity (OBJ, IOMMU, FRAME), and the retirement by its hart.
fn revoke(k: &Kernel, s: &Setup) {
    let here = 1;
    k.with_obj_iommu(here, |o, i, _| {
        o.table.close(&mut o.objs, s.h).unwrap();
        for dead in o.objs.drain_dead() {
            let KObj::Interrupt(x) = dead else { panic!() };
            let Iommu { sim, store } = i;
            assert!(o
                .vecs
                .revoke_begin(store, k.mem, sim, x.vector.unwrap())
                .is_some());
        }
    });
    thread::yield_now();
    k.wait_vector(s.d, K);
    let r = k.with_obj_iommu(here, |o, i, f| {
        let Obj {
            ids, vecs, harts, ..
        } = o;
        let mut fr = DbFrames {
            db: &mut f.mm.db,
            mem: k.mem,
            counts: &mut f.counts,
            fail: None,
        };
        vecs.revoke_end(ids, harts, &mut i.store, k.mem, &mut fr, s.v)
    });
    assert_eq!(r, Ok(()));
    while k.with_obj(here, |o| o.vecs.state(s.v).is_some()) {
        thread::yield_now();
    }
}

/// A move to hart `to` (`interrupt_deliver_here`): the new identity opened by its hart, the entry rewritten (OBJ, IOMMU), the fence
/// waited for with no lock, then `fenced` asks the old hart to retire the old identity. `early`: the negative control's bug, the old
/// identity drained (retired) before the fence.
fn move_to(k: &Kernel, s: &Setup, to: u16, early: bool) {
    let m = k.with_obj(to, |o| o.vecs.move_here(&mut o.ids, &mut o.harts, s.v));
    if m != Ok(Moved::Retarget) {
        return;
    }
    let queued = loop {
        let r = k.with_obj_iommu(to, |o, i, _| {
            let Obj {
                ids, vecs, harts, ..
            } = o;
            let Iommu { sim, store } = i;
            vecs.retarget_begin(ids, harts, store, k.mem, sim, s.v)
        });
        if r != Err(IoError::NotDone) {
            break r;
        }
        thread::yield_now();
    };
    assert!(matches!(queued, Ok(Some(_))), "{queued:?}");
    if early {
        k.with_obj(to, |o| {
            let old: Vec<Place> = o
                .ids
                .places_of(Route::Vector(s.v.n))
                .filter(|(_, sl)| matches!(sl, Slot::Moved { .. }))
                .map(|(p, _)| p)
                .collect();
            for p in old {
                o.ids.drained(p);
                o.harts.asked.insert(p.hart);
            }
        });
    }
    thread::yield_now();
    k.wait_vector(s.d, K);
    let f = k.with_obj_iommu(to, |o, i, _| {
        o.vecs.fenced(&mut o.ids, &mut o.harts, &mut i.store, s.v)
    });
    assert_eq!(f, Ok(true));
}

/// 10.6: the destruction's fence waited for with no lock, then the routes ended (OBJ) before the store hands the files back (IOMMU),
/// and the release (FRAME, JOBS).
fn destroy(k: &Kernel, s: &Setup) {
    let x = {
        let mut g = k.iommu.lock().unwrap();
        let Iommu { sim, store } = &mut *g;
        store.destroy(k.mem, sim, s.d).unwrap()
    };
    loop {
        let done = {
            let mut g = k.iommu.lock().unwrap();
            let Iommu { sim, store } = &mut *g;
            store.poll(sim, x.ticket()) == Fence::Done
        };
        if done {
            break;
        }
        thread::yield_now();
    }
    let det = k.with_obj_iommu(0, |o, i, _| {
        if i.store.destroy_done(&x) {
            o.vecs.domain_gone(&mut o.ids, &mut o.harts, x.domain());
        }
        i.store.destroy_complete(k.mem, &x)
    });
    let mut det = det.unwrap().unwrap();
    assert!(det.next_record().is_none());
    let mut o = k.obj.lock().unwrap();
    let mut f = k.frames.lock().unwrap();
    let Frames { mm, counts } = &mut *f;
    let mut fr = DbFrames {
        db: &mut mm.db,
        mem: k.mem,
        counts,
        fail: None,
    };
    while !det.step(k.mem, &mut fr) {}
    let strays = det.finish(k.mem, &mut fr, &mut o.jobs);
    drop(f);
    k.publish(&o);
    drop(o);
    k.iommu.lock().unwrap().store.count_strays(strays);
    k.iommu.lock().unwrap().store.destroy_released();
}

const SCHEDULES: usize = 1000;

#[test]
fn shuttle_revoke_against_a_message_in_flight() {
    shuttle::check_random(
        || {
            let s = setup();
            let m = spawn_machine(&s);
            revoke(&s.k, &s);
            assert_eq!(s.k.obj.lock().unwrap().vecs.state(s.v), None);
            finish(s, m, 0, false);
        },
        SCHEDULES,
    );
}

#[test]
fn shuttle_move_against_an_acknowledgement() {
    shuttle::check_random(
        || {
            let s = setup();
            let m = spawn_machine(&s);
            let (k, obj, v) = (s.k.clone(), s.obj, s.v);
            let t = thread::spawn(move || {
                for _ in 0..3 {
                    let _ = k.with_obj(0, |o| o.vecs.ack(&mut o.objs, k.mem, obj, v));
                    thread::yield_now();
                }
            });
            move_to(&s.k, &s, 1, false);
            t.join().unwrap();
            {
                let o = s.k.obj.lock().unwrap();
                assert_eq!(o.vecs.state(s.v), Some(RState::Live));
                assert_eq!(o.vecs.current(&o.ids, s.v).map(|(p, _)| p.hart), Some(1));
            }
            finish(s, m, 1, false);
        },
        SCHEDULES,
    );
}

#[test]
fn shuttle_destroy_against_the_notice_interrupt() {
    shuttle::check_random(
        || {
            let s = setup();
            let m = spawn_machine(&s);
            destroy(&s.k, &s);
            {
                let o = s.k.obj.lock().unwrap();
                assert!(
                    matches!(o.objs.get(s.obj), Some(KObj::Interrupt(_))),
                    "the handle stays, dead"
                );
            }
            finish(s, m, 0, true);
        },
        SCHEDULES,
    );
}

/// The negative control: a move that retires the old identity before its entry's fence (the IOMMU may still hold the old entry, and
/// the device keeps sending). The device's check must catch the notice on the freed identity.
#[test]
#[should_panic(expected = "IO10")]
fn shuttle_negative_control_retiring_before_the_fence_is_caught() {
    shuttle::check_random(
        || {
            let s = setup();
            let m = spawn_machine(&s);
            move_to(&s.k, &s, 1, true);
            finish(s, m, 1, false);
        },
        SCHEDULES,
    );
}
