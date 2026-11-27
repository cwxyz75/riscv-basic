//! TLB1 and TLB2 (INVARIANTS.md) by construction, on the host (T193, T283): a machine of several harts whose TLBs hold every
//! translation any thread could have walked, and lose one only when the call asks the machine to flush (`Machine::shootdown` for the
//! other harts, `Machine::flush_tlb` for the caller's). After every `process_unmap`, `process_protect` and `vmo_decommit` the model
//! checks that no hart caches a translation the page table no longer has (a page gone, a frame changed, a permission taken away). This
//! is the QEMU test `unmap_while_siblings_touch` without the emulator: there a stale entry shows only if the emulated TLB happens to
//! keep it (QEMU drops entries on its own schedule, T193); here every hart keeps everything, so a missing shootdown always shows.
//! The control `a_machine_that_skips_the_remote_flush_is_caught` runs the same rounds on a machine whose shootdown does nothing (the
//! `fault-skip-shootdown` kernel) and must be caught.
use abi::syscall as nr;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, Quota};
use kcore::job::JobId;
use kcore::pagetable::{PageTable, Translation};
use kcore::usercopy::{copy_from_user, copy_to_user};
use kcore::Mm;
use ksys::{proc_call, Machine, PCtx, ProcState, Process};
use std::collections::BTreeMap;

mod common;
use common::FakeMem;

const R: u64 = 1;
const W: u64 = 2;
const HARTS: usize = 4;
/// The caller runs on hart 0.
const ME: usize = 0;

/// Every hart's TLB: (process, page) -> what it caches.
struct Harts {
    tlb: Vec<BTreeMap<(u64, u64), Translation>>,
    /// The negative control: the remote flush does nothing (`fault-skip-shootdown`).
    skip_remote: bool,
    shootdowns: u32,
}

impl Harts {
    fn new(skip_remote: bool) -> Self {
        Harts {
            tlb: (0..HARTS).map(|_| BTreeMap::new()).collect(),
            skip_remote,
            shootdowns: 0,
        }
    }
}

impl Machine for Harts {
    fn shootdown(&mut self) {
        self.shootdowns += 1;
        if self.skip_remote {
            return;
        }
        for (h, t) in self.tlb.iter_mut().enumerate() {
            if h != ME {
                t.clear();
            }
        }
    }
    fn flush_tlb(&mut self) {
        self.tlb[ME].clear();
    }
}

struct World {
    objs: Kobjs,
    procs: ksys::ProcTable,
    mm: Mm<FakeMem>,
    vmos: Vmos,
    jobs: kcore::job::Jobs,
    harts: Harts,
}

fn process(mm: &mut Mm<FakeMem>, kernel: &PageTable, pid: u64) -> Process {
    let mut space = AddressSpace::new(PageTable::new_with_kernel_half(mm, kernel).unwrap());
    space.set_owner_fresh(pid);
    Process {
        live_tids: Default::default(),
        aspace: std::sync::Arc::new(klock::aslock::AsCell::new(pid, space)),
        handles: HandleTable::new(256),
        tag: kcore::asid::AsTag { asid: 1, gen: 1 },
        vmos: Vec::new(),
        entry: 0,
        arg: 0,
        exit_code: None,
        kernel_end: None,
        exc_channel: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 0,
        job: JobId::ROOT,
        table_charge: kcore::fmap::FMap::new(),
        charge_end: kcore::fmap::FMap::new(),
        stacks: Default::default(),
        stack_hint: 0,
        privileged: false,
        libsys: Default::default(),
    }
}

/// Process 1 calls; process 2 is the one whose threads, on every hart, touch the pages (the caller holds a MANAGE handle to it).
fn world(skip_remote: bool) -> (World, u64) {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut procs = kcore::fmap::FMap::new();
    ksys::put_process(&mut procs, 1, process(&mut mm, &kernel, 1));
    ksys::put_process(&mut procs, 2, process(&mut mm, &kernel, 2));
    let mut w = World {
        objs: Kobjs::new(),
        procs,
        mm,
        vmos: Vmos::new(),
        jobs: kcore::job::Jobs::new(),
        harts: Harts::new(skip_remote),
    };
    let p = w.procs.get_mut(&1).unwrap();
    let (h, _) = ipc::process_object_create_in(
        &mut w.objs,
        &mut p.handles,
        2,
        false,
        &mut Quota {
            jobs: &mut w.jobs,
            job: Some(JobId::ROOT),
        },
    )
    .unwrap();
    (w, h.0)
}

impl World {
    fn vmo(&mut self, pages: u64) -> u64 {
        let id = self.vmos.create_in(&mut self.jobs, pages, JobId::ROOT);
        let p = self.procs.get_mut(&1).unwrap();
        ipc::vmo_object_create_in(
            &mut self.objs,
            &mut p.handles,
            id,
            &mut Quota {
                jobs: &mut self.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap()
        .0
    }
    fn call(&mut self, n: usize, a: [u64; 6]) {
        let mut c = PCtx {
            objs: &mut self.objs,
            procs: &mut self.procs,
            me: 1,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            jobs: &mut self.jobs,
            machine: &mut self.harts,
        };
        proc_call(n, a, &mut c)
            .expect("handled")
            .unwrap_or_else(|e| panic!("call {n} {a:x?} failed: {e:?}"));
    }
    /// A thread of process 2 touches page `va` (read, or write when `write`): the page is faulted in as the kernel would.
    fn touch(&mut self, va: u64, write: bool) {
        let t = self.procs.get_mut(&2).unwrap();
        if write {
            copy_to_user(
                t.aspace.held_mut(),
                &mut self.mm,
                &mut self.vmos,
                &mut self.jobs,
                va,
                b"t",
            )
            .unwrap();
        } else {
            copy_from_user(
                t.aspace.held_mut(),
                &mut self.mm,
                &mut self.vmos,
                &mut self.jobs,
                va,
                1,
            )
            .unwrap();
        }
    }
    /// Every hart walks process 2's table and caches every translation in it (the pessimistic TLB: it keeps whatever it ever saw).
    fn every_hart_caches_everything(&mut self) {
        let mut leaves = Vec::new();
        self.procs[&2]
            .aspace
            .held_ref()
            .table()
            .for_each_leaf(&self.mm, &mut |va, t, _| leaves.push((va, t)));
        for t in self.harts.tlb.iter_mut() {
            for &(va, tr) in &leaves {
                t.insert((2, va), tr);
            }
        }
    }
    /// TLB1 and TLB2 after a call: every translation a hart caches is still in the table, to the same frame, with no permission the
    /// table no longer gives. Returns the first stale one.
    fn stale(&self) -> Option<String> {
        let table = self.procs[&2].aspace.held_ref();
        for (h, t) in self.harts.tlb.iter().enumerate() {
            for (&(pid, va), cached) in t {
                assert_eq!(pid, 2);
                let now = table.table().translate(&self.mm, va);
                let ok = now.is_some_and(|n| {
                    n.pfn == cached.pfn
                        && (n.perms.read || !cached.perms.read)
                        && (n.perms.write || !cached.perms.write)
                        && (n.perms.exec || !cached.perms.exec)
                });
                if !ok {
                    return Some(format!(
                        "hart {h} caches {va:#x} -> {cached:?}, the table has {now:?}"
                    ));
                }
            }
        }
        None
    }
}

const X: u64 = 0x40_0000;

/// The rounds of `unmap_while_siblings_touch` and its two siblings (decommit, protect), with a seeded mix of sizes and offsets. Returns
/// the first stale translation found after a call, with the round and the call.
fn rounds(skip_remote: bool, seed: u64) -> Result<u32, String> {
    let (mut w, ph) = world(skip_remote);
    let mut rng = seed | 1;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let mut checked = 0;
    for round in 1..=60u32 {
        let pages = 1 + next(8);
        let v = w.vmo(pages);
        w.call(nr::PROCESS_MAP, [ph, v, X, 0, pages * 4096, R | W]);
        // (each round's VMO stays open, so no frame of an earlier round comes back as this round's: the check compares frames)
        for p in 0..pages {
            if next(3) != 0 {
                w.touch(X + p * 4096, next(2) == 0);
            }
        }
        w.every_hart_caches_everything();
        let (what, n, a) = match next(3) {
            // take the whole mapping away
            0 => ("unmap", nr::PROCESS_UNMAP, [ph, X, 0, 0, 0, 0]),
            // take write away from a part of it
            1 => {
                let first = next(pages);
                let len = 1 + next(pages - first);
                (
                    "protect",
                    nr::PROCESS_PROTECT,
                    [ph, X + first * 4096, len * 4096, R, 0, 0],
                )
            }
            // give some of its pages back
            _ => {
                let first = next(pages);
                let len = 1 + next(pages - first);
                (
                    "decommit",
                    nr::VMO_DECOMMIT,
                    [v, first * 4096, len * 4096, 0, 0, 0],
                )
            }
        };
        w.call(n, a);
        checked += 1;
        if let Some(s) = w.stale() {
            return Err(format!("round {round}, after {what} {a:x?}: {s}"));
        }
        // every hart now drops what it held (the threads were switched out): the next round starts clean, so a stale entry is this round's
        for t in w.harts.tlb.iter_mut() {
            t.clear();
        }
        if what != "unmap" {
            // (a protect of a part split the mapping: the whole span goes)
            w.call(nr::PROCESS_UNMAP, [ph, X, pages * 4096, 0, 0, 0]);
        }
    }
    assert!(w.harts.shootdowns > 0);
    Ok(checked)
}

#[test]
fn no_hart_keeps_a_translation_the_table_lost() {
    for seed in 1..=40 {
        let checked = rounds(false, seed).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(checked, 60);
    }
}

#[test]
fn a_machine_that_skips_the_remote_flush_is_caught() {
    // the model must be able to fail: with the shootdown a no-op (the `fault-skip-shootdown` kernel), every seed finds a stale translation
    for seed in 1..=40 {
        let e = rounds(true, seed)
            .expect_err("a machine without the remote flush keeps stale translations");
        assert!(e.contains("caches"), "{e}");
    }
}

#[test]
fn an_unmap_reaches_every_other_hart() {
    // the plain case of the QEMU test, step by step: two harts besides the caller hold the page; after the unmap none does
    let (mut w, ph) = world(false);
    let v = w.vmo(1);
    w.call(nr::PROCESS_MAP, [ph, v, X, 0, 4096, R]);
    w.touch(X, false);
    w.every_hart_caches_everything();
    assert!(w.harts.tlb.iter().all(|t| t.len() == 1));
    w.call(nr::PROCESS_UNMAP, [ph, X, 0, 0, 0, 0]);
    assert_eq!(w.stale(), None);
    assert!(
        w.harts.tlb.iter().all(|t| t.is_empty()),
        "every hart dropped the page"
    );
}
