//! Sockets and ports: hand-derived scenarios for every status in SYSCALLS.md, and a random
//! test of socket streams and port readiness against an independent reference model.
mod common;
use abi::limits::{MAX_PORT_BINDINGS, SOCKET_BUFFER};
use abi::rights::Rights;
use abi::signals;
use abi::status::Status;
use abi::Handle;
use common::{Rng, SEEDS, STEPS};
use kcore::handle::HandleTable;
use kcore::ipc::*;
use std::collections::VecDeque;

/// `port_bind` with a 32-bit mask (the syscall's mask is 64 bits wide so that the kernel can
/// reject wider ones itself).
fn port_bind(
    reg: &mut Kobjs,
    t: &HandleTable,
    port: Handle,
    obj: Handle,
    mask: u32,
    key: u64,
) -> Result<(), Status> {
    kcore::ipc::port_bind(reg, t, port, obj, u64::from(mask), key)
}

fn world() -> (Kobjs, HandleTable) {
    (Kobjs::new(), HandleTable::new(abi::limits::MAX_HANDLES))
}

fn weaker(reg: &mut Kobjs, t: &mut HandleTable, h: Handle, rights: Rights) -> Handle {
    let _ = reg;
    let e = t.get(h).unwrap();
    t.insert(e.obj, rights).unwrap()
}

#[test]
fn socket_basics_and_end_of_stream() {
    let (mut reg, mut t) = world();
    let (a, b) = socket_create(&mut reg, &mut t).unwrap();
    assert_eq!(t.get(a).unwrap().rights, Rights::SOCKET);
    assert_eq!(socket_read(&mut reg, &t, b, 10), Err(Status::ShouldWait));
    assert_eq!(socket_write(&mut reg, &t, a, b"hello"), Ok(5));
    assert_eq!(
        signals_of(&reg, &t, b).unwrap() & signals::READABLE,
        signals::READABLE
    );
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::READABLE,
        0,
        "our own buffer is empty"
    );
    assert_eq!(socket_read(&mut reg, &t, b, 3).unwrap(), b"hel");
    assert_eq!(
        socket_read(&mut reg, &t, b, 10).unwrap(),
        b"lo",
        "short read"
    );
    assert_eq!(
        socket_read(&mut reg, &t, b, 0),
        Ok(vec![]),
        "max 0 is fine even when empty"
    );
    // data written before the peer closes can still be read; then the end of the stream
    assert_eq!(socket_write(&mut reg, &t, a, b"tail"), Ok(4));
    t.close(&mut reg, a).unwrap();
    let s = signals_of(&reg, &t, b).unwrap();
    assert_eq!(
        s & (signals::READABLE | signals::PEER_CLOSED),
        signals::READABLE | signals::PEER_CLOSED
    );
    assert_eq!(socket_read(&mut reg, &t, b, 10).unwrap(), b"tail");
    assert_eq!(
        socket_read(&mut reg, &t, b, 10),
        Err(Status::PeerClosed),
        "end of stream"
    );
    assert_eq!(socket_write(&mut reg, &t, b, b"x"), Err(Status::PeerClosed));
    assert_eq!(
        socket_write(&mut reg, &t, b, b""),
        Err(Status::PeerClosed),
        "even an empty write"
    );
}

#[test]
fn socket_buffer_limits_and_signals() {
    let (mut reg, mut t) = world();
    let (a, b) = socket_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE,
        signals::WRITABLE
    );
    let big = vec![7u8; SOCKET_BUFFER];
    assert_eq!(socket_write(&mut reg, &t, a, &big), Ok(SOCKET_BUFFER));
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE,
        0,
        "full: not writable"
    );
    assert_eq!(socket_write(&mut reg, &t, a, b"x"), Err(Status::ShouldWait));
    assert_eq!(
        socket_write(&mut reg, &t, a, b""),
        Ok(0),
        "an empty write is OK"
    );
    assert_eq!(
        socket_write(&mut reg, &t, a, &vec![0; SOCKET_BUFFER + 1]),
        Err(Status::TooBig)
    );
    assert_eq!(socket_read(&mut reg, &t, b, 10).unwrap().len(), 10);
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE,
        signals::WRITABLE,
        "room again"
    );
    assert_eq!(
        socket_write(&mut reg, &t, a, &[1u8; 100]),
        Ok(10),
        "a short write"
    );
    assert_eq!(
        socket_read(&mut reg, &t, b, SOCKET_BUFFER + 1),
        Err(Status::TooBig)
    );
    assert_eq!(socket_peek(&reg, &t, b, 5), Ok(5));
    assert_eq!(
        socket_peek(&reg, &t, b, SOCKET_BUFFER),
        Ok(SOCKET_BUFFER),
        "peek removes nothing"
    );
}

#[test]
fn socket_error_order() {
    let (mut reg, mut t) = world();
    let (a, _b) = socket_create(&mut reg, &mut t).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    let ro = weaker(&mut reg, &mut t, a, Rights::READ);
    let wo = weaker(&mut reg, &mut t, a, Rights::WRITE);
    assert_eq!(
        socket_write(&mut reg, &t, Handle(9999), b"x"),
        Err(Status::BadHandle)
    );
    assert_eq!(socket_write(&mut reg, &t, e, b"x"), Err(Status::WrongType));
    assert_eq!(
        socket_write(&mut reg, &t, ro, b"x"),
        Err(Status::AccessDenied)
    );
    assert_eq!(socket_read(&mut reg, &t, wo, 1), Err(Status::AccessDenied));
    assert_eq!(socket_read(&mut reg, &t, e, 1), Err(Status::WrongType));
    // rights are checked before sizes
    assert_eq!(
        socket_write(&mut reg, &t, ro, &vec![0; SOCKET_BUFFER + 1]),
        Err(Status::AccessDenied)
    );
}

#[test]
fn port_basics_levels_and_rotation() {
    let (mut reg, mut t) = world();
    let port = port_create(&mut reg, &mut t).unwrap();
    assert_eq!(t.get(port).unwrap().rights, Rights::PORT);
    let (a, b) = socket_create(&mut reg, &mut t).unwrap();
    let e1 = event_create(&mut reg, &mut t).unwrap();
    let e2 = event_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, b, signals::READABLE, 1).unwrap();
    port_bind(&mut reg, &t, port, e1, signals::USER_MASK & 1 << 24, 2).unwrap();
    port_bind(&mut reg, &t, port, e2, 1 << 24, 3).unwrap();
    assert_eq!(port_poll(&mut reg, &t, port), Ok(None), "nothing ready");
    assert_eq!(signals_of(&reg, &t, port).unwrap(), 0);
    socket_write(&mut reg, &t, a, b"x").unwrap();
    assert_eq!(
        signals_of(&reg, &t, port).unwrap(),
        signals::READABLE,
        "a port is readable when a binding is"
    );
    assert_eq!(
        port_poll(&mut reg, &t, port),
        Ok(Some((1, signals::READABLE)))
    );
    assert_eq!(
        port_poll(&mut reg, &t, port),
        Ok(Some((1, signals::READABLE))),
        "level triggered: not consumed"
    );
    event_signal(&mut reg, &t, e1, 0, 1 << 24).unwrap();
    event_signal(&mut reg, &t, e2, 0, 1 << 24).unwrap();
    // three ready bindings are reported in turn
    let mut seen = vec![];
    for _ in 0..6 {
        seen.push(port_poll(&mut reg, &t, port).unwrap().unwrap().0);
    }
    for k in [1u64, 2, 3] {
        assert_eq!(
            seen.iter().filter(|&&x| x == k).count(),
            2,
            "fair: {seen:?}"
        );
    }
    // observed is signals & mask
    event_signal(&mut reg, &t, e1, 0, 1 << 25).unwrap();
    let r = port_poll(&mut reg, &t, port).unwrap().unwrap();
    assert!(
        r.1 & !(1 << 24) == 0,
        "only masked bits are reported: {r:?}"
    );
}

#[test]
fn port_unbind_destroyed_objects_and_errors() {
    let (mut reg, mut t) = world();
    let port = port_create(&mut reg, &mut t).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    let e2 = event_create(&mut reg, &mut t).unwrap();
    let nowait = weaker(&mut reg, &mut t, e, Rights::SIGNAL);
    let nomanage = weaker(&mut reg, &mut t, port, Rights::WAIT);
    let nowait_port = weaker(&mut reg, &mut t, port, Rights::MANAGE);
    let m = 1u32 << 24;
    assert_eq!(
        port_bind(&mut reg, &t, Handle(9999), e, m, 1),
        Err(Status::BadHandle)
    );
    assert_eq!(
        port_bind(&mut reg, &t, port, Handle(9999), m, 1),
        Err(Status::BadHandle)
    );
    assert_eq!(
        port_bind(&mut reg, &t, e, e, m, 1),
        Err(Status::WrongType),
        "an event is no port"
    );
    assert_eq!(
        port_bind(&mut reg, &t, nomanage, e, m, 1),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        port_bind(&mut reg, &t, port, nowait, m, 1),
        Err(Status::AccessDenied),
        "object needs WAIT"
    );
    assert_eq!(
        port_bind(&mut reg, &t, port, e, 0, 1),
        Err(Status::InvalidArgs),
        "empty mask"
    );
    assert_eq!(
        port_bind(&mut reg, &t, port, e, 1 << 5, 1),
        Err(Status::InvalidArgs),
        "not a signal"
    );
    assert_eq!(
        port_bind(&mut reg, &t, port, port, signals::READABLE, 1),
        Err(Status::InvalidArgs),
        "a port in a port"
    );
    port_bind(&mut reg, &t, port, e, m, 1).unwrap();
    assert_eq!(
        port_bind(&mut reg, &t, port, e2, m, 1),
        Err(Status::InvalidArgs),
        "duplicate key"
    );
    assert_eq!(port_unbind(&mut reg, &t, port, 9), Err(Status::InvalidArgs));
    assert_eq!(
        port_unbind(&mut reg, &t, nomanage, 1),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        port_poll(&mut reg, &t, nowait_port),
        Err(Status::AccessDenied),
        "poll needs WAIT"
    );
    assert_eq!(port_poll(&mut reg, &t, e), Err(Status::WrongType));
    event_signal(&mut reg, &t, e, 0, m).unwrap();
    assert_eq!(port_poll(&mut reg, &t, port), Ok(Some((1, m))));
    port_unbind(&mut reg, &t, port, 1).unwrap();
    assert_eq!(port_poll(&mut reg, &t, port), Ok(None), "unbound");
    // a destroyed object's binding disappears
    port_bind(&mut reg, &t, port, e, m, 5).unwrap();
    let ent = t.get(e).unwrap();
    t.close(&mut reg, e).unwrap();
    t.close(&mut reg, nowait).unwrap();
    assert!(!reg.is_alive(ent.obj));
    assert_eq!(port_poll(&mut reg, &t, port), Ok(None));
    port_bind(&mut reg, &t, port, e2, m, 5).expect("the key of a pruned binding is free again");
}

#[test]
fn port_binding_limit() {
    let (mut reg, mut t) = world();
    let port = port_create(&mut reg, &mut t).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    for k in 0..MAX_PORT_BINDINGS as u64 {
        port_bind(&mut reg, &t, port, e, 1 << 24, k).unwrap();
    }
    assert_eq!(
        port_bind(&mut reg, &t, port, e, 1 << 24, 9999),
        Err(Status::NoMemory)
    );
    port_unbind(&mut reg, &t, port, 7).unwrap();
    port_bind(&mut reg, &t, port, e, 1 << 24, 9999).unwrap();
}

/// A random walk over two sockets and a port bound to the readable end, against a
/// byte-queue model.
#[test]
fn random_socket_stream_against_a_model() {
    for seed in 0..SEEDS / 5 {
        let mut rng = Rng::new(seed);
        let (mut reg, mut t) = world();
        let (a, b) = socket_create(&mut reg, &mut t).unwrap();
        let port = port_create(&mut reg, &mut t).unwrap();
        port_bind(&mut reg, &t, port, b, signals::READABLE, 1).unwrap();
        let mut model: VecDeque<u8> = VecDeque::new();
        let mut next = 0u8;
        for _ in 0..STEPS {
            match rng.below(3) {
                0 => {
                    let n = rng.below(20_000) as usize;
                    let data: Vec<u8> = (0..n)
                        .map(|_| {
                            next = next.wrapping_add(1);
                            next
                        })
                        .collect();
                    let room = SOCKET_BUFFER - model.len();
                    let want = if n == 0 {
                        Ok(0)
                    } else if room == 0 {
                        Err(Status::ShouldWait)
                    } else {
                        Ok(room.min(n))
                    };
                    assert_eq!(socket_write(&mut reg, &t, a, &data), want);
                    if let Ok(k) = want {
                        model.extend(&data[..k]);
                    }
                    next = next.wrapping_sub((n - want.unwrap_or(0)) as u8); // keep the stream contiguous
                }
                1 => {
                    let max = rng.below(40_000) as usize;
                    let want = if max == 0 {
                        Ok(vec![])
                    } else if model.is_empty() {
                        Err(Status::ShouldWait)
                    } else {
                        Ok(model.drain(..max.min(model.len())).collect())
                    };
                    assert_eq!(socket_read(&mut reg, &t, b, max), want);
                }
                _ => {
                    let ready = !model.is_empty();
                    assert_eq!(
                        signals_of(&reg, &t, b).unwrap() & signals::READABLE != 0,
                        ready
                    );
                    assert_eq!(port_poll(&mut reg, &t, port).unwrap().is_some(), ready);
                    assert_eq!(
                        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE != 0,
                        model.len() < SOCKET_BUFFER
                    );
                }
            }
        }
    }
}

#[test]
fn dead_bindings_vanish_at_once() {
    let (mut reg, mut t) = world();
    let port = port_create(&mut reg, &mut t).unwrap();
    let m = signals::READABLE | signals::PEER_CLOSED;
    let (a, b) = socket_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, b, m, 1).unwrap();
    t.close(&mut reg, b).unwrap();
    t.close(&mut reg, a).unwrap();
    assert_eq!(
        port_unbind(&mut reg, &t, port, 1),
        Err(Status::InvalidArgs),
        "already gone"
    );
    let (c, d) = socket_create(&mut reg, &mut t).unwrap();
    port_bind(&mut reg, &t, port, d, m, 1).expect("its key is free");
    // 256 dead bindings do not fill the port
    for k in 100..356u64 {
        let (x, y) = socket_create(&mut reg, &mut t).unwrap();
        port_bind(&mut reg, &t, port, y, m, k).unwrap();
        t.close(&mut reg, y).unwrap();
        t.close(&mut reg, x).unwrap();
    }
    let _ = c;
    port_bind(&mut reg, &t, port, d, m, 9999).expect("dead bindings are not counted");
    // a mask wider than 32 bits is INVALID_ARGS, after the handle and rights checks
    let e = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        kcore::ipc::port_bind(&mut reg, &t, port, e, 1 << 32, 5),
        Err(Status::InvalidArgs)
    );
    assert_eq!(
        kcore::ipc::port_bind(&mut reg, &t, e, e, 1 << 32, 5),
        Err(Status::WrongType),
        "type first"
    );
}

#[test]
fn the_system_wide_buffer_budget_is_enforced() {
    use abi::limits::MAX_IPC_BYTES;
    let (mut reg, mut t) = world();
    let chunk = vec![1u8; SOCKET_BUFFER];
    let mut ends = vec![];
    let mut total = 0usize;
    // fill sockets until the budget is gone: a write never succeeds past it
    loop {
        let (a, b) = socket_create(&mut reg, &mut t).unwrap();
        match socket_write(&mut reg, &t, a, &chunk) {
            Ok(n) => total += n,
            Err(e) => {
                assert_eq!(e, Status::NoMemory, "budget exhausted is NO_MEMORY");
                break;
            }
        }
        ends.push((a, b));
        assert!(total <= MAX_IPC_BYTES);
    }
    assert!(
        total <= MAX_IPC_BYTES && total + SOCKET_BUFFER > MAX_IPC_BYTES,
        "filled to the brim: {total}"
    );
    assert_eq!(
        reg.meter(),
        total,
        "the meter counts exactly what is queued"
    );
    // channels share the budget
    let (c, _d) = channel_create(&mut reg, &mut t).unwrap();
    let big = vec![0u8; 1000];
    let mut got = channel_write(&mut reg, &mut t, c, &big, &[]);
    while got.is_ok() {
        got = channel_write(&mut reg, &mut t, c, &big, &[]);
    }
    assert!(
        matches!(got, Err(Status::NoMemory) | Err(Status::ShouldWait)),
        "{got:?}"
    );
    // reading frees budget, destroying an endpoint frees what it held
    let (a, b) = ends[0];
    assert_eq!(socket_read(&mut reg, &t, b, 1000).unwrap().len(), 1000);
    assert_eq!(socket_write(&mut reg, &t, a, &[2u8; 400]), Ok(400));
    let before = reg.meter();
    t.close(&mut reg, a).unwrap();
    t.close(&mut reg, b).unwrap();
    assert_eq!(
        reg.meter(),
        before - (SOCKET_BUFFER - 1000 + 400),
        "destruction gives the bytes back"
    );
}
