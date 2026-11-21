//! MM8: two leaves write-faulting the same shared page, under shuttle. A source VMO (mapped in space 0) is snapshotted; the child is mapped
//! in space 1. Three model threads (harts) race under the one lock that guards the memory (the VMO store, the ledger and the frames: `kmem::get`
//! in the kernel): one writes the source's pages through space 0, one writes the child's pages through space 1, one snapshots the source again
//! (making a third member) and reads through its own mapping. Each operation is atomic under the lock, as in the kernel; what the checker
//! explores is every order of them. Properties after every operation: every translation shows what the writer wrote or what the snapshot
//! showed (never another member's later write), and at the end every frame, VMO and charge is back.
//!
//! Run by `make os-test` with `RUSTFLAGS="--cfg shuttle"`; without the cfg this file is empty.
#![cfg(shuttle)]

mod common;
use common::FakeMem;
use kcore::aspace::{invalidate_copies, Access, AddressSpace, ChildMode, Vmos};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;
use shuttle::rand::Rng;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

const PAGE: u64 = 4096;
const PAGES: u64 = 4;
const BASE: [u64; 3] = [
    0xffff_ffd0_0000_0000,
    0xffff_ffd0_0100_0000,
    0xffff_ffd0_0200_0000,
];

struct World {
    mm: Mm<FakeMem>,
    vmos: Vmos,
    jobs: Jobs,
    spaces: Vec<AddressSpace>,
    job: JobId,
    /// the VMO behind each mapping (a source, then each child), in the space it is mapped in
    members: Vec<(u64, usize, u64)>,
    /// what the first byte of each page of each member must read
    expect: Vec<Vec<u8>>,
}

impl World {
    fn store(&mut self, m: usize, page: u64, v: u8) {
        let (_, si, base) = self.members[m];
        let va = base + page * PAGE;
        for _ in 0..4 {
            if let Some(t) = self.spaces[si].table().translate(&self.mm, va) {
                if t.perms.write {
                    self.mm.mem.write_bytes(t.pfn << 12, &[v]);
                    self.expect[m][page as usize] = v;
                    return;
                }
            }
            self.spaces[si]
                .handle_fault(
                    &mut self.mm,
                    &mut self.vmos,
                    &mut self.jobs,
                    va,
                    Access::Write,
                )
                .unwrap();
            invalidate_copies(self.spaces.iter_mut(), &mut self.mm, &mut self.vmos);
        }
        panic!("the store never resolved");
    }

    fn check(&mut self) {
        for (m, &(_, si, base)) in self.members.iter().enumerate() {
            for p in 0..PAGES {
                if let Some(t) = self.spaces[si].table().translate(&self.mm, base + p * PAGE) {
                    let mut b = [0u8; 1];
                    self.mm.mem.read_bytes(t.pfn << 12, &mut b);
                    assert_eq!(b[0], self.expect[m][p as usize], "member {m} page {p}");
                }
            }
        }
    }
}

fn run() {
    let mut mm = Mm {
        db: FrameDb::new(0, 400),
        mem: FakeMem::default(),
    };
    let total = mm.db.free_count();
    let mut jobs = Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let mut vmos = Vmos::new();
    let spaces: Vec<AddressSpace> = (0..2)
        .map(|_| AddressSpace::new(PageTable::new(&mut mm).unwrap()))
        .collect();
    let src = vmos.create_in(&mut jobs, PAGES, job);
    for p in 0..PAGES {
        vmos.write(&mut mm, &mut jobs, src, p * PAGE, &[10 + p as u8])
            .unwrap();
    }
    let mut w = World {
        mm,
        vmos,
        jobs,
        spaces,
        job,
        members: vec![(src, 0, BASE[0])],
        expect: vec![(0..PAGES).map(|p| 10 + p as u8).collect()],
    };
    w.spaces[0]
        .map_region(&mut w.vmos, BASE[0], PAGES, src, 0, Perms::RW)
        .unwrap();
    // the first snapshot, mapped in space 1
    w.vmos.reserve_n(2).unwrap();
    let c = w
        .vmos
        .child_prepare(&mut w.jobs, src, 0, PAGES, job, ChildMode::Snapshot)
        .unwrap();
    w.vmos
        .child_attach(&mut w.jobs, c, src, 0, PAGES, ChildMode::Snapshot);
    w.spaces[0].write_protect_vmo(&mut w.mm, src);
    w.spaces[1]
        .map_region(&mut w.vmos, BASE[1], PAGES, c, 0, Perms::RW)
        .unwrap();
    w.members.push((c, 1, BASE[1]));
    w.expect.push(w.expect[0].clone());
    let world = Arc::new(Mutex::new(w));
    let mut hs = vec![];
    for t in 0..3usize {
        let world = world.clone();
        hs.push(thread::spawn(move || {
            let mut rng = shuttle::rand::thread_rng();
            for step in 0..12 {
                let mut guard = world.lock().unwrap();
                let w = &mut *guard;
                match t {
                    // the source writes (member 0), the child writes (member 1)
                    0 | 1 => {
                        let page = rng.gen_range(0..PAGES);
                        w.store(t, page, 100 + (step as u8) + 20 * t as u8);
                    }
                    // a third member: another snapshot of the source, with its own mapping in space 1, written once
                    _ => {
                        if w.members.len() < 3 {
                            let src = w.members[0].0;
                            w.vmos.reserve_n(2).unwrap();
                            let c = w
                                .vmos
                                .child_prepare(&mut w.jobs, src, 0, PAGES, job, ChildMode::Snapshot)
                                .unwrap();
                            w.vmos
                                .child_attach(&mut w.jobs, c, src, 0, PAGES, ChildMode::Snapshot);
                            w.spaces[0].write_protect_vmo(&mut w.mm, src);
                            w.spaces[1]
                                .map_region(&mut w.vmos, BASE[2], PAGES, c, 0, Perms::RW)
                                .unwrap();
                            w.members.push((c, 1, BASE[2]));
                            let e = w.expect[0].clone();
                            w.expect.push(e);
                        } else {
                            let page = rng.gen_range(0..PAGES);
                            w.store(2, page, 200 + step as u8);
                        }
                    }
                }
                w.check();
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let mut w = Arc::try_unwrap(world).ok().unwrap().into_inner().unwrap();
    w.vmos.audit_families(true).unwrap();
    let ids: Vec<(u64, usize, u64)> = w.members.clone();
    for (id, si, base) in ids {
        w.spaces[si]
            .unmap_region(&mut w.mm, &mut w.vmos, &mut w.jobs, base)
            .unwrap();
        w.vmos.release(&mut w.mm, &mut w.jobs, id).unwrap();
    }
    let World {
        mut mm,
        mut vmos,
        mut jobs,
        spaces,
        job,
        ..
    } = w;
    for s in spaces {
        s.destroy(&mut mm, &mut vmos, &mut jobs);
    }
    assert_eq!(vmos.count(), 0);
    assert_eq!(mm.db.free_count(), total, "no frame left");
    assert_eq!(jobs.usage(job).unwrap().get(Resource::Pages), 0);
}

#[test]
fn leaves_that_write_the_same_shared_pages_never_see_each_others_copies() {
    shuttle::check_random(run, 400);
}
