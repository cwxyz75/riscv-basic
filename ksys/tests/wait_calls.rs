//! The blocking calls without the blocking (TS3d): what a wait checks before it sleeps, the test that ends the sleep, and how it reports on waking.
use abi::rights::Rights;
use abi::status::Status;
use abi::user::{BASE, TOP};
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::FrameDb;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Quota};
use kcore::job::JobId;
use kcore::pagetable::{PageTable, Perms};
use kcore::Mm;
use ksys::{ProcState, Process, UserState};
use std::cell::Cell;

mod common;
use common::FakeMem;

const MEM: u64 = 0x50_0000;

struct W {
    us: UserState,
    vmos: Vmos,
    jobs: kcore::job::Jobs,
    mm: Mm<FakeMem>,
    futexes: kcore::futex::FutexTable<kcore::futex::LocalBuckets>,
}

/// `futex_wake` as the kernel runs it, collecting the ids it would wake.
fn wake(w: &W, pid: u64, a: [u64; 6]) -> Result<Vec<u64>, Status> {
    let mut ids = Vec::new();
    ksys::futex_wake(&w.futexes, pid, a, |t| ids.extend_from_slice(t))?;
    Ok(ids)
}

fn world() -> W {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut vmos = Vmos::new();
    let mut jobs = kcore::job::Jobs::new();
    let mut aspace = AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap());
    let v = vmos.create(&mut jobs, 4);
    aspace
        .map_region(&mut vmos, MEM, 4, v, 0, Perms::RW.user())
        .unwrap();
    let p = Process {
        live_tids: Default::default(),
        aspace: std::sync::Arc::new(klock::aslock::AsCell::new(1, aspace)),
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
        thread_objs: Default::default(),
        job: JobId::ROOT,
        table_charge: Default::default(),
        charge_end: Default::default(),
        stacks: Default::default(),
        stack_hint: 0,
        privileged: false,
        libsys: Default::default(),
    };
    let mut us = UserState::new();
    us.procs.insert(1, Box::new(p));
    W {
        us,
        vmos,
        jobs,
        mm,
        futexes: kcore::futex::FutexTable::new(kcore::futex::LocalBuckets::new()),
    }
}

impl W {
    fn q(jobs: &mut kcore::job::Jobs) -> Quota<'_> {
        Quota {
            jobs,
            job: Some(JobId::ROOT),
        }
    }
    fn event(&mut self) -> u64 {
        let p = self.us.procs.get_mut(&1).unwrap();

        ipc::event_create_in(
            &mut self.us.objs,
            &mut p.handles,
            &mut Self::q(&mut self.jobs),
        )
        .unwrap()
        .0
    }
    fn channel(&mut self) -> (u64, u64) {
        let p = self.us.procs.get_mut(&1).unwrap();
        let (a, b) = ipc::channel_create_in(
            &mut self.us.objs,
            &mut p.handles,
            &mut Self::q(&mut self.jobs),
        )
        .unwrap();
        (a.0, b.0)
    }
    fn poke(&mut self, at: u64, d: &[u8]) {
        let p = self.us.procs.get_mut(&1).unwrap();
        kcore::usercopy::copy_to_user(
            p.aspace.held_mut(),
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            at,
            d,
        )
        .unwrap();
    }
    fn peek(&mut self, at: u64, n: usize) -> Vec<u8> {
        let p = self.us.procs.get_mut(&1).unwrap();
        kcore::usercopy::copy_from_user(
            p.aspace.held_mut(),
            &mut self.mm,
            &mut self.vmos,
            &mut self.jobs,
            at,
            n,
        )
        .unwrap()
    }
}

// ---- object_wait ----

#[test]
fn object_wait_checks_in_order_and_reports_by_what_was_seen() {
    let mut w = world();
    let e = w.event();
    let ok = ksys::object_wait_check(&w.us, 1, [e, 1 << 24, 5, 0, 0, 0]).unwrap();
    assert_eq!((ok.mask, ok.deadline), (1 << 24, 5));
    assert_eq!(
        ksys::object_wait_check(&w.us, 1, [1 << 32, 1 << 24, 5, 0, 0, 0]),
        Err(Status::BadHandle)
    );
    assert_eq!(
        ksys::object_wait_check(&w.us, 1, [0x999, 1 << 24, 5, 0, 0, 0]),
        Err(Status::BadHandle)
    );
    let no_wait =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(e), Rights::SIGNAL)
            .unwrap();
    assert_eq!(
        ksys::object_wait_check(&w.us, 1, [no_wait.0, 1 << 24, 5, 0, 0, 0]),
        Err(Status::AccessDenied)
    );
    for mask in [0u64, 1 << 32, 1 << 8] {
        assert_eq!(
            ksys::object_wait_check(&w.us, 1, [e, mask, 5, 0, 0, 0]),
            Err(Status::InvalidArgs),
            "mask {mask:#x}"
        );
    }
    assert!(
        ksys::object_wait_check(&w.us, 1, [e, 0x7000_0000, 5, 0, 0, 0]).is_ok(),
        "the user signals are waitable"
    );
    // the report: signals seen win; a pending interrupt is taken only when a wait was made (deadline != 0); else TIMED_OUT with what was seen
    let w1 = ksys::ObjectWait {
        handle: abi::Handle(1),
        mask: 1 << 24,
        deadline: 5,
    };
    let taken = Cell::new(0);
    let take = |v: bool| {
        taken.set(taken.get() + 1);
        v
    };
    assert_eq!(
        ksys::object_wait_report(Ok(1 << 24), &w1, || take(true)),
        Ok((Some(1 << 24), None))
    );
    assert_eq!(
        taken.get(),
        0,
        "a satisfied wait leaves the interrupt flag alone"
    );
    assert_eq!(
        ksys::object_wait_report(Ok(0), &w1, || take(true)),
        Err((Status::Interrupted, Some(0), None))
    );
    assert_eq!(
        ksys::object_wait_report(Ok(0), &w1, || take(false)),
        Err((Status::TimedOut, Some(0), None))
    );
    let poll = ksys::ObjectWait {
        handle: abi::Handle(1),
        mask: 1 << 24,
        deadline: 0,
    };
    taken.set(0);
    assert_eq!(
        ksys::object_wait_report(Ok(0), &poll, || take(true)),
        Err((Status::TimedOut, Some(0), None))
    );
    assert_eq!(taken.get(), 0, "a poll never takes the interrupt");
    assert_eq!(
        ksys::object_wait_report(Err(Status::BadHandle), &w1, || false),
        Err((Status::BadHandle, None, None))
    );
}

// ---- port_wait ----

#[test]
fn port_wait_takes_a_full_register_handle_and_reports() {
    assert_eq!(
        ksys::port_wait_check([1 << 32, 7, 0, 0, 0, 0]),
        Ok((abi::Handle(1 << 32), 7))
    );
    assert_eq!(
        ksys::port_wait_check([3, 7, 0, 0, 0, 0]),
        Ok((abi::Handle(3), 7))
    );
    assert_eq!(
        ksys::port_wait_report(Some((9, 1 << 24)), 5, || false),
        Ok((Some(9), Some(1 << 24)))
    );
    assert_eq!(
        ksys::port_wait_report(None, 5, || true),
        Err((Status::Interrupted, None, None))
    );
    assert_eq!(
        ksys::port_wait_report(None, 5, || false),
        Err((Status::TimedOut, None, None))
    );
    assert_eq!(
        ksys::port_wait_report(None, 0, || true),
        Err((Status::TimedOut, None, None)),
        "a poll is never interrupted"
    );
}

#[test]
fn a_port_poll_finds_a_ready_binding() {
    let mut w = world();
    let p = w.us.procs.get_mut(&1).unwrap();
    let port = ipc::port_create_in(&mut w.us.objs, &mut p.handles, &mut W::q(&mut w.jobs)).unwrap();
    let e = w.event();
    let p = w.us.procs.get(&1).unwrap();
    ipc::port_bind_in(
        &mut w.us.objs,
        &p.handles,
        port,
        abi::Handle(e),
        1 << 24,
        77,
        &mut w.jobs,
    )
    .unwrap();
    assert_eq!(ksys::port_poll(&mut w.us, &mut w.jobs, 1, port), Ok(None));
    ipc::event_signal(
        &mut w.us.objs,
        &w.us.procs[&1].handles,
        abi::Handle(e),
        0,
        1 << 24,
    )
    .unwrap();
    assert_eq!(
        ksys::port_poll(&mut w.us, &mut w.jobs, 1, port).unwrap(),
        Some((77, 1 << 24))
    );
    assert_eq!(
        ksys::port_poll(&mut w.us, &mut w.jobs, 1, abi::Handle(0x999)),
        Err(Status::BadHandle)
    );
}

// ---- futex ----

#[test]
fn futex_wait_compares_polls_and_enqueues() {
    let mut w = world();
    w.poke(MEM, &5u32.to_le_bytes());
    let who = 42;
    let check = |w: &mut W, addr: u64, expected: u64, deadline: u64| {
        ksys::futex_wait_check(
            &mut w.us,
            &w.futexes,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            who,
            [addr, expected, deadline, 0, 0, 0],
        )
    };
    assert_eq!(
        check(&mut w, MEM, 1 << 32, 9),
        Err(Status::InvalidArgs),
        "expected is 32 bits"
    );
    assert_eq!(
        check(&mut w, MEM + 1, 5, 9),
        Err(Status::InvalidArgs),
        "an unaligned word"
    );
    assert_eq!(
        check(&mut w, 0x10, 5, 9),
        Err(Status::InvalidArgs),
        "an unmapped word"
    );
    assert_eq!(
        check(&mut w, MEM, 6, 9).unwrap(),
        ksys::FutexCheck::Done(Status::ShouldWait),
        "the word changed: do not sleep"
    );
    assert_eq!(
        check(&mut w, MEM, 5, 0).unwrap(),
        ksys::FutexCheck::Done(Status::TimedOut),
        "a poll"
    );
    assert!(
        !w.futexes.contains((1, MEM), who),
        "neither refusal enqueued"
    );
    assert_eq!(check(&mut w, MEM, 5, 9).unwrap(), ksys::FutexCheck::Sleep);
    assert!(w.futexes.contains((1, MEM), who));
    // woken by futex_wake: removed from the table, so the report is a plain success
    let ids = wake(&w, 1, [MEM, 1, 0, 0, 0, 0]).unwrap();
    assert_eq!(ids, vec![who]);
    assert_eq!(
        ksys::futex_wait_report(&w.futexes, (1, MEM), who, || true),
        Ok((None, None))
    );
    // a sleeper whose deadline passed is still in the table: the report removes it
    assert_eq!(check(&mut w, MEM, 5, 9).unwrap(), ksys::FutexCheck::Sleep);
    assert_eq!(
        ksys::futex_wait_report(&w.futexes, (1, MEM), who, || false),
        Err((Status::TimedOut, None, None))
    );
    assert!(!w.futexes.contains((1, MEM), who));
    assert_eq!(check(&mut w, MEM, 5, 9).unwrap(), ksys::FutexCheck::Sleep);
    assert_eq!(
        ksys::futex_wait_report(&w.futexes, (1, MEM), who, || true),
        Err((Status::Interrupted, None, None))
    );
}

#[test]
fn futex_wake_checks_the_address_and_counts() {
    let mut w = world();
    for addr in [BASE - 4, BASE + 2, TOP - 2, u64::MAX - 1, 0] {
        assert_eq!(
            wake(&w, 1, [addr, 1, 0, 0, 0, 0]),
            Err(Status::InvalidArgs),
            "{addr:#x}"
        );
    }
    w.poke(MEM, &5u32.to_le_bytes());
    for who in [1u64, 2, 3] {
        assert_eq!(
            ksys::futex_wait_check(
                &mut w.us,
                &w.futexes,
                &mut w.mm,
                &mut w.vmos,
                &mut w.jobs,
                1,
                who,
                [MEM, 5, 9, 0, 0, 0]
            )
            .unwrap(),
            ksys::FutexCheck::Sleep
        );
    }
    assert_eq!(
        wake(&w, 1, [MEM, 2, 0, 0, 0, 0]).unwrap(),
        vec![1, 2],
        "oldest first, at most count"
    );
    assert_eq!(wake(&w, 1, [MEM, 9, 0, 0, 0, 0]).unwrap(), vec![3]);
    assert_eq!(
        wake(&w, 1, [MEM, 9, 0, 0, 0, 0]).unwrap(),
        Vec::<u64>::new()
    );
}

// ---- channel_call ----

/// A call record at MEM: request bytes at MEM+0x100 (a transaction id then a body), reply buffer at MEM+0x400.
fn call_record(w: &mut W, ch: u64, nbytes: u32, deadline: u64) -> [u64; 6] {
    let mut rec = Vec::new();
    for v in [MEM + 0x100, MEM + 0x200] {
        rec.extend_from_slice(&v.to_le_bytes());
    }
    rec.extend_from_slice(&nbytes.to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes());
    for v in [MEM + 0x400, MEM + 0x500] {
        rec.extend_from_slice(&v.to_le_bytes());
    }
    rec.extend_from_slice(&64u32.to_le_bytes());
    rec.extend_from_slice(&4u32.to_le_bytes());
    rec.extend_from_slice(&deadline.to_le_bytes());
    w.poke(MEM, &rec);
    w.poke(MEM + 0x100, &[7, 0, 0, 0, 1, 2, 3, 4]);
    [ch, MEM, 0, 0, 0, 0]
}

fn send(w: &mut W, a: [u64; 6], interrupted: bool) -> Result<ksys::Call, Status> {
    ksys::channel_call_send(
        &mut w.us,
        &mut w.mm,
        &mut w.vmos,
        &mut w.jobs,
        1,
        7,
        a,
        || interrupted,
    )
}

#[test]
fn a_call_sends_waits_for_its_transaction_id_and_takes_the_reply() {
    let mut w = world();
    let (client, server) = w.channel();
    let a = call_record(&mut w, client, 8, 9);
    let call = send(&mut w, a, false).unwrap();
    assert_eq!((call.txid, call.deadline), (7, 9));
    // the request is on the server's end; nothing is ready for the client yet
    assert!(!ksys::channel_call_ready(&w.us, 1, &call, false));
    assert!(
        ksys::channel_call_ready(&w.us, 1, &call, true),
        "an interrupt ends the sleep"
    );
    w.us.procs.get_mut(&1).unwrap().killed = Some(1);
    assert!(
        ksys::channel_call_ready(&w.us, 1, &call, false),
        "so does a kill"
    );
    w.us.procs.get_mut(&1).unwrap().killed = None;
    assert!(
        ksys::channel_call_ready(&w.us, 9, &call, false),
        "and a process that is gone"
    );
    // an unrelated reply does not end it; the one with the transaction id does
    let ph =
        w.us.procs
            .get(&1)
            .unwrap()
            .handles
            .get(abi::Handle(server))
            .unwrap();
    let _ = ph;
    let mut other = vec![8u8, 0, 0, 0];
    other.extend_from_slice(b"no");
    ipc::channel_write_in(
        &mut w.us.objs,
        &mut w.us.procs.get_mut(&1).unwrap().handles,
        abi::Handle(server),
        &other,
        &[],
        &mut w.jobs,
    )
    .unwrap();
    assert!(!ksys::channel_call_ready(&w.us, 1, &call, false));
    let mut mine = vec![7u8, 0, 0, 0];
    mine.extend_from_slice(b"yes");
    ipc::channel_write_in(
        &mut w.us.objs,
        &mut w.us.procs.get_mut(&1).unwrap().handles,
        abi::Handle(server),
        &mine,
        &[],
        &mut w.jobs,
    )
    .unwrap();
    assert!(ksys::channel_call_ready(&w.us, 1, &call, false));
    let r = ksys::channel_call_reply(
        &mut w.us,
        &mut w.mm,
        &mut w.vmos,
        &mut w.jobs,
        1,
        &call,
        || false,
    );
    assert_eq!(r, Ok((Some(7), Some(0))));
    assert_eq!(w.peek(MEM + 0x400, 7), mine);
}

#[test]
fn the_request_half_checks_in_order() {
    let mut w = world();
    let (client, _server) = w.channel();
    let good = call_record(&mut w, client, 8, 9);
    assert_eq!(
        send(&mut w, [1 << 32, MEM, 0, 0, 0, 0], false),
        Err(Status::BadHandle)
    );
    assert_eq!(
        send(&mut w, [client, 0x10, 0, 0, 0, 0], false),
        Err(Status::InvalidArgs),
        "the record is read first"
    );
    let a = call_record(&mut w, client, abi::limits::MAX_MSG_BYTES as u32 + 1, 9);
    assert_eq!(send(&mut w, a, false), Err(Status::TooBig));
    let a = call_record(&mut w, client, 3, 9);
    assert_eq!(
        send(&mut w, a, false),
        Err(Status::InvalidArgs),
        "a request needs its transaction id"
    );
    let a = call_record(&mut w, client, 8, 9);
    assert_eq!(
        send(&mut w, a, true),
        Err(Status::Interrupted),
        "an interrupt ends it before anything is sent"
    );
    let a = call_record(&mut w, client, 8, 0);
    assert!(send(&mut w, a, true).is_ok(), "a poll is not interrupted");
    let a = call_record(&mut w, 0x999, 8, 9);
    assert_eq!(send(&mut w, a, false), Err(Status::BadHandle));
    let e = w.event();
    let a = call_record(&mut w, e, 8, 9);
    assert_eq!(send(&mut w, a, false), Err(Status::WrongType));
    let ro =
        w.us.procs
            .get_mut(&1)
            .unwrap()
            .handles
            .duplicate(&mut w.us.objs, abi::Handle(client), Rights::READ)
            .unwrap();
    let a = call_record(&mut w, ro.0, 8, 9);
    assert_eq!(send(&mut w, a, false), Err(Status::AccessDenied));
    let _ = good;
}

#[test]
fn the_reply_half_reports_by_what_is_there() {
    let mut w = world();
    let (client, server) = w.channel();
    let a = call_record(&mut w, client, 8, 9);
    let call = send(&mut w, a, false).unwrap();
    // nothing came: TIMED_OUT, or INTERRUPTED when an interrupt ended the wait (the request stays queued)
    assert_eq!(
        ksys::channel_call_reply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            &call,
            || false
        ),
        Err((Status::TimedOut, None, None))
    );
    assert_eq!(
        ksys::channel_call_reply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            &call,
            || true
        ),
        Err((Status::Interrupted, None, None))
    );
    // a reply too big for the buffer: the sizes come back, the message stays
    let mut big = vec![7u8, 0, 0, 0];
    big.extend_from_slice(&[0u8; 100]);
    ipc::channel_write_in(
        &mut w.us.objs,
        &mut w.us.procs.get_mut(&1).unwrap().handles,
        abi::Handle(server),
        &big,
        &[],
        &mut w.jobs,
    )
    .unwrap();
    assert_eq!(
        ksys::channel_call_reply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            &call,
            || false
        ),
        Err((Status::BufferTooSmall, Some(104), Some(0)))
    );
    // a bad reply buffer loses nothing: the checks come before the message is taken
    let mut c2 = ksys::Call {
        rd_bytes: 0x10,
        ..call
    };
    c2.rd_max_bytes = 200;
    assert_eq!(
        ksys::channel_call_reply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            &c2,
            || false
        ),
        Err((Status::InvalidArgs, None, None))
    );
    c2.rd_bytes = MEM + 0x400;
    assert_eq!(
        ksys::channel_call_reply(
            &mut w.us,
            &mut w.mm,
            &mut w.vmos,
            &mut w.jobs,
            1,
            &c2,
            || false
        ),
        Ok((Some(104), Some(0))),
        "the message was still there"
    );
}
