//! The syscall layer on the host (TS3a): the error rows of the moved calls in SYSCALLS.md, run against a fake machine whose user
//! memory is one window of bytes.
use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::status::Status;
use abi::syscall as nr;
use kcore::handle::HandleTable;
use kcore::ipc::Kobjs;
use kcore::job::{JobId, Jobs};
use ksys::{ipc_call, wide_argument_error, Ctx, Reply, UserMem};

const BASE: u64 = 0x1000;
const WIN: usize = 0x10000;
/// A mapped read-only page just past the window.
const RO: u64 = BASE + WIN as u64;

struct Fake {
    mem: Vec<u8>,
    jobs: Jobs,
}

impl Fake {
    fn range(&self, ptr: u64, len: usize, write: bool) -> Result<usize, Status> {
        if len == 0 {
            return Ok(0);
        }
        let end = ptr.checked_add(len as u64).ok_or(Status::InvalidArgs)?;
        if ptr >= BASE && end <= BASE + WIN as u64 {
            return Ok((ptr - BASE) as usize);
        }
        if !write && ptr >= RO && end <= RO + 0x1000 {
            return Ok(WIN);
        }
        Err(Status::InvalidArgs)
    }
}

impl UserMem for Fake {
    fn copy_in(&mut self, ptr: u64, len: usize) -> Result<Vec<u8>, Status> {
        let at = self.range(ptr, len, false)?;
        Ok(self
            .mem
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| vec![0; len]))
    }
    fn verify_writable(&mut self, ptr: u64, len: usize) -> Result<(), Status> {
        self.range(ptr, len, true).map(|_| ())
    }
    fn copy_out_verified(&mut self, ptr: u64, data: &[u8]) -> Result<(), Status> {
        let at = self.range(ptr, data.len(), true)?;
        self.mem[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn jobs(&mut self) -> &mut Jobs {
        &mut self.jobs
    }
}

struct World {
    objs: Kobjs,
    handles: HandleTable,
    mem: Fake,
}

fn world() -> World {
    World {
        objs: Kobjs::new(),
        handles: HandleTable::new(64),
        mem: Fake {
            mem: vec![0; WIN],
            jobs: Jobs::new(),
        },
    }
}

impl World {
    fn call(&mut self, n: usize, a: [u64; 6]) -> Option<Reply> {
        let mut c = Ctx {
            objs: &mut self.objs,
            handles: &mut self.handles,
            job: JobId::ROOT,
            mem: &mut self.mem,
            skip_forced_touch: false,
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
        let o = (at - BASE) as usize;
        self.mem.mem[o..o + d.len()].copy_from_slice(d);
    }
    fn peek(&self, at: u64, n: usize) -> Vec<u8> {
        let o = (at - BASE) as usize;
        self.mem.mem[o..o + n].to_vec()
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
    assert_ne!(w.err(nr::PORT_UNBIND, [e, 1, 0, 0, 0, 0]), Status::Ok);
    assert_ne!(w.err(nr::PORT_REARM, [e, 1, 0, 0, 0, 0]), Status::Ok);
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
    assert!(failed.is_some() && seen.len() == 64);
}
