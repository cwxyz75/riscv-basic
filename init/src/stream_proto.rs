//! S9 step 1: the stream data-path prototype (S9-SPEC 4.7). Included by `suite.rs` as a child module (it uses the suite's
//! helpers). No TCP is involved: these tests pin what the proposal of S9-SPEC 4 relies on the kernel doing for a socket
//! pair that a *client* creates and hands to a *server*, and they print the numbers the owner decides on (`##PROTO`).
//!
//! The "client" and the "server" are children running the socket hand (`crash.rs` behaviour 70) in jobs of their own, so
//! the jobs' ledgers (`job_info`, selector 5: IPC bytes) show who is charged. Where the test needs the server to be the
//! program under test (the stack's side of a stream), the suite process itself plays the server, in the root job.
use super::*;

const MAX: u64 = u64::MAX;
/// A job with an IPC byte limit and nothing else limited.
fn ipc_job(root: u64, ipc: u64) -> Result<u64, Detail> {
    Ok(ok(
        sys::job_create(root, &[MAX, MAX, MAX, MAX, MAX, ipc]),
        "an IPC-limited job",
    )?
    .a1 as u64)
}

/// A socket hand: a child that obeys one-letter commands on its bootstrap channel.
struct Hand {
    proc: u64,
    ch: u64,
}

struct Reply {
    a: u32,
    status: i32,
    handle: Option<u64>,
}

impl Hand {
    fn start(job: u64) -> Result<Hand, Detail> {
        let (proc, ch) = launch_image_in(IMAGE.load(Ordering::Relaxed), 70, job)?;
        Ok(Hand { proc, ch })
    }

    /// Send a command (`cmd`, `n`, and an optional handle) without waiting for the reply.
    fn send(&self, cmd: u8, n: u8, h: Option<u64>) -> Outcome {
        let m = [cmd, n];
        let hs = [h.unwrap_or(0)];
        ok(
            sys::channel_write(
                self.ch,
                m.as_ptr(),
                2,
                hs.as_ptr(),
                usize::from(h.is_some()),
            ),
            "send a command",
        )?;
        Ok(())
    }

    fn reply(&self) -> Result<Reply, Detail> {
        let r = sys::object_wait(
            self.ch,
            sig::READABLE | sig::PEER_CLOSED,
            sys::clock_get() + 5000,
        );
        ensure_eq!(r.status, 0, "the hand answered in time");
        let mut b = [0u8; 13];
        let mut h = [0u64; 1];
        let r = sys::channel_read(self.ch, b.as_mut_ptr(), 13, h.as_mut_ptr(), 1);
        ensure_eq!((r.status, r.a1), (0, 13), "a reply of 13 bytes");
        Ok(Reply {
            a: u32::from_le_bytes([b[1], b[2], b[3], b[4]]),
            status: i32::from_le_bytes([b[5], b[6], b[7], b[8]]),
            handle: (r.a2 == 1).then_some(h[0]),
        })
    }

    fn cmd(&self, cmd: u8, n: u8, h: Option<u64>) -> Result<Reply, Detail> {
        self.send(cmd, n, h)?;
        self.reply()
    }

    fn finish(self) {
        let _ = self.send(b'X', 0, None);
        let _ = wait_ended(self.proc, 3000);
        sys::handle_close(self.proc);
        sys::handle_close(self.ch);
    }
}

fn ipc_used(j: u64) -> Result<u64, Detail> {
    Ok(job_use(j, 5)?.0)
}

const KIB: u64 = 1024;

/// P1, both variants: who is charged for the bytes queued in a socket pair, before and after the end changed hands.
fn charged_to(client_creates: bool) -> Outcome {
    let root = root_job()?;
    let jc = ipc_job(root, 1024 * KIB)?;
    let js = ipc_job(root, 1024 * KIB)?;
    let client = Hand::start(jc)?;
    let server = Hand::start(js)?;
    let (creator, other, creator_job, other_job) = if client_creates {
        (&client, &server, jc, js)
    } else {
        (&server, &client, js, jc)
    };
    let (c0, o0) = (ipc_used(creator_job)?, ipc_used(other_job)?);
    let end = creator.cmd(b'C', 0, None)?.handle;
    ensure!(end.is_some(), "the creator sent an end back");
    // hand the end over: it moves to the other process, the creator keeps the other end
    ensure_eq!(other.cmd(b'H', 0, end)?.status, 0, "the end changed hands");
    // 64 KiB into each direction: 16 units of 4 KiB from each side
    let w1 = creator.cmd(b'W', 16, None)?;
    let w2 = other.cmd(b'W', 16, None)?;
    ensure_eq!(
        (w1.status, w1.a),
        (0, 64 * 1024),
        "64 KiB queued by the creator"
    );
    ensure_eq!(
        (w2.status, w2.a),
        (0, 64 * 1024),
        "64 KiB queued by the other side"
    );
    let (c1, o1) = (ipc_used(creator_job)?, ipc_used(other_job)?);
    let who = if client_creates { "client" } else { "server" };
    println!(
        "##PROTO p1 creator={who} creator_job_delta={} other_job_delta={}",
        c1 as i64 - c0 as i64,
        o1 as i64 - o0 as i64
    );
    ensure!(
        c1 == c0 + 128 * KIB,
        "the creator's job holds both directions' 64 KiB ({c0} -> {c1})"
    );
    ensure!(
        o1 <= o0 + 8 * KIB,
        "the other job's ledger did not move for the bytes ({o0} -> {o1})"
    );
    // the other side lets go of its end (it exits): what was queued toward it is freed, the creator's own direction stays
    other.finish_ref();
    let mut settled = false;
    for _ in 0..200 {
        let u = ipc_used(creator_job)?;
        if u <= c0 + 64 * KIB + 8 * KIB {
            settled = true;
            break;
        }
        nap(5)?;
    }
    ensure!(settled, "the bytes queued toward the closed end were freed");
    creator.finish_ref();
    job_use_settles(creator_job, 5, 0, 2000).or_else(|_| {
        // (a few hundred bytes of channel messages may linger a moment; the bound is what matters)
        let u = ipc_used(creator_job)?;
        ensure!(u <= 8 * KIB, "the creator's job ledger came back: {u}");
        Ok(())
    })?;
    sys::handle_close(client.proc);
    sys::handle_close(client.ch);
    sys::handle_close(server.proc);
    sys::handle_close(server.ch);
    sys::handle_close(jc);
    sys::handle_close(js);
    sys::handle_close(root);
    Ok(())
}

impl Hand {
    /// Ask the hand to exit and wait for it, keeping the handles (the caller closes them).
    fn finish_ref(&self) {
        let _ = self.send(b'X', 0, None);
        let _ = wait_ended(self.proc, 3000);
    }
}

pub fn stream_buffers_are_charged_to_the_client() -> Outcome {
    charged_to(true)?;
    charged_to(false)
}

/// P2: a client whose job has almost no IPC quota left: the server's write is refused with NO_MEMORY while the end is still
/// WRITABLE, the server's retry rule keeps the attempts few, another client is unaffected, and the first resumes as it reads.
pub fn a_client_at_quota_stalls_only_itself() -> Outcome {
    let root = root_job()?;
    let poor = ipc_job(root, 40 * KIB)?;
    let rich = ipc_job(root, 1024 * KIB)?;
    let c1 = Hand::start(poor)?;
    let c2 = Hand::start(rich)?;
    let e1 = c1
        .cmd(b'C', 0, None)?
        .handle
        .ok_or_else(|| msg_at(line!()))?;
    let e2 = c2
        .cmd(b'C', 0, None)?
        .handle
        .ok_or_else(|| msg_at(line!()))?;
    // the suite plays the server: write into each client's pair until it says no
    let block = [5u8; 1024];
    let fill = |h: u64| -> (u64, i32) {
        let mut n = 0u64;
        loop {
            let w = sys::socket_write(h, block.as_ptr(), block.len());
            if w.status != 0 {
                return (n, w.status as i32);
            }
            n += w.a1 as u64;
        }
    };
    let (n1, s1) = fill(e1);
    ensure_eq!(
        s1,
        S::NoMemory.code(),
        "the poor client's quota, not its buffer, ended the writes"
    );
    ensure!(
        (24 * KIB..=40 * KIB).contains(&n1),
        "the poor client took {n1} bytes of a 40 KiB quota"
    );
    ensure!(
        sys::object_signals(e1).a1 as u32 & sig::WRITABLE != 0,
        "WRITABLE stays set at NO_MEMORY (so a level-triggered wait on it would spin)"
    );
    // the retry rule of S9-SPEC 4.4: 20 ms, doubling to 500 ms; count the attempts in two seconds
    let (mut attempts, mut delay) = (0u32, 20u64);
    let end = sys::clock_get() + 2000;
    while sys::clock_get() < end {
        attempts += 1;
        let w = sys::socket_write(e1, block.as_ptr(), block.len());
        ensure_eq!(
            w.status as i32,
            S::NoMemory.code(),
            "still no memory while the client does not read"
        );
        nap(delay)?;
        delay = (delay * 2).min(500);
    }
    println!("##PROTO p2 attempts_in_2s={attempts}");
    ensure!(
        (3..=60).contains(&attempts),
        "{attempts} attempts in two seconds"
    );
    // another client is not affected: its whole buffer fills, and it is the buffer (SHOULD_WAIT) that ends it
    let (n2, s2) = fill(e2);
    ensure_eq!(s2, S::ShouldWait.code(), "the rich client fills its buffer");
    ensure_eq!(n2, 64 * KIB, "all 64 KiB");
    // the poor client reads what is queued: its quota comes back and the server's next write goes in
    let r = c1.cmd(b'R', 0, None)?;
    ensure_eq!(r.a as u64, n1, "the client read what the server wrote");
    let w = sys::socket_write(e1, block.as_ptr(), block.len());
    ensure_eq!(w.status, 0, "the stalled client resumed");
    sys::handle_close(e1);
    sys::handle_close(e2);
    c1.finish();
    c2.finish();
    sys::handle_close(poor);
    sys::handle_close(rich);
    sys::handle_close(root);
    Ok(())
}

/// P3: two one-shot bindings on one socket end in one port, with different keys and masks, re-armed independently.
pub fn one_socket_two_one_shot_bindings() -> Outcome {
    let (a, b) = {
        let r = ok(sys::socket_create(), "a socket pair")?;
        (r.a1 as u64, r.a2 as u64)
    };
    let port = ok(sys::port_create(), "a port")?.a1 as u64;
    let (rd, wr) = (1u64, 2u64);
    let bind = |mask: u32, key: u64| sys::port_bind_flags(port, a, mask, key, abi_oneshot());
    let first = bind(sig::READABLE, rd);
    let second = bind(sig::WRITABLE, wr);
    if first.status != 0 || second.status != 0 {
        println!(
            "##PROTO p3 two_bindings=refused (statuses {} {})",
            first.status, second.status
        );
        return Err(msg_at(line!()));
    }
    println!("##PROTO p3 two_bindings=accepted");
    let wait = |ms: u64| sys::port_wait(port, sys::clock_get() + ms);
    let r = wait(100);
    ensure_eq!(
        (r.status, r.a1 as u64),
        (0, wr),
        "WRITABLE fires first: the buffer is empty"
    );
    ensure_eq!(
        wait(30).status,
        S::TimedOut.code(),
        "one-shot: not again until re-armed, and nothing is readable yet"
    );
    ok(sys::socket_write(b, b"x".as_ptr(), 1), "write to the peer")?;
    let r = wait(100);
    ensure_eq!(
        (r.status, r.a1 as u64),
        (0, rd),
        "READABLE fires after the peer wrote"
    );
    ensure_eq!(wait(30).status, S::TimedOut.code(), "both are disarmed");
    ok(
        sys::port_rearm(port, wr),
        "re-arm the WRITABLE binding alone",
    )?;
    let r = wait(100);
    ensure_eq!(
        (r.status, r.a1 as u64),
        (0, wr),
        "it fires at once: still writable"
    );
    ensure_eq!(
        wait(30).status,
        S::TimedOut.code(),
        "the READABLE binding stayed disarmed"
    );
    ok(sys::port_rearm(port, rd), "re-arm READABLE")?;
    let r = wait(100);
    ensure_eq!(
        (r.status, r.a1 as u64),
        (0, rd),
        "it fires: the byte is still there"
    );
    sys::handle_close(port);
    sys::handle_close(a);
    sys::handle_close(b);
    Ok(())
}

fn abi_oneshot() -> u64 {
    rt::abi::port_flags::ONESHOT
}

/// P4: numbers, no pass threshold. A client-created pair; the suite is the server and reads 64 KiB the client writes after
/// being told to (so the reader really waits and wakes), and writes 64 KiB the client drains. Bytes per `socket_*` call,
/// wakes and time per MiB, for three chunk sizes.
pub fn stream_pipe_numbers() -> Outcome {
    let root = root_job()?;
    let j = ipc_job(root, 1024 * KIB)?;
    let hand = Hand::start(j)?;
    let end = hand
        .cmd(b'C', 0, None)?
        .handle
        .ok_or_else(|| msg_at(line!()))?;
    let mut buf = vec![0u8; 16 * 1024];
    // (chunk size, 4 KiB units per round, rounds): 4 KiB a byte at a time, 256 KiB in 512-byte chunks, 1 MiB in 16 KiB chunks
    for &(chunk, units, rounds) in &[(1usize, 1u8, 1u64), (512, 16, 4), (16 * 1024, 16, 16)] {
        let round = u64::from(units) * 4 * KIB;
        // server reads (client writes)
        let (mut wakes, mut calls) = (0u64, 0u64);
        let t0 = sys::clock_get();
        for _ in 0..rounds {
            hand.send(b'W', units, None)?;
            let mut got = 0usize;
            let deadline = sys::clock_get() + 10_000;
            while (got as u64) < round {
                ensure!(
                    sys::clock_get() < deadline,
                    "the hand wrote the round in time"
                );
                let r = sys::socket_read(end, buf.as_mut_ptr(), chunk);
                calls += 1;
                match r.status() {
                    Some(S::Ok) => got += r.a1,
                    Some(S::ShouldWait) => {
                        wakes += 1;
                        let w = sys::object_wait(
                            end,
                            sig::READABLE | sig::PEER_CLOSED,
                            sys::clock_get() + 2000,
                        );
                        ensure_eq!(w.status, 0, "the hand wrote something");
                    }
                    _ => return Err(msg_at(line!())),
                }
            }
            let _ = hand.reply()?;
        }
        let ms = sys::clock_get() - t0;
        let bytes = rounds * round;
        println!(
            "##PROTO p4 read chunk={chunk} bytes={bytes} ms={ms} wakes={wakes} calls={calls} bytes_per_wake={}",
            bytes / wakes.max(1)
        );
        // server writes (client drains)
        let (mut calls, t0) = (0u64, sys::clock_get());
        let data = vec![9u8; chunk];
        for _ in 0..rounds {
            let mut sent = 0u64;
            while sent < round {
                let w =
                    sys::socket_write(end, data.as_ptr(), data.len().min((round - sent) as usize));
                calls += 1;
                match w.status() {
                    Some(S::Ok) => sent += w.a1 as u64,
                    _ => return Err(msg_at(line!())),
                }
            }
            let r = hand.cmd(b'R', 0, None)?;
            ensure_eq!(u64::from(r.a), round, "the client drained the round");
        }
        let ms = sys::clock_get() - t0;
        println!("##PROTO p4 write chunk={chunk} bytes={bytes} ms={ms} calls={calls}");
    }
    sys::handle_close(end);
    hand.finish();
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// P5: kill the client in the middle of a transfer, with bytes queued both ways: the server's end sees PEER_CLOSED, what the
/// client had queued is still delivered, and every ledger of the client's job returns to nothing once the server lets go.
pub fn a_dead_client_frees_everything() -> Outcome {
    let root = root_job()?;
    let j = ipc_job(root, 1024 * KIB)?;
    let hand = Hand::start(j)?;
    let end = hand
        .cmd(b'C', 0, None)?
        .handle
        .ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(
        hand.cmd(b'W', 16, None)?.a as u64,
        64 * KIB,
        "64 KiB queued toward the server"
    );
    // and 32 KiB queued toward the client, which it never reads
    let chunk = [3u8; 4096];
    for _ in 0..8 {
        ok(
            sys::socket_write(end, chunk.as_ptr(), chunk.len()),
            "queue toward the client",
        )?;
    }
    ensure!(
        ipc_used(j)? >= 96 * KIB,
        "the client's job holds both queues"
    );
    ok(sys::process_kill(hand.proc, 9), "kill the client")?;
    let r = sys::object_wait(end, sig::PEER_CLOSED, sys::clock_get() + 2000);
    ensure!(
        r.status == 0 && r.a1 as u32 & sig::PEER_CLOSED != 0,
        "the server's end saw PEER_CLOSED"
    );
    ensure!(
        ipc_used(j)? >= 64 * KIB,
        "what the client queued toward the server is still held for it"
    );
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    loop {
        let r = sys::socket_read(end, buf.as_mut_ptr(), buf.len());
        if r.status != 0 {
            ensure_eq!(
                r.status(),
                Some(S::PeerClosed),
                "after the data: end of stream"
            );
            break;
        }
        total += r.a1;
    }
    ensure_eq!(
        total as u64,
        64 * KIB,
        "what the client queued before it died is delivered"
    );
    // the dead client's own inbound bytes went with it; once the server lets go too, nothing is left
    sys::handle_close(end);
    sys::handle_close(hand.proc);
    sys::handle_close(hand.ch);
    job_use_settles(j, 5, 0, 2000)?;
    job_use_settles(j, 4, 0, 2000)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// T233: a client writing to a socket is killed at a random moment while the server reads it in big chunks (the copy-out runs after the object lock is let go).
/// Every read ends Ok (with at most what was written), ShouldWait or PeerClosed: never INVALID_ARGS (a copy-out that was refused for a buffer that is mapped), and the
/// kernel does not panic. Rounds differ in how many reads happen before the kill and how many bytes the client had queued.
pub fn a_kill_storm_on_a_stream_loses_no_range() -> Outcome {
    let root = root_job()?;
    let j = ipc_job(root, 1024 * KIB)?;
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..120u32 {
        let hand = Hand::start(j)?;
        let end = hand
            .cmd(b'C', 0, None)?
            .handle
            .ok_or_else(|| msg_at(line!()))?;
        let units = 1 + (next() % 16) as u8;
        // the client writes `units` chunks of 4 KiB in the background while this thread reads
        hand.send(b'W', units, None)?;
        let kill_after = next() % 24;
        let mut reads = 0u64;
        let mut total = 0u64;
        let mut killed = false;
        let mut buf = [0u8; 4096];
        let mut spins = 0u32;
        loop {
            if !killed && reads >= kill_after {
                let _ = sys::process_kill(hand.proc, 9);
                killed = true;
            }
            let r = sys::socket_read(end, buf.as_mut_ptr(), buf.len());
            match r.status() {
                Some(S::Ok) => {
                    ensure!(r.a1 <= buf.len(), "a read returned more than the buffer");
                    total += r.a1 as u64;
                    reads += 1;
                }
                Some(S::ShouldWait) => {
                    reads += 1;
                    spins += 1;
                    if spins > 200_000 {
                        return Err(detail("the stream never ended"));
                    }
                    if killed {
                        let _ = sys::object_wait(
                            end,
                            sig::PEER_CLOSED | sig::READABLE,
                            sys::clock_get() + 50,
                        );
                    }
                }
                Some(S::PeerClosed) => break,
                other => {
                    let mut d = Detail::new();
                    let _ = core::fmt::Write::write_fmt(
                        &mut d,
                        format_args!(
                            "round {round}: a read of a stream whose client is killed returned {other:?} (status {}) after {reads} reads and {total} bytes",
                            r.status
                        ),
                    );
                    return Err(d);
                }
            }
            if !killed && total >= u64::from(units) * 4096 {
                // everything the client wrote has been read: let the client go on its own
                let _ = sys::process_kill(hand.proc, 9);
                killed = true;
            }
        }
        ensure!(
            total <= u64::from(units) * 4096,
            "a read returned more bytes than the client wrote"
        );
        sys::handle_close(end);
        sys::handle_close(hand.proc);
        sys::handle_close(hand.ch);
    }
    job_use_settles(j, 5, 0, 2000)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// What the stack must check about a socket a client passes in (S9-SPEC 8.3). Prototype of the check, written here so it can be
/// tested against the real kernel before it is written into `netd`.
#[derive(Debug, PartialEq, Eq)]
enum Refuse {
    NotASocket,
    NoReadOrWrite,
    NotBindable,
    BothEndsOfOnePair,
    AlreadyInUse,
}

struct Held(Vec<(u64, u64)>);

impl Held {
    fn admit(&mut self, h: u64) -> Result<(), Refuse> {
        // a zero-length read and write must both succeed: it is a socket and it has READ and WRITE
        let mut b = [0u8; 1];
        let (r, w) = (
            sys::socket_read(h, b.as_mut_ptr(), 0),
            sys::socket_write(h, b.as_ptr(), 0),
        );
        for x in [r, w] {
            match x.status() {
                Some(S::Ok) | Some(S::ShouldWait) | Some(S::PeerClosed) => {}
                Some(S::WrongType) | Some(S::BadHandle) => return Err(Refuse::NotASocket),
                _ => return Err(Refuse::NoReadOrWrite),
            }
        }
        // it can be bound (WAIT): probe with a throwaway port
        let port = sys::port_create();
        if port.status != 0 {
            return Err(Refuse::NotBindable);
        }
        let p = port.a1 as u64;
        let bound = sys::port_bind_flags(p, h, sig::READABLE, 0, abi_oneshot());
        sys::handle_close(p);
        if bound.status != 0 {
            return Err(Refuse::NotBindable);
        }
        let info = sys::object_info(h);
        let (koid, peer) = (info.a1 as u64, info.a2 as u64);
        if self
            .0
            .iter()
            .any(|&(k, p2)| k == koid || p2 == koid || k == peer)
        {
            return Err(if self.0.iter().any(|&(k, _)| k == koid) {
                Refuse::AlreadyInUse
            } else {
                Refuse::BothEndsOfOnePair
            });
        }
        self.0.push((koid, peer));
        Ok(())
    }
}

/// P6: a passed handle that is not a usable socket is refused and harms nothing.
pub fn a_bad_stream_handle_is_refused() -> Outcome {
    let mut held = Held(Vec::new());
    let sock = |what: &str| -> Result<(u64, u64), Detail> {
        let r = ok(sys::socket_create(), what)?;
        Ok((r.a1 as u64, r.a2 as u64))
    };
    let (a, b) = sock("a pair")?;
    ensure_eq!(held.admit(a), Ok(()), "a good socket is admitted");
    ensure_eq!(
        held.admit(a),
        Err(Refuse::AlreadyInUse),
        "the same socket twice"
    );
    ensure_eq!(
        held.admit(b),
        Err(Refuse::BothEndsOfOnePair),
        "both ends of one pair"
    );
    // a channel instead of a socket
    let ch = ok(sys::channel_create(), "a channel")?;
    ensure_eq!(
        held.admit(ch.a1 as u64),
        Err(Refuse::NotASocket),
        "a channel"
    );
    // a socket whose rights lack READ, or WRITE, or WAIT
    for (what, rights, want) in [
        (
            "without READ",
            Rights(Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
            Refuse::NoReadOrWrite,
        ),
        (
            "without WRITE",
            Rights(Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
            Refuse::NoReadOrWrite,
        ),
        (
            "without WAIT",
            Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0),
            Refuse::NotBindable,
        ),
    ] {
        let (x, y) = sock(what)?;
        let weak = ok(sys::handle_replace(x, rights.0), what)?.a1 as u64;
        ensure_eq!(held.admit(weak), Err(want), "{what}");
        sys::handle_close(weak);
        sys::handle_close(y);
    }
    // nothing above harmed the good one, and a fresh good socket is still admitted
    let (c, d) = sock("another pair")?;
    ensure_eq!(held.admit(c), Ok(()), "a fresh socket after the refusals");
    for h in [a, b, c, d, ch.a1 as u64, ch.a2 as u64] {
        sys::handle_close(h);
    }
    Ok(())
}

/// P7: the budgets. How many sockets a program with the launcher's defaults can hold (objects 256, IPC 192 KiB), and what
/// stalled streams cost against it. Numbers for S9-SPEC 6.4; the system-wide 2 MiB budget is only computed, not exhausted
/// (the suite's other servers share it).
pub fn stream_budgets_with_the_launcher_defaults() -> Outcome {
    let root = root_job()?;
    // the launcher's defaults: 256 objects, 192 KiB of IPC bytes
    let j = Ok::<u64, Detail>(
        ok(
            sys::job_create(root, &[MAX, MAX, MAX, MAX, 256, 192 * KIB]),
            "a job with the launcher's defaults",
        )?
        .a1 as u64,
    )?;
    let hand = Hand::start(j)?;
    // empty pairs: how many before the object quota ends it (each pair is two objects; the process and its channel count too)
    let mut pairs = 0u32;
    let mut ends = Vec::new();
    loop {
        let r = hand.cmd(b'C', 0, None)?;
        if r.status != 0 {
            break;
        }
        ends.extend(r.handle);
        pairs += 1;
        if pairs > 300 {
            break;
        }
    }
    println!("##PROTO p7 empty_pairs_before_the_object_quota={pairs}");
    ensure!(
        (64..=200).contains(&pairs),
        "{pairs} empty pairs in 256 objects"
    );
    for h in ends.drain(..) {
        sys::handle_close(h);
    }
    hand.finish();
    // stalled streams, 64 KiB queued each: how many fit in 192 KiB of IPC
    let hand = Hand::start(j)?;
    let mut stalled = 0u32;
    let mut held = Vec::new();
    loop {
        let r = hand.cmd(b'C', 0, None)?;
        if r.status != 0 {
            break;
        }
        held.extend(r.handle);
        let w = hand.cmd(b'W', 16, None)?;
        if w.status != 0 || w.a < 64 * 1024 {
            break;
        }
        stalled += 1;
        if stalled > 10 {
            break;
        }
    }
    ensure!(
        (1..=3).contains(&stalled),
        "{stalled} one-direction-full streams in 192 KiB"
    );
    println!(
        "##PROTO p7 full_64k_streams_in_192k={stalled} system_budget_streams={}",
        rt::abi::limits::MAX_IPC_BYTES / (64 * 1024)
    );
    for h in held.drain(..) {
        sys::handle_close(h);
    }
    hand.finish();
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// P8: the bystander cost (T60). A thread sleeps in `port_wait` on a port with many one-shot bindings while two unrelated
/// threads ping-pong over a channel; the ping-pong's time with 0, 64, 256 and 768 bindings is the cost, printed for the
/// owner. The pass condition is only that the sleeper is not woken for nothing (spurious wakes stay near the quiet run's).
pub fn bystander_cost_of_sleeping_bindings() -> Outcome {
    let quiet = ping_pong()?;
    println!(
        "##PROTO p8 bindings=0 ticks={} spurious={}",
        quiet.0, quiet.2
    );
    for want in [64usize, 256, 768] {
        let port = port()?;
        let mut socks = Vec::new();
        let mut bound = 0usize;
        // two bindings per socket end (READABLE and WRITABLE-after-full is not needed: two one-shot READABLE-class masks
        // with different keys), so 768 bindings need 384 sockets
        'outer: while bound < want {
            let r = sys::socket_create();
            if r.status != 0 {
                break;
            }
            let (a, b) = (r.a1 as u64, r.a2 as u64);
            socks.push((a, b));
            for (mask, key) in [(sig::READABLE, 1u64), (sig::PEER_CLOSED, 2)] {
                let k = ((socks.len() as u64) << 8) | key;
                if sys::port_bind_flags(port, a, mask, k, abi_oneshot()).status != 0 {
                    break 'outer;
                }
                bound += 1;
            }
        }
        new_thread(port_bystander, port as usize)?;
        nap_ticks(5);
        let busy = ping_pong()?;
        println!(
            "##PROTO p8 bindings={bound} ticks={} spurious={} blocks={}",
            busy.0, busy.2, busy.1
        );
        ensure!(
            busy.2 <= quiet.2 + 30,
            "{bound} sleeping bindings caused {} spurious wakes against {}",
            busy.2,
            quiet.2
        );
        sys::handle_close(port);
        nap_ticks(3);
        for (a, b) in socks {
            sys::handle_close(a);
            sys::handle_close(b);
        }
    }
    Ok(())
}
