//! MM8: families of VMOs (SNAPSHOT and PRIVATE children) against a reference, through the kernel's own paths: `Vmos::read` and `write`, and
//! two address spaces whose regions map every member, driven the way hardware drives them (access, fault if there is no translation or it
//! does not allow the access, retry). The reference keeps the same tree of nodes, hidden ones included, but its lookup is a few lines over
//! plain vectors, so what is checked is the kernel's page handling: moving pages into the hidden node, copying on write, merging a hidden
//! node into its last child, giving pages back and resizing members, the charge of every frame, and above all that **no translation ever
//! shows other contents than the reference says** (COW1: a stale read-only translation of a page that was copied, or the shared zero frame
//! of a page a parent has since written) and that **no translation of an inherited page is writable** (a write must fault and copy).
//! After every step the family's shape is audited (`Vmos::audit_families`: a hidden node has two or more children, no chain is deeper than
//! the limit). At the end everything is released and no frame, VMO or charge is left.
mod common;
use common::{FakeMem, Rng};
use kcore::aspace::{
    invalidate_copies, Access, AddressSpace, ChildMode, VmoError, Vmos, MAX_FAMILY_DEPTH,
};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;

const PAGE: u64 = 4096;
const BASE: u64 = 0xffff_ffd0_0000_0000;

struct Node {
    own: Vec<Option<u8>>,
    /// (node, offset in the parent)
    parent: Option<(usize, u64)>,
    /// pages `0 .. inherit` not held here are looked up in the parent
    inherit: u64,
    job: usize,
    /// the real VMO, for the members a program can reach; hidden nodes have none
    id: Option<u64>,
    /// a member whose handle is still open
    alive: bool,
    resizable: bool,
}

struct Model {
    nodes: Vec<Node>,
}

impl Model {
    fn read(&self, mut n: usize, mut p: u64) -> u8 {
        loop {
            let node = &self.nodes[n];
            if let Some(Some(v)) = node.own.get(p as usize) {
                return *v;
            }
            match node.parent {
                Some((up, off)) if p < node.inherit => {
                    n = up;
                    p += off;
                }
                _ => return 0,
            }
        }
    }

    fn depth(&self, mut n: usize) -> usize {
        let mut d = 0;
        while let Some((up, _)) = self.nodes[n].parent {
            n = up;
            d += 1;
        }
        d
    }

    /// Does the node still exist in the kernel: a member whose handle is open, or one with something that exists below it?
    fn exists(&self, n: usize) -> bool {
        self.nodes[n].alive
            || (0..self.nodes.len())
                .any(|k| self.nodes[k].parent.is_some_and(|(p, _)| p == n) && self.exists(k))
    }
}

struct World {
    mm: Mm<FakeMem>,
    vmos: Vmos,
    jobs: Jobs,
    job_ids: Vec<JobId>,
    spaces: Vec<AddressSpace>,
    m: Model,
    total: usize,
}

impl World {
    fn usage(&self, j: usize) -> u64 {
        self.jobs
            .usage(self.job_ids[j])
            .unwrap()
            .get(Resource::Pages)
    }

    fn va(&self, node: usize, page: u64) -> u64 {
        BASE + node as u64 * 0x10_0000 + page * PAGE
    }

    /// A member: a new real VMO is mapped by both spaces.
    fn add_member(&mut self, node: Node) -> usize {
        let pages = node.own.len() as u64;
        let id = node.id.unwrap();
        self.m.nodes.push(node);
        let n = self.m.nodes.len() - 1;
        for s in self.spaces.iter_mut() {
            s.map_region(
                &mut self.vmos,
                BASE + n as u64 * 0x10_0000,
                pages,
                id,
                0,
                Perms::RW,
            )
            .unwrap();
        }
        n
    }

    fn child(
        &mut self,
        source: u64,
        off: u64,
        len: u64,
        job: usize,
        mode: ChildMode,
    ) -> Result<u64, VmoError> {
        self.vmos.reserve_n(2).unwrap();
        let c =
            self.vmos
                .child_prepare(&mut self.jobs, source, off, len, self.job_ids[job], mode)?;
        self.vmos
            .child_attach(&mut self.jobs, c, source, off, len, mode);
        // (the kernel write-protects the source's mappings after a SNAPSHOT, `ksys::snapshot_protect`)
        if mode == ChildMode::Snapshot {
            for s in self.spaces.iter_mut() {
                s.write_protect_vmo(&mut self.mm, source);
            }
        }
        Ok(c)
    }

    /// What the kernel does after every call and fault (`ksys::cow_sync`).
    fn sync(&mut self) {
        invalidate_copies(self.spaces.iter_mut(), &mut self.mm, &mut self.vmos);
    }

    /// Access `page` of node `n` in space `si` like hardware: use the translation if it allows the access, else fault and retry.
    fn access(&mut self, si: usize, n: usize, page: u64, access: Access) -> u64 {
        let va = self.va(n, page);
        for _ in 0..4 {
            if let Some(t) = self.spaces[si].table().translate(&self.mm, va) {
                let ok = match access {
                    Access::Write => t.perms.write,
                    _ => t.perms.read,
                };
                if ok {
                    return t.pfn;
                }
            }
            self.spaces[si]
                .handle_fault(&mut self.mm, &mut self.vmos, &mut self.jobs, va, access)
                .unwrap();
            // (the kernel runs `cow_sync` before the thread continues)
            self.sync();
        }
        panic!("the access never resolved");
    }

    /// Unmap `first .. first + n` of the member's VMO and of everything below it, in every space (`ksys` does this before a decommit).
    fn unmap_below(&mut self, id: u64, first: u64, n: u64) {
        let (spaces, mm) = (&mut self.spaces, &mut self.mm);
        self.vmos.for_each_below(id, 0, &mut |d, base| {
            if let Some(f) = first.checked_sub(base).or((first + n > base).then_some(0)) {
                let k = (first + n).saturating_sub(base).saturating_sub(f);
                for s in spaces.iter_mut() {
                    s.unmap_vmo_pages(mm, d, f, k);
                }
            }
        });
    }

    fn check(&mut self, ctx: &str) {
        let members: Vec<usize> = (0..self.m.nodes.len())
            .filter(|&i| self.m.nodes[i].alive && self.m.nodes[i].id.is_some())
            .collect();
        for &i in &members {
            let id = self.m.nodes[i].id.unwrap();
            for p in 0..self.m.nodes[i].own.len() as u64 {
                let mut b = [0u8; 1];
                self.vmos.read(&mut self.mm, id, p * PAGE, &mut b).unwrap();
                assert_eq!(
                    b[0],
                    self.m.read(i, p),
                    "{ctx}: member {i} page {p} through the VMO"
                );
                // every translation shows what the reference says, and a writable one is the member's own page
                for (si, s) in self.spaces.iter().enumerate() {
                    if let Some(t) = s.table().translate(&self.mm, self.va(i, p)) {
                        let mut f = [0u8; 1];
                        self.mm.mem.read_bytes(t.pfn << 12, &mut f);
                        assert_eq!(
                            f[0],
                            self.m.read(i, p),
                            "{ctx}: space {si} member {i} page {p}: a translation shows other contents (stale)"
                        );
                        if t.perms.write {
                            assert_eq!(
                                self.vmos.frame_info(id, p),
                                Some((t.pfn, true)),
                                "{ctx}: space {si} member {i} page {p}: writable, but the member does not own the frame"
                            );
                        }
                    }
                }
            }
        }
        // every frame is charged once: the pages the jobs are charged for are the frames the VMOs hold (hidden nodes too) plus what the
        // address spaces have prepaid
        let prepaid: u64 = self.spaces.iter().map(|a| a.reserved_pages()).sum();
        assert_eq!(
            self.usage(0) + self.usage(1),
            self.vmos.frames_held() + prepaid,
            "{ctx}: every frame is charged to exactly one job"
        );
        self.vmos
            .audit_families()
            .unwrap_or_else(|e| panic!("{ctx}: {e}"));
    }
}

#[test]
fn families_agree_with_the_reference_through_every_path_and_no_translation_goes_stale() {
    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(20000, 500),
            mem: FakeMem::default(),
        };
        let total = mm.db.free_count();
        let mut jobs = Jobs::new();
        let job_ids: Vec<JobId> = (0..2)
            .map(|_| jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap())
            .collect();
        let spaces: Vec<AddressSpace> = (0..2)
            .map(|_| AddressSpace::new(PageTable::new(&mut mm).unwrap()))
            .collect();
        let mut w = World {
            mm,
            vmos: Vmos::new(),
            jobs,
            job_ids,
            spaces,
            m: Model { nodes: vec![] },
            total,
        };
        for j in 0..2 {
            let pages = 2 + rng.below(6);
            let id = w.vmos.create_in(&mut w.jobs, pages, w.job_ids[j]);
            w.vmos.set_resizable(id);
            w.add_member(Node {
                own: vec![None; pages as usize],
                parent: None,
                inherit: 0,
                job: j,
                id: Some(id),
                alive: true,
                resizable: true,
            });
        }
        for step in 0..300 {
            let ctx = format!("seed {seed} step {step}");
            let alive: Vec<usize> = (0..w.m.nodes.len())
                .filter(|&i| w.m.nodes[i].alive && w.m.nodes[i].id.is_some())
                .collect();
            if alive.is_empty() {
                break;
            }
            let pick = |rng: &mut Rng| {
                if rng.below(3) == 0 {
                    *alive.last().unwrap()
                } else {
                    alive[rng.below(alive.len() as u64) as usize]
                }
            };
            let op = rng.below(14);
            if std::env::var("FAMTRACE").is_ok() {
                eprintln!("{ctx}: op {op} alive {alive:?}");
            }
            match op {
                0 | 1 => {
                    // a write through the kernel (`vmo_write`): charged to the member's job at once
                    let i = pick(&mut rng);
                    let id = w.m.nodes[i].id.unwrap();
                    let p = rng.below(w.m.nodes[i].own.len() as u64);
                    let v = 1 + rng.below(250) as u8;
                    let job = w.m.nodes[i].job;
                    let before = [w.usage(0), w.usage(1)];
                    w.vmos
                        .write(&mut w.mm, &mut w.jobs, id, p * PAGE, &[v])
                        .unwrap();
                    w.sync();
                    w.m.nodes[i].own[p as usize] = Some(v);
                    // (the exact amount is not predicted: a page the reference sees as inherited may be the member's own by now, a merge moved the
                    // hidden node's pages into it; the sum over all frames is checked after every step)
                    let after = [w.usage(0), w.usage(1)];
                    let delta = [after[0] - before[0], after[1] - before[1]];
                    assert_eq!(delta[1 - job], 0, "{ctx}: only the writer's job is charged");
                    assert!(
                        delta[job] <= 1,
                        "{ctx}: a write charges at most the page it needed"
                    );
                }
                2 | 3 => {
                    // a store through a mapping
                    let i = pick(&mut rng);
                    let si = rng.below(2) as usize;
                    let p = rng.below(w.m.nodes[i].own.len() as u64);
                    let v = 1 + rng.below(250) as u8;
                    let pfn = w.access(si, i, p, Access::Write);
                    if std::env::var("FAMTRACE").is_ok() {
                        eprintln!(
                            "  store member {i} page {p} space {si} frame {pfn} chain {:?}",
                            {
                                let mut v = vec![];
                                let mut n = i;
                                let mut pg = p;
                                loop {
                                    let nd = &w.m.nodes[n];
                                    v.push((
                                        n,
                                        nd.id.is_none(),
                                        nd.alive,
                                        pg,
                                        nd.own.get(pg as usize).cloned(),
                                        nd.inherit,
                                    ));
                                    match nd.parent {
                                        Some((u, o)) => {
                                            n = u;
                                            pg += o;
                                        }
                                        None => break,
                                    }
                                }
                                v
                            }
                        );
                    }
                    w.mm.mem.write_bytes(pfn << 12, &[v]);
                    w.m.nodes[i].own[p as usize] = Some(v);
                }
                4 | 5 => {
                    // a load through a mapping
                    let i = pick(&mut rng);
                    let si = rng.below(2) as usize;
                    let p = rng.below(w.m.nodes[i].own.len() as u64);
                    let pfn = w.access(si, i, p, Access::Read);
                    let mut b = [0u8; 1];
                    w.mm.mem.read_bytes(pfn << 12, &mut b);
                    assert_eq!(b[0], w.m.read(i, p), "{ctx}: a load of member {i} page {p}");
                    // (a first touch of a page of a VMO that inherits nothing commits a zero page; in one that inherits, nothing is committed)
                    if w.m.nodes[i].parent.is_none() && w.m.nodes[i].own[p as usize].is_none() {
                        w.m.nodes[i].own[p as usize] = Some(0);
                    }
                }
                6 => {
                    // a SNAPSHOT child, same job
                    let i = pick(&mut rng);
                    let id = w.m.nodes[i].id.unwrap();
                    let pages = w.m.nodes[i].own.len() as u64;
                    let off = rng.below(pages);
                    let len = 1 + rng.below(pages - off);
                    let job = w.m.nodes[i].job;
                    let r = w.child(id, off, len, job, ChildMode::Snapshot);
                    // the reference mirrors the structure: the same reuse rule, the same limit on the longest chain
                    let src = &w.m.nodes[i];
                    let reuse = src.parent.is_some_and(|(h, _)| w.m.nodes[h].id.is_none())
                        && src.own.iter().all(Option::is_none)
                        && off + len <= src.inherit;
                    let deepest_below = {
                        let mut d = 0;
                        for k in 0..w.m.nodes.len() {
                            if w.m.exists(k) {
                                let mut x = k;
                                let mut hops = 0;
                                while x != i {
                                    match w.m.nodes[x].parent {
                                        Some((up, _)) => {
                                            x = up;
                                            hops += 1;
                                        }
                                        None => {
                                            hops = usize::MAX;
                                            break;
                                        }
                                    }
                                }
                                if hops != usize::MAX {
                                    d = d.max(hops);
                                }
                            }
                        }
                        d
                    };
                    let up = w.m.depth(i);
                    let longest = if reuse {
                        up
                    } else {
                        (up + 1 + deepest_below).max(up + 2)
                    };
                    if longest > MAX_FAMILY_DEPTH {
                        assert_eq!(r, Err(VmoError::TooDeep), "{ctx}");
                    } else {
                        let child = r.unwrap();
                        let (hn, old_parent) = if reuse {
                            let (h, o) = w.m.nodes[i].parent.unwrap();
                            (h, o)
                        } else {
                            let h = w.m.nodes.len();
                            let own = std::mem::replace(
                                &mut w.m.nodes[i].own,
                                vec![None; pages as usize],
                            );
                            let old_parent = w.m.nodes[i].parent;
                            let inherit = w.m.nodes[i].inherit;
                            w.m.nodes.push(Node {
                                own,
                                parent: old_parent,
                                inherit,
                                job,
                                id: None,
                                alive: false, // (a hidden node exists while something below it does)
                                resizable: false,
                            });
                            w.m.nodes[i].parent = Some((h, 0));
                            w.m.nodes[i].inherit = pages;
                            (h, 0)
                        };
                        w.add_member(Node {
                            own: vec![None; len as usize],
                            parent: Some((hn, old_parent + off)),
                            inherit: len,
                            job,
                            id: Some(child),
                            alive: true,
                            resizable: false,
                        });
                    }
                }
                7 => {
                    // a PRIVATE child, any job
                    let i = pick(&mut rng);
                    let id = w.m.nodes[i].id.unwrap();
                    let pages = w.m.nodes[i].own.len() as u64;
                    let off = rng.below(pages);
                    let len = 1 + rng.below(pages - off);
                    let job = rng.below(2) as usize;
                    let r = w.child(id, off, len, job, ChildMode::Private);
                    if w.m.depth(i) + 1 > MAX_FAMILY_DEPTH {
                        assert_eq!(r, Err(VmoError::TooDeep), "{ctx}");
                    } else {
                        let child = r.unwrap();
                        w.add_member(Node {
                            own: vec![None; len as usize],
                            parent: Some((i, off)),
                            inherit: len,
                            job,
                            id: Some(child),
                            alive: true,
                            resizable: false,
                        });
                    }
                }
                8 => {
                    // decommit a range of a member: it reads zeros afterwards, in the member and in everything that sees it
                    let i = pick(&mut rng);
                    let id = w.m.nodes[i].id.unwrap();
                    let pages = w.m.nodes[i].own.len() as u64;
                    let first = rng.below(pages);
                    let n = 1 + rng.below(pages - first);
                    w.unmap_below(id, first, n);
                    w.sync();
                    w.vmos
                        .decommit(&mut w.mm, &mut w.jobs, id, first, n)
                        .unwrap();
                    for p in first..first + n {
                        // inside the window a member inherits, the page becomes an explicit hole: zeros, whatever its parent shows now or later
                        let inside = w.m.nodes[i].parent.is_some() && p < w.m.nodes[i].inherit;
                        w.m.nodes[i].own[p as usize] = inside.then_some(0);
                    }
                }
                9 => {
                    // resize a resizable member
                    let i = pick(&mut rng);
                    if !w.m.nodes[i].resizable {
                        continue;
                    }
                    let id = w.m.nodes[i].id.unwrap();
                    let size = w.m.nodes[i].own.len() as u64;
                    let new = 1 + rng.below(8);
                    let has_children = (0..w.m.nodes.len())
                        .any(|k| w.m.nodes[k].parent.is_some_and(|(p, _)| p == i) && w.m.exists(k));
                    assert_eq!(w.vmos.has_children(id), has_children, "{ctx}");
                    if new < size {
                        w.unmap_below(id, new, size - new);
                        w.sync();
                    }
                    let r = w.vmos.set_size(&mut w.mm, &mut w.jobs, id, new);
                    if has_children {
                        assert_eq!(r, Err(VmoError::HasChildren), "{ctx}");
                    } else {
                        r.unwrap();
                        let n = &mut w.m.nodes[i];
                        n.own.resize(new as usize, None);
                        n.inherit = n.inherit.min(new);
                        // (the regions keep their length: a touch past the end is refused, so the check below reads only what is there)
                    }
                    // the regions of this member are as long as they were made: keep the reference's pages and the regions in step by
                    // remapping the member (nothing is lost: the new region maps the same VMO)
                    for si in 0..2 {
                        let base = w.va(i, 0);
                        w.spaces[si]
                            .unmap_region(&mut w.mm, &mut w.vmos, &mut w.jobs, base)
                            .unwrap();
                        let pages = w.m.nodes[i].own.len() as u64;
                        w.spaces[si]
                            .map_region(&mut w.vmos, base, pages, id, 0, Perms::RW)
                            .unwrap();
                    }
                    w.sync();
                }
                10 => {
                    // protect a member's region read-only and back (what an allocator or a loader does): no inherited page becomes writable
                    let i = pick(&mut rng);
                    let si = rng.below(2) as usize;
                    let pages = w.m.nodes[i].own.len() as u64;
                    let base = w.va(i, 0);
                    for perms in [Perms::R, Perms::RW] {
                        let r = w.spaces[si].protect_range(
                            &mut w.mm,
                            &mut w.vmos,
                            &mut w.jobs,
                            base,
                            pages * PAGE,
                            perms,
                        );
                        assert!(r.is_ok(), "{ctx}: {r:?}");
                    }
                }
                11 => {
                    // refused operations change nothing
                    let i = pick(&mut rng);
                    let id = w.m.nodes[i].id.unwrap();
                    let job = w.m.nodes[i].job;
                    let pages = w.m.nodes[i].own.len() as u64;
                    w.vmos.reserve_n(2).unwrap();
                    assert_eq!(
                        w.vmos.child_prepare(
                            &mut w.jobs,
                            id,
                            0,
                            1,
                            w.job_ids[1 - job],
                            ChildMode::Snapshot
                        ),
                        Err(VmoError::JobMismatch),
                        "{ctx}: a SNAPSHOT child is charged to the source's job"
                    );
                    assert_eq!(
                        w.vmos.child_prepare(
                            &mut w.jobs,
                            id,
                            pages,
                            1,
                            w.job_ids[job],
                            ChildMode::Private
                        ),
                        Err(VmoError::OutOfRange)
                    );
                    // a prepared child that is not attached (the handle had no room) leaves the source as it was
                    let before = w.vmos.count();
                    let c = w
                        .vmos
                        .child_prepare(&mut w.jobs, id, 0, 1, w.job_ids[job], ChildMode::Snapshot)
                        .unwrap();
                    assert!(!w.vmos.has_parent(c));
                    w.vmos.release(&mut w.mm, &mut w.jobs, c).unwrap();
                    assert_eq!(w.vmos.count(), before, "{ctx}: nothing was made");
                }
                _ => {
                    // a member goes away: its regions, then its handle
                    if alive.len() > 2 {
                        let i = pick(&mut rng);
                        let id = w.m.nodes[i].id.unwrap();
                        for si in 0..2 {
                            let base = w.va(i, 0);
                            w.spaces[si]
                                .unmap_region(&mut w.mm, &mut w.vmos, &mut w.jobs, base)
                                .unwrap();
                        }
                        w.vmos.release(&mut w.mm, &mut w.jobs, id).unwrap();
                        w.sync();
                        w.m.nodes[i].alive = false;
                    }
                }
            }
            w.check(&ctx);
        }
        // everything back
        for i in 0..w.m.nodes.len() {
            if w.m.nodes[i].alive {
                if let Some(id) = w.m.nodes[i].id {
                    for si in 0..2 {
                        let base = w.va(i, 0);
                        w.spaces[si]
                            .unmap_region(&mut w.mm, &mut w.vmos, &mut w.jobs, base)
                            .unwrap();
                    }
                    w.vmos.release(&mut w.mm, &mut w.jobs, id).unwrap();
                }
            }
        }
        let World {
            mut mm,
            mut vmos,
            mut jobs,
            spaces,
            total,
            job_ids,
            ..
        } = w;
        for s in spaces {
            s.destroy(&mut mm, &mut vmos, &mut jobs);
        }
        assert_eq!(
            vmos.count(),
            0,
            "seed {seed}: no VMO left, hidden nodes included"
        );
        assert_eq!(
            mm.db.free_count(),
            total,
            "seed {seed}: no frame left, the zero frame included"
        );
        for j in job_ids {
            assert_eq!(
                jobs.usage(j).unwrap().get(Resource::Pages),
                0,
                "seed {seed}"
            );
        }
        mm.db.audit().unwrap();
    }
}

#[test]
fn a_chain_of_children_is_at_most_64_deep_and_a_refusal_changes_nothing() {
    let mut mm = Mm {
        db: FrameDb::new(400, 100),
        mem: FakeMem::default(),
    };
    let free_before = mm.db.free_count();
    let mut vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let mut cur = vmos.create_in(&mut jobs, 2, job);
    vmos.write(&mut mm, &mut jobs, cur, 0, &[7]).unwrap();
    let mut made = vec![cur];
    let mut refused = false;
    for _ in 0..(MAX_FAMILY_DEPTH + 4) {
        vmos.reserve_n(2).unwrap();
        let before = vmos.count();
        match vmos.child_prepare(&mut jobs, cur, 0, 2, job, ChildMode::Private) {
            Ok(c) => {
                vmos.child_attach(&mut jobs, c, cur, 0, 2, ChildMode::Private);
                cur = c;
                made.push(c);
            }
            Err(e) => {
                assert_eq!(e, VmoError::TooDeep);
                assert_eq!(vmos.count(), before, "a refusal makes no VMO");
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the 65th link is refused");
    let mut b = [0u8; 1];
    vmos.read(&mut mm, cur, 0, &mut b).unwrap();
    assert_eq!(b[0], 7);
    // a SNAPSHOT of the root now would push every leaf one level down: refused, and the leaf still reads 7 (the limit covers descendants)
    vmos.reserve_n(2).unwrap();
    assert_eq!(
        vmos.child_prepare(&mut jobs, made[0], 0, 2, job, ChildMode::Snapshot),
        Err(VmoError::TooDeep),
        "the descendants count"
    );
    vmos.read(&mut mm, cur, 0, &mut b).unwrap();
    assert_eq!(b[0], 7);
    for id in made.into_iter().rev() {
        vmos.release(&mut mm, &mut jobs, id).unwrap();
    }
    assert_eq!(vmos.count(), 0);
    assert_eq!(
        mm.db.free_count(),
        free_before,
        "no frame left, the zero frame included"
    );
}
