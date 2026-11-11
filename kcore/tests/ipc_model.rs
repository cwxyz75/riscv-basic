//! Channels and events: hand-derived scenarios for every status in SYSCALLS.md, and a
//! random test against a reference model of objects, queues and reference counts.
mod common;
use abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES, MAX_QUEUED_MSGS};
use abi::rights::Rights;
use abi::signals;
use abi::status::Status;
use abi::Handle;
use common::{Rng, SEEDS, STEPS};
use kcore::handle::HandleTable;
use kcore::ipc::*;
use std::collections::{HashMap, VecDeque};

fn world() -> (Kobjs, HandleTable) {
    (Kobjs::new(), HandleTable::new(abi::limits::MAX_HANDLES))
}

#[test]
fn channel_basics() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    assert_eq!(reg.live(), 2);
    assert_eq!(t.get(a).unwrap().rights, Rights::CHANNEL);
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 100, 10),
        Err(ReadError::Status(Status::ShouldWait)),
        "empty and open"
    );
    channel_write(&mut reg, &mut t, a, b"one", &[]).unwrap();
    channel_write(&mut reg, &mut t, a, b"two", &[]).unwrap();
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 100, 10).unwrap().data,
        b"one",
        "in order"
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 100, 10).unwrap().data,
        b"two"
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, a, 100, 10),
        Err(ReadError::Status(Status::ShouldWait)),
        "a message goes to the peer, not back"
    );
    // handles travel: send an event through, rights unchanged
    let ev = event_create(&mut reg, &mut t).unwrap();
    let ev_obj = t.get(ev).unwrap().obj;
    channel_write(&mut reg, &mut t, a, b"with handle", &[ev]).unwrap();
    assert_eq!(
        t.get(ev),
        Err(Status::BadHandle),
        "the handle left the table"
    );
    assert_eq!(
        reg.refs(ev_obj),
        Some(1),
        "its reference travels with the message"
    );
    let got = channel_read(&mut reg, &mut t, b, 100, 10).unwrap();
    assert_eq!(got.handles.len(), 1);
    let e = t.get(got.handles[0]).unwrap();
    assert_eq!((e.obj, e.rights), (ev_obj, Rights::EVENT));
    // peer closed: queued messages are still delivered, then PEER_CLOSED
    channel_write(&mut reg, &mut t, a, b"last words", &[]).unwrap();
    t.close(&mut reg, a).unwrap();
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 100, 10).unwrap().data,
        b"last words"
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 100, 10),
        Err(ReadError::Status(Status::PeerClosed))
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, b, b"x", &[]),
        Err(Status::PeerClosed)
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn channel_errors() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let ev = event_create(&mut reg, &mut t).unwrap();
    let ev_no_transfer = t.replace(ev, Rights(Rights::SIGNAL.0)).unwrap();
    // BAD_HANDLE: the channel, then each listed handle
    assert_eq!(
        channel_write(&mut reg, &mut t, Handle(0), b"", &[]),
        Err(Status::BadHandle)
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, Handle(0x7777_0001), b"", &[]),
        Err(Status::BadHandle)
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[Handle(99)]),
        Err(Status::BadHandle)
    );
    // WRONG_TYPE
    assert_eq!(
        channel_write(&mut reg, &mut t, ev_no_transfer, b"", &[]),
        Err(Status::WrongType)
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, ev_no_transfer, 1, 1),
        Err(ReadError::Status(Status::WrongType))
    );
    // ACCESS_DENIED: no WRITE / no READ / handle without TRANSFER
    let dup_a = t.duplicate(&mut reg, a, Rights::SAME).unwrap();
    let read_only = t.replace(dup_a, Rights(Rights::READ.0)).unwrap();
    assert_eq!(
        channel_write(&mut reg, &mut t, read_only, b"", &[]),
        Err(Status::AccessDenied)
    );
    let dup_b = t.duplicate(&mut reg, b, Rights::SAME).unwrap();
    let write_only = t.replace(dup_b, Rights(Rights::WRITE.0)).unwrap();
    assert_eq!(
        channel_read(&mut reg, &mut t, write_only, 1, 1),
        Err(ReadError::Status(Status::AccessDenied))
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[ev_no_transfer]),
        Err(Status::AccessDenied)
    );
    assert!(
        t.get(ev_no_transfer).is_ok(),
        "a refused write leaves its handles in the table"
    );
    // INVALID_ARGS: the same handle twice (and an error leaves everything in place)
    let ev2 = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[ev2, ev2]),
        Err(Status::InvalidArgs)
    );
    assert!(t.get(ev2).is_ok());
    // order: an invalid handle later in the list beats the duplicate earlier in it
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[ev2, ev2, Handle(99)]),
        Err(Status::BadHandle)
    );
    // NOT_SUPPORTED: the endpoint over itself
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[a]),
        Err(Status::NotSupported)
    );
    assert!(t.get(a).is_ok());
    // TOO_BIG
    assert_eq!(
        channel_write(&mut reg, &mut t, a, &vec![0; MAX_MSG_BYTES + 1], &[]),
        Err(Status::TooBig)
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, a, &vec![0; MAX_MSG_BYTES], &[]),
        Ok(()),
        "exactly the limit is fine"
    );
    let many: Vec<Handle> = (0..=MAX_MSG_HANDLES)
        .map(|_| event_create(&mut reg, &mut t).unwrap())
        .collect();
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &many),
        Err(Status::TooBig)
    );
    assert!(many.iter().all(|&h| t.get(h).is_ok()));
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &many[..MAX_MSG_HANDLES]),
        Ok(())
    );
    // SHOULD_WAIT: the peer's queue is full (two messages are already queued above)
    for i in 0..MAX_QUEUED_MSGS - 2 {
        channel_write(&mut reg, &mut t, a, &[i as u8], &[]).unwrap();
    }
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE,
        0,
        "no room: not writable"
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"x", &[]),
        Err(Status::ShouldWait)
    );
    channel_read(&mut reg, &mut t, b, MAX_MSG_BYTES, MAX_MSG_HANDLES).unwrap();
    assert_eq!(
        signals_of(&reg, &t, a).unwrap() & signals::WRITABLE,
        signals::WRITABLE,
        "room again"
    );
    // BUFFER_TOO_SMALL keeps the message queued and reports sizes
    let (c, d) = channel_create(&mut reg, &mut t).unwrap();
    let ev3 = event_create(&mut reg, &mut t).unwrap();
    channel_write(&mut reg, &mut t, c, b"hello", &[ev3]).unwrap();
    assert_eq!(
        channel_read(&mut reg, &mut t, d, 4, 1),
        Err(ReadError::TooSmall {
            bytes: 5,
            handles: 1
        })
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, d, 5, 0),
        Err(ReadError::TooSmall {
            bytes: 5,
            handles: 1
        })
    );
    let ok = channel_read(&mut reg, &mut t, d, 5, 1).unwrap();
    assert_eq!(
        (ok.data.as_slice(), ok.handles.len()),
        (&b"hello"[..], 1),
        "still there after two refusals"
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn a_full_table_leaves_the_message_queued() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(4);
    let (a, b) = channel_create(&mut reg, &mut t).unwrap(); // 2 used
    let e1 = event_create(&mut reg, &mut t).unwrap();
    let e2 = event_create(&mut reg, &mut t).unwrap(); // 4 used: full
    assert_eq!(event_create(&mut reg, &mut t), Err(Status::NoMemory));
    channel_write(&mut reg, &mut t, a, b"m", &[e1, e2]).unwrap(); // frees 2 slots
    let (_c, _d) = channel_create(&mut reg, &mut t).unwrap(); // fills them again
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 10, 10),
        Err(ReadError::Status(Status::NoMemory)),
        "no room for the two handles"
    );
    t.close(&mut reg, _c).unwrap();
    t.close(&mut reg, _d).unwrap();
    let got = channel_read(&mut reg, &mut t, b, 10, 10).unwrap();
    assert_eq!(got.handles.len(), 2, "the message was still there");
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn channel_txid() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let msg = |id: u32, tail: &[u8]| [id.to_le_bytes().as_slice(), tail].concat();
    channel_write(&mut reg, &mut t, a, &msg(7, b"first"), &[]).unwrap();
    channel_write(&mut reg, &mut t, a, &msg(9, b"reply"), &[]).unwrap();
    channel_write(&mut reg, &mut t, a, b"ab", &[]).unwrap(); // shorter than a txid: never matches
    assert_eq!(
        channel_read_txid(&mut reg, &mut t, b, 5, 99, 9),
        Err(ReadError::Status(Status::ShouldWait)),
        "no message 5, although others are queued"
    );
    assert_eq!(
        channel_read_txid(&mut reg, &mut t, b, 9, 99, 9)
            .unwrap()
            .data,
        msg(9, b"reply"),
        "skips the older message"
    );
    assert_eq!(
        channel_read(&mut reg, &mut t, b, 99, 9).unwrap().data,
        msg(7, b"first"),
        "the skipped one is still first in line"
    );
    t.drain(&mut reg);
}

#[test]
fn event_basics() {
    let (mut reg, mut t) = world();
    let e = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(signals_of(&reg, &t, e), Ok(0));
    event_signal(&mut reg, &t, e, 0, 1 << 24 | 1 << 31).unwrap();
    assert_eq!(signals_of(&reg, &t, e), Ok(1 << 24 | 1 << 31));
    event_signal(&mut reg, &t, e, 1 << 24, 1 << 25).unwrap();
    assert_eq!(
        signals_of(&reg, &t, e),
        Ok(1 << 25 | 1 << 31),
        "clear first, then set"
    );
    assert_eq!(
        event_signal(&mut reg, &t, e, 0, 1),
        Err(Status::InvalidArgs),
        "kernel signals are not the user's to set"
    );
    assert_eq!(
        event_signal(&mut reg, &t, e, 4, 0),
        Err(Status::InvalidArgs)
    );
    let (a, _b) = channel_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        event_signal(&mut reg, &t, a, 0, 1 << 24),
        Err(Status::WrongType)
    );
    assert_eq!(
        event_signal(&mut reg, &t, Handle(0), 0, 0),
        Err(Status::BadHandle)
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn channel_signals() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    assert_eq!(signals_of(&reg, &t, a), Ok(signals::WRITABLE));
    channel_write(&mut reg, &mut t, a, b"x", &[]).unwrap();
    assert_eq!(
        signals_of(&reg, &t, b),
        Ok(signals::WRITABLE | signals::READABLE)
    );
    t.close(&mut reg, a).unwrap();
    assert_eq!(
        signals_of(&reg, &t, b),
        Ok(signals::READABLE | signals::PEER_CLOSED),
        "not writable once the peer is gone"
    );
    t.drain(&mut reg);
}

#[test]
fn deeply_nested_channels_release_without_recursion() {
    // Each channel is queued inside the previous one; dropping the outermost handle
    // must release them all (iteratively: a recursive release would overflow the stack).
    let (mut reg, mut t) = world();
    let (mut outer_a, outer_b) = channel_create(&mut reg, &mut t).unwrap();
    let first_b = outer_b;
    for _ in 0..20_000 {
        let (a, b) = channel_create(&mut reg, &mut t).unwrap();
        // put end b of the new pair into the current outer channel's queue
        channel_write(&mut reg, &mut t, outer_a, b"nest", &[b]).unwrap();
        t.close(&mut reg, outer_a).unwrap();
        outer_a = a;
    }
    assert!(reg.live() > 20_000);
    t.close(&mut reg, outer_a).unwrap();
    t.close(&mut reg, first_b).unwrap();
    assert_eq!(reg.live(), 0, "the whole chain was freed");
}

#[test]
fn cycles_are_swept() {
    // X and Z are queued in each other's channels with no handle left to either, so
    // reference counting alone can never free them.
    let (mut reg, mut t) = world();
    let (x, w) = channel_create(&mut reg, &mut t).unwrap(); // X and its peer W
    let (z, y) = channel_create(&mut reg, &mut t).unwrap(); // Z and its peer Y
    channel_write(&mut reg, &mut t, y, b"", &[x]).unwrap(); // X travels to Z's queue
    channel_write(&mut reg, &mut t, w, b"", &[z]).unwrap(); // Z travels to X's queue
    t.close(&mut reg, w).unwrap();
    t.close(&mut reg, y).unwrap();
    assert_eq!(t.len(), 0);
    assert_eq!(
        reg.live(),
        2,
        "the cycle keeps itself alive: that is the leak counting cannot see"
    );
    let roots: Vec<_> = t.objects().collect();
    assert_eq!(reg.sweep(&roots), 2);
    assert_eq!(reg.live(), 0);
    // A sweep never touches what a table still reaches, including through a queue.
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let (c, d) = channel_create(&mut reg, &mut t).unwrap();
    channel_write(&mut reg, &mut t, a, b"", &[c]).unwrap(); // c sits in b's queue
    t.close(&mut reg, d).unwrap();
    let roots: Vec<_> = t.objects().collect();
    assert_eq!(reg.sweep(&roots), 0, "everything is reachable from a or b");
    assert_eq!(reg.live(), 3);
    t.close(&mut reg, a).unwrap();
    t.close(&mut reg, b).unwrap();
    assert_eq!(reg.live(), 0);
}

// ---------------------------------------------------------------------------
// The random model. Two "processes" (handle tables) share one registry. The model
// tracks objects (channel ends and events), their queues, every handle, and reference
// counts independently, and follows SYSCALLS.md's order of checks.

type Obj = usize;

struct MMsg {
    data: Vec<u8>,
    handles: Vec<(Obj, Rights)>,
}

struct MObj {
    is_event: bool,
    signals: u32,
    peer: Option<Obj>,
    queue: VecDeque<MMsg>,
    peer_closed: bool,
    refs: u32,
    alive: bool,
}

impl MObj {
    fn channel(peer: Option<Obj>) -> MObj {
        MObj {
            is_event: false,
            signals: 0,
            peer,
            queue: VecDeque::new(),
            peer_closed: false,
            refs: 1,
            alive: true,
        }
    }
}

struct Model {
    objs: Vec<MObj>,
    tables: [HashMap<Handle, (Obj, Rights)>; 2],
    limit: usize,
}

impl Model {
    /// Drop a reference, destroying what that frees (by worklist).
    fn release(&mut self, o: Obj) {
        let mut work = vec![o];
        while let Some(o) = work.pop() {
            let ob = &mut self.objs[o];
            if !ob.alive {
                continue;
            }
            ob.refs -= 1;
            if ob.refs > 0 {
                continue;
            }
            ob.alive = false;
            let queue = std::mem::take(&mut ob.queue);
            let peer = ob.peer;
            for m in queue {
                work.extend(m.handles.iter().map(|&(h, _)| h));
            }
            if let Some(p) = peer {
                if self.objs[p].alive {
                    self.objs[p].peer = None;
                    self.objs[p].peer_closed = true;
                }
            }
        }
    }

    /// The reachability sweep, from every handle in both tables.
    fn sweep(&mut self) {
        let mut reached = vec![false; self.objs.len()];
        let mut work: Vec<Obj> = self
            .tables
            .iter()
            .flat_map(|t| t.values().map(|&(o, _)| o))
            .collect();
        while let Some(o) = work.pop() {
            if !self.objs[o].alive || std::mem::replace(&mut reached[o], true) {
                continue;
            }
            work.extend(
                self.objs[o]
                    .queue
                    .iter()
                    .flat_map(|m| m.handles.iter().map(|&(h, _)| h)),
            );
        }
        let doomed: Vec<Obj> = (0..self.objs.len())
            .filter(|&o| self.objs[o].alive && !reached[o])
            .collect();
        let mut given_back = vec![];
        for &o in &doomed {
            self.objs[o].alive = false;
            given_back.extend(
                std::mem::take(&mut self.objs[o].queue)
                    .into_iter()
                    .flat_map(|m| m.handles.into_iter().map(|(h, _)| h)),
            );
        }
        for &o in &doomed {
            if let Some(p) = self.objs[o].peer {
                if self.objs[p].alive {
                    self.objs[p].peer = None;
                    self.objs[p].peer_closed = true;
                }
            }
        }
        for h in given_back {
            if self.objs[h].alive {
                self.release(h);
            }
        }
    }
}

fn prefix(t: u8) -> [u8; 4] {
    [t, 0, 0, 0]
}

#[test]
#[allow(clippy::needless_range_loop)]
fn random_channels_agree_with_the_model() {
    // Which of the limit paths the random run actually reached: TOO_BIG (bytes),
    // TOO_BIG (handles), SHOULD_WAIT (queue full), NOT_SUPPORTED.
    let mut hit = [false; 4];
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let mut reg = Kobjs::new();
        // Sometimes a table big enough to reach MAX_MSG_HANDLES.
        let limit = if rng.chance(15) {
            150
        } else {
            6 + rng.below(20) as usize
        };
        let mut tables = [HandleTable::new(limit), HandleTable::new(limit)];
        let mut m = Model {
            objs: vec![],
            tables: [HashMap::new(), HashMap::new()],
            limit,
        };
        let mut ids: HashMap<Obj, kcore::object::ObjId> = HashMap::new();

        for step in 0..STEPS * 2 {
            let ctx = format!("seed {seed} step {step}");
            let p = rng.below(2) as usize;
            // Sorted: HashMap order is random per run, and a failing seed must replay.
            let mut handles: Vec<Handle> = m.tables[p].keys().copied().collect();
            handles.sort_by_key(|h| h.0);
            let any = |rng: &mut Rng| handles[rng.below(handles.len() as u64) as usize];
            match rng.below(13) {
                0 | 1 => {
                    let result = channel_create(&mut reg, &mut tables[p]);
                    if m.limit - m.tables[p].len() < 2 {
                        assert_eq!(result, Err(Status::NoMemory), "{ctx}");
                    } else {
                        let (ha, hb) = result.unwrap_or_else(|e| panic!("{ctx}: {e:?}"));
                        let (a, b) = (m.objs.len(), m.objs.len() + 1);
                        m.objs.push(MObj::channel(Some(b)));
                        m.objs.push(MObj::channel(Some(a)));
                        m.tables[p].insert(ha, (a, Rights::CHANNEL));
                        m.tables[p].insert(hb, (b, Rights::CHANNEL));
                        ids.insert(a, tables[p].get(ha).unwrap().obj);
                        ids.insert(b, tables[p].get(hb).unwrap().obj);
                    }
                }
                2 => {
                    let result = event_create(&mut reg, &mut tables[p]);
                    if m.tables[p].len() >= m.limit {
                        assert_eq!(result, Err(Status::NoMemory), "{ctx}");
                    } else {
                        let h = result.unwrap();
                        let o = m.objs.len();
                        m.objs.push(MObj {
                            is_event: true,
                            signals: 0,
                            peer: None,
                            queue: VecDeque::new(),
                            peer_closed: false,
                            refs: 1,
                            alive: true,
                        });
                        m.tables[p].insert(h, (o, Rights::EVENT));
                        ids.insert(o, tables[p].get(h).unwrap().obj);
                    }
                }
                3 if !handles.is_empty() => {
                    let h = any(&mut rng);
                    let (clear, set) = (
                        if rng.chance(50) {
                            rng.below(256) as u32
                        } else {
                            0
                        } << 24,
                        (rng.below(256) as u32) << 24 | if rng.chance(10) { 1 } else { 0 },
                    );
                    let result = event_signal(&mut reg, &tables[p], h, clear, set);
                    let &(o, r) = &m.tables[p][&h];
                    if !m.objs[o].is_event {
                        assert_eq!(result, Err(Status::WrongType), "{ctx}");
                    } else if !r.contains(Rights::SIGNAL) {
                        assert_eq!(result, Err(Status::AccessDenied), "{ctx}");
                    } else if (clear | set) & !abi::signals::USER_MASK != 0 {
                        assert_eq!(result, Err(Status::InvalidArgs), "{ctx}");
                    } else {
                        assert_eq!(result, Ok(()), "{ctx}");
                        m.objs[o].signals = (m.objs[o].signals & !clear) | set;
                    }
                }
                4..=6 if !handles.is_empty() => {
                    // write a message with a few handles of this table
                    let ch = any(&mut rng);
                    // Now and then a burst of tiny writes, to reach the queue-full limit.
                    let burst = if rng.chance(3) {
                        MAX_QUEUED_MSGS + 4
                    } else {
                        1
                    };
                    for rep in 0..burst {
                        let mut data: Vec<u8> =
                            (0..rng.below(6)).map(|_| rng.below(4) as u8).collect();
                        if rng.chance(50) {
                            data.splice(0..0, prefix(rng.below(3) as u8)); // a transaction id
                        }
                        if rng.chance(3) {
                            data = vec![1; MAX_MSG_BYTES + rng.below(2) as usize];
                            // at and over the size limit
                        }
                        let mut list: Vec<Handle> =
                            (0..rng.below(4)).map(|_| any(&mut rng)).collect();
                        if rng.chance(4) {
                            list.push(Handle(rng.next() as u32)); // a forged one
                        }
                        if rng.chance(3) {
                            list = handles
                                .iter()
                                .copied()
                                .cycle()
                                .take(MAX_MSG_HANDLES + rng.below(2) as usize)
                                .collect(); // limit-sized (with repeats)
                        }
                        if burst > 1 {
                            data = vec![rep as u8];
                            list.clear();
                        }
                        let result = channel_write(&mut reg, &mut tables[p], ch, &data, &list);
                        // The model, from SYSCALLS.md's list of errors, in order.
                        let expect = (|| {
                            if data.len() > MAX_MSG_BYTES {
                                hit[0] = true;
                                return Err(Status::TooBig);
                            }
                            if list.len() > MAX_MSG_HANDLES {
                                hit[1] = true;
                                return Err(Status::TooBig);
                            }
                            let &(co, cr) = m.tables[p].get(&ch).ok_or(Status::BadHandle)?;
                            for h in &list {
                                m.tables[p].get(h).ok_or(Status::BadHandle)?;
                            }
                            if m.objs[co].is_event {
                                return Err(Status::WrongType);
                            }
                            if !cr.contains(Rights::WRITE) {
                                return Err(Status::AccessDenied);
                            }
                            for h in &list {
                                if !m.tables[p][h].1.contains(Rights::TRANSFER) {
                                    return Err(Status::AccessDenied);
                                }
                            }
                            if list.iter().enumerate().any(|(i, h)| list[..i].contains(h)) {
                                return Err(Status::InvalidArgs);
                            }
                            let peer = m.objs[co].peer;
                            for h in &list {
                                let o = m.tables[p][h].0;
                                if o == co || Some(o) == peer {
                                    hit[3] = true;
                                    return Err(Status::NotSupported);
                                }
                            }
                            let peer = peer.ok_or(Status::PeerClosed)?;
                            let queued_bytes: usize =
                                m.objs[peer].queue.iter().map(|q| q.data.len()).sum();
                            if m.objs[peer].queue.len() >= MAX_QUEUED_MSGS
                                || queued_bytes + data.len() > abi::limits::MAX_QUEUED_BYTES
                            {
                                hit[2] = true;
                                return Err(Status::ShouldWait);
                            }
                            Ok(peer)
                        })();
                        match (result, expect) {
                            (Ok(()), Ok(peer)) => {
                                let moved: Vec<_> = list
                                    .iter()
                                    .map(|h| m.tables[p].remove(h).unwrap())
                                    .collect();
                                m.objs[peer].queue.push_back(MMsg {
                                    data,
                                    handles: moved,
                                });
                            }
                            (Err(got), Err(want)) => assert_eq!(got, want, "{ctx}: write status"),
                            (got, want) => panic!("{ctx}: write gave {got:?}, model says {want:?}"),
                        }
                    }
                }
                7 | 8 if !handles.is_empty() => {
                    let ch = any(&mut rng);
                    let (max_b, max_h) = (rng.below(7) as usize, rng.below(4) as usize);
                    let txid = if rng.chance(40) {
                        Some(rng.below(3) as u8)
                    } else {
                        None
                    };
                    let (max_b, max_h) = if rng.chance(10) {
                        (MAX_MSG_BYTES, MAX_MSG_HANDLES)
                    } else {
                        (max_b, max_h)
                    };
                    let result = match txid {
                        Some(t) => channel_read_txid(
                            &mut reg,
                            &mut tables[p],
                            ch,
                            u32::from_le_bytes(prefix(t)),
                            max_b,
                            max_h,
                        ),
                        None => channel_read(&mut reg, &mut tables[p], ch, max_b, max_h),
                    };
                    let &(co, cr) = &m.tables[p][&ch];
                    if m.objs[co].is_event {
                        assert_eq!(result, Err(ReadError::Status(Status::WrongType)), "{ctx}");
                        continue;
                    }
                    if !cr.contains(Rights::READ) {
                        assert_eq!(
                            result,
                            Err(ReadError::Status(Status::AccessDenied)),
                            "{ctx}"
                        );
                        continue;
                    }
                    let pos = match txid {
                        Some(t) => m.objs[co]
                            .queue
                            .iter()
                            .position(|q| q.data.len() >= 4 && q.data[..4] == prefix(t)),
                        None => (!m.objs[co].queue.is_empty()).then_some(0),
                    };
                    let Some(i) = pos else {
                        let want = if m.objs[co].peer_closed {
                            Status::PeerClosed
                        } else {
                            Status::ShouldWait
                        };
                        assert_eq!(
                            result,
                            Err(ReadError::Status(want)),
                            "{ctx}: nothing to read"
                        );
                        continue;
                    };
                    let front = &m.objs[co].queue[i];
                    if front.data.len() > max_b || front.handles.len() > max_h {
                        assert_eq!(
                            result,
                            Err(ReadError::TooSmall {
                                bytes: front.data.len(),
                                handles: front.handles.len()
                            }),
                            "{ctx}"
                        );
                    } else if m.limit - m.tables[p].len() < front.handles.len() {
                        assert_eq!(result, Err(ReadError::Status(Status::NoMemory)), "{ctx}");
                    } else {
                        let got = result.unwrap_or_else(|e| panic!("{ctx}: read {e:?}"));
                        let msg = m.objs[co].queue.remove(i).unwrap();
                        assert_eq!(got.data, msg.data, "{ctx}: data");
                        assert_eq!(got.handles.len(), msg.handles.len(), "{ctx}");
                        for (h, (o, r)) in got.handles.iter().zip(msg.handles) {
                            let e = tables[p].get(*h).unwrap();
                            assert_eq!((e.obj, e.rights), (ids[&o], r), "{ctx}: received handle");
                            m.tables[p].insert(*h, (o, r));
                        }
                    }
                }
                9 if !handles.is_empty() => {
                    let h = any(&mut rng);
                    tables[p].close(&mut reg, h).unwrap();
                    let (o, _) = m.tables[p].remove(&h).unwrap();
                    m.release(o);
                }
                10 if !handles.is_empty() => {
                    let h = any(&mut rng);
                    let (o, have) = m.tables[p][&h];
                    let result = tables[p].duplicate(&mut reg, h, Rights::SAME);
                    if !have.contains(Rights::DUPLICATE) {
                        assert_eq!(result, Err(Status::AccessDenied), "{ctx}");
                    } else if m.tables[p].len() >= m.limit {
                        assert_eq!(result, Err(Status::NoMemory), "{ctx}");
                    } else {
                        m.tables[p].insert(result.unwrap(), (o, have));
                        m.objs[o].refs += 1;
                    }
                }
                11 if !handles.is_empty() => {
                    // rights shrink: replace one handle with a weaker one
                    let h = any(&mut rng);
                    let (o, have) = m.tables[p][&h];
                    let weaker = Rights(have.0 & rng.next() as u32);
                    let nh = tables[p].replace(h, weaker).unwrap();
                    m.tables[p].remove(&h);
                    m.tables[p].insert(nh, (o, weaker));
                }
                _ => {
                    // The kernel's pressure sweep, from every handle in both tables.
                    if rng.chance(25) {
                        let roots: Vec<_> = tables.iter().flat_map(|t| t.objects()).collect();
                        let swept = reg.sweep(&roots);
                        let before = m.objs.iter().filter(|o| o.alive).count();
                        m.sweep();
                        assert_eq!(
                            swept,
                            before - m.objs.iter().filter(|o| o.alive).count(),
                            "{ctx}: sweep count"
                        );
                    }
                }
            }
            // Agreement after every step.
            for t in 0..2 {
                assert_eq!(tables[t].len(), m.tables[t].len(), "{ctx}: table {t} size");
                for (&h, &(o, r)) in &m.tables[t] {
                    let e = tables[t]
                        .get(h)
                        .unwrap_or_else(|e| panic!("{ctx}: handle {h:?} lost: {e:?}"));
                    assert_eq!((e.obj, e.rights), (ids[&o], r), "{ctx}");
                }
            }
            let alive = m.objs.iter().filter(|o| o.alive).count();
            assert_eq!(reg.live(), alive, "{ctx}: live objects");
            let mut want_refs: HashMap<Obj, u32> = HashMap::new();
            for t in &m.tables {
                for &(o, _) in t.values() {
                    *want_refs.entry(o).or_default() += 1;
                }
            }
            for o in m.objs.iter().filter(|o| o.alive) {
                for msg in &o.queue {
                    for &(h, _) in &msg.handles {
                        *want_refs.entry(h).or_default() += 1;
                    }
                }
            }
            for (i, o) in m.objs.iter().enumerate().filter(|(_, o)| o.alive) {
                assert_eq!(
                    o.refs,
                    want_refs.get(&i).copied().unwrap_or(0),
                    "{ctx}: model refs of {i}"
                );
                assert_eq!(
                    reg.refs(ids[&i]),
                    Some(o.refs),
                    "{ctx}: registry refs of {i}"
                );
            }
            // Signals agree.
            for t in 0..2 {
                for (&h, &(o, _)) in &m.tables[t] {
                    let want = if m.objs[o].is_event {
                        m.objs[o].signals
                    } else {
                        (if !m.objs[o].queue.is_empty() {
                            signals::READABLE
                        } else {
                            0
                        }) | if m.objs[o].peer_closed {
                            signals::PEER_CLOSED
                        } else if m.objs[o].peer.is_some_and(|p| {
                            m.objs[p].queue.len() < MAX_QUEUED_MSGS
                                && m.objs[p].queue.iter().map(|q| q.data.len()).sum::<usize>()
                                    < abi::limits::MAX_QUEUED_BYTES
                        }) {
                            signals::WRITABLE
                        } else {
                            0
                        }
                    };
                    assert_eq!(
                        signals_of(&reg, &tables[t], h),
                        Ok(want),
                        "{ctx}: signals of {h:?}"
                    );
                }
            }
        }
        // Everything torn down: what counting can free is freed; the sweep finds the rest.
        for t in &mut tables {
            t.drain(&mut reg);
        }
        let leaked = reg.live();
        let swept = reg.sweep(&[]);
        assert_eq!(swept, leaked, "seed {seed}: sweep count");
        assert_eq!(reg.live(), 0, "seed {seed}: objects left after sweeping");
    }
    assert!(
        hit.iter().all(|&h| h),
        "the random run never reached a limit path: {hit:?}"
    );
}

#[test]
fn a_sweep_gives_back_the_references_doomed_objects_held() {
    // The reviewer's scenario: an event and a channel end are queued, together, in the
    // channel of a doomed pair. After the sweep the survivors' counts must be right,
    // or closing the last handle would never free them.
    let (mut reg, mut t) = world();
    let (x, _x_peer) = channel_create(&mut reg, &mut t).unwrap();
    let (y, _y_peer) = channel_create(&mut reg, &mut t).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    let e2 = t.duplicate(&mut reg, e, Rights::SAME).unwrap();
    let y2 = t.duplicate(&mut reg, y, Rights::SAME).unwrap();
    channel_write(&mut reg, &mut t, x, b"", &[e2, y2]).unwrap(); // queued on x's peer
                                                                 // Close x's peer and both handles of y... leaving a message in a channel nobody holds.
    let e_obj = t.get(e).unwrap().obj;
    assert_eq!(reg.refs(e_obj), Some(2), "the handle e and the queued copy");
    t.close(&mut reg, _x_peer).unwrap(); // destroys the queue holder; its references are released by counting
    assert_eq!(reg.refs(e_obj), Some(1));
    // A genuinely unreachable pair (a cycle) that also references a survivor:
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let (c, d) = channel_create(&mut reg, &mut t).unwrap();
    let e3 = t.duplicate(&mut reg, e, Rights::SAME).unwrap();
    channel_write(&mut reg, &mut t, b, b"", &[c, e3]).unwrap(); // a's queue: c, event
    channel_write(&mut reg, &mut t, d, b"", &[a]).unwrap(); // c's queue: a  (cycle a <-> c)
    t.close(&mut reg, b).unwrap();
    t.close(&mut reg, d).unwrap();
    assert_eq!(
        reg.refs(e_obj),
        Some(2),
        "e and the copy queued in the cycle"
    );
    let roots: Vec<_> = t.objects().collect();
    assert_eq!(reg.sweep(&roots), 2, "the cycle a, c is garbage");
    assert_eq!(
        reg.refs(e_obj),
        Some(1),
        "the sweep returned the reference the dead queue held"
    );
    t.close(&mut reg, e).unwrap();
    assert!(
        !reg.is_alive(e_obj),
        "so closing the last handle frees the event"
    );
    t.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn a_process_cannot_grow_kernel_memory_by_making_cycles() {
    // The reviewer's loop: send a duplicate of one end through its own pair, drop every
    // handle. Sending either end of the channel through it is refused...
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let b2 = t.duplicate(&mut reg, b, Rights::SAME).unwrap();
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[b2]),
        Err(Status::NotSupported),
        "the peer end over this channel"
    );
    let a2 = t.duplicate(&mut reg, a, Rights::SAME).unwrap();
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"", &[a2]),
        Err(Status::NotSupported),
        "a duplicate of this end"
    );
    t.drain(&mut reg);
    // ...but two channels can still be made to hold each other, so the kernel sweeps under
    // pressure: the syscall layer asks `needs_sweep` before creating objects.
    let mut t = HandleTable::new(64);
    let mut peak = 0;
    for _ in 0..5000 {
        if reg.needs_sweep() {
            let roots: Vec<_> = t.objects().collect();
            reg.sweep(&roots);
        }
        let (x, w) = channel_create(&mut reg, &mut t).unwrap();
        let (z, y) = channel_create(&mut reg, &mut t).unwrap();
        channel_write(&mut reg, &mut t, y, b"", &[x]).unwrap();
        channel_write(&mut reg, &mut t, w, b"", &[z]).unwrap();
        t.close(&mut reg, w).unwrap();
        t.close(&mut reg, y).unwrap();
        peak = peak.max(reg.live());
    }
    assert!(
        peak < 1000,
        "garbage grew to {peak} objects: the pressure sweep is not bounding it"
    );
    assert!(peak > 100, "the loop really made garbage ({peak})");
}

#[test]
fn the_object_count_is_capped() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(65535);
    let mut made = 0;
    loop {
        match event_create(&mut reg, &mut t) {
            Ok(_) => made += 1,
            Err(e) => {
                assert_eq!(e, Status::NoMemory);
                break;
            }
        }
        if t.room() == 0 {
            t = HandleTable::new(65535); // forget the handles (their objects stay, as leaked garbage would)
        }
        assert!(made <= abi::limits::MAX_OBJECTS, "no cap");
    }
    assert_eq!(made, abi::limits::MAX_OBJECTS);
}

#[test]
fn channel_write_checks_in_the_documented_order() {
    let (mut reg, mut t) = world();
    let (a, _b) = channel_create(&mut reg, &mut t).unwrap();
    let dup = t.duplicate(&mut reg, a, Rights::SAME).unwrap();
    let read_only = t.replace(dup, Rights(Rights::READ.0)).unwrap();
    let ev = event_create(&mut reg, &mut t).unwrap();
    let forged = Handle(0x1234_0001);
    // BAD_HANDLE (a listed handle) comes before WRONG_TYPE and ACCESS_DENIED on the channel
    assert_eq!(
        channel_write(&mut reg, &mut t, read_only, b"", &[forged]),
        Err(Status::BadHandle)
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, ev, b"", &[forged]),
        Err(Status::BadHandle)
    );
    // then WRONG_TYPE, then ACCESS_DENIED
    assert_eq!(
        channel_write(&mut reg, &mut t, ev, b"", &[]),
        Err(Status::WrongType)
    );
    assert_eq!(
        channel_write(&mut reg, &mut t, read_only, b"", &[]),
        Err(Status::AccessDenied)
    );
    // TOO_BIG comes first of all (it bounds the work of the rest)
    let many = vec![forged; MAX_MSG_HANDLES + 1];
    assert_eq!(
        channel_write(&mut reg, &mut t, Handle(0), b"", &many),
        Err(Status::TooBig)
    );
    assert_eq!(
        channel_write(
            &mut reg,
            &mut t,
            Handle(0),
            &vec![0; MAX_MSG_BYTES + 1],
            &[]
        ),
        Err(Status::TooBig)
    );
    t.drain(&mut reg);
}

#[test]
fn a_txid_read_on_a_closed_channel_does_not_wait_forever() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    channel_write(&mut reg, &mut t, a, &[1, 0, 0, 0, 9], &[]).unwrap(); // txid 1
    t.close(&mut reg, a).unwrap();
    assert_eq!(
        channel_read_txid(&mut reg, &mut t, b, 2, 16, 0),
        Err(ReadError::Status(Status::PeerClosed)),
        "no txid 2 will ever come"
    );
    assert_eq!(
        channel_read_txid(&mut reg, &mut t, b, 1, 16, 0)
            .unwrap()
            .data,
        vec![1, 0, 0, 0, 9],
        "but the queued txid 1 can still be read"
    );
    t.drain(&mut reg);
}

#[test]
fn queued_bytes_are_capped_per_endpoint() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    let chunk = vec![0u8; MAX_MSG_BYTES];
    for _ in 0..abi::limits::MAX_QUEUED_BYTES / MAX_MSG_BYTES {
        channel_write(&mut reg, &mut t, a, &chunk, &[]).unwrap();
    }
    assert_eq!(
        channel_write(&mut reg, &mut t, a, b"x", &[]),
        Err(Status::ShouldWait),
        "the byte budget is used up, not just the message count"
    );
    assert_eq!(signals_of(&reg, &t, a).unwrap() & signals::WRITABLE, 0);
    channel_read(&mut reg, &mut t, b, MAX_MSG_BYTES, 0).unwrap();
    assert_eq!(channel_write(&mut reg, &mut t, a, b"x", &[]), Ok(()));
    t.drain(&mut reg);
}

#[test]
fn the_object_cap_applies_to_channel_pairs_too() {
    let mut reg = Kobjs::new();
    let mut t = HandleTable::new(65535);
    let mut made = 0;
    loop {
        match channel_create(&mut reg, &mut t) {
            Ok(_) => made += 2,
            Err(e) => {
                assert_eq!(e, Status::NoMemory);
                break;
            }
        }
        if t.room() < 2 {
            t = HandleTable::new(65535); // forget the handles; the objects stay
        }
        assert!(made <= abi::limits::MAX_OBJECTS, "no cap");
    }
    assert_eq!(
        made,
        abi::limits::MAX_OBJECTS,
        "a pair is refused if it would pass the cap"
    );
    assert_eq!(reg.live(), abi::limits::MAX_OBJECTS);
}

#[test]
fn peek_predicts_exactly_what_a_read_would_do() {
    let (mut reg, mut t) = world();
    let (a, b) = channel_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        channel_peek(&reg, &t, b, None, 9, 9),
        Err(ReadError::Status(Status::ShouldWait))
    );
    let e = event_create(&mut reg, &mut t).unwrap();
    channel_write(&mut reg, &mut t, a, &[5, 0, 0, 0, 1, 2], &[e]).unwrap();
    assert_eq!(channel_peek(&reg, &t, b, None, 6, 1), Ok((6, 1)));
    assert_eq!(
        channel_peek(&reg, &t, b, None, 5, 1),
        Err(ReadError::TooSmall {
            bytes: 6,
            handles: 1
        })
    );
    assert_eq!(
        channel_peek(&reg, &t, b, None, 6, 0),
        Err(ReadError::TooSmall {
            bytes: 6,
            handles: 1
        })
    );
    assert_eq!(channel_peek(&reg, &t, b, Some(5), 6, 1), Ok((6, 1)));
    assert_eq!(
        channel_peek(&reg, &t, b, Some(6), 6, 1),
        Err(ReadError::Status(Status::ShouldWait)),
        "no such txid"
    );
    assert_eq!(
        channel_peek(&reg, &t, Handle(0), None, 6, 1),
        Err(ReadError::Status(Status::BadHandle))
    );
    let ev = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(
        channel_peek(&reg, &t, ev, None, 6, 1),
        Err(ReadError::Status(Status::WrongType))
    );
    // peeking changes nothing: the real read then agrees
    let got = channel_read(&mut reg, &mut t, b, 6, 1).unwrap();
    assert_eq!((got.data.len(), got.handles.len()), (6, 1));
    t.close(&mut reg, a).unwrap();
    assert_eq!(
        channel_peek(&reg, &t, b, None, 6, 1),
        Err(ReadError::Status(Status::PeerClosed))
    );
    t.drain(&mut reg);
    // a full table
    let mut small = HandleTable::new(4);
    let (a, b) = channel_create(&mut reg, &mut small).unwrap();
    let e1 = event_create(&mut reg, &mut small).unwrap();
    let e2 = event_create(&mut reg, &mut small).unwrap();
    channel_write(&mut reg, &mut small, a, b"", &[e1, e2]).unwrap();
    let _fill = (
        event_create(&mut reg, &mut small).unwrap(),
        event_create(&mut reg, &mut small).unwrap(),
    );
    assert_eq!(
        channel_peek(&reg, &small, b, None, 0, 2),
        Err(ReadError::Status(Status::NoMemory))
    );
    small.drain(&mut reg);
    assert_eq!(reg.live(), 0);
}

#[test]
fn destroyed_vmos_and_processes_are_handed_back_for_cleanup_exactly_once() {
    let (mut reg, mut t) = world();
    let v1 = vmo_object_create(&mut reg, &mut t, 101).unwrap();
    let v2 = vmo_object_create(&mut reg, &mut t, 102).unwrap();
    let (p1, p1_obj) = process_object_create(&mut reg, &mut t, 7, true).unwrap();
    let (_c1, _c2) = channel_create(&mut reg, &mut t).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    assert!(reg.drain_dead().is_empty(), "nothing has died yet");
    // a duplicate keeps the VMO alive: closing one handle is not a destruction
    let v2b = t.duplicate(&mut reg, v2, Rights::SAME).unwrap();
    t.close(&mut reg, v2).unwrap();
    assert!(reg.drain_dead().is_empty());
    t.close(&mut reg, v2b).unwrap();
    t.close(&mut reg, e).unwrap(); // events need no cleanup
    t.close(&mut reg, v1).unwrap();
    let dead: Vec<u64> = reg
        .drain_dead()
        .into_iter()
        .filter_map(|o| {
            if let KObj::Vmo(id) = o {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        dead,
        vec![102, 101],
        "both VMOs, once each, in destruction order"
    );
    assert!(reg.drain_dead().is_empty(), "drained means gone");
    // a process object destroyed while it is still running is handed back as not terminated
    t.close(&mut reg, p1).unwrap();
    match reg.drain_dead().pop() {
        Some(KObj::Process(p)) => assert_eq!((p.pid, p.terminated), (7, false)),
        _ => panic!("the process object was not handed back"),
    }
    assert!(!reg.is_alive(p1_obj));
    t.drain(&mut reg);
    // and objects destroyed by a sweep (a cycle holding a VMO in a queue) are handed back too
    let mut t = HandleTable::new(64);
    let (x, w) = channel_create(&mut reg, &mut t).unwrap();
    let (z, y) = channel_create(&mut reg, &mut t).unwrap();
    let v = vmo_object_create(&mut reg, &mut t, 555).unwrap();
    channel_write(&mut reg, &mut t, y, b"", &[x]).unwrap();
    channel_write(&mut reg, &mut t, w, b"", &[z, v]).unwrap(); // z and the VMO ride in x's queue
    t.close(&mut reg, w).unwrap();
    t.close(&mut reg, y).unwrap();
    assert!(
        reg.drain_dead().is_empty(),
        "counting alone frees nothing here"
    );
    let roots: Vec<_> = t.objects().collect();
    reg.sweep(&roots);
    let dead: Vec<u64> = reg
        .drain_dead()
        .into_iter()
        .filter_map(|o| {
            if let KObj::Vmo(id) = o {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(dead, vec![555], "the sweep reports the VMO it destroyed");
    assert_eq!(reg.live(), 0);
}

#[test]
fn process_and_vmo_handles_check_type_and_rights() {
    let (mut reg, mut t) = world();
    let v = vmo_object_create(&mut reg, &mut t, 9).unwrap();
    let (p, pobj) = process_object_create(&mut reg, &mut t, 3, true).unwrap();
    let e = event_create(&mut reg, &mut t).unwrap();
    assert_eq!(vmo_of(&reg, &t, v, Rights::MAP), Ok(9));
    assert_eq!(vmo_of(&reg, &t, p, Rights::NONE), Err(Status::WrongType));
    assert_eq!(
        vmo_of(&reg, &t, Handle(77), Rights::NONE),
        Err(Status::BadHandle)
    );
    let dup_v = t.duplicate(&mut reg, v, Rights::SAME).unwrap();
    let ro = t.replace(dup_v, Rights::READ).unwrap();
    assert_eq!(vmo_of(&reg, &t, ro, Rights::READ), Ok(9));
    assert_eq!(
        vmo_of(&reg, &t, ro, Rights::WRITE),
        Err(Status::AccessDenied)
    );
    assert_eq!(
        process_of(&reg, &t, p, Rights::MANAGE).map(|(_, pid, term, code)| (pid, term, code)),
        Ok((3, false, 0))
    );
    assert_eq!(
        process_of(&reg, &t, e, Rights::NONE).err(),
        Some(Status::WrongType)
    );
    assert_eq!(
        process_of(&reg, &t, v, Rights::NONE).err(),
        Some(Status::WrongType)
    );
    let dup_p = t.duplicate(&mut reg, p, Rights::SAME).unwrap();
    let inspect_less = t.replace(dup_p, Rights::WAIT).unwrap();
    assert_eq!(
        process_of(&reg, &t, inspect_less, Rights::INSPECT).err(),
        Some(Status::AccessDenied)
    );
    assert_eq!(signals_of(&reg, &t, p), Ok(0));
    process_mark_terminated(&mut reg, pobj, -1015);
    assert_eq!(signals_of(&reg, &t, p), Ok(signals::TERMINATED));
    assert_eq!(
        process_of(&reg, &t, p, Rights::INSPECT).map(|(_, _, term, code)| (term, code)),
        Ok((true, -1015))
    );
    assert_eq!(signals_of(&reg, &t, v), Ok(0));
    t.drain(&mut reg);
    let _ = reg.drain_dead();
    assert_eq!(reg.live(), 0);
}
