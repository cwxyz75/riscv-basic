//! Peek, copy, commit (OD161, T285): a big `channel_read` or `socket_read` claims what it will take, copies it out with the object lock let
//! go, and takes it only when the copy has succeeded. These tests put the kernel's steps apart (`World::call_split` with `defer_out`, then
//! `copy_out`, then `commit`) and put between them what another thread of the machine can do: unmap or decommit the destination with no
//! memory to commit it again, read the same endpoint, close the handle, close the peer. Whatever happens, nothing is lost or delivered twice,
//! the order of a channel and of a stream is kept, and every reference is accounted for.
//!
//! Mutation check (by hand, T285): make `read_message` commit before it hands the claim to its caller (take the message in the deferred
//! branch, as the code before OD161 did) and `a_failed_copy_out_leaves_the_message_queued` and the socket twin fail.
use abi::status::Status;
use abi::syscall as nr;

mod common;
use common::*;

const DST: u64 = BASE + 0x4000;
const DST_HANDLES: u64 = BASE + 0xE000;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Queue `data` (written through the source at BASE) with `handles` on `x`.
fn send(w: &mut World, x: u64, data: &[u8], handles: &[u64]) {
    w.poke(BASE, data);
    let words: Vec<u8> = handles.iter().flat_map(|h| h.to_le_bytes()).collect();
    w.poke(BASE + 0xF000, &words);
    w.ok(
        nr::CHANNEL_WRITE,
        [
            x,
            BASE,
            data.len() as u64,
            BASE + 0xF000,
            handles.len() as u64,
            0,
        ],
    );
}

/// The read the kernel makes, to its claim: (the call's provisional reply, the claim).
fn claim(w: &mut World, y: u64) -> ksys::Deferred {
    let (r, d) = w.call_split(
        nr::CHANNEL_READ,
        [y, DST, 65536, DST_HANDLES, 8, 0],
        None,
        true,
    );
    r.expect("handled").expect("claimed");
    d.expect("a big message is left for the caller")
}

/// A plain read (copied under the lock) into a fresh place: (bytes, handle values).
fn read_now(w: &mut World, y: u64) -> (Vec<u8>, Vec<u64>) {
    let at = BASE + 0x8000;
    let (nb, nh) = w.ok(nr::CHANNEL_READ, [y, at, 65536, DST_HANDLES, 8, 0]);
    let (nb, nh) = (nb.unwrap() as usize, nh.unwrap() as usize);
    let words = w.peek(DST_HANDLES, nh * 8);
    (
        w.peek(at, nb),
        words
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

/// The VMO behind the 16 read-write pages at BASE.
fn rw_vmo(w: &World) -> u64 {
    w.aspace
        .regions_info()
        .into_iter()
        .find(|r| r.start == BASE)
        .expect("the read-write region")
        .vmo
}

/// What a sibling thread's `vmo_decommit` does to `pages` pages of the destination from `page` (MM6), then every free frame taken away:
/// the copy out must commit those pages again and cannot.
fn decommit_and_starve(w: &mut World, page: u64, pages: u64) -> Vec<kcore::frame::Pfn> {
    let v = rw_vmo(w);
    w.aspace.unmap_vmo_pages(&mut w.mm, v, page, pages);
    w.vmos
        .decommit(&mut w.mm, &mut w.jobs, v, page, pages)
        .unwrap();
    let mut held = Vec::new();
    while let Some(p) = w.mm.db.alloc() {
        held.push(p);
    }
    held
}

fn give_back(w: &mut World, held: Vec<kcore::frame::Pfn>) {
    for p in held {
        w.mm.db.dec(p).unwrap();
    }
}

fn check(w: &mut World) {
    cleanup(w);
    audit(w).unwrap();
}

#[test]
fn a_failed_copy_out_leaves_the_message_queued() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let (e1, e2) = (
        w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap(),
        w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap(),
    );
    let m1 = pattern(5000, 1);
    send(&mut w, x, &m1, &[e1, e2]);
    send(&mut w, x, b"second", &[]);
    let d = claim(&mut w, y);
    assert_eq!((d.data.len(), d.nhandles), (5000, 2));
    // a sibling decommits the destination, and there is no memory to commit it again
    let held = decommit_and_starve(&mut w, 4, 2);
    assert_eq!(
        w.copy_out(&d),
        Err(Status::NoMemory),
        "real exhaustion is NO_MEMORY, not a bad address (OD161)"
    );
    give_back(&mut w, held);
    // the message is still there, first, with both handles
    let (bytes, hs) = read_now(&mut w, y);
    assert_eq!(
        bytes, m1,
        "the message a failed copy-out claimed is still queued, whole"
    );
    assert_eq!(hs.len(), 2);
    for h in hs {
        assert_eq!(
            w.call(nr::OBJECT_SIGNALS, [h, 0, 0, 0, 0, 0])
                .unwrap()
                .map(|_| ()),
            Ok(()),
            "a handle the message carried is live in the reader's table"
        );
    }
    assert_eq!(read_now(&mut w, y).0, b"second");
    check(&mut w);
}

#[test]
fn an_unmapped_destination_is_a_bad_address_and_loses_nothing() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let m1 = pattern(3000, 2);
    send(&mut w, x, &m1, &[]);
    let d = claim(&mut w, y);
    // a sibling unmaps the whole region the destination is in
    w.aspace
        .unmap_range(&mut w.mm, &mut w.vmos, &mut w.jobs, BASE, 16 * 4096)
        .unwrap();
    assert_eq!(w.copy_out(&d), Err(Status::InvalidArgs));
    // (a fresh region for the second read)
    let v = w.vmos.create(&mut w.jobs, 16);
    w.aspace
        .map_region(
            &mut w.vmos,
            BASE,
            16,
            v,
            0,
            kcore::pagetable::Perms::RW.user(),
        )
        .unwrap();
    assert_eq!(read_now(&mut w, y).0, m1);
    check(&mut w);
}

#[test]
fn a_second_reader_between_claim_and_commit_takes_the_message_and_the_first_reads_the_next() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let (m1, m2) = (pattern(4096, 3), pattern(2048, 4));
    send(&mut w, x, &m1, &[]);
    send(&mut w, x, &m2, &[]);
    // reader A claims m1 and copies it; reader B (another thread, same handle) reads m1 under the lock before A commits
    let a = claim(&mut w, y);
    assert_eq!(a.data, m1);
    w.copy_out(&a).unwrap();
    assert_eq!(read_now(&mut w, y).0, m1, "B takes the front message");
    assert!(w.commit(&a).is_none(), "A's claim is gone: moved");
    // A reads again (the kernel's loop) and gets the next message: each message once, in order
    let a2 = claim(&mut w, y);
    assert_eq!(a2.data, m2);
    w.copy_out(&a2).unwrap();
    assert_eq!(w.commit(&a2), Some(Ok((Some(2048), Some(0)))));
    assert_eq!(w.peek(DST, 2048), m2);
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, DST, 65536, DST_HANDLES, 8, 0]),
        Status::ShouldWait
    );
    check(&mut w);
}

#[test]
fn two_deferred_readers_of_one_message_deliver_it_once() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    let m1 = pattern(4096, 5);
    send(&mut w, x, &m1, &[e]);
    let a = claim(&mut w, y);
    let b = claim(&mut w, y);
    w.copy_out(&b).unwrap();
    assert_eq!(w.commit(&b), Some(Ok((Some(4096), Some(1)))));
    w.copy_out(&a).unwrap();
    assert!(
        w.commit(&a).is_none(),
        "the second commit of one message is moved"
    );
    assert_eq!(
        w.handles.len(),
        3,
        "the pair and one received handle, not two"
    );
    check(&mut w);
}

#[test]
fn a_txid_read_commits_its_own_message_when_another_read_takes_an_earlier_one() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let mut reply = pattern(2000, 6);
    reply[..4].copy_from_slice(&9u32.to_le_bytes());
    send(&mut w, x, b"other", &[]);
    send(&mut w, x, &reply, &[]);
    let (r, d) = w.call_split(
        nr::CHANNEL_READ_TXID,
        [y, 9, DST, 65536, DST_HANDLES, 8],
        None,
        true,
    );
    r.unwrap().unwrap();
    let d = d.unwrap();
    // another reader takes the front message, which moves the reply's index but not its serial
    assert_eq!(read_now(&mut w, y).0, b"other");
    w.copy_out(&d).unwrap();
    assert_eq!(w.commit(&d), Some(Ok((Some(2000), Some(0)))));
    assert_eq!(w.peek(DST, 2000), reply);
    check(&mut w);
}

#[test]
fn a_peer_that_closes_between_claim_and_commit_changes_nothing() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let m1 = pattern(4096, 7);
    send(&mut w, x, &m1, &[]);
    let d = claim(&mut w, y);
    w.ok(nr::HANDLE_CLOSE, [x, 0, 0, 0, 0, 0]);
    check(&mut w);
    w.copy_out(&d).unwrap();
    assert_eq!(w.commit(&d), Some(Ok((Some(4096), Some(0)))));
    assert_eq!(
        w.err(nr::CHANNEL_READ, [y, DST, 65536, DST_HANDLES, 8, 0]),
        Status::PeerClosed
    );
    check(&mut w);
}

#[test]
fn a_handle_closed_between_claim_and_commit_takes_nothing_and_leaks_nothing() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    send(&mut w, x, &pattern(4096, 8), &[e]);
    let y2 = w
        .ok(nr::HANDLE_DUPLICATE, [y, 1 << 31, 0, 0, 0, 0])
        .0
        .unwrap();
    let d = claim(&mut w, y);
    w.ok(nr::HANDLE_CLOSE, [y, 0, 0, 0, 0, 0]);
    w.copy_out(&d).unwrap();
    assert_eq!(
        w.commit(&d),
        Some(Err((Status::BadHandle, None, None))),
        "the claim's handle is gone"
    );
    // the endpoint lives on through the duplicate, its message with it
    assert_eq!(read_now(&mut w, y2).1.len(), 1);
    check(&mut w);
    // and when the last handle goes, the queued message's references go with it
    let (x3, y3) = w.pair(nr::CHANNEL_CREATE);
    let e3 = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
    send(&mut w, x3, &pattern(4096, 9), &[e3]);
    let d = claim(&mut w, y3);
    w.ok(nr::HANDLE_CLOSE, [y3, 0, 0, 0, 0, 0]);
    check(&mut w);
    w.copy_out(&d).unwrap();
    assert_eq!(w.commit(&d), Some(Err((Status::BadHandle, None, None))));
    check(&mut w);
}

#[test]
fn a_failed_copy_of_the_handle_numbers_puts_the_handles_back() {
    let mut w = world();
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    let (e1, e2) = (
        w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap(),
        w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap(),
    );
    let m1 = pattern(4096, 10);
    send(&mut w, x, &m1, &[e1, e2]);
    let before = w.handles.len();
    let d = claim(&mut w, y);
    w.copy_out(&d).unwrap();
    // the page of the handle numbers is decommitted and cannot be committed again: the commit fails, and nothing moved
    let held = decommit_and_starve(&mut w, 0xE, 1);
    assert_eq!(w.commit(&d), Some(Err((Status::NoMemory, None, None))));
    assert_eq!(
        w.handles.len(),
        before,
        "the handles went back into the message"
    );
    give_back(&mut w, held);
    check(&mut w);
    let (bytes, hs) = read_now(&mut w, y);
    assert_eq!((bytes, hs.len()), (m1, 2));
    check(&mut w);
}

#[test]
fn a_failed_stream_copy_out_leaves_the_bytes_queued() {
    let mut w = world();
    let (s, t) = w.pair(nr::SOCKET_CREATE);
    let data = pattern(6000, 11);
    w.poke(BASE, &data);
    w.ok(nr::SOCKET_WRITE, [s, BASE, 6000, 0, 0, 0]);
    let (r, d) = w.call_split(nr::SOCKET_READ, [t, DST, 6000, 0, 0, 0], None, true);
    assert_eq!(r.unwrap().unwrap().0, Some(6000));
    let d = d.unwrap();
    let held = decommit_and_starve(&mut w, 5, 1);
    assert_eq!(w.copy_out(&d), Err(Status::NoMemory));
    give_back(&mut w, held);
    let at = BASE + 0x8000;
    let (n, _) = w.ok(nr::SOCKET_READ, [t, at, 6000, 0, 0, 0]);
    assert_eq!(n, Some(6000));
    assert_eq!(w.peek(at, 6000), data, "every byte, in order");
    check(&mut w);
}

#[test]
fn a_second_stream_reader_between_claim_and_commit_keeps_the_stream_in_order() {
    let mut w = world();
    let (s, t) = w.pair(nr::SOCKET_CREATE);
    let data = pattern(8000, 12);
    w.poke(BASE, &data);
    w.ok(nr::SOCKET_WRITE, [s, BASE, 8000, 0, 0, 0]);
    // A claims the first 4096 bytes; B reads 100 bytes under the lock; A's commit is moved and its next claim starts at byte 100
    let (_, a) = w.call_split(nr::SOCKET_READ, [t, DST, 4096, 0, 0, 0], None, true);
    let a = a.unwrap();
    w.copy_out(&a).unwrap();
    let at = BASE + 0x9000;
    assert_eq!(w.ok(nr::SOCKET_READ, [t, at, 100, 0, 0, 0]).0, Some(100));
    assert!(w.commit(&a).is_none(), "moved");
    w.defer_reads = true;
    assert_eq!(w.ok(nr::SOCKET_READ, [t, DST, 4096, 0, 0, 0]).0, Some(4096));
    let mut got = w.peek(at, 100);
    got.extend(w.peek(DST, 4096));
    assert_eq!(got, data[..4196], "the stream is read once, in order");
    assert_eq!(w.ok(nr::SOCKET_READ, [t, DST, 4096, 0, 0, 0]).0, Some(3804));
    assert_eq!(w.peek(DST, 3804), data[4196..]);
    check(&mut w);
}

#[test]
fn the_deferred_protocol_delivers_what_the_locked_read_delivers() {
    // (the whole loop, as the kernel runs it, against the plain read: the same bytes, handles and accounting)
    let mut w = world();
    w.defer_reads = true;
    let (x, y) = w.pair(nr::CHANNEL_CREATE);
    for (i, len) in [10usize, 1023, 1024, 5000, 40000].into_iter().enumerate() {
        let m = pattern(len, i as u8);
        let hs = if i % 2 == 0 {
            vec![]
        } else {
            vec![w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap()]
        };
        send(&mut w, x, &m, &hs);
        let (nb, nh) = w.ok(nr::CHANNEL_READ, [y, BASE, 65536, DST_HANDLES, 8, 0]);
        assert_eq!((nb, nh), (Some(len as u64), Some(hs.len() as u64)));
        assert_eq!(w.peek(BASE, len), m);
        check(&mut w);
    }
}
