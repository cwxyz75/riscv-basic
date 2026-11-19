//! The syscall layer on the host (TS3a): the error rows of the moved calls in SYSCALLS.md, run against a fake machine whose user
//! memory is one window of bytes.
use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use kcore::aspace::{AddressSpace, Vmos};
use kcore::frame::{FrameDb, Pfn};
use kcore::handle::HandleTable;
use kcore::ipc::Kobjs;
use kcore::job::{JobId, Limits};
use kcore::pagetable::{PageTable, Perms, PhysMem};
use kcore::Mm;
use ksys::{ipc_call, wide_argument_error, Ctx, Faults, Reply};
use std::collections::HashMap;

/// Physical memory as a map from frame number to 512 words, created on demand (the kernel's direct map, on the host).
#[derive(Default)]
struct FakeMem {
    frames: HashMap<Pfn, Box<[u64; 512]>>,
}

impl PhysMem for FakeMem {
    fn read_u64(&self, pa: u64) -> u64 {
        self.frames
            .get(&(pa >> 12))
            .map_or(0xdead_beef_dead_beef, |f| f[((pa & 0xfff) / 8) as usize])
    }
    fn write_u64(&mut self, pa: u64, value: u64) {
        self.frames
            .entry(pa >> 12)
            .or_insert_with(|| Box::new([0; 512]))[((pa & 0xfff) / 8) as usize] = value;
    }
}

/// A user page range mapped read-write (16 pages, lazily backed) and one mapped read-only page.
const BASE: u64 = 0x1_0000;
const RO: u64 = 0x20_0000;

struct World {
    objs: Kobjs,
    handles: HandleTable,
    mm: Mm<FakeMem>,
    vmos: Vmos,
    aspace: AddressSpace,
    job: JobId,
    faults: Faults,
}

fn world_with(handles: usize) -> World {
    let mut mm = Mm {
        db: FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = PageTable::new(&mut mm).unwrap();
    let mut aspace = AddressSpace::new(PageTable::new_with_kernel_half(&mut mm, &kernel).unwrap());
    let mut vmos = Vmos::new();
    let rw = vmos.create(16);
    aspace
        .map_region(&vmos, BASE, 16, rw, 0, Perms::RW.user())
        .unwrap();
    let ro = vmos.create(1);
    aspace
        .map_region(&vmos, RO, 1, ro, 0, Perms::R.user())
        .unwrap();
    World {
        objs: Kobjs::new(),
        handles: HandleTable::new(handles),
        mm,
        vmos,
        aspace,
        job: JobId::ROOT,
        faults: Faults::default(),
    }
}

fn world() -> World {
    world_with(64)
}

impl World {
    fn call(&mut self, n: usize, a: [u64; 6]) -> Option<Reply> {
        let mut c = Ctx {
            objs: &mut self.objs,
            handles: &mut self.handles,
            job: self.job,
            aspace: &mut self.aspace,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            faults: self.faults,
        };
        ipc_call(n, a, &mut c)
    }
    fn ok(&mut self, n: usize, a: [u64; 6]) -> (Option<u64>, Option<u64>) {
        self.call(n, a).expect("handled").expect("succeeds")
    }
    fn err(&mut self, n: usize, a: [u64; 6]) -> Status {
        self.call(n, a).expect("handled").expect_err("fails").0
    }
    fn pair(&mut self, n: usize) -> (u64, u64) {
        let (x, y) = self.ok(n, [0; 6]);
        (x.unwrap(), y.unwrap())
    }
    fn poke(&mut self, at: u64, d: &[u8]) {
        kcore::usercopy::copy_to_user(&mut self.aspace, &mut self.mm, &mut self.vmos, at, d)
            .unwrap();
    }
    fn peek(&mut self, at: u64, n: usize) -> Vec<u8> {
        kcore::usercopy::copy_from_user(&mut self.aspace, &mut self.mm, &mut self.vmos, at, n)
            .unwrap()
    }
    /// A duplicate of `h` with exactly `rights`.
    fn narrow(&mut self, h: u64, rights: Rights) -> u64 {
        self.ok(nr::HANDLE_DUPLICATE, [h, u64::from(rights.0), 0, 0, 0, 0])
            .0
            .unwrap()
    }
}

#[test]
fn calls_that_are_not_these_are_left_to_the_kernel() {
    let mut w = world();
    assert!(w.call(nr::VMO_CREATE, [0; 6]).is_none());
    assert!(w.call(nr::PROCESS_CREATE, [0; 6]).is_none());
    assert!(w.call(9999, [0; 6]).is_none());
}

#[test]
fn wide_arguments_are_refused_not_truncated() {
    let big = 1u64 << 32;
    let a = |i: usize| {
        let mut a = [0u64; 6];
        a[i] = big;
        a
    };
    assert_eq!(
        wide_argument_error(nr::HANDLE_CLOSE, &a(0)),
        Some(Status::BadHandle)
    );
    assert_eq!(
        wide_argument_error(nr::HANDLE_DUPLICATE, &a(1)),
        Some(Status::AccessDenied)
    );
    assert_eq!(
        wide_argument_error(nr::EVENT_SIGNAL, &a(1)),
        Some(Status::InvalidArgs)
    );
    assert_eq!(
        wide_argument_error(nr::EVENT_SIGNAL, &a(2)),
        Some(Status::InvalidArgs)
    );
    assert_eq!(
        wide_argument_error(nr::PORT_BIND, &a(1)),
        Some(Status::BadHandle)
    );
    assert_eq!(
        wide_argument_error(nr::VMO_CREATE, &a(1)),
        Some(Status::BadHandle)
    );
    assert_eq!(
        wide_argument_error(nr::CHANNEL_READ_TXID, &a(1)),
        Some(Status::InvalidArgs)
    );
    assert_eq!(
        wide_argument_error(nr::HANDLE_CLOSE, &[u64::from(u32::MAX), 0, 0, 0, 0, 0]),
        None
    );
    assert_eq!(wide_argument_error(nr::CHANNEL_CREATE, &[big; 6]), None);
}

#[test]
fn handles_close_duplicate_replace() {
    let mut w = world();
    let (x, _y) = w.pair(nr::CHANNEL_CREATE);
    let same = u64::from(abi::rights::Rights::SAME.0);
    assert_eq!(
        w.err(nr::HANDLE_CLOSE, [0xdead, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let d = w.ok(nr::HANDLE_DUPLICATE, [x, same, 0, 0, 0, 0]).0.unwrap();
    assert_ne!(d, x);
    assert_eq!(
        w.err(nr::HANDLE_DUPLICATE, [x, 0x7fff_ffff, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    let r = w.ok(nr::HANDLE_REPLACE, [d, same, 0, 0, 0, 0]).0.unwrap();
    assert_eq!(
        w.err(nr::HANDLE_CLOSE, [d, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    w.ok(nr::HANDLE_CLOSE, [r, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::HANDLE_CLOSE, [r, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
}

#[test]
fn channel_round_trip_with_a_handle() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    w.poke(BASE, b"hello");
    w.poke(BASE + 0x100, &(e as u32).to_le_bytes());
    w.ok(nr::CHANNEL_WRITE, [x, BASE, 5, BASE + 0x100, 1, 0]);
    assert_eq!(
        w.err(nr::HANDLE_CLOSE, [e, 0, 0, 0, 0, 0]),
        Status::BadHandle,
        "the handle moved out of the sender"
    );
    let (nb, nh) = w.ok(nr::CHANNEL_READ, [y, BASE + 0x200, 64, BASE + 0x300, 4, 0]);
    assert_eq!((nb, nh), (Some(5), Some(1)));
    assert_eq!(w.peek(BASE + 0x200, 5), b"hello");
    let got = u32::from_le_bytes(w.peek(BASE + 0x300, 4).try_into().unwrap());
    w.ok(nr::HANDLE_CLOSE, [u64::from(got), 0, 0, 0, 0, 0]);
}

#[test]
fn channel_write_errors() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(
            nr::CHANNEL_WRITE,
            [x, BASE, MAX_MSG_BYTES as u64 + 1, BASE, 0, 0]
        ),
        Status::TooBig
    );
    assert_eq!(
        w.err(
            nr::CHANNEL_WRITE,
            [x, BASE, 0, BASE, MAX_MSG_HANDLES as u64 + 1, 0]
        ),
        Status::TooBig
    );
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, 0x10, 4, BASE, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 4, 0x10, 1, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [0x999, BASE, 4, BASE, 0, 0]),
        Status::BadHandle
    );
    w.poke(BASE, &0x777u32.to_le_bytes());
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 0, BASE, 1, 0]),
        Status::BadHandle
    );
    w.ok(nr::HANDLE_CLOSE, [y, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 1, BASE, 0, 0]),
        Status::PeerClosed
    );
}

#[test]
fn channel_read_errors_lose_no_message() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, BASE, 8, BASE, 0, 0]),
        Status::ShouldWait
    );
    w.poke(BASE, b"0123456789");
    w.ok(nr::CHANNEL_WRITE, [x, BASE, 10, BASE, 0, 0]);
    let e = w
        .call(nr::CHANNEL_READ, [y, BASE + 0x200, 4, BASE + 0x300, 0, 0])
        .unwrap()
        .unwrap_err();
    assert_eq!(e, (Status::BufferTooSmall, Some(10), Some(0)));
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, 0x10, 64, BASE + 0x300, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, RO, 64, BASE + 0x300, 0, 0]),
        Status::InvalidArgs
    );
    let (nb, _) = w.ok(
        nr::CHANNEL_READ,
        [y, BASE + 0x200, u64::MAX >> 1, BASE + 0x300, 0, 0],
    );
    assert_eq!(
        nb,
        Some(10),
        "a generous declared capacity needs no generous mapping"
    );
    assert_eq!(w.peek(BASE + 0x200, 10), b"0123456789");
    w.ok(nr::HANDLE_CLOSE, [x, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, BASE, 8, BASE, 0, 0]),
        Status::PeerClosed
    );
}

#[test]
fn socket_round_trip_and_errors() {
    let mut w = world();
    let (x, y) = w.pair(nr::SOCKET_CREATE);
    assert_eq!(
        w.err(nr::SOCKET_READ, [y, BASE, 8, 0, 0, 0]),
        Status::ShouldWait
    );
    w.poke(BASE, b"abcdef");
    assert_eq!(w.ok(nr::SOCKET_WRITE, [x, BASE, 6, 0, 0, 0]).0, Some(6));
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [x, 0x10, 2, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::SOCKET_READ, [y, 0x10, 8, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.ok(nr::SOCKET_READ, [y, BASE + 0x400, 4, 0, 0, 0]).0,
        Some(4),
        "a bad destination lost no data"
    );
    assert_eq!(w.peek(BASE + 0x400, 4), b"abcd");
    assert_eq!(
        w.ok(nr::SOCKET_READ, [y, BASE + 0x400, 4, 0, 0, 0]).0,
        Some(2)
    );
    let (c, _) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [c, BASE, 1, 0, 0, 0]),
        Status::WrongType
    );
    assert_eq!(
        w.err(nr::SOCKET_READ, [c, BASE, 1, 0, 0, 0]),
        Status::WrongType
    );
    w.ok(nr::SOCKET_SHUTDOWN, [x, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [x, BASE, 1, 0, 0, 0]),
        Status::BadState
    );
    assert_eq!(
        w.err(nr::SOCKET_SHUTDOWN, [0x55, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
}

#[test]
fn events_objects_and_ports() {
    let mut w = world();
    let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    assert_eq!(
        w.err(nr::EVENT_SIGNAL, [0x44, 0, 1, 0, 0, 0]),
        Status::BadHandle
    );
    w.ok(nr::EVENT_SIGNAL, [e, 0, 1 << 24, 0, 0, 0]);
    assert_eq!(
        w.ok(nr::OBJECT_SIGNALS, [e, 0, 0, 0, 0, 0])
            .0
            .map(|s| s & (1 << 24)),
        Some(1 << 24)
    );
    assert_eq!(
        w.err(nr::OBJECT_SIGNALS, [0x44, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert!(w.ok(nr::OBJECT_INFO, [e, 0, 0, 0, 0, 0]).0.is_some());
    assert_eq!(
        w.err(nr::OBJECT_INFO, [0x44, 0, 0, 0, 0, 0]),
        Status::BadHandle
    );
    let p = w.ok(nr::PORT_CREATE, [0; 6]).0.unwrap();
    assert_eq!(
        w.err(nr::PORT_BIND, [0x44, p, 1, 1, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::PORT_UNBIND, [e, 1, 0, 0, 0, 0]),
        Status::WrongType
    );
    assert_eq!(w.err(nr::PORT_REARM, [e, 1, 0, 0, 0, 0]), Status::WrongType);
}

#[test]
fn a_full_handle_table_is_an_error() {
    let mut w = world();
    let mut seen = std::collections::BTreeSet::new();
    let mut failed = None;
    for _ in 0..100 {
        match w.call(nr::EVENT_CREATE, [0; 6]).unwrap() {
            Ok((Some(h), _)) => assert!(seen.insert(h), "handles are distinct"),
            Ok(_) => panic!("an event create returns a handle"),
            Err((s, _, _)) => {
                failed = Some(s);
                break;
            }
        }
    }
    assert_eq!(failed, Some(Status::NoMemory));
    assert_eq!(seen.len(), 64);
}

const SAME: u64 = Rights::SAME.0 as u64;
const SIG0: u64 = 1 << 24;

#[test]
fn every_wide_argument_is_refused_table_driven() {
    // (call, argument index, status): the whole table of `wide_argument_error`, probed one entry at a time, and a clean value passes.
    let table: &[(usize, usize, Status)] = &[
        (nr::HANDLE_CLOSE, 0, Status::BadHandle),
        (nr::OBJECT_SIGNALS, 0, Status::BadHandle),
        (nr::OBJECT_INFO, 0, Status::BadHandle),
        (nr::CHANNEL_WRITE, 0, Status::BadHandle),
        (nr::CHANNEL_READ, 0, Status::BadHandle),
        (nr::CHANNEL_READ_TXID, 0, Status::BadHandle),
        (nr::CHANNEL_READ_TXID, 1, Status::InvalidArgs),
        (nr::VMO_READ, 0, Status::BadHandle),
        (nr::VMO_WRITE, 0, Status::BadHandle),
        (nr::VMO_GET_SIZE, 0, Status::BadHandle),
        (nr::VMO_PHYS_ADDR, 0, Status::BadHandle),
        (nr::VMO_COMMIT, 0, Status::BadHandle),
        (nr::VMO_CREATE_PHYSICAL, 0, Status::BadHandle),
        (nr::VMO_CREATE_DMA, 0, Status::BadHandle),
        (nr::RESOURCE_DTB, 0, Status::BadHandle),
        (nr::INTERRUPT_CREATE, 0, Status::BadHandle),
        (nr::INTERRUPT_ACK, 0, Status::BadHandle),
        (nr::VMO_CREATE, 1, Status::BadHandle),
        (nr::SOCKET_WRITE, 0, Status::BadHandle),
        (nr::SOCKET_READ, 0, Status::BadHandle),
        (nr::SOCKET_SHUTDOWN, 0, Status::BadHandle),
        (nr::PORT_UNBIND, 0, Status::BadHandle),
        (nr::PORT_REARM, 0, Status::BadHandle),
        (nr::PORT_BIND, 0, Status::BadHandle),
        (nr::PORT_BIND, 1, Status::BadHandle),
        (nr::HANDLE_DUPLICATE, 0, Status::BadHandle),
        (nr::HANDLE_REPLACE, 0, Status::BadHandle),
        (nr::HANDLE_DUPLICATE, 1, Status::AccessDenied),
        (nr::HANDLE_REPLACE, 1, Status::AccessDenied),
        (nr::EVENT_SIGNAL, 0, Status::BadHandle),
        (nr::EVENT_SIGNAL, 1, Status::InvalidArgs),
        (nr::EVENT_SIGNAL, 2, Status::InvalidArgs),
    ];
    for &(n, i, st) in table {
        let mut a = [0u64; 6];
        a[i] = 1 << 32;
        assert_eq!(
            wide_argument_error(n, &a),
            Some(st),
            "call {n} argument {i}"
        );
        a[i] = u64::from(u32::MAX);
        assert_eq!(
            wide_argument_error(n, &a),
            None,
            "call {n} argument {i} at the largest 32-bit value"
        );
    }
}

#[test]
fn channel_read_txid_picks_its_message_and_orders_its_errors() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    assert_eq!(
        w.err(nr::CHANNEL_READ_TXID, [y, 7, BASE, 64, BASE + 0x400, 0]),
        Status::ShouldWait,
        "nothing matches, peer alive"
    );
    for (tx, body) in [(5u32, b"five"), (7, b"sevn")] {
        let mut m = tx.to_le_bytes().to_vec();
        m.extend_from_slice(body);
        w.poke(BASE + 0x800, &m);
        w.ok(nr::CHANNEL_WRITE, [x, BASE + 0x800, 8, BASE, 0, 0]);
    }
    // the arguments are shifted by one against channel_read: txid, bytes, max_bytes, handles, max_handles
    let r = w.ok(
        nr::CHANNEL_READ_TXID,
        [y, 7, BASE + 0x100, 64, BASE + 0x200, 0],
    );
    assert_eq!(r, (Some(8), Some(0)));
    assert_eq!(
        &w.peek(BASE + 0x100, 8)[4..],
        b"sevn",
        "the later message was taken, the earlier one skipped"
    );
    let small = w
        .call(
            nr::CHANNEL_READ_TXID,
            [y, 5, BASE + 0x100, 4, BASE + 0x200, 0],
        )
        .unwrap()
        .unwrap_err();
    assert_eq!(small, (Status::BufferTooSmall, Some(8), Some(0)));
    assert_eq!(
        w.err(nr::CHANNEL_READ_TXID, [y, 5, 0x10, 64, BASE + 0x200, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.ok(
            nr::CHANNEL_READ_TXID,
            [y, 5, BASE + 0x100, 64, BASE + 0x200, 0]
        )
        .0,
        Some(8),
        "the failures lost nothing"
    );
    // no match and the peer gone: PEER_CLOSED, not a wait that never ends
    w.ok(nr::HANDLE_CLOSE, [x, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::CHANNEL_READ_TXID, [y, 9, BASE, 64, BASE + 0x200, 0]),
        Status::PeerClosed
    );
    assert_eq!(
        w.err(nr::CHANNEL_READ_TXID, [0x999, 9, BASE, 64, BASE + 0x200, 0]),
        Status::BadHandle
    );
}

#[test]
fn channel_write_rights_and_handle_list_rules() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let (c, _) = w.pair(nr::CHANNEL_CREATE);
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    // wrong type, and a handle without WRITE
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [ev, BASE, 0, BASE, 0, 0]),
        Status::WrongType
    );
    let no_write = w.narrow(x, Rights::READ);
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [no_write, BASE, 0, BASE, 0, 0]),
        Status::AccessDenied
    );
    // a listed handle without TRANSFER
    let no_xfer = w.narrow(ev, Rights::SIGNAL);
    w.poke(BASE, &(no_xfer as u32).to_le_bytes());
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 0, BASE, 1, 0]),
        Status::AccessDenied
    );
    // a handle listed twice
    let mut two = (c as u32).to_le_bytes().to_vec();
    two.extend_from_slice(&(c as u32).to_le_bytes());
    w.poke(BASE, &two);
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 0, BASE, 2, 0]),
        Status::InvalidArgs
    );
    // the channel's own endpoint in the list
    w.poke(BASE, &(x as u32).to_le_bytes());
    assert_eq!(
        w.err(nr::CHANNEL_WRITE, [x, BASE, 0, BASE, 1, 0]),
        Status::NotSupported
    );
    // after all those failures every handle is still there
    for h in [x, c, ev, no_xfer] {
        assert!(
            w.call(nr::OBJECT_SIGNALS, [h, 0, 0, 0, 0, 0])
                .unwrap()
                .is_ok(),
            "handle {h} survived the failed writes"
        );
    }
    let _ = y;
}

#[test]
fn channel_write_fills_the_queue() {
    let mut w = world();
    let (x, _y) = w.pair(nr::CHANNEL_CREATE);
    let mut full = false;
    for _ in 0..100_000 {
        match w.call(nr::CHANNEL_WRITE, [x, BASE, 1, BASE, 0, 0]).unwrap() {
            Ok(_) => {}
            Err((s, _, _)) => {
                assert_eq!(s, Status::ShouldWait);
                full = true;
                break;
            }
        }
    }
    assert!(full, "a channel queue is bounded");
}

#[test]
fn channel_read_rights_handle_room_and_error_ordering() {
    let mut w = world_with(8);
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    assert_eq!(
        w.err(nr::CHANNEL_READ, [0x999, BASE, 8, BASE, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::CHANNEL_READ, [ev, BASE, 8, BASE, 0, 0]),
        Status::WrongType
    );
    let no_read = w.narrow(y, Rights::WRITE);
    assert_eq!(
        w.err(nr::CHANNEL_READ, [no_read, BASE, 8, BASE, 0, 0]),
        Status::AccessDenied
    );
    // ordering: an empty queue is SHOULD_WAIT even with a wild pointer
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, 0x10, 8, 0x10, 4, 0]),
        Status::ShouldWait
    );
    // send a message carrying one handle
    w.poke(BASE, b"msg!");
    w.poke(BASE + 0x100, &(ev as u32).to_le_bytes());
    w.ok(nr::CHANNEL_WRITE, [x, BASE, 4, BASE + 0x100, 1, 0]);
    // too small for the handle, reported with both sizes, and that wins over a bad pointer
    let e = w
        .call(nr::CHANNEL_READ, [y, 0x10, 64, 0x10, 0, 0])
        .unwrap()
        .unwrap_err();
    assert_eq!(e, (Status::BufferTooSmall, Some(4), Some(1)));
    // a bad handles pointer, with the bytes pointer good
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, BASE + 0x200, 64, 0x10, 4, 0]),
        Status::InvalidArgs
    );
    // the table has no room: the message stays queued
    let mut fill = vec![];
    while let Ok((Some(h), _)) = w.call(nr::EVENT_CREATE, [0; 6]).unwrap() {
        fill.push(h);
    }
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, BASE + 0x200, 64, BASE + 0x300, 4, 0]),
        Status::NoMemory
    );
    w.ok(nr::HANDLE_CLOSE, [fill[0], 0, 0, 0, 0, 0]);
    assert_eq!(
        w.ok(nr::CHANNEL_READ, [y, BASE + 0x200, 64, BASE + 0x300, 4, 0]),
        (Some(4), Some(1))
    );
}

#[test]
fn handle_duplicate_and_replace_rows() {
    let mut w = world();
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    assert_eq!(
        w.err(nr::HANDLE_DUPLICATE, [0x999, SAME, 0, 0, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::HANDLE_REPLACE, [0x999, SAME, 0, 0, 0, 0]),
        Status::BadHandle
    );
    // an unassigned bit and a right the handle lacks are both refused, the second also when the bit is assigned
    assert_eq!(
        w.err(nr::HANDLE_DUPLICATE, [ev, 1 << 30, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    let signal_only = w.narrow(ev, Rights::SIGNAL);
    assert_eq!(
        w.err(
            nr::HANDLE_DUPLICATE,
            [signal_only, u64::from(Rights::SIGNAL.0), 0, 0, 0, 0]
        ),
        Status::AccessDenied,
        "no DUPLICATE right"
    );
    let dup_wait = w.narrow(ev, Rights(Rights::DUPLICATE.0 | Rights::WAIT.0));
    assert_eq!(
        w.err(
            nr::HANDLE_DUPLICATE,
            [dup_wait, u64::from(Rights::SIGNAL.0), 0, 0, 0, 0]
        ),
        Status::AccessDenied,
        "a right it does not have"
    );
    assert_eq!(
        w.err(
            nr::HANDLE_REPLACE,
            [dup_wait, u64::from(Rights::SIGNAL.0), 0, 0, 0, 0]
        ),
        Status::AccessDenied
    );
    assert!(
        w.call(nr::OBJECT_SIGNALS, [dup_wait, 0, 0, 0, 0, 0])
            .unwrap()
            .is_ok(),
        "the handle is still valid after the failed replace"
    );
}

#[test]
fn handle_duplicate_with_a_full_table() {
    let mut w = world_with(2);
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    w.ok(nr::HANDLE_DUPLICATE, [ev, SAME, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::HANDLE_DUPLICATE, [ev, SAME, 0, 0, 0, 0]),
        Status::NoMemory
    );
}

#[test]
fn event_signal_rows() {
    let mut w = world();
    let (c, _) = w.pair(nr::CHANNEL_CREATE);
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    assert_eq!(
        w.err(nr::EVENT_SIGNAL, [c, 0, SIG0, 0, 0, 0]),
        Status::WrongType
    );
    let no_sig = w.narrow(ev, Rights::WAIT);
    assert_eq!(
        w.err(nr::EVENT_SIGNAL, [no_sig, 0, SIG0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::EVENT_SIGNAL, [ev, 0, 1, 0, 0, 0]),
        Status::InvalidArgs,
        "a set bit outside the user signals"
    );
    assert_eq!(
        w.err(nr::EVENT_SIGNAL, [ev, 1, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "a clear bit outside the user signals"
    );
    w.ok(nr::EVENT_SIGNAL, [ev, 0, SIG0 | (1 << 31), 0, 0, 0]);
    w.ok(nr::EVENT_SIGNAL, [ev, SIG0, 0, 0, 0, 0]);
    assert_eq!(
        w.ok(nr::OBJECT_SIGNALS, [ev, 0, 0, 0, 0, 0]).0,
        Some(1 << 31)
    );
}

#[test]
fn replace_reports_the_touch_unless_the_negative_control_is_on() {
    for skip in [false, true] {
        let mut w = world();
        w.faults.skip_forced_touch = skip;
        let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
        w.objs.take_dirty();
        w.ok(nr::HANDLE_REPLACE, [ev, SAME, 0, 0, 0, 0]);
        assert_eq!(
            w.objs.take_dirty().items.iter().any(|&(_, forced)| forced),
            !skip,
            "skip_forced_touch = {skip}"
        );
    }
}

#[test]
fn socket_rows() {
    let mut w = world();
    let (x, y) = w.pair(nr::SOCKET_CREATE);
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    for n in [nr::SOCKET_WRITE, nr::SOCKET_READ] {
        assert_eq!(w.err(n, [0x999, BASE, 1, 0, 0, 0]), Status::BadHandle);
        assert_eq!(w.err(n, [ev, BASE, 1, 0, 0, 0]), Status::WrongType);
    }
    assert_eq!(
        w.err(nr::SOCKET_SHUTDOWN, [ev, 0, 0, 0, 0, 0]),
        Status::WrongType
    );
    let read_only = w.narrow(x, Rights::READ);
    let write_only = w.narrow(y, Rights::WRITE);
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [read_only, BASE, 1, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::SOCKET_READ, [write_only, BASE, 1, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::SOCKET_SHUTDOWN, [read_only, 0, 0, 0, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [x, BASE, usize::MAX as u64 >> 1, 0, 0, 0]),
        Status::TooBig
    );
    assert_eq!(
        w.err(nr::SOCKET_READ, [y, BASE, usize::MAX as u64 >> 1, 0, 0, 0]),
        Status::TooBig
    );
    // zero-length reads and writes on a live socket
    assert_eq!(
        w.ok(nr::SOCKET_READ, [y, BASE, 0, 0, 0, 0]).0,
        Some(0),
        "max 0 is OK with read 0"
    );
    assert_eq!(w.ok(nr::SOCKET_WRITE, [x, BASE, 0, 0, 0, 0]).0, Some(0));
    // shutdown is idempotent; the reader sees the end after the data is gone, the writer's peer is gone when closed
    w.ok(nr::SOCKET_SHUTDOWN, [x, 0, 0, 0, 0, 0]);
    w.ok(nr::SOCKET_SHUTDOWN, [x, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::SOCKET_READ, [y, BASE, 8, 0, 0, 0]),
        Status::PeerClosed
    );
    w.ok(nr::HANDLE_CLOSE, [y, 0, 0, 0, 0, 0]);
    w.ok(nr::HANDLE_CLOSE, [write_only, 0, 0, 0, 0, 0]);
    let (a, b) = w.pair(nr::SOCKET_CREATE);
    w.ok(nr::HANDLE_CLOSE, [b, 0, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [a, BASE, 1, 0, 0, 0]),
        Status::PeerClosed
    );
    assert_eq!(
        w.err(nr::SOCKET_WRITE, [a, BASE, 0, 0, 0, 0]),
        Status::PeerClosed,
        "the zero-length probe reports a closed peer"
    );
}

#[test]
fn socket_write_fills_the_buffer() {
    let mut w = world();
    let (x, _y) = w.pair(nr::SOCKET_CREATE);
    let mut total = 0u64;
    loop {
        match w.call(nr::SOCKET_WRITE, [x, BASE, 4096, 0, 0, 0]).unwrap() {
            Ok((Some(n), _)) => total += n,
            Ok(_) => panic!("a write returns a count"),
            Err((s, _, _)) => {
                assert_eq!(s, Status::ShouldWait);
                break;
            }
        }
        assert!(total < 1 << 24, "the buffer is bounded");
    }
    assert!(total > 0);
}

#[test]
fn ports_bind_rearm_unbind_rows() {
    let mut w = world();
    let p = w.ok(nr::PORT_CREATE, [0; 6]).0.unwrap();
    let ev = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    let (c, _) = w.pair(nr::CHANNEL_CREATE);
    // a bind that works, then every refusal
    w.ok(nr::PORT_BIND, [p, ev, SIG0, 1, 0, 0]);
    assert_eq!(
        w.err(nr::PORT_BIND, [p, ev, SIG0, 1, 0, 0]),
        Status::InvalidArgs,
        "the key is taken"
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, 0x999, SIG0, 2, 0, 0]),
        Status::BadHandle
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [ev, ev, SIG0, 2, 0, 0]),
        Status::WrongType
    );
    let no_manage = w.narrow(p, Rights::WAIT);
    assert_eq!(
        w.err(nr::PORT_BIND, [no_manage, ev, SIG0, 2, 0, 0]),
        Status::AccessDenied
    );
    let no_wait = w.narrow(ev, Rights::SIGNAL);
    assert_eq!(
        w.err(nr::PORT_BIND, [p, no_wait, SIG0, 2, 0, 0]),
        Status::AccessDenied
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, ev, 0, 2, 0, 0]),
        Status::InvalidArgs,
        "an empty mask"
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, ev, 1 << 40, 2, 0, 0]),
        Status::InvalidArgs,
        "a mask wider than the signals"
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, ev, 1 << 20, 2, 0, 0]),
        Status::InvalidArgs,
        "a mask bit that is not waitable"
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, p, SIG0, 2, 0, 0]),
        Status::InvalidArgs,
        "a port cannot be bound to a port"
    );
    assert_eq!(
        w.err(nr::PORT_BIND, [p, c, 1, 3, 0xff, 0]),
        Status::InvalidArgs,
        "unknown flags"
    );
    // rearm and unbind
    w.ok(nr::PORT_REARM, [p, 1, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::PORT_REARM, [p, 99, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::PORT_UNBIND, [p, 99, 0, 0, 0, 0]),
        Status::InvalidArgs
    );
    assert_eq!(
        w.err(nr::PORT_UNBIND, [0x999, 1, 0, 0, 0, 0]),
        Status::BadHandle
    );
    w.ok(nr::PORT_UNBIND, [p, 1, 0, 0, 0, 0]);
    assert_eq!(
        w.err(nr::PORT_UNBIND, [p, 1, 0, 0, 0, 0]),
        Status::InvalidArgs,
        "already unbound"
    );
}

#[test]
fn job_quota_refuses_creation_with_no_memory() {
    let mut w = world();
    let job = w
        .vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                objects: 2,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.job = job;
    // a channel is two objects: it fits once
    w.pair(nr::CHANNEL_CREATE);
    assert_eq!(w.err(nr::CHANNEL_CREATE, [0; 6]), Status::NoMemory);
    assert_eq!(w.err(nr::SOCKET_CREATE, [0; 6]), Status::NoMemory);
    assert_eq!(w.err(nr::PORT_CREATE, [0; 6]), Status::NoMemory);
    assert_eq!(w.err(nr::EVENT_CREATE, [0; 6]), Status::NoMemory);
    // the root job is not limited
    w.job = JobId::ROOT;
    w.ok(nr::EVENT_CREATE, [0; 6]);
}

#[test]
fn queued_bytes_are_charged_to_the_job() {
    let mut w = world();
    let job = w
        .vmos
        .jobs
        .create(
            JobId::ROOT,
            Limits {
                ipc_bytes: 64,
                ..Limits::UNLIMITED
            },
        )
        .unwrap();
    w.job = job;
    let (x, _y) = w.pair(nr::CHANNEL_CREATE);
    let mut st = Status::Ok;
    for _ in 0..64 {
        if let Err((s, _, _)) = w
            .call(nr::CHANNEL_WRITE, [x, BASE, 32, BASE, 0, 0])
            .unwrap()
        {
            st = s;
            break;
        }
    }
    assert_eq!(st, Status::NoMemory, "a job's queued bytes are bounded");
}
