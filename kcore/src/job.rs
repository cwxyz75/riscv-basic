//! Jobs and resource limits (S4.5, decisions OD56-OD58): a tree of accounts. A job has limits on
//! six resources (live processes, live threads, committed memory pages, live child jobs, live
//! kernel objects, bytes queued in channels, sockets and ports); a charge against a
//! job is applied to the job **and every ancestor**, so a child can never use more than its
//! parent allows however its own limits are set.
//!
//! This is only the ledger: it knows nothing about processes or pages. The kernel charges it
//! when it creates a process or thread or commits a VMO page, and credits it back when they go.
//!
//! Every holder of a job id (a job object, a process, a VMO, a child job) holds one *use* of
//! it; a record is freed when its last use is released, and releasing a child's last use
//! releases the use it held on its parent. A charge is always held by something that holds a
//! use, so a record with usage is never freed (`debug_assert`ed, and tested).
#![allow(clippy::len_without_is_empty)]

use alloc::vec::Vec;

/// Most jobs alive at once (the records live in kernel memory).
pub const MAX_JOBS: usize = 256;

/// "No limit" for any resource.
pub const UNLIMITED: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(u32);

impl JobId {
    /// The root job: unlimited, ancestor of every job, never freed.
    pub const ROOT: JobId = JobId(0);

    pub fn index(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Processes,
    Threads,
    Pages,
    /// Jobs below this one (children, grandchildren, ...).
    Jobs,
    /// Kernel objects (channels, events, sockets, ports, VMOs, processes, threads, jobs) alive that
    /// were created by processes of this job.
    Objects,
    /// Bytes queued in the channels, sockets and ports created by this job's processes.
    IpcBytes,
    /// Kernel memory the job causes beyond pages it maps (MM5, KERNEL-V1-SPEC 4.7): stacks, page tables, object bodies, handle tables,
    /// register state, queued data, in bytes. Its limit is not set separately: it is an eighth of the page limit in bytes.
    KernelBytes,
}

/// The two device resources of IOMMU-SPEC 5.3 (OD166 point 4), kept apart from `Resource` and `Limits` because they are set by a call of
/// their own (`job_set_io_limits`) and have no default: a job on which none is set, and none of whose ancestors has one, is unlimited (as
/// the region limit). Usage is counted on the job and every ancestor (J1), and the smallest limit along the ancestors applies, so a child
/// inherits its parent's cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoResource {
    /// Pages mapped into device domains whose job this is (one per page per mapping).
    MappedPages,
    /// Live interrupt vectors of device domains whose job this is (charged by IOMMU7).
    Vectors,
}

const IO_RESOURCES: usize = 2;

impl IoResource {
    const fn index(self) -> usize {
        match self {
            IoResource::MappedPages => 0,
            IoResource::Vectors => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub processes: u64,
    pub threads: u64,
    pub pages: u64,
    pub jobs: u64,
    pub objects: u64,
    pub ipc_bytes: u64,
}

impl Limits {
    pub const UNLIMITED: Limits = Limits {
        processes: UNLIMITED,
        threads: UNLIMITED,
        pages: UNLIMITED,
        jobs: UNLIMITED,
        objects: UNLIMITED,
        ipc_bytes: UNLIMITED,
    };

    fn get(&self, r: Resource) -> u64 {
        match r {
            Resource::Processes => self.processes,
            Resource::Threads => self.threads,
            Resource::Pages => self.pages,
            Resource::Jobs => self.jobs,
            Resource::Objects => self.objects,
            Resource::IpcBytes => self.ipc_bytes,
            // an eighth of the pages the job may hold, in bytes (no limit on pages, no limit here)
            Resource::KernelBytes => match self.pages {
                UNLIMITED => UNLIMITED,
                p => p.saturating_mul(PAGE_BYTES / KERNEL_BYTES_FRACTION),
            },
        }
    }

    /// The limit that applies to `r`: the stored one, or for `KernelBytes` the one derived from the page limit.
    pub fn effective(&self, r: Resource) -> u64 {
        self.get(r)
    }
}

/// Bytes in a page, and the fraction of a job's page limit its kernel-memory limit is (one eighth).
const PAGE_BYTES: u64 = 4096;
const KERNEL_BYTES_FRACTION: u64 = 8;

/// What a job (and everything below it) uses now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub processes: u64,
    pub threads: u64,
    pub pages: u64,
    pub jobs: u64,
    pub objects: u64,
    pub ipc_bytes: u64,
    pub kernel_bytes: u64,
}

impl Usage {
    fn slot(&mut self, r: Resource) -> &mut u64 {
        match r {
            Resource::Processes => &mut self.processes,
            Resource::Threads => &mut self.threads,
            Resource::Pages => &mut self.pages,
            Resource::Jobs => &mut self.jobs,
            Resource::Objects => &mut self.objects,
            Resource::IpcBytes => &mut self.ipc_bytes,
            Resource::KernelBytes => &mut self.kernel_bytes,
        }
    }

    pub fn get(&self, r: Resource) -> u64 {
        match r {
            Resource::Processes => self.processes,
            Resource::Threads => self.threads,
            Resource::Pages => self.pages,
            Resource::Jobs => self.jobs,
            Resource::Objects => self.objects,
            Resource::IpcBytes => self.ipc_bytes,
            Resource::KernelBytes => self.kernel_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobError {
    /// The id names no live job.
    NoSuchJob,
    /// A charge would take the job or an ancestor past its limit.
    Exceeded,
    /// `MAX_JOBS` jobs exist.
    TooManyJobs,
}

/// What a job may do with priority (SCHED-SMP-SPEC 4.9). Invariants (PR1, PR2): a job's values never exceed its
/// parent's, so a thread's base priority (bounded by its job's `max_prio`) is bounded by every ancestor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedLimits {
    /// The highest base priority a thread of this job may have (0 to 63; above 31 is the real-time band).
    pub max_prio: u8,
    /// Real-time CPU nanoseconds per 100 ms window (0: none). Enforced (SCHED-SMP-SPEC 4.5).
    pub rt_budget: u64,
    /// Reserved for the fair class (1 to 65536); stored.
    pub cpu_weight: u32,
}

impl SchedLimits {
    /// The root job's: everything.
    pub const ROOT: SchedLimits = SchedLimits {
        max_prio: 63,
        rt_budget: 100_000_000,
        cpu_weight: 1024,
    };
}

/// Why `set_sched` refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedError {
    NoSuchJob,
    /// A value above the parent's (PR2).
    AboveParent,
    /// `max_prio` above 63 or `cpu_weight` outside 1 to 65536.
    Invalid,
}

struct Rec {
    parent: Option<JobId>,
    limits: Limits,
    sched: SchedLimits,
    /// Most regions one address space of this job's processes may hold (PL8: the per-job quota that replaces a constant; the effective value is the
    /// smallest along the ancestors; `UNLIMITED` is the hard backstop only).
    regions: u64,
    /// The job's own limits on the two device resources (IOMMU-SPEC 5.3), by `IoResource`: `UNLIMITED` unless set. Hierarchical like `regions`: the smallest
    /// along the ancestors applies, and a charge is checked against every one of them.
    io_limit: [u64; IO_RESOURCES],
    /// Usage of the two device resources by this job and everything below it (J1: counted on every ancestor).
    io_used: [u64; IO_RESOURCES],
    usage: Usage,
    /// Holders of this id (see the module comment).
    uses: u32,
}

pub struct Jobs {
    recs: Vec<Option<Rec>>,
    free: Vec<u32>,
    live: usize,
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs::new()
    }
}

impl Jobs {
    /// A ledger with only the root job (held by the kernel).
    pub fn new() -> Jobs {
        let mut recs = Vec::with_capacity(MAX_JOBS);
        recs.push(Some(Rec {
            parent: None,
            limits: Limits::UNLIMITED,
            sched: SchedLimits::ROOT,
            regions: UNLIMITED,
            io_limit: [UNLIMITED; IO_RESOURCES],
            io_used: [0; IO_RESOURCES],
            usage: Usage::default(),
            uses: 1,
        }));
        Jobs {
            recs,
            // (a release never allocates: the free list can hold every record there can be, KT9)
            free: Vec::with_capacity(MAX_JOBS),
            live: 1,
        }
    }

    fn rec(&self, id: JobId) -> Option<&Rec> {
        self.recs.get(id.0 as usize)?.as_ref()
    }

    fn rec_mut(&mut self, id: JobId) -> Option<&mut Rec> {
        self.recs.get_mut(id.0 as usize)?.as_mut()
    }

    /// How many jobs are alive (the root included).
    pub fn live(&self) -> usize {
        self.live
    }

    /// Call `f` for every live job (the root included), lowest slot first; allocates nothing.
    pub fn for_each_live(&self, mut f: impl FnMut(JobId)) {
        for i in 0..MAX_JOBS as u32 {
            if self.exists(JobId(i)) {
                f(JobId(i));
            }
        }
    }

    /// The ids of the live jobs (the root included), lowest slot first.
    pub fn live_ids(&self) -> Vec<JobId> {
        (0..MAX_JOBS as u32)
            .map(JobId)
            .filter(|&id| self.exists(id))
            .collect()
    }

    pub fn exists(&self, id: JobId) -> bool {
        self.rec(id).is_some()
    }

    /// A new child of `parent` with `limits`; the caller holds the one use of the new id (and
    /// the child holds one of the parent).
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing
    )] // INVARIANT: the parent was checked to exist just above
    pub fn create(&mut self, parent: JobId, limits: Limits) -> Result<JobId, JobError> {
        if !self.exists(parent) {
            return Err(JobError::NoSuchJob);
        }
        if self.live >= MAX_JOBS {
            return Err(JobError::TooManyJobs);
        }
        // a job below `parent` counts against the parent's limit on jobs, and every ancestor's
        self.charge(parent, Resource::Jobs, 1)?;
        let inherited = self.rec(parent).expect("checked").sched;
        let rec = Rec {
            parent: Some(parent),
            limits,
            sched: inherited,
            regions: UNLIMITED,
            io_limit: [UNLIMITED; IO_RESOURCES],
            io_used: [0; IO_RESOURCES],
            usage: Usage::default(),
            uses: 1,
        };
        let id = if let Some(i) = self.free.pop() {
            self.recs[i as usize] = Some(rec);
            JobId(i)
        } else {
            self.recs.push(Some(rec));
            JobId(self.recs.len() as u32 - 1)
        };
        self.live += 1;
        self.rec_mut(parent).expect("checked").uses += 1;
        Ok(id)
    }

    /// One more holder of `id`.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: uses is a count of live references, bounded by the object quota
    pub fn retain(&mut self, id: JobId) {
        if id == JobId::ROOT {
            return; // permanent: its use count is never touched
        }
        if let Some(r) = self.rec_mut(id) {
            r.uses += 1;
        }
    }

    /// One holder less; frees the record at the last one, and then releases the parent in turn.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: uses >= 1 for a reference being released (every release pairs with a retain)
    pub fn release(&mut self, id: JobId) {
        let mut cur = Some(id);
        while let Some(id) = cur {
            cur = None;
            if id == JobId::ROOT {
                return; // the kernel's own use is permanent
            }
            let Some(r) = self.rec_mut(id) else { return };
            debug_assert!(r.uses > 0);
            r.uses -= 1;
            if r.uses == 0 {
                debug_assert_eq!(r.usage, Usage::default(), "a freed job still has usage");
                debug_assert_eq!(
                    r.io_used, [0; IO_RESOURCES],
                    "a freed job still has device usage"
                );
                cur = r.parent;
                self.recs[id.0 as usize] = None;
                self.free.push(id.0);
                self.live -= 1;
                if let Some(parent) = cur {
                    self.credit(parent, Resource::Jobs, 1);
                }
            }
        }
    }

    pub fn parent(&self, id: JobId) -> Option<JobId> {
        self.rec(id)?.parent
    }

    /// Is `id` equal to `ancestor` or below it?
    pub fn is_within(&self, id: JobId, ancestor: JobId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.parent(c);
        }
        false
    }

    pub fn limits(&self, id: JobId) -> Option<Limits> {
        Some(self.rec(id)?.limits)
    }

    pub fn sched(&self, id: JobId) -> Option<SchedLimits> {
        Some(self.rec(id)?.sched)
    }

    /// The most regions one address space of a process of `id` may hold: the smallest limit set on the job and its ancestors (`UNLIMITED` if none).
    pub fn region_limit(&self, id: JobId) -> u64 {
        let mut cur = Some(id);
        let mut lim = UNLIMITED;
        while let Some(c) = cur {
            match self.rec(c) {
                Some(r) => lim = lim.min(r.regions),
                None => break,
            }
            cur = self.parent(c);
        }
        lim
    }

    /// Set the job's own limit on regions per address space (see `region_limit`).
    pub fn set_region_limit(&mut self, id: JobId, regions: u64) -> Result<(), JobError> {
        self.rec_mut(id).ok_or(JobError::NoSuchJob)?.regions = regions;
        Ok(())
    }

    /// Set a job's scheduling limits. Each value may not exceed the parent's (PR2); lowering one lowers it in every
    /// descendant too (PR1), so the caller must then clamp the base priority of the threads in the subtree (see
    /// `is_within`).
    #[allow(clippy::indexing_slicing)] // INVARIANT: the job index was bounds-checked by the lookup
    pub fn set_sched(&mut self, id: JobId, s: SchedLimits) -> Result<(), SchedError> {
        let parent = self.rec(id).ok_or(SchedError::NoSuchJob)?.parent;
        // (an RT budget is a share of a 100 ms window: more than the window is meaningless, and without this the
        // root, which has no parent to bound it, could be given anything)
        if s.max_prio > 63 || !(1..=65536).contains(&s.cpu_weight) || s.rt_budget > 100_000_000 {
            return Err(SchedError::Invalid);
        }
        if let Some(p) = parent.and_then(|p| self.rec(p)) {
            if s.max_prio > p.sched.max_prio || s.rt_budget > p.sched.rt_budget {
                return Err(SchedError::AboveParent);
            }
        }
        for i in 0..self.recs.len() as u32 {
            let within = self.is_within(JobId(i), id);
            if let Some(r) = self.recs[i as usize].as_mut().filter(|_| within) {
                if JobId(i) == id {
                    r.sched = s;
                } else {
                    r.sched.max_prio = r.sched.max_prio.min(s.max_prio);
                    r.sched.rt_budget = r.sched.rt_budget.min(s.rt_budget);
                }
            }
        }
        Ok(())
    }

    pub fn usage(&self, id: JobId) -> Option<Usage> {
        Some(self.rec(id)?.usage)
    }

    /// Change a job's limits. Usage above a new limit is not an error: later charges fail until
    /// usage is back under it.
    pub fn set_limits(&mut self, id: JobId, limits: Limits) -> Result<(), JobError> {
        self.rec_mut(id).ok_or(JobError::NoSuchJob)?.limits = limits;
        Ok(())
    }

    /// Charge `n` of `r` to `id` and every ancestor; all or nothing.
    #[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the record was checked to exist just above
    pub fn charge(&mut self, id: JobId, r: Resource, n: u64) -> Result<(), JobError> {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let rec = self.rec(c).ok_or(JobError::NoSuchJob)?;
            let limit = rec.limits.get(r);
            if limit != UNLIMITED && rec.usage.get(r).saturating_add(n) > limit {
                return Err(JobError::Exceeded);
            }
            cur = rec.parent;
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let rec = self.rec_mut(c).expect("checked above");
            *rec.usage.slot(r) += n;
            cur = rec.parent;
        }
        Ok(())
    }

    /// How much more of `r` a charge to `id` could take right now: the smallest room along the
    /// chain (`u64::MAX` if nothing limits it).
    pub fn room(&self, id: JobId, r: Resource) -> u64 {
        let mut room = UNLIMITED;
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(rec) = self.rec(c) else { return 0 };
            let limit = rec.limits.get(r);
            if limit != UNLIMITED {
                room = room.min(limit.saturating_sub(rec.usage.get(r)));
            }
            cur = rec.parent;
        }
        room
    }

    /// Set the job's own limits on I/O-mapped pages and on interrupt vectors (`job_set_io_limits`, IOMMU-SPEC 5.3; `UNLIMITED` for none).
    /// Usage above a new limit is not an error: later charges fail until usage is back under it.
    pub fn set_io_limits(
        &mut self,
        id: JobId,
        io_pages: u64,
        vectors: u64,
    ) -> Result<(), JobError> {
        self.rec_mut(id).ok_or(JobError::NoSuchJob)?.io_limit = [io_pages, vectors];
        Ok(())
    }

    /// The limit on `r` that applies to `id`: the smallest set on the job and its ancestors (`UNLIMITED` if none).
    #[allow(clippy::indexing_slicing)] // INVARIANT: IoResource::index() < IO_RESOURCES
    pub fn io_limit(&self, id: JobId, r: IoResource) -> u64 {
        let mut cur = Some(id);
        let mut lim = UNLIMITED;
        while let Some(c) = cur {
            let Some(rec) = self.rec(c) else { break };
            lim = lim.min(rec.io_limit[r.index()]);
            cur = rec.parent;
        }
        lim
    }

    /// The job's own limit on `r` as set (not inherited).
    #[allow(clippy::indexing_slicing)] // INVARIANT: IoResource::index() < IO_RESOURCES
    pub fn io_own_limit(&self, id: JobId, r: IoResource) -> Option<u64> {
        Some(self.rec(id)?.io_limit[r.index()])
    }

    /// What the job and everything below it use of `r`.
    #[allow(clippy::indexing_slicing)] // INVARIANT: IoResource::index() < IO_RESOURCES
    pub fn io_usage(&self, id: JobId, r: IoResource) -> Option<u64> {
        Some(self.rec(id)?.io_used[r.index()])
    }

    /// Charge `n` of `r` to `id` and every ancestor, all or nothing (J1): refused if the job or any ancestor would pass its own limit, which
    /// is the same as passing the smallest limit along the chain for the job's own usage.
    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::expect_used
    )] // INVARIANT: IoResource::index() < IO_RESOURCES; the record was checked to exist in the first pass; the first pass checked each sum against a limit below u64::MAX, or it is the unlimited case where usage is bounded by the window
    pub fn charge_io(&mut self, id: JobId, r: IoResource, n: u64) -> Result<(), JobError> {
        let i = r.index();
        let mut cur = Some(id);
        while let Some(c) = cur {
            let rec = self.rec(c).ok_or(JobError::NoSuchJob)?;
            let limit = rec.io_limit[i];
            if limit != UNLIMITED && rec.io_used[i].saturating_add(n) > limit {
                return Err(JobError::Exceeded);
            }
            cur = rec.parent;
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let rec = self.rec_mut(c).expect("checked above");
            rec.io_used[i] = rec.io_used[i].saturating_add(n);
            cur = rec.parent;
        }
        Ok(())
    }

    /// Give back what `charge_io` took (J2: exactly once, by the caller's bookkeeping).
    #[allow(clippy::indexing_slicing)] // INVARIANT: IoResource::index() < IO_RESOURCES
    pub fn credit_io(&mut self, id: JobId, r: IoResource, n: u64) {
        let i = r.index();
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(rec) = self.rec_mut(c) else { return };
            debug_assert!(rec.io_used[i] >= n, "credit of more than was charged");
            rec.io_used[i] = rec.io_used[i].saturating_sub(n);
            cur = rec.parent;
        }
    }

    /// Give back what `charge` took.
    pub fn credit(&mut self, id: JobId, r: Resource, n: u64) {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(rec) = self.rec_mut(c) else { return };
            let slot = rec.usage.slot(r);
            debug_assert!(*slot >= n, "credit of more than was charged");
            *slot = slot.saturating_sub(n);
            cur = rec.parent;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn lim(p: u64, t: u64, g: u64) -> Limits {
        Limits {
            processes: p,
            threads: t,
            pages: g,
            jobs: UNLIMITED,
            objects: UNLIMITED,
            ipc_bytes: UNLIMITED,
        }
    }

    #[test]
    fn the_root_is_unlimited_and_permanent() {
        let mut j = Jobs::new();
        assert_eq!(j.limits(JobId::ROOT), Some(Limits::UNLIMITED));
        assert_eq!(j.charge(JobId::ROOT, Resource::Pages, 1 << 40), Ok(()));
        j.credit(JobId::ROOT, Resource::Pages, 1 << 40);
        j.retain(JobId::ROOT);
        j.release(JobId::ROOT);
        j.release(JobId::ROOT); // (the kernel's own use is permanent: nothing changes it)
        assert!(j.exists(JobId::ROOT));
        assert_eq!(j.live(), 1);
    }

    #[test]
    fn a_charge_stops_at_the_limit_and_changes_nothing_when_it_fails() {
        let mut j = Jobs::new();
        let a = j.create(JobId::ROOT, lim(2, 4, 10)).unwrap();
        assert_eq!(j.charge(a, Resource::Processes, 2), Ok(()));
        assert_eq!(j.charge(a, Resource::Processes, 1), Err(JobError::Exceeded));
        assert_eq!(j.usage(a).unwrap().processes, 2);
        assert_eq!(j.charge(a, Resource::Pages, 10), Ok(()));
        assert_eq!(j.charge(a, Resource::Pages, 1), Err(JobError::Exceeded));
        j.credit(a, Resource::Pages, 4);
        assert_eq!(j.charge(a, Resource::Pages, 4), Ok(()));
        assert_eq!(j.charge(a, Resource::Pages, 1), Err(JobError::Exceeded));
        // the resources are independent
        assert_eq!(j.charge(a, Resource::Threads, 4), Ok(()));
        assert_eq!(j.charge(a, Resource::Threads, 1), Err(JobError::Exceeded));
    }

    #[test]
    fn a_child_is_bounded_by_every_ancestor() {
        let mut j = Jobs::new();
        let parent = j
            .create(JobId::ROOT, lim(UNLIMITED, UNLIMITED, 10))
            .unwrap();
        let child = j.create(parent, Limits::UNLIMITED).unwrap();
        let grand = j.create(child, lim(UNLIMITED, UNLIMITED, 100)).unwrap();
        assert_eq!(j.charge(grand, Resource::Pages, 10), Ok(()));
        // the grandchild's own limit (100) is no help: the grandparent allows 10 in all
        assert_eq!(j.charge(grand, Resource::Pages, 1), Err(JobError::Exceeded));
        assert_eq!(j.charge(child, Resource::Pages, 1), Err(JobError::Exceeded));
        // usage is visible at every level
        assert_eq!(j.usage(parent).unwrap().pages, 10);
        assert_eq!(j.usage(child).unwrap().pages, 10);
        assert_eq!(j.usage(grand).unwrap().pages, 10);
        // siblings share the parent's allowance
        let sibling = j.create(parent, Limits::UNLIMITED).unwrap();
        assert_eq!(
            j.charge(sibling, Resource::Pages, 1),
            Err(JobError::Exceeded)
        );
        j.credit(grand, Resource::Pages, 3);
        assert_eq!(j.charge(sibling, Resource::Pages, 3), Ok(()));
        assert_eq!(j.usage(parent).unwrap().pages, 10);
    }

    #[test]
    fn a_failed_charge_touches_no_level() {
        let mut j = Jobs::new();
        let top = j.create(JobId::ROOT, lim(UNLIMITED, UNLIMITED, 5)).unwrap();
        let low = j.create(top, Limits::UNLIMITED).unwrap();
        assert_eq!(j.charge(low, Resource::Pages, 6), Err(JobError::Exceeded));
        assert_eq!(j.usage(low).unwrap(), Usage::default());
        assert_eq!(j.usage(top).unwrap().pages, 0);
        assert_eq!(j.usage(top).unwrap().processes, 0);
    }

    #[test]
    fn lowering_a_limit_below_usage_blocks_new_charges_only() {
        let mut j = Jobs::new();
        let a = j.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        j.charge(a, Resource::Pages, 8).unwrap();
        j.set_limits(a, lim(UNLIMITED, UNLIMITED, 4)).unwrap();
        assert_eq!(j.usage(a).unwrap().pages, 8);
        assert_eq!(j.charge(a, Resource::Pages, 1), Err(JobError::Exceeded));
        j.credit(a, Resource::Pages, 5); // 3 used
        assert_eq!(j.charge(a, Resource::Pages, 1), Ok(()));
        assert_eq!(j.charge(a, Resource::Pages, 1), Err(JobError::Exceeded));
    }

    #[test]
    fn records_live_until_their_last_use_and_free_their_parents_in_turn() {
        let mut j = Jobs::new();
        let a = j.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        let b = j.create(a, Limits::UNLIMITED).unwrap();
        assert_eq!(j.live(), 3);
        j.release(a); // a's creator lets go: b still holds a
        assert!(j.exists(a));
        j.retain(b); // a process in b
        j.release(b); // b's creator lets go: the process still holds it
        assert!(j.exists(b) && j.exists(a));
        j.release(b); // the process ends: b goes, and with it b's hold on a
        assert!(!j.exists(b) && !j.exists(a));
        assert_eq!(j.live(), 1);
        // an old id never names a later job
        let c = j.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        assert!(j.exists(c));
        assert_eq!(
            j.charge(JobId(77), Resource::Pages, 1),
            Err(JobError::NoSuchJob)
        );
        assert_eq!(
            j.create(JobId(77), Limits::UNLIMITED),
            Err(JobError::NoSuchJob)
        );
    }

    #[test]
    fn child_jobs_are_limited_and_counted_for_every_ancestor() {
        let mut j = Jobs::new();
        let top = j
            .create(
                JobId::ROOT,
                Limits {
                    jobs: 3,
                    ..Limits::UNLIMITED
                },
            )
            .unwrap();
        let a = j.create(top, Limits::UNLIMITED).unwrap();
        let b = j.create(a, Limits::UNLIMITED).unwrap(); // a grandchild counts for top too
        let c = j.create(top, Limits::UNLIMITED).unwrap();
        assert_eq!(j.usage(top).unwrap().jobs, 3);
        assert_eq!(j.create(a, Limits::UNLIMITED), Err(JobError::Exceeded));
        assert_eq!(j.create(top, Limits::UNLIMITED), Err(JobError::Exceeded));
        assert_eq!(j.live(), 5, "a refused job leaves no record");
        // a job's record goes when its last use does, and the count falls with it
        j.release(c);
        assert_eq!(j.usage(top).unwrap().jobs, 2);
        j.release(b);
        j.release(a);
        assert_eq!(j.usage(top).unwrap().jobs, 0);
        assert!(j.create(top, Limits::UNLIMITED).is_ok());
    }

    #[test]
    fn within_follows_the_tree() {
        let mut j = Jobs::new();
        let a = j.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        let b = j.create(a, Limits::UNLIMITED).unwrap();
        let c = j.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
        assert!(j.is_within(b, a) && j.is_within(b, JobId::ROOT) && j.is_within(a, a));
        assert!(!j.is_within(a, b) && !j.is_within(c, a) && !j.is_within(b, c));
    }

    #[test]
    fn the_number_of_jobs_is_bounded() {
        let mut j = Jobs::new();
        let mut made = std::vec::Vec::new();
        loop {
            match j.create(JobId::ROOT, Limits::UNLIMITED) {
                Ok(id) => made.push(id),
                Err(e) => {
                    assert_eq!(e, JobError::TooManyJobs);
                    break;
                }
            }
        }
        assert_eq!(j.live(), MAX_JOBS);
        j.release(made.pop().unwrap());
        assert!(j.create(JobId::ROOT, Limits::UNLIMITED).is_ok());
    }

    /// An independent model: each job's usage is recomputed from per-job *own* charges by
    /// summing over its subtree, and a charge is allowed when every ancestor's recomputed total
    /// plus the new amount fits. Random operations on both, compared after every step.
    #[test]
    fn random_operations_agree_with_a_recomputing_model() {
        struct Model {
            parent: std::vec::Vec<Option<usize>>,
            alive: std::vec::Vec<bool>,
            limit: std::vec::Vec<u64>, // pages only (the other resources behave the same)
            own: std::vec::Vec<u64>,
        }
        impl Model {
            fn total(&self, i: usize) -> u64 {
                let mut t = self.own[i];
                for k in 0..self.parent.len() {
                    if k != i && self.alive[k] && self.below(k, i) {
                        t += self.own[k];
                    }
                }
                t
            }
            fn below(&self, mut k: usize, anc: usize) -> bool {
                while let Some(p) = self.parent[k] {
                    if p == anc {
                        return true;
                    }
                    k = p;
                }
                false
            }
            fn allows(&self, i: usize, n: u64) -> bool {
                let mut cur = Some(i);
                while let Some(c) = cur {
                    if self.limit[c] != UNLIMITED && self.total(c) + n > self.limit[c] {
                        return false;
                    }
                    cur = self.parent[c];
                }
                true
            }
        }
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut j = Jobs::new();
        let mut ids = std::vec![JobId::ROOT];
        let mut m = Model {
            parent: std::vec![None],
            alive: std::vec![true],
            limit: std::vec![UNLIMITED],
            own: std::vec![0],
        };
        // (the model recomputes every total, which is slow under Miri: fewer steps there)
        let steps = if cfg!(miri) { 100 } else { 20_000 };
        for _ in 0..steps {
            let i = rnd(ids.len() as u64) as usize;
            match rnd(6) {
                0 if ids.len() < 40 => {
                    let limit = if rnd(3) == 0 { UNLIMITED } else { rnd(30) };
                    let id = j.create(ids[i], lim(UNLIMITED, UNLIMITED, limit)).unwrap();
                    ids.push(id);
                    m.parent.push(Some(i));
                    m.alive.push(true);
                    m.limit.push(limit);
                    m.own.push(0);
                }
                1 | 2 => {
                    let n = rnd(8);
                    let want = m.allows(i, n);
                    let got = j.charge(ids[i], Resource::Pages, n).is_ok();
                    assert_eq!(got, want, "charge {n} to job {i}");
                    if want {
                        m.own[i] += n;
                    }
                }
                3 => {
                    let n = rnd(m.own[i] + 1);
                    j.credit(ids[i], Resource::Pages, n);
                    m.own[i] -= n;
                }
                4 => {
                    let limit = if rnd(3) == 0 { UNLIMITED } else { rnd(30) };
                    j.set_limits(ids[i], lim(UNLIMITED, UNLIMITED, limit))
                        .unwrap();
                    m.limit[i] = limit;
                }
                _ => {}
            }
            for (k, &id) in ids.iter().enumerate() {
                assert_eq!(j.usage(id).unwrap().pages, m.total(k), "usage of job {k}");
            }
        }
    }
}
