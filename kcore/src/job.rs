//! Jobs and resource limits (S4.5, decisions OD56-OD58): a tree of accounts. A job has limits on
//! three resources (live processes, live threads, committed memory pages); a charge against a
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub processes: u64,
    pub threads: u64,
    pub pages: u64,
}

impl Limits {
    pub const UNLIMITED: Limits = Limits {
        processes: UNLIMITED,
        threads: UNLIMITED,
        pages: UNLIMITED,
    };

    fn get(&self, r: Resource) -> u64 {
        match r {
            Resource::Processes => self.processes,
            Resource::Threads => self.threads,
            Resource::Pages => self.pages,
        }
    }
}

/// What a job (and everything below it) uses now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub processes: u64,
    pub threads: u64,
    pub pages: u64,
}

impl Usage {
    fn slot(&mut self, r: Resource) -> &mut u64 {
        match r {
            Resource::Processes => &mut self.processes,
            Resource::Threads => &mut self.threads,
            Resource::Pages => &mut self.pages,
        }
    }

    pub fn get(&self, r: Resource) -> u64 {
        match r {
            Resource::Processes => self.processes,
            Resource::Threads => self.threads,
            Resource::Pages => self.pages,
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

struct Rec {
    parent: Option<JobId>,
    limits: Limits,
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
            usage: Usage::default(),
            uses: 1,
        }));
        Jobs {
            recs,
            free: Vec::new(),
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

    pub fn exists(&self, id: JobId) -> bool {
        self.rec(id).is_some()
    }

    /// A new child of `parent` with `limits`; the caller holds the one use of the new id (and
    /// the child holds one of the parent).
    pub fn create(&mut self, parent: JobId, limits: Limits) -> Result<JobId, JobError> {
        if !self.exists(parent) {
            return Err(JobError::NoSuchJob);
        }
        if self.live >= MAX_JOBS {
            return Err(JobError::TooManyJobs);
        }
        let rec = Rec {
            parent: Some(parent),
            limits,
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
    pub fn retain(&mut self, id: JobId) {
        if let Some(r) = self.rec_mut(id) {
            r.uses += 1;
        }
    }

    /// One holder less; frees the record at the last one, and then releases the parent in turn.
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
                cur = r.parent;
                self.recs[id.0 as usize] = None;
                self.free.push(id.0);
                self.live -= 1;
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
        }
    }

    #[test]
    fn the_root_is_unlimited_and_permanent() {
        let mut j = Jobs::new();
        assert_eq!(j.limits(JobId::ROOT), Some(Limits::UNLIMITED));
        assert_eq!(j.charge(JobId::ROOT, Resource::Pages, 1 << 40), Ok(()));
        j.credit(JobId::ROOT, Resource::Pages, 1 << 40);
        j.release(JobId::ROOT);
        j.release(JobId::ROOT); // (over-release of the kernel's own use is harmless)
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
        assert_eq!(j.usage(top).unwrap(), Usage::default());
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
        let steps = if cfg!(miri) { 300 } else { 20_000 };
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
