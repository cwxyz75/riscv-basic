//! IOMMU3: the device-domain store (`kcore::iodomain`) against a reference model written from IOMMU-SPEC sections 4 to 7, with the
//! simulated IOMMU (`iommucore::sim`) as the hardware. The simulated device walks the real tables with its own walker and keeps its own
//! translation and context caches, which only the invalidation commands drop, so it does not share the store's bookkeeping.
//!
//! The random test runs create, map, unmap, the fence waits (with a device that runs, is slow, or stalls so that fences time out),
//! complete, abandon, destroy (by call, by the last handle, by "process death" of an owner that drops every handle it holds), VMO handle
//! closes and premature completions, in any order, and after **every** step checks:
//!
//! * IO3 and M8: the device reaches a frame only through a mapping the reference says is mapped, pending, leaked or in a domain being
//!   destroyed, and **never a frame that is in the frame database's free lists**; a mapping of an attached domain is reached, at its
//!   frame, with exactly its access (IO4);
//! * the counts are exact: frames held (DMA pages, table frames, roots), the table frames of each domain (the tables its live pages
//!   need, independently computed), the I/O-mapped pages and kernel bytes of every job (J1), the VMO reference counts;
//! * at the end, the baseline (M3): every frame back, every job at zero, unless a fence was abandoned, and then exactly what was leaked
//!   is missing and counted (IO8).
mod common;
mod iodomain_common;

use common::Rng;
use iodomain_common::*;
use iommucore::faultq::{FaultQueue, FaultRecord, TransactionType};
use iommucore::iopt::Access;
use iommucore::PhysMem64;
use kcore::iodomain::*;
use kcore::job::{IoResource, JobId, Limits, Resource};
use std::collections::{BTreeMap, BTreeSet};

const DEVICES: u32 = 6;

// ---------------------------------------------------------------------------------------------------------------------------
// the reference model

#[derive(Debug, Clone)]
struct RMap {
    d: DomainId,
    device: u32,
    job: JobId,
    page: u64,
    pages: u64,
    vmo: u64,
    vmo_page: u64,
    pfns: Vec<u64>,
    write: bool,
    /// The map's fence completed: the address was given to the caller.
    visible: bool,
}

#[derive(Debug)]
struct RDom {
    device: u32,
    job: JobId,
    /// The create's fence completed: handles exist and the device must see its mappings.
    ready: bool,
    detaching: bool,
    handles: [u32; 2],
}

#[derive(Debug)]
enum Op {
    Create(DomainId, Ticket),
    Map(DomainId, u64, Ticket),
    Unmap(Pending),
    Destroy(Destroying),
}

#[derive(Default)]
struct Ref {
    doms: BTreeMap<DomainId, RDom>,
    live: BTreeMap<(DomainId, u64), RMap>,
    pending: Vec<(Pending, RMap)>,
    leaked: Vec<RMap>,
    /// Leaked domains: their job, and the table frames their live pages held (the root and MSI table are counted by the frame
    /// counters).
    leaked_doms: Vec<(JobId, u64)>,
    vmo_handles: Vec<(usize, u64)>,
    vmo_pages: BTreeMap<u64, u64>,
    ops: Vec<Op>,
    touched: BTreeMap<u32, BTreeSet<u64>>,
    jobs: Vec<JobId>,
    abandoned: bool,
    /// The ids of domains that are gone (destroyed or leaked): no call may find them again.
    dead: Vec<DomainId>,
}

impl Ref {
    fn holders(&self, vmo: u64) -> u64 {
        let h = self.vmo_handles.iter().filter(|(_, v)| *v == vmo).count();
        let l = self.live.values().filter(|m| m.vmo == vmo).count();
        let p = self.pending.iter().filter(|(_, m)| m.vmo == vmo).count();
        let k = self.leaked.iter().filter(|m| m.vmo == vmo).count();
        (h + l + p + k) as u64
    }

    fn records_of(&self, job: JobId) -> impl Iterator<Item = &RMap> {
        self.live
            .values()
            .chain(self.pending.iter().map(|(_, m)| m))
            .chain(self.leaked.iter())
            .filter(move |m| m.job == job)
    }

    /// Move a domain's mappings (live and pending) to the leaked list (its destruction's fence was abandoned).
    fn leak_domain(&mut self, d: DomainId) {
        let keys: Vec<_> = self.live.keys().filter(|k| k.0 == d).copied().collect();
        let mut pages = vec![];
        for k in keys {
            let m = self.live.remove(&k).unwrap();
            pages.extend(m.page..m.page + m.pages);
            self.leaked.push(m);
        }
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, m)| m.d != d);
        self.pending = keep;
        self.leaked.extend(gone.into_iter().map(|(_, m)| m));
        let dom = self.doms.remove(&d).unwrap();
        self.dead.push(d);
        self.leaked_doms
            .push((dom.job, tables_for(pages.into_iter())));
    }

    /// A domain whose destruction completed: its live and pending mappings were settled.
    fn drop_domain(&mut self, d: DomainId) {
        self.live.retain(|k, _| k.0 != d);
        self.pending.retain(|(_, m)| m.d != d);
        self.doms.remove(&d);
        self.dead.push(d);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// the checks

/// The device's reach, probed for every page any domain of each device ever mapped (and some never mapped).
fn check_reach(w: &mut World, r: &Ref, seed: u64, step: u64) {
    for did in 0..DEVICES {
        let mut probes: BTreeSet<u64> = r.touched.get(&did).cloned().unwrap_or_default();
        probes.insert((IO_BASE >> 12) + IO_VECTOR_PAGES); // the first DMA page
        probes.insert((IO_BASE >> 12) + IO_VECTOR_PAGES + 977);
        for page in probes {
            // what may be reached at this page, and what must be
            let mut allowed: Vec<(u64, bool)> = vec![];
            let mut required: Option<(u64, bool)> = None;
            let covers = |m: &RMap| m.device == did && (m.page..m.page + m.pages).contains(&page);
            for m in r.live.values().filter(|m| covers(m)) {
                let pfn = m.pfns[(page - m.page) as usize];
                allowed.push((pfn, m.write));
                let dom = &r.doms[&m.d];
                // (a mapping must be reached once its fence has completed: before, an earlier unmap's whole-context invalidation may
                // still be queued, and the page-walk cache may still name the table frames it detached, A18; such a walk faults and
                // reaches no freed frame, which the checks below keep asserting)
                if dom.ready && !dom.detaching && m.visible {
                    required = Some((pfn, m.write));
                }
            }
            for m in r
                .pending
                .iter()
                .map(|(_, m)| m)
                .chain(r.leaked.iter())
                .filter(|m| covers(m))
            {
                allowed.push((m.pfns[(page - m.page) as usize], m.write));
            }
            let ctx = format!("seed {seed} step {step} device {did} page {page:#x}");
            match w.sim.read(did, page << 12) {
                Ok(pa) => {
                    let pfn = pa >> 12;
                    assert!(
                        allowed.iter().any(|(p, _)| *p == pfn),
                        "{ctx}: the device read frame {pfn:#x}, which nothing maps there ({allowed:x?})"
                    );
                    assert!(
                        w.mm.db.refs(pfn).is_some(),
                        "{ctx}: IO3: the device can read frame {pfn:#x}, which is free"
                    );
                    if let Some((p, _)) = required {
                        assert_eq!(pfn, p, "{ctx}: a live mapping reaches its own frame");
                    }
                }
                Err(cause) => assert!(
                    required.is_none(),
                    "{ctx}: a live mapping of an attached domain faults on read (cause {cause})"
                ),
            }
            match w.sim.write(did, page << 12, 0x5a5a_5a5a) {
                Ok(iommucore::sim::WriteOutcome::Memory(pa)) => {
                    let pfn = pa >> 12;
                    assert!(
                        allowed.iter().any(|&(p, wr)| p == pfn && wr),
                        "{ctx}: IO4: the device wrote frame {pfn:#x} without a writable mapping ({allowed:x?})"
                    );
                    assert!(
                        w.mm.db.refs(pfn).is_some(),
                        "{ctx}: IO3: the device can write frame {pfn:#x}, which is free"
                    );
                }
                Ok(other) => panic!("{ctx}: a DMA page is not an MSI: {other:?}"),
                Err(_) => assert!(
                    required.is_none_or(|(_, wr)| !wr),
                    "{ctx}: a writable live mapping faults on write"
                ),
            }
        }
    }
}

/// The counts: frames, tables, jobs, VMOs, the store's own view.
fn check_counts(w: &World, r: &Ref, seed: u64, step: u64) {
    let ctx = format!("seed {seed} step {step}");
    w.mm.db.audit().unwrap();
    // VMOs: alive exactly while held, with exactly their holders
    let mut vmo_frames = 0;
    for (&vmo, &pages) in &r.vmo_pages {
        let h = r.holders(vmo);
        assert_eq!(
            w.vmos.exists(vmo),
            h > 0,
            "{ctx}: VMO {vmo:#x} alive iff held (M8)"
        );
        if h > 0 {
            assert_eq!(
                w.vmos.refs(vmo),
                Some(h as u32),
                "{ctx}: VMO {vmo:#x} holders"
            );
            vmo_frames += pages;
        }
    }
    // the table frames of each live domain are exactly those its live pages need
    let mut known_tables = 0u64;
    let mut msi = r.leaked_doms.len() as u64;
    for (&d, dom) in &r.doms {
        let pages = r
            .live
            .values()
            .filter(|m| m.d == d)
            .flat_map(|m| m.page..m.page + m.pages);
        let t = tables_for(pages);
        let (tables, io_pages) = w.store.usage(d).unwrap();
        assert_eq!(tables, t, "{ctx}: domain {d:?} table frames");
        let want_io: u64 = r
            .live
            .values()
            .filter(|m| m.d == d)
            .chain(r.pending.iter().map(|(_, m)| m).filter(|m| m.d == d))
            .chain(r.leaked.iter().filter(|m| m.d == d))
            .map(|m| m.pages)
            .sum();
        assert_eq!(io_pages, want_io, "{ctx}: domain {d:?} I/O-mapped pages");
        let (live, pending, leaked) = w.store.counts(d).unwrap();
        assert_eq!(
            (live, pending, leaked),
            (
                r.live.keys().filter(|k| k.0 == d).count() as u64,
                r.pending.iter().filter(|(_, m)| m.d == d).count() as u64,
                r.leaked.iter().filter(|m| m.d == d).count() as u64,
            ),
            "{ctx}: domain {d:?} records"
        );
        for m in r.live.values().filter(|m| m.d == d) {
            let access = if m.write {
                Access::ReadWrite
            } else {
                Access::Read
            };
            assert_eq!(
                w.store.mapping_at(d, m.page << 12),
                Some((m.pages, m.vmo, m.vmo_page, access)),
                "{ctx}: the record of {:#x}",
                m.page
            );
        }
        known_tables += t;
        msi += 1;
        // the domain's identity: its device, its job, and the device's domain (attached or detaching)
        assert_eq!(
            w.store.device_of(d),
            Some(dom.device),
            "{ctx}: device of {d:?}"
        );
        assert_eq!(w.store.job_of(d), Some(dom.job), "{ctx}: job of {d:?}");
        assert_eq!(
            w.store.domain_of(dom.device),
            Some(d),
            "{ctx}: domain of device {}",
            dom.device
        );
    }
    // each live domain has its own soft context, never GSCID 0 (the quarantine's)
    let gscids: BTreeSet<u16> = r
        .doms
        .keys()
        .map(|&d| w.store.gscid_of(d).unwrap())
        .collect();
    assert_eq!(
        gscids.len(),
        r.doms.len(),
        "{ctx}: one GSCID per live domain"
    );
    assert!(!gscids.contains(&0), "{ctx}: GSCID 0 is the quarantine's");
    // a domain that is gone is never found again, though its slot is reused
    for &d in &r.dead {
        assert_eq!(
            (w.store.device_of(d), w.store.gscid_of(d)),
            (None, None),
            "{ctx}: {d:?} is gone"
        );
    }
    for &(_, t) in &r.leaked_doms {
        known_tables += t;
    }
    let chained = w.counts.tables as i64 - known_tables as i64 - msi as i64;
    assert!(
        chained >= 0,
        "{ctx}: table frames held: {} < {known_tables} + {msi}",
        w.counts.tables
    );
    // every frame held is a VMO's, a table, a root, or the bring-up's
    let held = FRAMES - w.mm.db.free_count();
    assert_eq!(
        held as u64,
        w.fixed as u64 + vmo_frames + w.counts.tables + 4 * w.counts.roots,
        "{ctx}: frames held"
    );
    assert_eq!(
        w.counts.roots,
        (r.doms.len() + r.leaked_doms.len()) as u64,
        "{ctx}: roots"
    );
    // each domain job: I/O-mapped pages exactly its records'; kernel bytes exactly its domain's, its records' and its tables'
    let mut residual = 0u64;
    for &job in &r.jobs {
        let io: u64 = r.records_of(job).map(|m| m.pages).sum();
        assert_eq!(
            w.jobs.io_usage(job, IoResource::MappedPages),
            Some(io),
            "{ctx}: job {job:?} I/O-mapped pages (J1)"
        );
        let doms = r.doms.values().filter(|d| d.job == job).count() as u64
            + r.leaked_doms.iter().filter(|(j, _)| *j == job).count() as u64;
        let tables: u64 = r
            .doms
            .iter()
            .filter(|(_, d)| d.job == job)
            .map(|(&d, _)| {
                tables_for(
                    r.live
                        .values()
                        .filter(|m| m.d == d)
                        .flat_map(|m| m.page..m.page + m.pages),
                )
            })
            .sum::<u64>()
            + r.leaked_doms
                .iter()
                .filter(|(j, _)| *j == job)
                .map(|(_, t)| t)
                .sum::<u64>();
        let known =
            doms * DOMAIN_BYTES + r.records_of(job).count() as u64 * MAPPING_BYTES + tables * PAGE;
        let kb = w.jobs.usage(job).unwrap().get(Resource::KernelBytes);
        assert!(
            kb >= known,
            "{ctx}: job {job:?} kernel bytes {kb} < {known}"
        );
        assert_eq!((kb - known) % PAGE, 0, "{ctx}: job {job:?} kernel bytes");
        residual += (kb - known) / PAGE;
    }
    assert_eq!(
        residual, chained as u64,
        "{ctx}: the detached table frames waiting for a fence are charged, each once"
    );
    let s = w.store.stats();
    assert_eq!(s.domains, r.doms.len() as u64, "{ctx}: live domains");
    let io_all: u64 = r
        .live
        .values()
        .chain(r.pending.iter().map(|(_, m)| m))
        .map(|m| m.pages)
        .sum();
    assert_eq!(
        s.io_pages, io_all,
        "{ctx}: I/O-mapped pages counter (live and pending)"
    );
    let leaked: u64 = r.leaked.iter().map(|m| m.pages).sum();
    assert_eq!(s.pages_leaked, leaked, "{ctx}: leaked pages counted apart");
    assert_eq!(
        s.holds_leaked,
        r.leaked.len() as u64,
        "{ctx}: leaked holds counted"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// the random test

fn new_job(w: &mut World, r: &mut Ref, parent: JobId, rng: &mut Rng) -> JobId {
    let job = w.jobs.create(parent, Limits::UNLIMITED).unwrap();
    if rng.chance(30) {
        w.jobs
            .set_io_limits(job, 4 + rng.below(40), u64::MAX)
            .unwrap();
    }
    r.jobs.push(job);
    job
}

/// Destroy a domain whose handles are gone (or by call): phase 1 now, the rest is an operation in flight.
fn start_destroy(w: &mut World, r: &mut Ref, d: DomainId) {
    if let Some(x) = w.destroy(d) {
        r.doms.get_mut(&d).unwrap().detaching = true;
        r.ops.push(Op::Destroy(x));
    }
}

fn close_domain_handle(w: &mut World, r: &mut Ref, d: DomainId, owner: usize) {
    let dom = r.doms.get_mut(&d).unwrap();
    dom.handles[owner] -= 1;
    if dom.handles == [0, 0] {
        start_destroy(w, r, d);
    }
}

/// Finish or time out operation `i`: the fence is done, or (`timeout`) the kernel's bound passed.
/// What the random test reached, so a test that stops reaching a path fails.
static HITS: [std::sync::atomic::AtomicU64; 12] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 12];
const HIT_NAMES: [&str; 12] = [
    "create completed",
    "create timed out",
    "map timed out",
    "unmap completed",
    "unmap abandoned",
    "destroy completed",
    "destroy abandoned",
    "map rolled back",
    "map refused",
    "destroy settled a pending record",
    "map waited",
    "map skipped (no domain or VMO)",
];
fn hit(i: usize) {
    HITS[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn settle_op(w: &mut World, r: &mut Ref, i: usize, timeout: bool, rng: &mut Rng) {
    let op = r.ops.remove(i);
    match op {
        Op::Create(d, _) if !timeout => {
            hit(0);
            let dom = r.doms.get_mut(&d).unwrap();
            dom.ready = true;
            dom.handles[0] = 1;
            if rng.chance(70) {
                dom.handles[1] = 1;
            }
        }
        Op::Create(d, _) => {
            hit(1);
            // the create returns NO_MEMORY: the domain it made is destroyed again
            start_destroy(w, r, d);
        }
        Op::Map(d, page, _) if !timeout => {
            if let Some(m) = r.live.get_mut(&(d, page)) {
                m.visible = true;
            }
        }
        Op::Map(d, page, _) => {
            hit(2);
            // NO_MEMORY to the map, whose leaves are cleared again: the roll-back is an unmap
            let Some(m) = r.live.get(&(d, page)).cloned() else {
                return;
            };
            match w.unmap(d, page << 12, m.pages) {
                Ok(p) => {
                    r.live.remove(&(d, page));
                    r.pending.push((p, m));
                    r.ops.push(Op::Unmap(p));
                }
                Err(e) => {
                    assert_eq!(e, IoError::BadState);
                    assert!(
                        r.doms[&d].detaching,
                        "only a destroyed domain refuses the roll-back"
                    );
                }
            }
        }
        Op::Unmap(p) if !timeout => {
            let rel = w.store.complete(&p).unwrap();
            let at = r.pending.iter().position(|(q, _)| *q == p);
            match (rel, at) {
                (Some(rel), Some(at)) => {
                    hit(3);
                    let (_, m) = r.pending.remove(at);
                    assert_eq!((rel.vmo(), rel.pages()), (m.vmo, m.pages));
                    w.settle(rel);
                }
                (None, None) => hit(9), // the domain's destruction settled it
                (rel, at) => panic!("complete: store {:?}, reference {at:?}", rel.is_some()),
            }
        }
        Op::Unmap(p) => {
            let at = r.pending.iter().position(|(q, _)| *q == p);
            assert_eq!(w.store.abandon(&p), at.is_some());
            if let Some(at) = at {
                hit(4);
                let (_, m) = r.pending.remove(at);
                r.leaked.push(m);
                r.abandoned = true;
            }
        }
        Op::Destroy(x) if !timeout => {
            let det = w
                .store
                .destroy_complete(w.mem, &x)
                .unwrap()
                .expect("a destruction completes once");
            hit(5);
            let d = x.domain();
            assert_eq!(det.job(), r.doms[&d].job);
            // the records the store hands back are exactly the domain's live and pending ones
            let mut want: Vec<(u64, u64)> = r
                .live
                .values()
                .filter(|m| m.d == d)
                .chain(r.pending.iter().map(|(_, m)| m).filter(|m| m.d == d))
                .map(|m| (m.vmo, m.pages))
                .collect();
            let mut det = det;
            let mut got = vec![];
            while let Some(rel) = det.next_record() {
                got.push((rel.vmo(), rel.pages()));
                w.settle(rel);
            }
            want.sort();
            got.sort();
            assert_eq!(got, want, "the destruction returns every record");
            w.finish(det);
            r.drop_domain(d);
        }
        Op::Destroy(x) => {
            assert!(w.store.destroy_abandon(&x));
            hit(6);
            r.abandoned = true;
            r.leak_domain(x.domain());
        }
    }
}

/// Queue an operation's commands again (its fence found the queue full).
fn requeue_op(w: &mut World, op: &mut Op) -> Fence {
    match op {
        Op::Create(d, t) => w.store.requeue_create(w.mem, &mut w.sim, *d, t),
        Op::Map(d, page, t) => {
            let mut m = Mapped {
                iova: *page << 12,
                ticket: *t,
                tables: 0,
            };
            let f = w.store.requeue_map(w.mem, &mut w.sim, *d, &mut m);
            *t = m.ticket;
            f
        }
        Op::Unmap(p) => w.store.requeue_unmap(w.mem, &mut w.sim, p),
        Op::Destroy(x) => w.store.requeue_destroy(w.mem, &mut w.sim, x),
    }
}

/// At the end, with the device running: poll, queueing again while the queue was full.
fn finish_poll(w: &mut World, op: &mut Op) -> Fence {
    loop {
        match w.store.poll(&mut w.sim, op_ticket(op)) {
            Fence::Unqueued => {
                if requeue_op(w, op) == Fence::Done {
                    return Fence::Done;
                }
                w.sim.process_commands();
            }
            f => return f,
        }
    }
}

fn op_ticket(op: &Op) -> Ticket {
    match op {
        Op::Create(_, t) | Op::Map(_, _, t) => *t,
        Op::Unmap(p) => p.ticket(),
        Op::Destroy(x) => x.ticket(),
    }
}

#[test]
fn random_maps_unmaps_fences_and_destructions_agree_with_the_model() {
    let seeds = if cfg!(miri) {
        2
    } else {
        std::env::var("IOD_SEEDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60)
    };
    let steps = if cfg!(miri) { 40 } else { 300 };
    for seed in 0..seeds {
        let mem = IoMem::new();
        let mut w = World::new(&mem, false);
        let mut r = Ref::default();
        let mut rng = Rng::new(seed);
        let base_free = w.mm.db.free_count();
        let drivers = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        let bufs = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        let mut stalled = false;
        for step in 0..steps {
            // the mix: creates only while few domains live, maps often, deaths rarely
            const MIX: [u64; 24] = [
                0, 1, 1, 2, 2, 2, 2, 2, 2, 3, 4, 4, 5, 5, 6, 7, 8, 8, 9, 9, 10, 11, 12, 13,
            ];
            let mut op = MIX[rng.below(MIX.len() as u64) as usize];
            if op == 0 && r.doms.len() >= 4 {
                op = 2;
            }
            // closes and destructions are rarer than maps, so domains live long enough to fill
            if (10..=13).contains(&op) && !rng.chance(35) {
                op = 2;
            }
            if op == 13 && !rng.chance(30) {
                op = 12;
            }
            match op {
                // create a domain for a free device, paid by a fresh driver job
                0 => {
                    let did = rng.below(DEVICES as u64) as u32;
                    let job = new_job(&mut w, &mut r, drivers, &mut rng);
                    let busy = r.doms.values().any(|d| d.device == did);
                    match w.create(did, job) {
                        Ok((d, t)) => {
                            assert!(
                                !busy,
                                "seed {seed}: IO5: a second live domain for device {did}"
                            );
                            r.doms.insert(
                                d,
                                RDom {
                                    device: did,
                                    job,
                                    ready: false,
                                    detaching: false,
                                    handles: [0, 0],
                                },
                            );
                            r.ops.push(Op::Create(d, t));
                        }
                        Err(e) => assert!(
                            busy && e == IoError::BadState || e == IoError::NoMemory,
                            "seed {seed}: create {did}: {e:?}"
                        ),
                    }
                }
                // a DMA VMO for an owner (now and then a large one, which is given back in batches)
                1 => {
                    let pages = if rng.chance(10) {
                        65 + rng.below(80)
                    } else {
                        1 + rng.below(8)
                    };
                    if let Ok(v) = w.vmos.create_dma_in(&mut w.mm, &mut w.jobs, pages, bufs) {
                        r.vmo_pages.insert(v, pages);
                        r.vmo_handles.push((rng.below(2) as usize, v));
                    }
                }
                // map: an owner with a handle to a ready domain and to a VMO
                2 | 3 => {
                    let owner = rng.below(2) as usize;
                    let doms: Vec<DomainId> = r
                        .doms
                        .iter()
                        .filter(|(_, d)| d.ready && !d.detaching && d.handles[owner] > 0)
                        .map(|(&d, _)| d)
                        .collect();
                    let vmos: Vec<u64> = r
                        .vmo_handles
                        .iter()
                        .filter(|(o, _)| *o == owner)
                        .map(|(_, v)| *v)
                        .collect();
                    if doms.is_empty() || vmos.is_empty() {
                        hit(11);
                        continue;
                    }
                    let d = doms[rng.below(doms.len() as u64) as usize];
                    let v = vmos[rng.below(vmos.len() as u64) as usize];
                    let size = r.vmo_pages[&v];
                    let first = rng.below(size);
                    let pages = if rng.chance(20) {
                        size - first
                    } else {
                        1 + rng.below((size - first).min(6))
                    };
                    let write = rng.chance(60);
                    let access = if write {
                        Access::ReadWrite
                    } else {
                        Access::Read
                    };
                    let job = r.doms[&d].job;
                    let device = r.doms[&d].device;
                    let pfns: Vec<u64> = (first..first + pages)
                        .map(|p| w.vmos.frame(v, p).unwrap())
                        .collect();
                    let mut m = RMap {
                        d,
                        device,
                        job,
                        page: 0,
                        pages,
                        vmo: v,
                        vmo_page: first,
                        pfns,
                        write,
                        visible: false,
                    };
                    let io_before = w.jobs.io_usage(job, IoResource::MappedPages).unwrap();
                    let fail = rng.chance(15).then(|| rng.below(3));
                    match w.map(d, v, first, pages, access, fail) {
                        MapStart::Wait(mapped) => {
                            hit(10);
                            m.page = mapped.iova >> 12;
                            assert!(
                                mapped.iova >= IO_BASE + IO_VECTOR_PAGES * PAGE
                                    && mapped.iova + pages * PAGE
                                        <= IO_BASE + IO_WINDOW_PAGES * PAGE,
                                "seed {seed}: the address is in the DMA part of the window"
                            );
                            assert!(
                                !r.live.values().any(|q| q.d == d
                                    && q.page < m.page + pages
                                    && m.page < q.page + q.pages),
                                "seed {seed}: a fresh range overlaps a live one"
                            );
                            for (_, q) in r.pending.iter().filter(|(_, q)| q.d == d) {
                                assert!(
                                    q.page + q.pages <= m.page || m.page + pages <= q.page,
                                    "seed {seed}: a pending range was handed out again"
                                );
                            }
                            for q in r.leaked.iter().filter(|q| q.d == d) {
                                assert!(
                                    q.page + q.pages <= m.page || m.page + pages <= q.page,
                                    "seed {seed}: a leaked range was handed out again"
                                );
                            }
                            r.touched
                                .entry(device)
                                .or_default()
                                .extend(m.page..m.page + pages);
                            r.ops.push(Op::Map(d, m.page, mapped.ticket));
                            r.live.insert((d, m.page), m);
                        }
                        MapStart::RolledBack(p) => {
                            hit(7);
                            m.page = p.range().1 >> 12;
                            assert_eq!(p.range().0, d);
                            r.touched
                                .entry(device)
                                .or_default()
                                .extend(m.page..m.page + pages);
                            r.pending.push((p, m));
                            r.ops.push(Op::Unmap(p));
                        }
                        MapStart::Refused(e) => {
                            hit(8);
                            assert_eq!(e, IoError::NoMemory, "seed {seed}: map refused");
                            assert_eq!(
                                w.jobs.io_usage(job, IoResource::MappedPages).unwrap(),
                                io_before,
                                "seed {seed}: a refused map charges nothing"
                            );
                        }
                    }
                }
                // unmap a mapping the caller was given
                4 => {
                    let vis: Vec<(DomainId, u64)> = r
                        .live
                        .iter()
                        .filter(|(_, m)| m.visible && !r.doms[&m.d].detaching)
                        .map(|(k, _)| *k)
                        .collect();
                    if vis.is_empty() {
                        continue;
                    }
                    let k = vis[rng.below(vis.len() as u64) as usize];
                    let m = r.live[&k].clone();
                    // a wrong length is refused and changes nothing
                    assert_eq!(
                        w.unmap(k.0, k.1 << 12, m.pages + 1),
                        Err(IoError::InvalidArgs)
                    );
                    let p = w.unmap(k.0, k.1 << 12, m.pages).unwrap();
                    assert_eq!(
                        w.unmap(k.0, k.1 << 12, m.pages),
                        Err(IoError::InvalidArgs),
                        "unmapped twice"
                    );
                    r.live.remove(&k);
                    r.pending.push((p, m));
                    r.ops.push(Op::Unmap(p));
                }
                // the device runs its queue (often), or stalls and resumes
                5..=7 => {
                    if !stalled {
                        w.sim.process_commands();
                    }
                    if rng.chance(8) {
                        stalled = !stalled;
                    }
                }
                // a waiter polls: done, or (with the device stalled) its bound passes
                8 | 9 => {
                    if r.ops.is_empty() {
                        continue;
                    }
                    let i = rng.below(r.ops.len() as u64) as usize;
                    let t = op_ticket(&r.ops[i]);
                    match w.store.poll(&mut w.sim, t) {
                        Fence::Done => settle_op(&mut w, &mut r, i, false, &mut rng),
                        Fence::Failed => settle_op(&mut w, &mut r, i, true, &mut rng),
                        Fence::Unqueued => {
                            // the queue was full: queue again, or (the device stalled) let the bound pass
                            if requeue_op(&mut w, &mut r.ops[i]) == Fence::Unqueued
                                && stalled
                                && rng.chance(30)
                            {
                                settle_op(&mut w, &mut r, i, true, &mut rng);
                            }
                        }
                        Fence::Waiting => {
                            // a step that runs too early is refused and changes nothing
                            match &r.ops[i] {
                                Op::Unmap(p) => {
                                    // (a record the domain's destruction took is not there: nothing to hand out)
                                    let there = r.pending.iter().any(|(q, _)| q == p);
                                    match w.store.complete(p) {
                                        Err(IoError::NotDone) => assert!(there),
                                        Ok(None) => assert!(!there),
                                        other => panic!(
                                            "an early complete: {:?}",
                                            other.map(|x| x.is_some())
                                        ),
                                    }
                                }
                                Op::Destroy(x) => assert_eq!(
                                    w.store.destroy_complete(w.mem, x).err(),
                                    Some(IoError::NotDone)
                                ),
                                _ => {}
                            }
                            if stalled && rng.chance(30) {
                                settle_op(&mut w, &mut r, i, true, &mut rng);
                            }
                        }
                    }
                }
                // the supervisor destroys by call
                10 => {
                    let ds: Vec<DomainId> = r
                        .doms
                        .iter()
                        .filter(|(_, d)| d.ready && !d.detaching && d.handles[0] > 0)
                        .map(|(&d, _)| d)
                        .collect();
                    if let Some(&d) = ds.get(rng.below(ds.len().max(1) as u64) as usize) {
                        start_destroy(&mut w, &mut r, d);
                        assert!(
                            w.destroy(d).is_none(),
                            "destroying a destroyed domain does nothing"
                        );
                    }
                }
                // a VMO handle is closed
                11 => {
                    if r.vmo_handles.is_empty() {
                        continue;
                    }
                    let (_, v) = r
                        .vmo_handles
                        .remove(rng.below(r.vmo_handles.len() as u64) as usize);
                    w.close_vmo(v);
                }
                // a domain handle is closed
                12 => {
                    let hs: Vec<(DomainId, usize)> = r
                        .doms
                        .iter()
                        .flat_map(|(&d, dom)| {
                            (0..2)
                                .filter(move |&o| dom.handles[o] > 0)
                                .map(move |o| (d, o))
                        })
                        .collect();
                    if let Some(&(d, o)) = hs.get(rng.below(hs.len().max(1) as u64) as usize) {
                        close_domain_handle(&mut w, &mut r, d, o);
                    }
                }
                // an owner dies: every handle it holds is closed
                _ => {
                    let owner = rng.below(2) as usize;
                    let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut r.vmo_handles)
                        .into_iter()
                        .partition(|(o, _)| *o == owner);
                    r.vmo_handles = keep;
                    for (_, v) in gone {
                        w.close_vmo(v);
                    }
                    let ds: Vec<DomainId> = r.doms.keys().copied().collect();
                    for d in ds {
                        while r.doms.get(&d).is_some_and(|dom| dom.handles[owner] > 0) {
                            close_domain_handle(&mut w, &mut r, d, owner);
                        }
                    }
                }
            }
            check_reach(&mut w, &r, seed, step);
            check_counts(&w, &r, seed, step);
        }
        // the end: the device runs again, every operation finishes, every domain goes, every handle closes
        w.sim.process_commands();
        while !r.ops.is_empty() {
            let f = finish_poll(&mut w, &mut r.ops[0]);
            settle_op(&mut w, &mut r, 0, f != Fence::Done, &mut rng);
            w.sim.process_commands();
        }
        let ds: Vec<DomainId> = r.doms.keys().copied().collect();
        for d in ds {
            start_destroy(&mut w, &mut r, d);
        }
        w.sim.process_commands();
        while !r.ops.is_empty() {
            let f = finish_poll(&mut w, &mut r.ops[0]);
            settle_op(&mut w, &mut r, 0, f != Fence::Done, &mut rng);
            w.sim.process_commands();
        }
        for (_, v) in std::mem::take(&mut r.vmo_handles) {
            w.close_vmo(v);
        }
        check_reach(&mut w, &r, seed, steps);
        check_counts(&w, &r, seed, steps);
        let s = w.store.stats();
        if !r.abandoned {
            assert_eq!(
                w.mm.db.free_count(),
                base_free,
                "seed {seed}: M3: every frame is back"
            );
            assert_eq!(
                (s.holds_leaked, s.tables_leaked, s.fence_timeouts),
                (0, 0, 0)
            );
            for &job in &r.jobs {
                assert_eq!(w.jobs.usage(job).unwrap().get(Resource::KernelBytes), 0);
                assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
            }
            assert_eq!(w.vmos.count(), 0);
            // J2: every domain gave its job's use back: the creator's is the last
            for &job in &r.jobs {
                w.jobs.release(job);
                assert!(
                    !w.jobs.exists(job),
                    "seed {seed}: a domain kept its job's use"
                );
            }
        } else {
            // IO8: what is missing is exactly what was leaked, and it is counted
            assert!(s.fence_timeouts > 0);
            let leaked_vmo_pages: u64 = r
                .vmo_pages
                .iter()
                .filter(|(v, _)| r.holders(**v) > 0)
                .map(|(_, p)| p)
                .sum();
            assert_eq!(
                (base_free - w.mm.db.free_count()) as u64,
                leaked_vmo_pages + w.counts.tables + 4 * w.counts.roots,
                "seed {seed}: the frames missing are the leaked ones"
            );
            assert_eq!(
                s.holds_leaked,
                r.leaked.len() as u64,
                "seed {seed}: leaked holds counted"
            );
            assert_eq!(
                s.tables_leaked,
                w.counts.tables + 4 * w.counts.roots,
                "seed {seed}: leaked table frames, roots and MSI tables counted exactly"
            );
            // J2: a job whose charges were leaked keeps its record (its usage stays); every other one goes with its creator's use
            for &job in &r.jobs {
                let leaks = r.leaked.iter().any(|m| m.job == job)
                    || r.leaked_doms.iter().any(|(j, _)| *j == job);
                if !leaks {
                    assert_eq!(w.jobs.usage(job).unwrap().get(Resource::KernelBytes), 0);
                }
                w.jobs.release(job);
                assert_eq!(
                    w.jobs.exists(job),
                    leaks,
                    "seed {seed}: job {job:?} kept iff it has leaked charges"
                );
            }
        }
    }
    for (i, h) in HITS.iter().enumerate() {
        let n = h.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("{:>34}: {n}", HIT_NAMES[i]);
        assert!(n > 0, "the random test never reached: {}", HIT_NAMES[i]);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// mappings and faults, one by one

#[test]
fn a_domain_takes_far_more_than_64_mappings_and_returns_every_record() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let base = w.mm.db.free_count();
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let bufs = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 3, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 8, bufs)
        .unwrap();
    let mut maps = vec![];
    for i in 0..300u64 {
        let iova = map_ok(&mut w, d, v, i % 8, 1, Access::ReadWrite);
        maps.push(iova);
        assert_eq!(
            w.sim.read(3, iova).unwrap() >> 12,
            w.vmos.frame(v, i % 8).unwrap()
        );
    }
    assert_eq!(w.store.counts(d), Some((300, 0, 0)));
    assert_eq!(w.vmos.refs(v), Some(301), "M8: one hold per mapping");
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(300));
    assert_eq!(
        kernel_bytes(&w, job),
        DOMAIN_BYTES + 300 * MAPPING_BYTES + w.store.usage(d).unwrap().0 * PAGE
    );
    // half unmapped one by one, in a scrambled order: every record comes back
    let mut rng = Rng::new(7);
    for _ in 0..150 {
        let iova = maps.swap_remove(rng.below(maps.len() as u64) as usize);
        assert_eq!(unmap_ok(&mut w, d, iova, 1), (v, 1));
        assert!(w.sim.read(3, iova).is_err(), "an unmapped page faults");
    }
    assert_eq!(w.store.counts(d), Some((150, 0, 0)));
    // the rest with the domain
    let got = destroy_ok(&mut w, d);
    assert_eq!(
        got,
        vec![(v, 1); 150],
        "the destruction returns every record"
    );
    assert_eq!(w.vmos.refs(v), Some(1));
    w.close_vmo(v);
    assert_eq!(w.mm.db.free_count(), base, "M3");
    assert_eq!(kernel_bytes(&w, job), 0);
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(0));
    assert_eq!(kernel_bytes(&w, bufs), 0);
}

#[test]
fn the_window_bounds_a_domain_and_a_pending_range_is_not_reused_before_its_fence() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let base = w.mm.db.free_count();
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 0, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 64, job)
        .unwrap();
    let n = IO_DMA_PAGES / 64;
    assert_eq!(n, 4092);
    let mut first = 0;
    for i in 0..n {
        let iova = map_ok(&mut w, d, v, 0, 64, Access::Read);
        if i == 0 {
            first = iova;
        }
    }
    assert_eq!(
        w.store.usage(d).unwrap().1,
        IO_DMA_PAGES,
        "the whole DMA part of the window is mapped"
    );
    let io = w.jobs.io_usage(job, IoResource::MappedPages);
    let kb = kernel_bytes(&w, job);
    assert!(matches!(
        w.map(d, v, 0, 1, Access::Read, None),
        MapStart::Refused(IoError::NoMemory)
    ));
    assert_eq!(
        (
            w.jobs.io_usage(job, IoResource::MappedPages),
            kernel_bytes(&w, job)
        ),
        (io, kb),
        "refused: nothing charged"
    );
    assert_eq!(w.vmos.refs(v), Some(1 + n as u32), "refused: no hold left");
    // an unmap whose fence has not completed: the range is not handed out again
    let p = w.unmap(d, first, 64).unwrap();
    assert!(matches!(
        w.map(d, v, 0, 64, Access::Read, None),
        MapStart::Refused(IoError::NoMemory)
    ));
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Done);
    let rel = w.store.complete(&p).unwrap().unwrap();
    w.settle(rel);
    assert_eq!(
        map_ok(&mut w, d, v, 0, 64, Access::Read),
        first,
        "after its fence it is free again"
    );
    assert_eq!(destroy_ok(&mut w, d).len() as u64, n);
    w.close_vmo(v);
    assert_eq!(w.mm.db.free_count(), base, "M3");
    assert_eq!(kernel_bytes(&w, job), 0);
}

#[test]
fn the_io_page_limit_of_an_ancestor_binds_a_domain_job_and_a_refusal_charges_nothing() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let parent = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    w.jobs.set_io_limits(parent, 100, u64::MAX).unwrap();
    let job = w.jobs.create(parent, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 2, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 64, parent)
        .unwrap();
    map_ok(&mut w, d, v, 0, 64, Access::ReadWrite);
    let kb = (kernel_bytes(&w, job), kernel_bytes(&w, parent));
    assert!(matches!(
        w.map(d, v, 0, 64, Access::ReadWrite, None),
        MapStart::Refused(IoError::NoMemory)
    ));
    assert_eq!(w.jobs.io_usage(job, IoResource::MappedPages), Some(64));
    assert_eq!(w.jobs.io_usage(parent, IoResource::MappedPages), Some(64));
    assert_eq!(
        (kernel_bytes(&w, job), kernel_bytes(&w, parent)),
        kb,
        "nothing charged on any job of the chain"
    );
    assert_eq!(w.vmos.refs(v), Some(2), "no hold left");
    map_ok(&mut w, d, v, 0, 36, Access::ReadWrite);
    assert_eq!(
        w.jobs.io_usage(parent, IoResource::MappedPages),
        Some(100),
        "exactly the limit"
    );
    assert_eq!(destroy_ok(&mut w, d).len(), 2);
    assert_eq!(
        w.jobs.io_usage(parent, IoResource::MappedPages),
        Some(0),
        "J2: credited once"
    );
    w.close_vmo(v);
}

#[test]
fn a_second_domain_for_a_live_device_is_refused_and_the_context_follows_the_domain() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    assert_eq!(w.store.device_state(w.mem, 5), DeviceState::Unclaimed);
    let d = ready(&mut w, 5, job);
    assert_eq!(w.store.device_state(w.mem, 5), DeviceState::Attached);
    let kb = kernel_bytes(&w, job);
    assert_eq!(
        w.create(5, job).unwrap_err(),
        IoError::BadState,
        "IO5: one live domain per device"
    );
    assert_eq!(
        kernel_bytes(&w, job),
        kb,
        "a refused create charges nothing"
    );
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 2, job)
        .unwrap();
    let iova = map_ok(&mut w, d, v, 0, 2, Access::Read);
    assert!(
        w.sim.write(5, iova, 1).is_err(),
        "IO4: a read-only mapping refuses a write"
    );
    // while the destruction waits for its fence the domain refuses maps and unmaps, and a second domain
    let x = w.destroy(d).unwrap();
    assert!(matches!(
        w.map(d, v, 0, 1, Access::Read, None),
        MapStart::Refused(IoError::BadState)
    ));
    assert_eq!(w.unmap(d, iova, 2).unwrap_err(), IoError::BadState);
    assert_eq!(w.create(5, job).unwrap_err(), IoError::BadState);
    assert_eq!(
        w.store.device_state(w.mem, 5),
        DeviceState::Quarantined,
        "the context is quarantined at once"
    );
    w.sim.process_commands();
    assert!(
        w.sim.read(5, iova).is_err(),
        "after the fence the device reaches nothing"
    );
    assert_eq!(w.store.poll(&mut w.sim, x.ticket()), Fence::Done);
    let mut det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    while let Some(r) = det.next_record() {
        w.settle(r);
    }
    w.finish(det);
    assert_eq!(
        w.store.device_state(w.mem, 5),
        DeviceState::Quarantined,
        "6.3: the device stays quarantined"
    );
    assert!(w.sim.read(5, iova).is_err());
    // and can be claimed again
    let d2 = ready(&mut w, 5, job);
    assert_ne!(d2, d);
    assert_eq!(w.store.job_of(d), None, "the old id is dead");
    destroy_ok(&mut w, d2);
    w.close_vmo(v);
    assert_eq!(kernel_bytes(&w, job), 0);
}

#[test]
fn the_fault_ring_keeps_the_sixteen_oldest_and_counts_the_rest() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, true);
    let mut fq = FaultQueue::new(FQ_PA, 128).unwrap();
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    // a domain made and destroyed first: the next one reuses its slot with a new generation, which each fault must name
    let old = ready(&mut w, 2, job);
    destroy_ok(&mut w, old);
    let d = ready(&mut w, 1, job);
    assert_ne!(d, old);
    let at = |i: u64| IO_BASE + (IO_VECTOR_PAGES + 300 + i) * PAGE;
    for i in 0..20 {
        assert!(w.sim.read(1, at(i)).is_err());
    }
    let out = drain_faults(&mut w, &mut fq, 5);
    assert_eq!(out.len(), 20);
    assert!(
        out.iter().all(|o| o.domain == Some(d)),
        "each fault finds its domain"
    );
    assert_eq!(
        out.iter().filter(|o| o.print).count() as u32,
        FAULT_LINES_PER_SECOND,
        "7.2: at most 16 console lines a second"
    );
    assert!(out[..16].iter().all(|o| o.print));
    assert_eq!(w.store.stats().faults, 20);
    assert_eq!(w.store.stats().faults_unprinted, 4);
    // a second later the console speaks again
    assert!(w.sim.read(1, at(99)).is_err());
    let late = drain_faults(&mut w, &mut fq, 1005);
    assert!(late[0].print);
    // the sixteen oldest, in order; the first read carries the dropped count (four, and the late one) and resets it
    for i in 0..16 {
        let r = w.store.fault_read(d).unwrap().unwrap();
        assert_eq!(r.record.iotval, at(i), "the {i}th oldest");
        assert_eq!(r.record.device_id, 1);
        assert_eq!(
            r.dropped,
            if i == 0 { 5 } else { 0 },
            "the dropped count, reset by a read"
        );
        assert_eq!(r.more, i < 15);
    }
    assert!(
        w.store.fault_read(d).unwrap().is_none(),
        "SHOULD_WAIT when empty"
    );
    // a new fault after the read is stored, with nothing dropped
    assert!(w.sim.read(1, at(50)).is_err());
    drain_faults(&mut w, &mut fq, 2000);
    let r = w.store.fault_read(d).unwrap().unwrap();
    assert_eq!((r.record.iotval, r.dropped), (at(50), 0));
    // a full ring, a read, one more fault: the dropped count says only what was lost since that read
    for i in 0..18 {
        assert!(w.sim.read(1, at(i)).is_err());
    }
    drain_faults(&mut w, &mut fq, 3000);
    assert_eq!(w.store.fault_read(d).unwrap().unwrap().dropped, 2);
    assert!(w.sim.read(1, at(60)).is_err());
    drain_faults(&mut w, &mut fq, 3000);
    let mut last = None;
    let mut dropped = 0;
    while let Some(r) = w.store.fault_read(d).unwrap() {
        dropped += r.dropped;
        last = Some(r.record.iotval);
    }
    assert_eq!(
        (last, dropped),
        (Some(at(60)), 0),
        "the ring had room for the new one"
    );
    // a detaching domain's and a quarantined device's faults reach no ring
    let x = w.destroy(d).unwrap();
    let rec = FaultRecord {
        cause: 1,
        ttype: TransactionType::UntranslatedRead,
        device_id: 1,
        pv: false,
        pid: 0,
        privileged: false,
        iotval: at(1),
        iotval2: 0,
    };
    assert_eq!(w.store.fault(rec, 4000, 1000).domain, None, "detaching");
    assert!(w.store.fault_read(d).unwrap().is_none());
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, x.ticket()), Fence::Done);
    let det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    w.finish(det);
    assert_eq!(w.store.device_state(w.mem, 1), DeviceState::Quarantined);
    assert!(w.sim.read(1, at(2)).is_err());
    for o in drain_faults(&mut w, &mut fq, 5000) {
        assert_eq!(o.domain, None, "quarantined");
    }
    assert_eq!(w.store.fault(rec, 5000, 1000).domain, None, "quarantined");
    assert_eq!(
        w.store.fault_read(d).unwrap_err(),
        IoError::BadState,
        "the domain is gone"
    );
    // a fault for a device no domain ever had is counted and reaches no ring
    let stranger = FaultRecord {
        device_id: 40,
        ..rec
    };
    assert_eq!(w.store.fault(stranger, 5000, 1000).domain, None);
    assert_eq!(w.store.stats().last_fault, Some((40, at(1), 1)));
}

// ---------------------------------------------------------------------------------------------------------------------------
// the command queue: the invalidation's form, a full queue, a queue error

/// The commands the simulated IOMMU ran since `from`.
fn ran_since(w: &World, from: usize) -> Vec<iommucore::cmdq::Command> {
    w.sim.executed()[from..].to_vec()
}

#[test]
fn an_unmap_that_empties_a_table_invalidates_the_whole_soft_context() {
    use iommucore::cmdq::Command;
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let g = w.store.gscid_of(d).unwrap();
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 2, job)
        .unwrap();
    let a = map_ok(&mut w, d, v, 0, 1, Access::Read);
    let b = map_ok(&mut w, d, v, 1, 1, Access::Read);
    assert_eq!(a >> 21, b >> 21, "the two pages share a leaf table");
    // the first unmap leaves the table in use: the page alone is invalidated
    let at = w.sim.executed().len();
    unmap_ok(&mut w, d, a, 1);
    let ran = ran_since(&w, at);
    assert!(ran.contains(&Command::iotinval_gvma(g, Some(a))));
    assert!(!ran.contains(&Command::iotinval_gvma(g, None)));
    // the second empties the leaf table and the one above: the non-leaf entries cached for them go with the whole soft context
    let at = w.sim.executed().len();
    unmap_ok(&mut w, d, b, 1);
    let ran = ran_since(&w, at);
    assert!(
        ran.contains(&Command::iotinval_gvma(g, None)),
        "IO3 for table frames: a page-wise invalidation need not drop the page-walk cache ({ran:x?})"
    );
    assert_eq!(w.store.usage(d).unwrap().0, 0, "the tables were freed");
    destroy_ok(&mut w, d);
    w.close_vmo(v);
}

/// Fill the stalled queue with maps of `v` until one finds no room: the maps that were queued, and the one that was not.
fn fill_queue(w: &mut World, d: DomainId, v: u64) -> (Vec<Mapped>, Mapped) {
    let mut queued = vec![];
    for _ in 0..1000 {
        match w.map(d, v, 0, 1, Access::Read, None) {
            MapStart::Wait(m) => {
                if w.store.poll(&mut w.sim, m.ticket) == Fence::Unqueued {
                    return (queued, m);
                }
                queued.push(m);
            }
            _ => panic!("map refused"),
        }
    }
    panic!("the queue never filled");
}

#[test]
fn a_full_queue_is_queued_again_not_failed() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 1, job)
        .unwrap();

    // a map
    let (queued, mut m) = fill_queue(&mut w, d, v);
    let stuck = w.store.stats().queue_stuck;
    assert!(stuck > 0);
    assert_eq!(
        w.store.stats().fence_timeouts,
        0,
        "a full queue is not a timeout"
    );
    assert_eq!(
        w.store.requeue_map(w.mem, &mut w.sim, d, &mut m),
        Fence::Unqueued,
        "still full"
    );
    w.sim.process_commands();
    assert_eq!(
        w.store.requeue_map(w.mem, &mut w.sim, d, &mut m),
        Fence::Waiting
    );
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, m.ticket), Fence::Done);
    for q in &queued {
        assert_eq!(w.store.poll(&mut w.sim, q.ticket), Fence::Done);
    }
    let mut maps: Vec<u64> = queued.iter().map(|q| q.iova).collect();
    maps.push(m.iova);

    // an unmap
    let (more, mut last) = fill_queue(&mut w, d, v);
    let iova = maps.pop().unwrap();
    let mut p = w.unmap(d, iova, 1).unwrap();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Unqueued);
    assert_eq!(
        w.store.complete(&p).unwrap_err(),
        IoError::NotDone,
        "an unqueued fence is not done"
    );
    w.sim.process_commands();
    assert_eq!(
        w.store.requeue_unmap(w.mem, &mut w.sim, &mut p),
        Fence::Waiting
    );
    assert_eq!(
        w.store.requeue_map(w.mem, &mut w.sim, d, &mut last),
        Fence::Waiting
    );
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Done);
    let rel = w
        .store
        .complete(&p)
        .unwrap()
        .expect("the requeued record completes");
    w.settle(rel);
    maps.extend(more.iter().map(|q| q.iova));
    maps.push(last.iova);
    for iova in maps.drain(..) {
        unmap_ok(&mut w, d, iova, 1);
    }

    // a destruction, and a create
    let (rest, _) = fill_queue(&mut w, d, v);
    let mut x = w.destroy(d).unwrap();
    assert_eq!(w.store.poll(&mut w.sim, x.ticket()), Fence::Unqueued);
    let (d2, mut t2) = w.create(2, job).unwrap();
    assert_eq!(w.store.poll(&mut w.sim, t2), Fence::Unqueued);
    w.sim.process_commands();
    assert_eq!(
        w.store.requeue_destroy(w.mem, &mut w.sim, &mut x),
        Fence::Waiting
    );
    assert_eq!(
        w.store.requeue_create(w.mem, &mut w.sim, d2, &mut t2),
        Fence::Waiting
    );
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, t2), Fence::Done);
    assert_eq!(w.store.poll(&mut w.sim, x.ticket()), Fence::Done);
    let mut det = w.store.destroy_complete(w.mem, &x).unwrap().unwrap();
    let mut n = 0;
    while let Some(r) = det.next_record() {
        n += 1;
        w.settle(r);
    }
    w.finish(det);
    assert_eq!(
        n,
        rest.len() + 1,
        "the destruction returns the mappings, the unqueued one too"
    );
    destroy_ok(&mut w, d2);
    w.close_vmo(v);
    let s = w.store.stats();
    assert_eq!((s.fence_timeouts, s.holds_leaked), (0, 0));
    assert_eq!(kernel_bytes(&w, job), 0);
}

#[test]
fn a_queue_error_or_a_head_out_of_range_fails_the_fence() {
    use iommucore::Regs;
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 1, job)
        .unwrap();
    let iova = map_ok(&mut w, d, v, 0, 1, Access::Read);

    // a head out of range (an IOMMU that misbehaves): the fence fails
    struct BadHead<'a, 'm>(&'a mut iommucore::sim::Sim<'m, IoMem>);
    impl Regs for BadHead<'_, '_> {
        fn read32(&mut self, offset: usize) -> u32 {
            if offset == iommucore::regs::CQH {
                0xffff
            } else {
                self.0.read32(offset)
            }
        }
        fn write32(&mut self, offset: usize, value: u32) {
            self.0.write32(offset, value)
        }
        fn read64(&mut self, offset: usize) -> u64 {
            self.0.read64(offset)
        }
        fn write64(&mut self, offset: usize, value: u64) {
            self.0.write64(offset, value)
        }
    }
    let p = w.unmap(d, iova, 1).unwrap();
    assert_eq!(
        w.store.poll(&mut BadHead(&mut w.sim), p.ticket()),
        Fence::Failed
    );

    // a command the IOMMU refuses (the queue's entry overwritten): the queue stops with an error, and the fence fails
    let cqb = w.sim.read64(iommucore::regs::CQB);
    let base = ((cqb >> 10) & ((1 << 44) - 1)) << 12;
    let head = u64::from(w.sim.read32(iommucore::regs::CQH));
    mem.write(base + head * 16, 0x7f);
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Failed);
    assert!(w.store.abandon(&p), "the waiter abandons (6.4)");
    assert_eq!(w.store.stats().fence_timeouts, 1);
    assert_eq!(w.store.stats().holds_leaked, 1);
}

// ---------------------------------------------------------------------------------------------------------------------------
// identities: soft-context ids and domain ids

/// Eighty domains take eighty distinct GSCIDs (past the first bitmap word), a destroyed domain's GSCID is free again for the next
/// create, and its slot's next domain has a new id that the old one never finds.
#[test]
fn soft_context_ids_are_distinct_and_freed_and_domain_ids_are_not_reused() {
    let mem = IoMem::new();
    // (a one-level directory has 64 devices: three levels for 80)
    let mut w = World::with_levels(&mem, false, FRAMES, true, iommucore::ddt::Levels::Three);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let doms: Vec<DomainId> = (1..=80).map(|did| ready(&mut w, did, job)).collect();
    let gscids: Vec<u16> = doms.iter().map(|&d| w.store.gscid_of(d).unwrap()).collect();
    let set: BTreeSet<u16> = gscids.iter().copied().collect();
    assert_eq!(set.len(), 80, "one GSCID each: {gscids:?}");
    assert!(!set.contains(&0), "GSCID 0 is the quarantine's");
    for (i, &d) in doms.iter().enumerate() {
        assert_eq!(w.store.device_of(d), Some(i as u32 + 1));
        assert_eq!(w.store.domain_of(i as u32 + 1), Some(d));
    }
    // destroy the domain on GSCID 70 (the second bitmap word): the next create takes that GSCID and that slot
    let i = gscids
        .iter()
        .position(|&g| g == 70)
        .expect("GSCIDs are taken lowest first");
    let old = doms[i];
    destroy_ok(&mut w, old);
    assert_eq!(w.store.device_of(old), None);
    let new = ready(&mut w, 200, job);
    assert_eq!(
        w.store.gscid_of(new),
        Some(70),
        "the destroyed domain's GSCID is free again"
    );
    assert_ne!(new, old, "the slot's next domain has a new id");
    assert_eq!(
        (w.store.device_of(old), w.store.gscid_of(old)),
        (None, None)
    );
    assert_eq!(w.store.domain_of(200), Some(new));
    assert_eq!(
        w.store.stats().slots,
        80,
        "the destroyed domain's slot was reused"
    );
    // a create on a quarantined device also invalidates the quarantine's soft context (GSCID 0); one on a fresh device need not
    for did in [i as u32 + 1, 300] {
        let state = w.store.device_state(w.mem, did);
        let quarantined = state == DeviceState::Quarantined;
        let want = if did == 300 {
            DeviceState::Unclaimed
        } else {
            DeviceState::Quarantined
        };
        assert_eq!(state, want, "device {did}");
        let at = w.sim.executed().len();
        let again = ready(&mut w, did, job);
        let zero = iommucore::cmdq::Command::iotinval_gvma(0, None);
        assert_eq!(
            ran_since(&w, at).contains(&zero),
            quarantined,
            "device {did}"
        );
        destroy_ok(&mut w, again);
    }
    for d in doms.into_iter().filter(|&d| d != old).chain([new]) {
        destroy_ok(&mut w, d);
    }
    assert_eq!(w.store.stats().domains, 0);
}

/// A map whose table allocation fails part way, after it wrote leaves for the pages before a leaf-table boundary: the roll-back
/// clears exactly those leaves, the device reaches none of the range, every table frame comes back, and the range is free again once
/// its fence completes. The command counter counts every command the IOMMU ran.
#[test]
fn a_map_rolled_back_after_writing_leaves_clears_them() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 600, job)
        .unwrap();
    // a filler that ends two pages before a leaf table's boundary (a leaf table maps 512 pages)
    let a = map_ok(&mut w, d, v, 0, 1, Access::Read);
    let to_edge = 512 - ((a >> 12) % 512);
    assert!(to_edge > 3, "the window starts away from a boundary");
    let filler = map_ok(&mut w, d, v, 1, to_edge - 3, Access::Read);
    assert_eq!(filler, a + PAGE, "ranges are handed out in order");
    let start = filler + (to_edge - 3) * PAGE;
    assert_eq!((start >> 12) % 512, 510, "two pages before the boundary");
    let tables = w.counts.tables;
    // four pages: two in the filler's leaf table, two needing a new one, whose allocation fails
    let p = match w.map(d, v, 100, 4, Access::ReadWrite, Some(0)) {
        MapStart::RolledBack(p) => p,
        _ => panic!("the map is rolled back"),
    };
    assert_eq!(p.range(), (d, start));
    for i in 0..4 {
        assert!(
            w.sim.read(1, start + i * PAGE).is_err(),
            "page {i} of the rolled-back range"
        );
    }
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, p.ticket()), Fence::Done);
    let rel = w
        .store
        .complete(&p)
        .unwrap()
        .expect("the record comes back");
    assert_eq!((rel.vmo(), rel.pages()), (v, 4));
    w.settle(rel);
    assert_eq!(w.counts.tables, tables, "no table frame kept");
    // the range is free again: the same four pages map, and the device reaches them
    let again = map_ok(&mut w, d, v, 100, 4, Access::ReadWrite);
    assert_eq!(again, start);
    for i in 0..4 {
        assert!(w.sim.read(1, start + i * PAGE).is_ok());
    }
    for i in 1..=(to_edge - 3) {
        assert!(
            w.sim.read(1, a + i * PAGE).is_ok(),
            "the filler stays mapped"
        );
    }
    assert_eq!(
        w.store.stats().commands,
        w.sim.executed().len() as u64,
        "every command counted"
    );
    unmap_ok(&mut w, d, again, 4);
    unmap_ok(&mut w, d, filler, to_edge - 3);
    unmap_ok(&mut w, d, a, 1);
    destroy_ok(&mut w, d);
}

/// A record of no pages, or one whose frame list does not match its length, is refused with nothing changed.
#[test]
fn a_map_of_no_pages_or_the_wrong_frame_count_is_refused() {
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 2, job)
        .unwrap();
    let pfns = [w.vmos.frame(v, 0).unwrap(), w.vmos.frame(v, 1).unwrap()];
    let tables = w.counts.tables;
    for (pages, given) in [(0u64, 0usize), (2, 1), (1, 2)] {
        let rec = Record::new(v, 0, pages, Access::Read).unwrap();
        let World {
            mem,
            sim,
            store,
            mm,
            counts,
            ..
        } = &mut w;
        let mut f = DbFrames {
            db: &mut mm.db,
            mem,
            counts,
            fail: None,
        };
        match store.map(*mem, sim, &mut f, d, rec, &pfns[..given]) {
            Err(MapFailed::Refused(e, rec)) => {
                assert_eq!(e, IoError::InvalidArgs, "{pages} pages, {given} frames");
                assert_eq!(rec.pages(), pages);
            }
            _ => panic!("{pages} pages with {given} frames is refused"),
        }
        assert_eq!(w.store.counts(d), Some((0, 0, 0)));
        assert_eq!(w.counts.tables, tables);
    }
    destroy_ok(&mut w, d);
}

/// The queue is called full only when a map's commands and its fence do not fit: with an odd number of entries outstanding first (a
/// create on a quarantined device: two commands and a fence), the maps (an invalidation and a fence each) fill it to the last entry.
#[test]
fn the_queue_is_full_only_when_the_commands_and_fence_do_not_fit() {
    use iommucore::Regs;
    let mem = IoMem::new();
    let mut w = World::new(&mem, false);
    let job = w.jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let d = ready(&mut w, 1, job);
    let q = ready(&mut w, 2, job);
    destroy_ok(&mut w, q);
    let v = w
        .vmos
        .create_dma_in(&mut w.mm, &mut w.jobs, 1, job)
        .unwrap();
    let entries = 1u32 << ((w.sim.read64(iommucore::regs::CQB) & 0x1f) + 1);
    let used = |w: &mut World| {
        let (head, tail) = (
            w.sim.read32(iommucore::regs::CQH),
            w.sim.read32(iommucore::regs::CQT),
        );
        (tail + entries - head) % entries
    };
    assert_eq!(used(&mut w), 0);
    let (q, t) = w.create(2, job).unwrap();
    assert_eq!(used(&mut w), 3, "a create on a quarantined device");
    let (queued, m) = fill_queue(&mut w, d, v);
    assert_eq!(
        used(&mut w),
        3 + 2 * queued.len() as u32,
        "two entries a map"
    );
    assert_eq!(
        used(&mut w),
        entries - 1,
        "every entry used before the queue is called full"
    );
    // drain: every fence completes, and the unqueued map goes in after
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, t), Fence::Done);
    for x in &queued {
        assert_eq!(w.store.poll(&mut w.sim, x.ticket), Fence::Done);
    }
    let mut m = m;
    assert_eq!(
        w.store.requeue_map(w.mem, &mut w.sim, d, &mut m),
        Fence::Waiting
    );
    w.sim.process_commands();
    assert_eq!(w.store.poll(&mut w.sim, m.ticket), Fence::Done);
    for x in queued.iter().chain([&m]) {
        unmap_ok(&mut w, d, x.iova, 1);
    }
    destroy_ok(&mut w, q);
    destroy_ok(&mut w, d);
    w.close_vmo(v);
}
