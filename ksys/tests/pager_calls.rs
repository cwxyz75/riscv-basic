//! Pager calls on the host (MM9): the channel and the requests on it (once per page), supply moving frames and waking the waiters, failure,
//! and a destroyed pager failing every wait.
use abi::rights::Rights;
use abi::status::Status;
use abi::Handle;
use kcore::aspace::{AddressSpace, PagerNeed, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Quota};
use kcore::job::{JobId, Jobs};
use kcore::pagetable::PageTable;
use kcore::Mm;
use ksys::pager::*;
use ksys::{Machine, ProcState, Process, UserState};
use std::collections::BTreeMap;

mod common;
use common::FakeMem;

#[derive(Default)]
struct Tlb;
impl Machine for Tlb {
    fn shootdown(&mut self) {}
    fn flush_tlb(&mut self) {}
}

struct W {
    mm: Mm<FakeMem>,
    vmos: Vmos,
    jobs: Jobs,
    us: UserState,
    pager: u64,
    chan: u64,
}

fn world() -> W {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut us = UserState::new();
    us.procs.insert(
        1,
        Box::new(Process {
            live_tids: Default::default(),
            aspace: AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap()),
            handles: HandleTable::new(64),
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
            thread_objs: BTreeMap::new(),
            job: JobId::ROOT,
            table_charge: BTreeMap::new(),
            charge_end: BTreeMap::new(),
            stacks: BTreeMap::new(),
            stack_hint: 0,
            privileged: false,
        }),
    );
    let mut jobs = Jobs::new();
    let (pager, chan) = pager_create(&mut us, &mut jobs, 1).unwrap();
    W {
        mm,
        vmos: Vmos::new(),
        jobs,
        us,
        pager,
        chan,
    }
}

impl W {
    /// A VMO handle in process 1 for store id `id`.
    fn vmo_handle(&mut self, id: u64, rights: Rights) -> u64 {
        let p = self.us.procs.get_mut(&1).unwrap();
        ipc::vmo_object_create_with(
            &mut self.us.objs,
            &mut p.handles,
            id,
            rights,
            &mut Quota {
                jobs: &mut self.jobs,
                job: Some(JobId::ROOT),
            },
        )
        .unwrap()
        .0
    }
    fn pager_vmo(&mut self, key: u64, pages: u64) -> (u64, u64) {
        let id = self
            .vmos
            .create_pager_in(&mut self.jobs, pages, JobId::ROOT, 1, key);
        (id, self.vmo_handle(id, Rights::PAGER_VMO))
    }
    /// An anonymous VMO of `pages` committed pages that hold `fill`.
    fn source(&mut self, pages: u64, fill: u8) -> (u64, u64) {
        let id = self.vmos.create(&mut self.jobs, pages);
        self.vmos
            .write(
                &mut self.mm,
                &mut self.jobs,
                id,
                0,
                &vec![fill; pages as usize * 4096],
            )
            .unwrap();
        (id, self.vmo_handle(id, Rights::VMO))
    }
    fn begin(&mut self, tid: u64, vmo: u64, page: u64) -> Result<Vec<u64>, Status> {
        pager_wait_begin(
            &mut self.us,
            &self.vmos,
            &mut self.jobs,
            tid,
            PagerNeed {
                pager: 1,
                vmo,
                page,
            },
        )
    }
    fn requests(&mut self) -> Vec<[u64; 5]> {
        let p = self.us.procs.get_mut(&1).unwrap();
        let mut out = vec![];
        while let Ok(r) = ipc::channel_read_in(
            &mut self.us.objs,
            &mut p.handles,
            Handle(self.chan),
            64,
            0,
            &mut self.jobs,
        ) {
            let w: Vec<u64> = r
                .data
                .chunks(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                .collect();
            out.push(w.try_into().unwrap());
        }
        out
    }
    fn supply(
        &mut self,
        vmo_h: u64,
        off: u64,
        len: u64,
        src_h: u64,
        soff: u64,
    ) -> Result<Vec<u64>, Status> {
        pager_supply(
            &mut self.us,
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            &mut Tlb,
            1,
            [self.pager, vmo_h, off, len, src_h, soff],
        )
    }
}

#[test]
fn a_page_is_asked_for_once_however_many_wait_and_a_supply_wakes_them_all() {
    let mut w = world();
    let (vmo, vh) = w.pager_vmo(0x77, 8);
    w.begin(10, vmo, 3).unwrap();
    w.begin(11, vmo, 3).unwrap();
    w.begin(12, vmo, 4).unwrap();
    assert_eq!(
        w.requests(),
        vec![
            [0x77, 3 * 4096, 4096, abi::pager::READ, 0],
            [0x77, 4 * 4096, 4096, abi::pager::READ, 0]
        ],
        "one request per page, in order, with the VMO's key"
    );
    let (_, sh) = w.source(2, 0x42);
    let mut woken = w.supply(vh, 3 * 4096, 2 * 4096, sh, 0).unwrap();
    woken.sort();
    assert_eq!(woken, vec![10, 11, 12]);
    for t in [10, 11, 12] {
        assert!(pager_wait_done(&w.us, t));
        assert_eq!(pager_wait_end(&mut w.us, t), Some(Ok(())));
    }
    assert_eq!(w.us.pagers.waits(), 0);
    // the pages are in the VMO now
    let mut b = [0u8; 4];
    w.vmos.read(&mut w.mm, vmo, 3 * 4096 + 7, &mut b).unwrap();
    assert_eq!(b, [0x42; 4]);
}

#[test]
fn a_failure_wakes_the_waiters_with_the_status_and_asks_nothing_again_until_someone_waits() {
    let mut w = world();
    let (vmo, vh) = w.pager_vmo(1, 4);
    w.begin(10, vmo, 0).unwrap();
    let fail = |w: &mut W, st: i64| {
        pager_fail(&mut w.us, &w.vmos, 1, [w.pager, vh, 0, 4096, st as u64, 0])
    };
    assert_eq!(
        fail(&mut w, 0),
        Err(Status::InvalidArgs),
        "OK is not a failure"
    );
    assert_eq!(fail(&mut w, -999), Err(Status::InvalidArgs));
    assert_eq!(fail(&mut w, Status::TimedOut.code() as i64), Ok(vec![10]));
    assert_eq!(pager_wait_end(&mut w.us, 10), Some(Err(Status::TimedOut)));
    w.requests();
    w.begin(11, vmo, 0).unwrap();
    assert_eq!(w.requests().len(), 1, "a new wait asks again");
}

#[test]
fn a_destroyed_pager_fails_every_wait_and_refuses_new_ones() {
    let mut w = world();
    let (vmo, _) = w.pager_vmo(1, 4);
    w.begin(10, vmo, 0).unwrap();
    w.begin(11, vmo, 1).unwrap();
    let mut woken = pager_destroyed(&mut w.us, 1);
    woken.sort();
    assert_eq!(woken, vec![10, 11]);
    assert_eq!(pager_wait_end(&mut w.us, 10), Some(Err(Status::PeerClosed)));
    assert_eq!(pager_wait_end(&mut w.us, 11), Some(Err(Status::PeerClosed)));
    assert_eq!(w.begin(12, vmo, 0), Err(Status::PeerClosed));
    assert_eq!(w.us.pagers.waits(), 0);
}

#[test]
fn a_closed_channel_fails_the_waiters_and_a_full_one_keeps_the_request_pending() {
    let mut w = world();
    let (vmo, _) = w.pager_vmo(1, 1024);
    // fill the channel: requests that do not fit stay pending, and the waiters stay registered
    let mut sent = 0;
    for t in 0..(abi::limits::MAX_QUEUED_MSGS as u64 + 5) {
        w.begin(100 + t, vmo, t).unwrap();
        sent += 1;
    }
    assert!(sent > 0);
    assert!(w.us.pagers.any_unsent(), "the channel is full");
    let before = w.us.pagers.waits();
    assert_eq!(before as u64, abi::limits::MAX_QUEUED_MSGS as u64 + 5);
    // the pager reads some: a flush writes the rest
    let got = w.requests();
    assert!(!got.is_empty());
    assert!(pager_flush(&mut w.us, &w.vmos, &mut w.jobs, 1).is_empty());
    // the pager closes its end of the channel: the next flush fails everything
    let p = w.us.procs.get_mut(&1).unwrap();
    p.handles.close(&mut w.us.objs, Handle(w.chan)).unwrap();
    assert_eq!(w.begin(1, vmo, 1000), Err(Status::PeerClosed));
    assert!(!w.us.pagers.exists(1));
    assert!(
        !w.us.pending_wake.is_empty(),
        "the old waiters are woken through pending_wake"
    );
}

#[test]
fn a_channel_closed_after_the_request_was_sent_kills_the_pager_for_waiters_and_newcomers() {
    let mut w = world();
    let (vmo, _) = w.pager_vmo(1, 4);
    w.begin(10, vmo, 0).unwrap();
    w.requests();
    assert!(!w.us.pagers.any_unsent(), "the request was written");
    let p = w.us.procs.get_mut(&1).unwrap();
    p.handles.close(&mut w.us.objs, Handle(w.chan)).unwrap();
    assert_eq!(
        reap_closed_channels(&mut w.us),
        vec![10],
        "the waiter fails"
    );
    assert_eq!(pager_wait_end(&mut w.us, 10), Some(Err(Status::PeerClosed)));
    assert_eq!(w.begin(11, vmo, 0), Err(Status::PeerClosed));
}

#[test]
fn a_cancelled_wait_takes_its_request_and_a_supplied_page_is_not_waited_for() {
    let mut w = world();
    let (vmo, vh) = w.pager_vmo(1, 4);
    w.begin(10, vmo, 0).unwrap();
    w.begin(11, vmo, 0).unwrap();
    assert_eq!(w.us.pagers.pending_requests(), 1);
    assert_eq!(pager_wait_end(&mut w.us, 10), None);
    assert_eq!(
        w.us.pagers.pending_requests(),
        1,
        "another thread still waits"
    );
    assert_eq!(pager_wait_end(&mut w.us, 11), None);
    assert_eq!(
        w.us.pagers.pending_requests(),
        0,
        "nobody waits: nothing stays pending"
    );
    let (_, sh) = w.source(1, 3);
    w.supply(vh, 0, 4096, sh, 0).unwrap();
    assert_eq!(w.begin(12, vmo, 0), Ok(vec![]));
    assert!(
        !w.us.pagers.waiting(12),
        "the page is there: nothing registered"
    );
}

#[test]
fn supply_checks_its_arguments_in_order() {
    let mut w = world();
    let (_, vh) = w.pager_vmo(1, 4);
    let (_, sh) = w.source(2, 1);
    let (_, plain) = w.source(1, 1);
    let s = |w: &mut W, p: u64, v: u64, off: u64, len: u64, src: u64, so: u64| {
        pager_supply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            &mut Tlb,
            1,
            [p, v, off, len, src, so],
        )
        .map(|_| ())
    };
    let pg = w.pager;
    assert_eq!(s(&mut w, 0x999, vh, 0, 4096, sh, 0), Err(Status::BadHandle));
    assert_eq!(s(&mut w, vh, vh, 0, 4096, sh, 0), Err(Status::WrongType));
    assert_eq!(s(&mut w, pg, 0x999, 0, 4096, sh, 0), Err(Status::BadHandle));
    assert_eq!(s(&mut w, pg, vh, 1, 4096, sh, 0), Err(Status::InvalidArgs));
    assert_eq!(s(&mut w, pg, vh, 0, 0, sh, 0), Err(Status::InvalidArgs));
    assert_eq!(
        s(&mut w, pg, plain, 0, 4096, sh, 0),
        Err(Status::InvalidArgs),
        "not a pager's VMO"
    );
    assert_eq!(
        s(&mut w, pg, vh, 3 * 4096, 2 * 4096, sh, 0),
        Err(Status::InvalidArgs),
        "past its end"
    );
    // a source without WRITE
    let ro = {
        let p = w.us.procs.get_mut(&1).unwrap();
        p.handles
            .duplicate(&mut w.us.objs, Handle(sh), Rights::READ)
            .unwrap()
            .0
    };
    assert_eq!(s(&mut w, pg, vh, 0, 4096, ro, 0), Err(Status::AccessDenied));
    // a source range beyond what is committed, and the good one
    assert_eq!(
        s(&mut w, pg, vh, 0, 3 * 4096, sh, 0),
        Err(Status::InvalidArgs)
    );
    assert_eq!(s(&mut w, pg, vh, 0, 2 * 4096, sh, 0), Ok(()));
}
