//! Peek, copy, commit (OD161, T285) under shuttle, over the real `kcore::ipc` queue: two readers of one endpoint that claim under the
//! object lock, copy with it let go (a yield point, where the copy may fail), and commit under it again, against a writer (the channel's
//! last message, the stream's second half), a killer that
//! closes the readers' handle (what a process teardown does to the table) and a peer that closes. Whatever the interleaving: every message
//! and every stream byte is delivered exactly once, in the queue's order, a failed copy loses nothing, the bytes a reader copied are the
//! bytes its commit removed, and when everything is closed nothing is left alive (H4, H7). Run with
//!   RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test read_commit_concurrency --target-dir target/shuttle
//! Without the cfg this file is empty.
//!
//! The negative controls are the protocol as it was before OD161 (take at the claim, then copy: a failed copy loses the message) and a
//! commit that does not name what it copied (it removes whatever is at the front: a second reader makes it deliver bytes it did not
//! remove); shuttle must find a violating schedule for each. (A txid read claims by the same serial; it is covered on the host,
//! `ksys/tests/read_commit.rs`, and the write of the handle numbers failing, which runs inside one lock section, there too.)
#![cfg(shuttle)]

use abi::status::Status;
use abi::Handle;
use kcore::handle::HandleTable;
use kcore::ipc::{self, CommitError, Kobjs};
use kcore::job::Jobs;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

/// The object domain: everything the object lock covers, with what the readers delivered, in commit order.
struct Dom {
    reg: Kobjs,
    table: HandleTable,
    jobs: Jobs,
    /// (message id or first stream byte, the bytes the reader copied)
    delivered: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Protocol {
    /// OD161: claim, copy, commit by the claim.
    PeekCopyCommit,
    /// Before OD161: take the message at the claim, then copy (a failed copy loses it).
    TakeThenCopy,
    /// Stream only: copy what was claimed, then remove whatever `n` bytes are at the front then.
    CommitWithoutClaim,
}

/// A reader's loop: up to `msgs + 2` reads, the first copy failing when `fail_first` (a sibling unmapped the buffer).
fn channel_reader(d: Arc<Mutex<Dom>>, y: Handle, fail_first: bool, p: Protocol, msgs: u8) {
    let mut fail = fail_first;
    for _ in 0..msgs + 2 {
        let copied = {
            let mut g = d.lock().unwrap();
            let Dom {
                reg, table, jobs, ..
            } = &mut *g;
            match p {
                Protocol::TakeThenCopy => match ipc::channel_read_in(reg, table, y, 64, 4, jobs) {
                    Ok(r) => (None, r.data),
                    Err(_) => return,
                },
                _ => match ipc::channel_claim(reg, table, y, None, 64, 4) {
                    Ok((c, _, _)) => (Some(c), ipc::claimed_bytes(reg, &c).unwrap().to_vec()),
                    Err(_) => return,
                },
            }
        };
        // the copy, with the object lock let go
        thread::yield_now();
        if std::mem::take(&mut fail) {
            continue; // (the copy failed: the call reports its status; the caller reads again)
        }
        let mut g = d.lock().unwrap();
        let Dom {
            reg,
            table,
            jobs,
            delivered,
        } = &mut *g;
        match copied {
            (None, bytes) => delivered.push(bytes),
            (Some(c), bytes) => match ipc::channel_commit(reg, table, y, &c, jobs, |_, _| Ok(())) {
                Ok(r) => {
                    assert_eq!(
                        r.data, bytes,
                        "the commit took the message the reader copied"
                    );
                    delivered.push(bytes);
                }
                Err(CommitError::Moved) => {}
                Err(CommitError::Status(Status::BadHandle)) => return,
                Err(e) => panic!("unexpected commit error {e:?}"),
            },
        }
    }
}

/// `msgs` messages, two readers (the first copy of the first failing), the killer, and the peer's close when `closer`.
fn channel_model(p: Protocol, msgs: u8, closer: bool) {
    let mut reg = Kobjs::new();
    let mut table = HandleTable::new(64);
    let (x, y) = ipc::channel_create(&mut reg, &mut table).unwrap();
    // the readers' own handle, which the killer closes; `keep` drains what is left at the end
    let ry = table
        .duplicate(&mut reg, y, abi::rights::Rights::CHANNEL)
        .unwrap();
    // (all but the last now; the writer thread sends the last while the readers run)
    for id in 0..msgs - 1 {
        let e = ipc::event_create(&mut reg, &mut table).unwrap();
        ipc::channel_write(&mut reg, &mut table, x, &[id; 8], &[e]).unwrap();
    }
    let d = Arc::new(Mutex::new(Dom {
        reg,
        table,
        jobs: Jobs::new(),
        delivered: Vec::new(),
    }));
    let readers: Vec<_> = [true, false]
        .into_iter()
        .map(|fail_first| {
            let d = d.clone();
            thread::spawn(move || channel_reader(d, ry, fail_first, p, msgs))
        })
        .collect();
    let killer = {
        let d = d.clone();
        thread::spawn(move || {
            thread::yield_now();
            let mut g = d.lock().unwrap();
            let Dom { reg, table, .. } = &mut *g;
            let _ = table.close(reg, ry);
        })
    };
    // the writer: the last message, then (when `closer`) the peer's close
    let closer = {
        let d = d.clone();
        thread::spawn(move || {
            {
                let mut g = d.lock().unwrap();
                let Dom { reg, table, .. } = &mut *g;
                let e = ipc::event_create(reg, table).unwrap();
                ipc::channel_write(reg, table, x, &[msgs - 1; 8], &[e]).unwrap();
            }
            if closer {
                thread::yield_now();
                let mut g = d.lock().unwrap();
                let Dom { reg, table, .. } = &mut *g;
                let _ = table.close(reg, x);
            }
        })
    };
    for t in readers {
        t.join().unwrap();
    }
    killer.join().unwrap();
    closer.join().unwrap();
    let mut g = d.lock().unwrap();
    let Dom {
        reg,
        table,
        jobs,
        delivered,
    } = &mut *g;
    while let Ok(r) = ipc::channel_read_in(reg, table, y, 64, 4, jobs) {
        delivered.push(r.data);
    }
    let ids: Vec<u8> = delivered.iter().map(|m| m[0]).collect();
    assert_eq!(
        ids,
        (0..msgs).collect::<Vec<u8>>(),
        "every message delivered once, in order (a lost message is missing here)"
    );
    table.drain(reg);
    while !reg.release_step(1 << 20) {}
    assert_eq!(
        reg.live(),
        0,
        "nothing left alive once every handle is closed"
    );
}

const STREAM: u8 = 24;

fn socket_reader(d: Arc<Mutex<Dom>>, t: Handle, fail_first: bool, p: Protocol) {
    let mut fail = fail_first;
    for _ in 0..6 {
        let (claim, n, copied) = {
            let mut g = d.lock().unwrap();
            let Dom { reg, table, .. } = &mut *g;
            match ipc::socket_claim(reg, table, t, 7) {
                Ok((c, n)) => {
                    let (a, b) = ipc::claimed_stream(reg, &c, n).unwrap();
                    (c, n, [a, b].concat())
                }
                Err(Status::ShouldWait) => {
                    drop(g);
                    thread::yield_now();
                    continue;
                }
                Err(_) => return,
            }
        };
        thread::yield_now();
        if std::mem::take(&mut fail) {
            continue;
        }
        let mut g = d.lock().unwrap();
        let Dom {
            reg,
            table,
            jobs,
            delivered,
        } = &mut *g;
        if p == Protocol::CommitWithoutClaim {
            match ipc::socket_read_in(reg, table, t, n, jobs) {
                Ok(_) => delivered.push(copied),
                Err(_) => return,
            }
            continue;
        }
        match ipc::socket_commit(reg, table, t, &claim, n, jobs) {
            Ok(()) => delivered.push(copied),
            Err(CommitError::Moved) => {}
            Err(_) => return,
        }
    }
}

fn socket_model(p: Protocol) {
    let mut reg = Kobjs::new();
    let mut table = HandleTable::new(64);
    let (s, t) = ipc::socket_create(&mut reg, &mut table).unwrap();
    let rt = table
        .duplicate(&mut reg, t, abi::rights::Rights::SOCKET)
        .unwrap();
    let data: Vec<u8> = (0..STREAM).collect();
    ipc::socket_write(&mut reg, &table, s, &data[..12]).unwrap();
    let d = Arc::new(Mutex::new(Dom {
        reg,
        table,
        jobs: Jobs::new(),
        delivered: Vec::new(),
    }));
    let readers: Vec<_> = [true, false]
        .into_iter()
        .map(|fail_first| {
            let d = d.clone();
            thread::spawn(move || socket_reader(d, rt, fail_first, p))
        })
        .collect();
    let writer = {
        let d = d.clone();
        let rest = data[12..].to_vec();
        thread::spawn(move || {
            for chunk in rest.chunks(6) {
                let mut g = d.lock().unwrap();
                let Dom { reg, table, .. } = &mut *g;
                ipc::socket_write(reg, table, s, chunk).unwrap();
                drop(g);
                thread::yield_now();
            }
        })
    };
    let killer = {
        let d = d.clone();
        thread::spawn(move || {
            thread::yield_now();
            thread::yield_now();
            let mut g = d.lock().unwrap();
            let Dom { reg, table, .. } = &mut *g;
            let _ = table.close(reg, rt);
        })
    };
    for r in readers {
        r.join().unwrap();
    }
    writer.join().unwrap();
    killer.join().unwrap();
    let mut g = d.lock().unwrap();
    let Dom {
        reg,
        table,
        jobs,
        delivered,
    } = &mut *g;
    while let Ok(b) = ipc::socket_read_in(reg, table, t, 64, jobs) {
        if b.is_empty() {
            break;
        }
        delivered.push(b);
    }
    assert_eq!(
        delivered.concat(),
        data,
        "the stream delivered once, in order, each reader's bytes the ones its commit removed"
    );
    table.drain(reg);
    while !reg.release_step(1 << 20) {}
    assert_eq!(reg.live(), 0);
}

#[test]
fn channel_reads_lose_nothing_and_deliver_once() {
    shuttle::check_random(|| channel_model(Protocol::PeekCopyCommit, 3, true), 20000);
    shuttle::check_pct(|| channel_model(Protocol::PeekCopyCommit, 3, true), 5000, 3);
}

#[test]
fn stream_reads_lose_nothing_and_keep_the_order() {
    shuttle::check_random(|| socket_model(Protocol::PeekCopyCommit), 20000);
    shuttle::check_pct(|| socket_model(Protocol::PeekCopyCommit), 5000, 3);
}

#[test]
#[should_panic(expected = "every message delivered once")]
fn negative_control_take_then_copy_loses_a_message() {
    shuttle::check_random(|| channel_model(Protocol::TakeThenCopy, 3, true), 20000);
}

#[test]
#[should_panic(expected = "the stream delivered once")]
fn negative_control_a_commit_that_does_not_name_its_claim_reorders_the_stream() {
    shuttle::check_random(|| socket_model(Protocol::CommitWithoutClaim), 20000);
}
