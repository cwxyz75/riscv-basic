//! IOMMU7: interrupt vectors (`kcore::iovec` over the store of `kcore::iodomain` and the identities of `kcore::msiid`) against a
//! reference written from IOMMU-SPEC 8.2 to 8.5, 6.2 and 10.6, with the simulated IOMMU (`iommucore::sim`) as the hardware. The device
//! walks the real MSI page tables with its own decoder and keeps its own MSI-entry cache, which only the invalidation commands drop, so
//! a stale entry is the hardware's and not the store's bookkeeping.
//!
//! The random test grants, signals (data 1, other data words, words no file holds, misaligned addresses, ungranted pages and devices
//! with no domain), acknowledges, moves (the first wait or acknowledgement, the explicit call), revokes (the last handle), destroys
//! domains and takes harts offline and back, each step as the kernel runs it, with a message sent through the entry **between** the
//! entry write and its fence (where the IOMMU may still hold the old entry). After **every** step:
//!
//! * a message reaches a hart file **iff** the reference says a granted vector whose file is enabled was signalled with data 1, and
//!   only on an identity routed to that vector (IO10, IO11): never on a Free identity, never another route's;
//! * **no identity is Free while any MSI entry names it**, in the tables the IOMMU walks or in its cache (IO10);
//! * every live vector holds exactly one identity, its object's INTERRUPT is the reference's, and the vector charge of the job is the
//!   number of vectors whose identities are not all free yet (J1);
//! * every stray bit (a data word other than 1) is counted, once, at the acknowledgement or the revoke that finds it (8.4).
//!
//! At the end, everything closed and destroyed: every identity, file frame, charge and object is back (M3).
mod common;
mod iodomain_common;

use common::Rng;
use iodomain_common::*;
use iommucore::PhysMem64;
use kcore::handle::HandleTable;
use kcore::iodomain::*;
use kcore::iovec::{Harts, MoveError, Moved, RState, Vectors};
use kcore::ipc::{self, KObj, Kobjs, Quota, VectorRef};
use kcore::job::{IoResource, JobId};
use kcore::msiid::{MsiIds, Place, Route, Slot};
use kcore::object::ObjId;
use std::collections::{BTreeMap, BTreeSet};

const HARTS: u16 = 4;
const NUM_IDS: u32 = 31;
const IMSIC: u64 = 0x2800_0000;
const DEVICES: u32 = 4;
const INDICES: u16 = 6;
const MSI_INVALID: u16 = 262;
const MSI_MISCONFIGURED: u16 = 263;

struct MHarts {
    here: u16,
    offline: [bool; HARTS as usize],
    asked: BTreeSet<u16>,
}

impl Harts for MHarts {
    fn here(&self) -> Option<u16> {
        Some(self.here)
    }
    fn usable(&self, f: u16) -> bool {
        f < HARTS && !self.offline[usize::from(f)]
    }
    fn leaving(&self, f: u16) -> bool {
        !self.usable(f)
    }
    fn open_here(&mut self, _id: u16) {}
    fn ask(&mut self, f: u16) {
        self.asked.insert(f);
    }
    fn file_page(&self, f: u16) -> u64 {
        IMSIC + u64::from(f) * 4096
    }
}

/// The reference's vector.
#[derive(Debug, Clone)]
struct RV {
    v: VectorRef,
    did: u32,
    k: u16,
    obj: ObjId,
    handle: abi::Handle,
    /// The domain was destroyed: the handle names a dead object.
    dead: bool,
    /// The file's enable bit (armed), INTERRUPT, and the pending bit of identity 1 while fired.
    armed: bool,
    fired: bool,
    pending: bool,
    /// Stray bits set in the file (data words other than 1), not yet counted.
    strays: BTreeSet<u32>,
}

struct M<'m> {
    w: World<'m>,
    ids: MsiIds,
    vecs: Vectors,
    harts: MHarts,
    objs: Kobjs,
    table: HandleTable,
    doms: BTreeMap<u32, DomainId>,
    refs: BTreeMap<u32, RV>,
    seen: usize,
    strays: u64,
    free_ids: u64,
    rng: Rng,
    log: Vec<String>,
    /// Coverage: messages delivered, entries rewritten by a move, messages sent through a stale entry.
    retargets: u64,
    stale: u64,
}

impl<'m> M<'m> {
    /// The harts asked to work in their files open their Opening identities and retire their Retiring ones (the kernel's `deliver`).
    fn hart_work(&mut self) {
        while let Some(f) = self.harts.asked.pop_first() {
            for id in 2..=self.ids.num_ids() {
                let p = Place { hart: f, id };
                match self.ids.slot(p) {
                    Some(Slot::Opening { .. }) => {
                        if let Some(o) = self.ids.opened(p) {
                            assert!(
                                o.retiring.is_none(),
                                "a vector's move holds its old identity (Moved)"
                            );
                        }
                    }
                    Some(Slot::Retiring { .. }) => {
                        if let Some((Route::Vector(n), _)) = self.ids.retired(p) {
                            self.vecs.retired(&self.ids, n);
                        }
                    }
                    _ => {}
                }
            }
        }
        while let Some(job) = self.vecs.take_spent() {
            credit_vector(&mut self.w.jobs, job);
        }
    }

    /// The messages the IOMMU delivered since the last look: each must be on an identity routed to `expect` (the vector the device
    /// signalled), and is taken as the kernel's interrupt path takes it. Returns how many arrived.
    fn take(&mut self, expect: Option<u32>) -> usize {
        let mut n = 0;
        while let Some(d) = self.w.sim.deliveries().get(self.seen).copied() {
            self.seen += 1;
            n += 1;
            assert_eq!(
                d.via,
                iommucore::sim::Via::Notice,
                "IO11: never a basic-translate message"
            );
            let f = (d.page - IMSIC) / 4096;
            assert!(
                d.page >= IMSIC && d.page % 4096 == 0 && f < u64::from(HARTS),
                "a page the kernel wrote: {d:?}"
            );
            let p = Place {
                hart: f as u16,
                id: d.identity as u16,
            };
            let route = match self.ids.slot(p) {
                Some(
                    Slot::Live { route, .. }
                    | Slot::Moved { route, .. }
                    | Slot::Retiring { route, moved: true },
                ) => route,
                s => panic!(
                    "IO10: a message on {p:?}, which is {s:?}\n{}",
                    self.log.join("\n")
                ),
            };
            assert_eq!(
                Some(route),
                expect.map(Route::Vector),
                "the message is the signalled vector's"
            );
            let fired = self.vecs.notice(&mut self.objs, &self.ids, self.w.mem, p);
            let r = self.refs.get_mut(&expect.unwrap()).unwrap();
            assert!(r.armed, "a notice only while the file is enabled");
            assert_eq!(fired, Some(true), "an armed vector's notice fires it");
            r.armed = false;
            r.fired = true;
        }
        n
    }

    /// Device `did` writes `data` at `addr`; what the reference expects is checked.
    fn signal(&mut self, did: u32, addr: u64, data: u32) {
        let k = addr.wrapping_sub(IO_BASE) / 4096;
        let target = self
            .refs
            .values()
            .find(|r| r.did == did && u64::from(r.k) == k && !r.dead && addr >= IO_BASE)
            .map(|r| r.v.n);
        let res = self.w.sim.write(did, addr, data);
        match target {
            Some(n) => {
                let aligned = addr.is_multiple_of(4);
                if data > 2047 || !aligned {
                    assert_eq!(res, Err(MSI_MISCONFIGURED));
                    assert_eq!(self.take(None), 0);
                    return;
                }
                assert!(res.is_ok(), "a granted page takes the message: {res:?}");
                let r = self.refs.get(&n).unwrap().clone();
                if data != 1 {
                    assert_eq!(self.take(None), 0, "a stray bit sends nothing");
                    self.refs.get_mut(&n).unwrap().strays.insert(data);
                } else if r.armed {
                    assert_eq!(
                        self.take(Some(n)),
                        1,
                        "an armed vector's message is delivered once"
                    );
                } else {
                    assert_eq!(
                        self.take(None),
                        0,
                        "masked: the message sets pending and sends nothing"
                    );
                    self.refs.get_mut(&n).unwrap().pending = true;
                }
            }
            None => {
                if self.doms.contains_key(&did) && (IO_BASE..IO_BASE + 256 * 4096).contains(&addr) {
                    assert_eq!(
                        res,
                        Err(MSI_INVALID),
                        "an ungranted page: the entry is invalid"
                    );
                } else {
                    assert!(res.is_err(), "no grant: a fault");
                }
                assert_eq!(self.take(None), 0, "nothing is delivered");
            }
        }
    }

    fn fence(&mut self, d: DomainId, k: u16) {
        self.w.sim.process_commands();
        if let Some(t) = self.w.store.vector_ticket(d, k) {
            assert_eq!(self.w.store.poll(&mut self.w.sim, t), Fence::Done);
        }
    }

    fn grant(&mut self, did: u32, k: u16) {
        let d = self.doms[&did];
        let free = self.w.store.vector_state(d, k) == Some(VectorState::Free);
        let before = self.ids.free_total();
        if !free {
            return;
        }
        charge_vector(&mut self.w.jobs, JobId::ROOT).unwrap();
        let v = match self
            .vecs
            .grant_begin(&mut self.ids, &mut self.harts, d, k, JobId::ROOT)
        {
            Ok(v) => v,
            Err(_) => {
                credit_vector(&mut self.w.jobs, JobId::ROOT);
                assert_eq!(
                    self.ids.free_total(),
                    before,
                    "a refused grant takes no identity"
                );
                return;
            }
        };
        // its hart opens the identity; then the entry
        let here = self
            .vecs
            .current(&self.ids, v)
            .map(|(p, ready)| (p.hart, ready));
        let write = |m: &mut M<'_>| {
            let M {
                w,
                ids,
                vecs,
                harts,
                ..
            } = m;
            let mut f = DbFrames {
                db: &mut w.mm.db,
                mem: w.mem,
                counts: &mut w.counts,
                fail: None,
            };
            vecs.grant_write(ids, harts, &mut w.store, w.mem, &mut w.sim, &mut f, v)
        };
        let mut t = write(self);
        if here.is_some_and(|(_, ready)| !ready) {
            // (placed on another hart: the entry waits until that hart opened the identity)
            assert_eq!(
                t,
                Err(IoError::NotDone),
                "the entry waits for the identity to be open"
            );
            self.hart_work();
            t = write(self);
        } else {
            assert_eq!(
                here.map(|(h, _)| h),
                Some(self.harts.here),
                "opened at once only on the calling hart"
            );
        }
        assert!(t.is_ok(), "{t:?}");
        self.fence(d, k);
        assert_eq!(
            self.vecs
                .fenced(&mut self.ids, &mut self.harts, &mut self.w.store, v),
            Ok(true)
        );
        let (h, obj) = ipc::vector_object_create_in(
            &mut self.objs,
            &mut self.table,
            v,
            &mut Quota {
                jobs: &mut self.w.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap();
        assert!(self.vecs.attach(&mut self.objs, self.w.mem, v, obj));
        self.refs.insert(
            v.n,
            RV {
                v,
                did,
                k,
                obj,
                handle: h,
                dead: false,
                armed: true,
                fired: false,
                pending: false,
                strays: BTreeSet::new(),
            },
        );
    }

    /// A move of `n` to the calling hart (`here`), as `move_vector_here` and `retarget` run it, with a message sent between the
    /// entry's rewrite and its fence.
    fn move_here(&mut self, n: u32, stale: bool) {
        let r = self.refs[&n].clone();
        match self.vecs.move_here(&mut self.ids, &mut self.harts, r.v) {
            Ok(Moved::Stays) => {}
            Ok(Moved::Retarget) => self.retarget(n, stale),
            Err(MoveError::Dead) => assert!(r.dead),
            Err(MoveError::NoMemory) => {}
            Err(MoveError::Busy) => panic!("no move is in flight between steps"),
        }
    }

    fn retarget(&mut self, n: u32, stale: bool) {
        let r = self.refs[&n].clone();
        let d = self.doms[&r.did];
        self.hart_work();
        let q = {
            let M {
                w,
                ids,
                vecs,
                harts,
                ..
            } = self;
            vecs.retarget_begin(ids, harts, &mut w.store, w.mem, &mut w.sim, r.v)
        };
        let Ok(Some(_)) = q else {
            assert!(matches!(q, Ok(None)), "{q:?}");
            return;
        };
        self.retargets += 1;
        if stale {
            self.stale += 1;
            // (the IOMMU may hold the old entry: the message reaches the old identity, which still routes to the vector)
            self.signal(r.did, IO_BASE + u64::from(r.k) * 4096, 1);
        }
        self.fence(d, r.k);
        assert_eq!(
            self.vecs
                .fenced(&mut self.ids, &mut self.harts, &mut self.w.store, r.v),
            Ok(true)
        );
        self.hart_work();
    }

    fn ack(&mut self, n: u32) {
        let r = self.refs[&n].clone();
        let got = self.vecs.ack(&mut self.objs, self.w.mem, r.obj, r.v);
        if r.dead || !r.fired {
            assert_eq!(got.map(|_| ()), Err(abi::status::Status::BadState));
            return;
        }
        let (_, place) = got.unwrap();
        let x = self.refs.get_mut(&n).unwrap();
        self.strays += x.strays.len() as u64;
        x.strays.clear();
        x.fired = x.pending;
        x.armed = !x.pending;
        x.pending = false;
        if place {
            let stale = self.rng.chance(50);
            self.move_here(n, stale);
        }
    }

    /// The last handle closes: revoke steps 1 to 3 at the cleanup, then the drain (the fence, the file, the identities, the charge),
    /// with a message sent before the fence.
    fn close(&mut self, n: u32, stale: bool) {
        let r = self.refs.remove(&n).unwrap();
        self.table.close(&mut self.objs, r.handle).unwrap();
        for o in self.objs.drain_dead() {
            let KObj::Interrupt(i) = o else {
                panic!("only vector objects die here")
            };
            assert_eq!(i.vector, Some(r.v));
            let t = self
                .vecs
                .revoke_begin(&mut self.w.store, self.w.mem, &mut self.w.sim, r.v);
            assert_eq!(t.is_some(), !r.dead, "a dead vector has nothing to revoke");
        }
        if r.dead {
            return;
        }
        self.strays += r.strays.len() as u64;
        if stale {
            // the enable bit is clear first: a message through a cached entry sets a bit and sends nothing
            let _ = self.w.sim.write(r.did, IO_BASE + u64::from(r.k) * 4096, 1);
            assert_eq!(self.take(None), 0, "a revoked vector never fires");
        }
        let d = self.doms[&r.did];
        assert_eq!(self.vecs.next_revoking(0), Some(r.v));
        self.fence(d, r.k);
        let e = {
            let M {
                w,
                ids,
                vecs,
                harts,
                ..
            } = self;
            let mut f = DbFrames {
                db: &mut w.mm.db,
                mem: w.mem,
                counts: &mut w.counts,
                fail: None,
            };
            vecs.revoke_end(ids, harts, &mut w.store, w.mem, &mut f, r.v)
        };
        assert_eq!(e, Ok(()));
        self.hart_work();
        assert_eq!(self.vecs.state(r.v), None, "the route is free again");
    }

    /// The domain's destruction (10.6): the fence, its routes ended, the files freed with the domain.
    fn destroy(&mut self, did: u32) {
        let d = self.doms.remove(&did).unwrap();
        let x = self
            .w
            .store
            .destroy(self.w.mem, &mut self.w.sim, d)
            .unwrap();
        self.w.sim.process_commands();
        assert_eq!(self.w.store.poll(&mut self.w.sim, x.ticket()), Fence::Done);
        assert!(self.w.store.destroy_done(&x));
        self.vecs.domain_gone(&mut self.ids, &mut self.harts, d);
        let mut det = self
            .w
            .store
            .destroy_complete(self.w.mem, &x)
            .unwrap()
            .unwrap();
        while let Some(rel) = det.next_record() {
            self.w.settle(rel);
        }
        let strays = {
            let M { w, .. } = self;
            let mut f = DbFrames {
                db: &mut w.mm.db,
                mem: w.mem,
                counts: &mut w.counts,
                fail: None,
            };
            while !det.step(w.mem, &mut f) {}
            det.finish(w.mem, &mut f, &mut w.jobs)
        };
        self.w.store.count_strays(strays);
        // (the stray bits of its live vectors' files are found when the files are freed)
        self.strays += self
            .refs
            .values_mut()
            .filter(|r| r.did == did && !r.dead)
            .map(|r| std::mem::take(&mut r.strays).len() as u64)
            .sum::<u64>();
        self.w.store.destroy_released();
        self.hart_work();
        for r in self.refs.values_mut().filter(|r| r.did == did) {
            r.dead = true;
            r.armed = false;
        }
    }

    fn offline(&mut self, f: u16) {
        let usable: Vec<bool> = (0..HARTS)
            .map(|g| g != f && !self.harts.offline[usize::from(g)])
            .collect();
        let asked = &mut self.harts.asked;
        let ok = self.ids.evacuate(
            f,
            |g| usable[usize::from(g)],
            |c| match c {
                kcore::msiid::Evacuated::Cancelled { at, .. } => {
                    asked.insert(at.hart);
                }
                kcore::msiid::Evacuated::Moved { from, to, .. } => {
                    asked.insert(from.hart);
                    asked.insert(to.hart);
                }
                kcore::msiid::Evacuated::Replaced { from, to, .. } => {
                    asked.insert(from.hart);
                    asked.insert(to.hart);
                }
            },
        );
        if ok.is_err() {
            return;
        }
        self.harts.offline[usize::from(f)] = true;
        self.hart_work();
        // `settle_moves`: each vector whose entry does not name its open identity is retargeted (a message in flight each time)
        for _ in 0..64 {
            let Some(v) = self.vecs.next_to_settle(&self.ids) else {
                break;
            };
            let stale = self.rng.chance(50);
            self.retarget(v.n, stale);
        }
        assert_eq!(self.vecs.next_to_settle(&self.ids), None);
    }

    /// What must hold between steps.
    fn check(&self) {
        // IO10: no identity is Free while an MSI entry names it, in the tables or the IOMMU's cache
        let mut named = BTreeSet::new();
        let decode = |p: [u64; 2]| -> Option<(u64, u16)> {
            if p[0] & 1 == 0 || (p[0] >> 1) & 3 != 1 {
                return None;
            }
            let page = ((p[1] >> 10) & ((1 << 44) - 1)) << 12;
            Some((page, ((p[1] & 0x3ff) | (((p[1] >> 60) & 1) << 10)) as u16))
        };
        for &d in self.doms.values() {
            let root = self.w.store.msi_root(d).unwrap();
            for k in 0..256u64 {
                let p = [
                    self.w.mem.read(root + k * 16),
                    self.w.mem.read(root + k * 16 + 8),
                ];
                if p[0] & 1 != 0 {
                    assert_eq!((p[0] >> 1) & 3, 1, "IO11: every valid entry is MRIF");
                }
                named.extend(decode(p));
            }
        }
        named.extend(self.w.sim.msi_cached_entries().filter_map(decode));
        for (page, id) in named {
            let p = Place {
                hart: ((page - IMSIC) / 4096) as u16,
                id,
            };
            assert!(
                !matches!(self.ids.slot(p), Some(Slot::Free) | None),
                "IO10: {p:?} is free while an entry names it\n{}",
                self.log.join("\n")
            );
        }
        // every live vector holds one identity; its object is the reference's
        let live = self.refs.values().filter(|r| !r.dead).count() as u64;
        assert_eq!(
            self.ids.free_total(),
            self.free_ids - live,
            "identities held"
        );
        for r in self.refs.values() {
            match self.objs.get(r.obj) {
                Some(KObj::Interrupt(i)) => assert_eq!(i.fired, r.fired, "INTERRUPT of {r:?}"),
                _ => panic!("the object is alive while its handle is"),
            }
            if !r.dead {
                assert_eq!(self.vecs.state(r.v), Some(RState::Live));
                let (p, ready) = self.vecs.current(&self.ids, r.v).unwrap();
                assert!(
                    ready && !self.harts.offline[usize::from(p.hart)],
                    "placed on an online hart"
                );
            }
        }
        assert_eq!(
            self.w.jobs.io_usage(JobId::ROOT, IoResource::Vectors),
            Some(live),
            "J1: the vector charge"
        );
        let c = self.vecs.counts();
        assert_eq!((c.dead, c.leaked), (0, 0));
        assert_eq!(
            c.strays + self.w.store.stats().strays,
            self.strays,
            "8.4: each stray bit counted once"
        );
    }
}

fn run(seed: u64, steps: usize) -> [u64; 3] {
    let mem = IoMem::new();
    let w = World::new(&mem, false);
    let frames0 = w.mm.db.free_count();
    let ids = MsiIds::new(usize::from(HARTS), NUM_IDS).unwrap();
    let free_ids = ids.free_total();
    let mut m = M {
        w,
        ids,
        vecs: Vectors::new(usize::from(HARTS) * NUM_IDS as usize * 2).unwrap(),
        harts: MHarts {
            here: 0,
            offline: [false; HARTS as usize],
            asked: BTreeSet::new(),
        },
        objs: Kobjs::new(),
        table: HandleTable::new(100_000),
        doms: BTreeMap::new(),
        refs: BTreeMap::new(),
        seen: 0,
        strays: 0,
        free_ids,
        rng: Rng::new(seed),
        log: vec![],
        retargets: 0,
        stale: 0,
    };
    for i in 0..steps {
        let pick = m.rng.below(100);
        let did = m.rng.below(u64::from(DEVICES)) as u32;
        let k = m.rng.below(u64::from(INDICES)) as u16;
        let vs: Vec<u32> = m.refs.keys().copied().collect();
        let n = (!vs.is_empty()).then(|| vs[m.rng.below(vs.len() as u64) as usize]);
        m.harts.here = loop {
            let h = m.rng.below(u64::from(HARTS)) as u16;
            if !m.harts.offline[usize::from(h)] {
                break h;
            }
        };
        let step = match pick {
            0..=7 if !m.doms.contains_key(&did) => {
                let d = ready(&mut m.w, did, JobId::ROOT);
                m.doms.insert(did, d);
                format!("create {did}")
            }
            0..=19 if m.doms.contains_key(&did) => {
                m.grant(did, k);
                format!("grant {did}/{k}")
            }
            20..=49 => {
                let data = match m.rng.below(10) {
                    0 => 2 + m.rng.below(60) as u32,
                    1 => 2048 + m.rng.below(100) as u32,
                    _ => 1,
                };
                let addr = IO_BASE + u64::from(k) * 4096 + if m.rng.chance(5) { 2 } else { 0 };
                let addr = if m.rng.chance(3) { IMSIC } else { addr };
                m.signal(did, addr, data);
                format!("signal {did} {addr:#x} {data}")
            }
            50..=64 if n.is_some() => {
                m.ack(n.unwrap());
                format!("ack {n:?} on {}", m.harts.here)
            }
            65..=72 if n.is_some() => {
                let x = n.unwrap();
                let first = m.vecs.first_wait(&m.objs, m.refs[&x].obj);
                if first.is_some() || m.rng.chance(50) {
                    let stale = m.rng.chance(50);
                    if !m.refs[&x].dead {
                        m.move_here(x, stale);
                    }
                }
                format!("move {n:?} to {}", m.harts.here)
            }
            73..=82 if n.is_some() => {
                let stale = m.rng.chance(50);
                m.close(n.unwrap(), stale);
                format!("close {n:?}")
            }
            83..=88 if m.doms.contains_key(&did) => {
                m.destroy(did);
                format!("destroy {did}")
            }
            89..=94 => {
                let f = m.rng.below(u64::from(HARTS)) as u16;
                let online = m.harts.offline.iter().filter(|o| !**o).count();
                if m.harts.offline[usize::from(f)] {
                    m.harts.offline[usize::from(f)] = false;
                    format!("online {f}")
                } else if online > 1 {
                    m.offline(f);
                    format!("offline {f}")
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        m.log.push(format!("{i}: {step}"));
        m.check();
    }
    // the baseline: every handle closed, every domain destroyed
    let vs: Vec<u32> = m.refs.keys().copied().collect();
    for n in vs {
        m.close(n, false);
    }
    let ds: Vec<u32> = m.doms.keys().copied().collect();
    for did in ds {
        m.destroy(did);
    }
    m.check();
    let c = m.vecs.counts();
    assert_eq!((c.live, c.dead, c.leaked, c.used), (0, 0, 0, 0), "{c:?}");
    assert_eq!(m.ids.free_total(), free_ids);
    assert_eq!(m.w.mm.db.free_count(), frames0, "M3: every frame back");
    assert_eq!(m.objs.live(), 0);
    assert_eq!(m.w.jobs.io_usage(JobId::ROOT, IoResource::Vectors), Some(0));
    [m.seen as u64, m.retargets, m.stale]
}

#[test]
fn random_vector_sequences_keep_messages_identities_and_counts_exact() {
    let seeds: u64 = std::env::var("IOVEC_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let mut seen = [0u64; 3];
    for seed in 0..seeds {
        let got = run(seed, 400);
        for (a, b) in seen.iter_mut().zip(got) {
            *a += b;
        }
    }
    // (the run reaches what it is for: messages delivered, moves, and messages through the old entry of a move)
    eprintln!("delivered, retargets, stale: {seen:?}");
    assert!(seen.iter().all(|&x| x >= seeds), "coverage {seen:?}");
}
