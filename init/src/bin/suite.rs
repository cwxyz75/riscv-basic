//! The first user program: the boot-time user-level test suite (S3b). It exercises every
//! syscall and every error in SYSCALLS.md through the real ABI, in user mode, and reports
//! each test as a `##TEST` line. Expected values are worked out from SYSCALLS.md, not
//! read back from the kernel.
#![no_std]
#![no_main]
// Test failures carry a fixed 200-byte message buffer by design (there is no allocator).
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use rt::abi::limits::{MAX_MSG_BYTES, MAX_MSG_HANDLES};
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::abi::status::Status as S;
use rt::{ensure, ensure_eq, println, report, sys, Detail, Ret};

type Outcome = Result<(), Detail>;
type Test = (&'static str, fn() -> Outcome);

const BAD: u32 = 0x7777_0001; // a handle value nobody was ever given
const KERNEL_ADDR: usize = 0xffff_ffff_8020_0000;

fn st(r: Ret) -> Option<S> {
    r.status()
}

fn ok(r: Ret, what: &str) -> Result<Ret, Detail> {
    if r.status == 0 {
        Ok(r)
    } else {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("{what}: status {}", r.status));
        Err(d)
    }
}

fn channel() -> Result<(u32, u32), Detail> {
    let r = ok(sys::channel_create(), "channel_create")?;
    Ok((r.a1 as u32, r.a2 as u32))
}

fn event() -> Result<u32, Detail> {
    Ok(ok(sys::event_create(), "event_create")?.a1 as u32)
}

fn user_mode_runs_and_prints() -> Outcome {
    // Reaching this line means the kernel loaded the ELF, entered user mode, and the
    // debug_write syscall works.
    ensure_eq!(
        st(sys::debug_write(b"")),
        Some(S::Ok),
        "an empty debug_write"
    );
    Ok(())
}

fn clock_advances_in_user_mode() -> Outcome {
    let t0 = sys::clock_get();
    let mut spins = 0u64;
    while sys::clock_get() < t0 + 3 {
        spins += 1;
        ensure!(
            spins < 50_000_000,
            "the clock did not advance: user-mode code was never interrupted by the timer"
        );
    }
    Ok(())
}

fn unknown_syscalls_are_refused() -> Outcome {
    ensure_eq!(
        st(rt::syscall(999, [0; 6])),
        Some(S::BadSyscall),
        "syscall 999"
    );
    ensure_eq!(
        st(rt::syscall(4, [0; 6])),
        Some(S::BadSyscall),
        "unassigned number 4"
    );
    ensure_eq!(
        st(rt::syscall(usize::MAX, [0; 6])),
        Some(S::BadSyscall),
        "number usize::MAX"
    );
    Ok(())
}

fn debug_write_pointer_rules() -> Outcome {
    let big = [b'x'; 4097];
    ensure_eq!(st(sys::debug_write(&big)), Some(S::TooBig), "4097 bytes");
    let bad = |ptr: usize, len: usize| st(rt::syscall(abi_debug(), [ptr, len, 0, 0, 0, 0]));
    ensure_eq!(bad(0, 1), Some(S::InvalidArgs), "null pointer");
    ensure_eq!(
        bad(0, 0),
        Some(S::Ok),
        "null pointer with length zero never touches memory"
    );
    ensure_eq!(
        bad(KERNEL_ADDR, 8),
        Some(S::InvalidArgs),
        "a kernel address"
    );
    ensure_eq!(
        bad(usize::MAX - 2, 8),
        Some(S::InvalidArgs),
        "a range that wraps"
    );
    ensure_eq!(
        bad(0x10_0000_0000, 1),
        Some(S::InvalidArgs),
        "inside the user half but mapped by nothing"
    );
    // a range that starts in my stack and runs off its top (the page above is unmapped)
    let local = 0u8;
    let top = 0x20_0000_0000usize;
    ensure!(
        (&local as *const u8 as usize) < top,
        "stack is where SYSCALLS.md says"
    );
    ensure_eq!(
        bad(top - 4, 8),
        Some(S::InvalidArgs),
        "starts on the stack, runs past its top"
    );
    ensure_eq!(
        bad(top - 4, 4),
        Some(S::Ok),
        "the last four bytes of the stack are fine"
    );
    Ok(())
}

fn abi_debug() -> usize {
    rt::abi::syscall::DEBUG_WRITE
}

fn handle_syscalls_reject_bad_values() -> Outcome {
    for h in [0u32, 0x0002_0001, BAD, u32::MAX, 0x0001_0000] {
        ensure_eq!(st(sys::handle_close(h)), Some(S::BadHandle), "close {h:#x}");
        ensure_eq!(
            st(sys::handle_duplicate(h, Rights::SAME.0)),
            Some(S::BadHandle),
            "duplicate {h:#x}"
        );
        ensure_eq!(
            st(sys::handle_replace(h, Rights::SAME.0)),
            Some(S::BadHandle),
            "replace {h:#x}"
        );
        ensure_eq!(
            st(sys::object_signals(h)),
            Some(S::BadHandle),
            "object_signals {h:#x}"
        );
    }
    let e = event()?;
    ensure_eq!(st(sys::handle_close(e)), Some(S::Ok), "close a live handle");
    ensure_eq!(
        st(sys::handle_close(e)),
        Some(S::BadHandle),
        "close it again"
    );
    Ok(())
}

fn event_syscalls() -> Outcome {
    let e = event()?;
    ensure_eq!(
        sys::object_signals(e).a1 as u32,
        0,
        "a new event has no signals"
    );
    ensure_eq!(
        st(sys::event_signal(e, 0, 1 << 24 | 1 << 31)),
        Some(S::Ok),
        "set two user signals"
    );
    ensure_eq!(
        sys::object_signals(e).a1 as u32,
        1 << 24 | 1 << 31,
        "signals after set"
    );
    ensure_eq!(
        st(sys::event_signal(e, 1 << 24, 1 << 25)),
        Some(S::Ok),
        "clear one, set one"
    );
    ensure_eq!(
        sys::object_signals(e).a1 as u32,
        1 << 25 | 1 << 31,
        "clear is applied before set"
    );
    ensure_eq!(
        st(sys::event_signal(e, 0, 1)),
        Some(S::InvalidArgs),
        "kernel signal bit"
    );
    ensure_eq!(
        st(sys::event_signal(e, 4, 0)),
        Some(S::InvalidArgs),
        "kernel signal bit in clear"
    );
    let (a, _b) = channel()?;
    ensure_eq!(
        st(sys::event_signal(a, 0, 1 << 24)),
        Some(S::WrongType),
        "a channel is not an event"
    );
    let weak = ok(sys::handle_replace(e, Rights::WAIT.0), "replace")?.a1 as u32;
    ensure_eq!(
        st(sys::event_signal(weak, 0, 1 << 24)),
        Some(S::AccessDenied),
        "no SIGNAL right"
    );
    ensure_eq!(
        st(sys::event_signal(BAD, 0, 0)),
        Some(S::BadHandle),
        "bad handle"
    );
    Ok(())
}

fn channel_round_trip() -> Outcome {
    let (a, b) = channel()?;
    ensure!(
        a != 0 && b != 0 && a != b,
        "two distinct nonzero handles: {a:#x} {b:#x}"
    );
    let mut buf = [0u8; 64];
    ensure_eq!(
        st(sys::channel_read(
            b,
            buf.as_mut_ptr(),
            64,
            core::ptr::null_mut(),
            0
        )),
        Some(S::ShouldWait),
        "empty channel"
    );
    ensure_eq!(
        sys::object_signals(a).a1 as u32,
        signals::WRITABLE,
        "fresh: writable only"
    );
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"one".as_ptr(),
            3,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write one"
    );
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"two!".as_ptr(),
            4,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write two"
    );
    ensure_eq!(
        sys::object_signals(b).a1 as u32,
        signals::WRITABLE | signals::READABLE,
        "peer sees a message"
    );
    let r = sys::channel_read(b, buf.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, 3, 0),
        "first read (status, nbytes, nhandles)"
    );
    ensure_eq!(&buf[..3], b"one", "first message");
    let r = sys::channel_read(b, buf.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
    ensure_eq!((r.status, r.a1), (0, 4), "second read");
    ensure_eq!(&buf[..4], b"two!", "second message, in order");
    ensure_eq!(
        st(sys::channel_read(
            a,
            buf.as_mut_ptr(),
            64,
            core::ptr::null_mut(),
            0
        )),
        Some(S::ShouldWait),
        "nothing for the writer"
    );
    Ok(())
}

fn channel_sends_handles() -> Outcome {
    let (a, b) = channel()?;
    let e = event()?;
    let handles = [e];
    ensure_eq!(
        st(sys::channel_write(a, b"h".as_ptr(), 1, handles.as_ptr(), 1)),
        Some(S::Ok),
        "write with a handle"
    );
    ensure_eq!(
        st(sys::handle_close(e)),
        Some(S::BadHandle),
        "the sent handle left this table"
    );
    let mut buf = [0u8; 8];
    let mut got = [0u32; 4];
    let r = sys::channel_read(b, buf.as_mut_ptr(), 8, got.as_mut_ptr(), 4);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, 1, 1),
        "read (status, nbytes, nhandles)"
    );
    let e2 = got[0];
    ensure_eq!(
        st(sys::event_signal(e2, 0, 1 << 26)),
        Some(S::Ok),
        "the received handle works, with the same rights"
    );
    ensure_eq!(
        sys::object_signals(e2).a1 as u32,
        1 << 26,
        "and it is the same event"
    );
    Ok(())
}

fn channel_syscall_errors() -> Outcome {
    let (a, b) = channel()?;
    let e = event()?;
    let e_no_transfer = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u32;
    let none8 = core::ptr::null::<u8>();
    let none32 = core::ptr::null::<u32>();
    // order: TOO_BIG first (before any pointer or handle is looked at)
    ensure_eq!(
        st(sys::channel_write(
            a,
            0x10 as *const u8,
            MAX_MSG_BYTES + 1,
            none32,
            0
        )),
        Some(S::TooBig),
        "too many bytes, bad pointer"
    );
    ensure_eq!(
        st(sys::channel_write(
            BAD,
            none8,
            0,
            0x10 as *const u32,
            MAX_MSG_HANDLES + 1
        )),
        Some(S::TooBig),
        "too many handles, bad everything"
    );
    // BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED
    ensure_eq!(
        st(sys::channel_write(BAD, none8, 0, none32, 0)),
        Some(S::BadHandle),
        "bad channel"
    );
    let forged = [BAD];
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, forged.as_ptr(), 1)),
        Some(S::BadHandle),
        "bad listed handle"
    );
    ensure_eq!(
        st(sys::channel_write(e, none8, 0, none32, 0)),
        Some(S::WrongType),
        "an event is not a channel"
    );
    let read_only = ok(
        sys::handle_replace(
            ok(sys::handle_duplicate(a, Rights::SAME.0), "dup")?.a1 as u32,
            Rights::READ.0,
        ),
        "replace",
    )?
    .a1 as u32;
    ensure_eq!(
        st(sys::channel_write(read_only, none8, 0, none32, 0)),
        Some(S::AccessDenied),
        "no WRITE"
    );
    ensure_eq!(
        st(sys::channel_write(read_only, none8, 0, forged.as_ptr(), 1)),
        Some(S::BadHandle),
        "a bad listed handle beats the missing right"
    );
    let list = [e_no_transfer];
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, list.as_ptr(), 1)),
        Some(S::AccessDenied),
        "a handle without TRANSFER"
    );
    ensure_eq!(
        st(sys::handle_close(e_no_transfer)),
        Some(S::Ok),
        "and it is still in the table"
    );
    // INVALID_ARGS: twice in the list; NOT_SUPPORTED: either end of this channel
    let twice = [e, e];
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, twice.as_ptr(), 2)),
        Some(S::InvalidArgs),
        "a handle listed twice"
    );
    let me = [a];
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, me.as_ptr(), 1)),
        Some(S::NotSupported),
        "an endpoint over itself"
    );
    let peer = [b];
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, peer.as_ptr(), 1)),
        Some(S::NotSupported),
        "the peer endpoint over this channel"
    );
    // bad user pointers
    ensure_eq!(
        st(sys::channel_write(a, 0x10 as *const u8, 4, none32, 0)),
        Some(S::InvalidArgs),
        "bad data pointer"
    );
    ensure_eq!(
        st(sys::channel_write(
            a,
            KERNEL_ADDR as *const u8,
            4,
            none32,
            0
        )),
        Some(S::InvalidArgs),
        "kernel data pointer"
    );
    ensure_eq!(
        st(sys::channel_write(a, none8, 0, 0x10 as *const u32, 1)),
        Some(S::InvalidArgs),
        "bad handles pointer"
    );
    // reads
    let mut buf = [0u8; 8];
    ensure_eq!(
        st(sys::channel_read(
            BAD,
            buf.as_mut_ptr(),
            8,
            core::ptr::null_mut(),
            0
        )),
        Some(S::BadHandle),
        "read: bad handle"
    );
    ensure_eq!(
        st(sys::channel_read(
            e,
            buf.as_mut_ptr(),
            8,
            core::ptr::null_mut(),
            0
        )),
        Some(S::WrongType),
        "read: an event"
    );
    let write_only = ok(
        sys::handle_replace(
            ok(sys::handle_duplicate(b, Rights::SAME.0), "dup")?.a1 as u32,
            Rights::WRITE.0,
        ),
        "replace",
    )?
    .a1 as u32;
    ensure_eq!(
        st(sys::channel_read(
            write_only,
            buf.as_mut_ptr(),
            8,
            core::ptr::null_mut(),
            0
        )),
        Some(S::AccessDenied),
        "read: no READ"
    );
    Ok(())
}

fn channel_buffer_too_small_keeps_message() -> Outcome {
    let (a, b) = channel()?;
    let e = event()?;
    let list = [e];
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"hello".as_ptr(),
            5,
            list.as_ptr(),
            1
        )),
        Some(S::Ok),
        "write"
    );
    let mut buf = [0u8; 8];
    let mut hs = [0u32; 2];
    let r = sys::channel_read(b, buf.as_mut_ptr(), 4, hs.as_mut_ptr(), 2);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (S::BufferTooSmall.code(), 5, 1),
        "bytes buffer too small: sizes come back"
    );
    let r = sys::channel_read(b, buf.as_mut_ptr(), 8, hs.as_mut_ptr(), 0);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (S::BufferTooSmall.code(), 5, 1),
        "handles buffer too small"
    );
    let r = sys::channel_read(b, buf.as_mut_ptr(), 5, hs.as_mut_ptr(), 1);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, 5, 1),
        "still queued after two refusals"
    );
    ensure_eq!(&buf[..5], b"hello", "intact");
    Ok(())
}

fn buffers_are_validated_before_a_read() -> Outcome {
    let (a, b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"keep".as_ptr(),
            4,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write"
    );
    ensure_eq!(
        st(sys::channel_read(
            b,
            0x10 as *mut u8,
            4,
            core::ptr::null_mut(),
            0
        )),
        Some(S::InvalidArgs),
        "unmapped destination"
    );
    ensure_eq!(
        st(sys::channel_read(
            b,
            KERNEL_ADDR as *mut u8,
            4,
            core::ptr::null_mut(),
            0
        )),
        Some(S::InvalidArgs),
        "kernel destination"
    );
    // the read-only data segment is mapped but not writable by me
    static CONST_DATA: [u8; 8] = *b"readonly";
    ensure_eq!(
        st(sys::channel_read(
            b,
            CONST_DATA.as_ptr() as *mut u8,
            4,
            core::ptr::null_mut(),
            0
        )),
        Some(S::InvalidArgs),
        "a read-only destination"
    );
    let mut buf = [0u8; 4];
    let r = sys::channel_read(b, buf.as_mut_ptr(), 4, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1),
        (0, 4),
        "the message was not lost by the refused reads"
    );
    ensure_eq!(&buf, b"keep", "message intact");
    Ok(())
}

fn channel_txid_read() -> Outcome {
    let (a, b) = channel()?;
    let m1 = [7u8, 0, 0, 0, b'f'];
    let m2 = [9u8, 0, 0, 0, b'r'];
    sys::channel_write(a, m1.as_ptr(), 5, core::ptr::null(), 0);
    sys::channel_write(a, m2.as_ptr(), 5, core::ptr::null(), 0);
    let mut buf = [0u8; 8];
    let r = sys::channel_read_txid(b, 5, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(st(r), Some(S::ShouldWait), "no message with txid 5");
    let r = sys::channel_read_txid(b, 9, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1, buf[4]),
        (0, 5, b'r'),
        "txid 9 skips the older message"
    );
    let r = sys::channel_read(b, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, buf[4]),
        (0, b'f'),
        "the skipped message is still first in line"
    );
    Ok(())
}

fn channel_peer_closed() -> Outcome {
    let (a, b) = channel()?;
    sys::channel_write(a, b"bye".as_ptr(), 3, core::ptr::null(), 0);
    ensure_eq!(st(sys::handle_close(a)), Some(S::Ok), "close one end");
    ensure_eq!(
        sys::object_signals(b).a1 as u32,
        signals::READABLE | signals::PEER_CLOSED,
        "peer closed, message waiting"
    );
    let mut buf = [0u8; 8];
    ensure_eq!(
        sys::channel_read(b, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0).status,
        0,
        "queued messages are still delivered"
    );
    ensure_eq!(
        st(sys::channel_read(
            b,
            buf.as_mut_ptr(),
            8,
            core::ptr::null_mut(),
            0
        )),
        Some(S::PeerClosed),
        "then PEER_CLOSED"
    );
    ensure_eq!(
        st(sys::channel_write(
            b,
            b"x".as_ptr(),
            1,
            core::ptr::null(),
            0
        )),
        Some(S::PeerClosed),
        "writing to a closed peer"
    );
    let r = sys::channel_read_txid(b, 1, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        st(r),
        Some(S::PeerClosed),
        "a txid read does not wait forever on a closed channel"
    );
    Ok(())
}

// --- Waiting and calling, against the kernel-side helper on the bootstrap channel. The
// helper reads each request (first 4 bytes: a transaction id; 5th byte: the operation) and
// answers: 'E' echo, 'D' echo after 3 ticks, 'S' stay silent, 'N' send an unrelated message
// first and then echo, 'C' close its end.

use rt::abi::signals as sig;
use rt::{CallArgs, BOOTSTRAP};

/// The kernel passes start argument 1 when it runs a scheduling class that ignores
/// priorities (plain round robin): a woken thread then waits for a time slice, so upper
/// bounds on how late something may be are not checked, only that it was not early.
fn loose() -> bool {
    rt::start_arg() == 1
}

/// `lo <= elapsed <= hi`, with no upper bound when the scheduler is the loose one.
fn within(elapsed: u64, lo: u64, hi: u64) -> bool {
    // The ticks follow the host's clock, so even the strict windows allow some slack.
    elapsed >= lo && (loose() || elapsed <= hi + 20)
}

fn request(txid: u32, op: u8, tail: &[u8]) -> ([u8; 64], usize) {
    let mut m = [0u8; 64];
    m[..4].copy_from_slice(&txid.to_le_bytes());
    m[4] = op;
    m[5..5 + tail.len()].copy_from_slice(tail);
    (m, 5 + tail.len())
}

fn call(req: &[u8], reply: &mut [u8], deadline: u64) -> Ret {
    let args = CallArgs {
        req_bytes: req.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: req.len() as u32,
        req_nhandles: 0,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: 0,
        rep_max_bytes: reply.len() as u32,
        rep_max_handles: 0,
        deadline,
    };
    sys::channel_call(BOOTSTRAP, &args)
}

fn wait_arguments_are_checked() -> Outcome {
    let e = event()?;
    let weak = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u32;
    ensure_eq!(
        st(sys::object_wait(BAD, 1, 0)),
        Some(S::BadHandle),
        "bad handle"
    );
    ensure_eq!(
        st(sys::object_wait(BAD, 0, 0)),
        Some(S::BadHandle),
        "a bad handle beats a bad mask"
    );
    ensure_eq!(
        st(sys::object_wait(weak, 1 << 24, 0)),
        Some(S::AccessDenied),
        "no WAIT right"
    );
    ensure_eq!(
        st(sys::object_wait(e, 0, 0)),
        Some(S::InvalidArgs),
        "an empty mask"
    );
    ensure_eq!(
        st(sys::object_wait(e, 1 << 5, 0)),
        Some(S::InvalidArgs),
        "a bit that is no signal"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::OBJECT_WAIT,
            [e as usize, 1 << 32, 0, 0, 0, 0]
        )),
        Some(S::InvalidArgs),
        "a mask wider than 32 bits"
    );
    Ok(())
}

fn wait_returns_at_once_when_the_condition_holds() -> Outcome {
    let r = sys::object_wait(BOOTSTRAP, sig::WRITABLE, 0);
    ensure_eq!(
        (r.status, r.a1 as u32 & sig::WRITABLE),
        (0, sig::WRITABLE),
        "a writable channel, deadline 0"
    );
    let e = event()?;
    ensure_eq!(st(sys::event_signal(e, 0, 1 << 25)), Some(S::Ok), "signal");
    let t0 = sys::clock_get();
    let r = sys::object_wait(e, 1 << 25, u64::MAX);
    ensure_eq!(r.status, 0, "an already set signal, waiting forever");
    ensure!(sys::clock_get() - t0 <= 40, "it did not wait"); // loose: a loaded host can preempt between the clock reads (a real wait would be unbounded)
    Ok(())
}

fn wait_with_deadline_zero_polls() -> Outcome {
    let e = event()?;
    let t0 = sys::clock_get();
    let r = sys::object_wait(e, 1 << 24, 0);
    ensure_eq!(
        (r.status, r.a1),
        (S::TimedOut.code(), 0),
        "nothing set: TIMED_OUT, observed 0"
    );
    // A poll does not block; it may still be preempted between the two clock reads on a
    // loaded host, so the bound is loose (a real wait would be unbounded).
    ensure!(sys::clock_get() - t0 <= 25, "it did not wait");
    Ok(())
}

fn wait_times_out_at_the_deadline() -> Outcome {
    let t0 = sys::clock_get();
    let r = sys::object_wait(BOOTSTRAP, sig::READABLE, t0 + 5);
    let elapsed = sys::clock_get() - t0;
    ensure_eq!(r.status, S::TimedOut.code(), "nothing arrives");
    ensure_eq!(
        r.a1 as u32,
        sig::WRITABLE,
        "observed signals are reported on a timeout too"
    );
    ensure!(within(elapsed, 5, 7), "waited {elapsed} ticks, wanted 5");
    Ok(())
}

fn wait_is_woken_by_a_message() -> Outcome {
    let (m, n) = request(77, b'D', b"hi");
    ensure_eq!(
        st(sys::channel_write(
            BOOTSTRAP,
            m.as_ptr(),
            n,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send the request"
    );
    let t0 = sys::clock_get();
    let r = sys::object_wait(BOOTSTRAP, sig::READABLE, t0 + 100);
    let elapsed = sys::clock_get() - t0;
    ensure_eq!(r.status, 0, "woken by the reply");
    ensure!(r.a1 as u32 & sig::READABLE != 0, "readable");
    ensure!(
        within(elapsed, 2, 12),
        "the reply was due after 3 ticks, wait took {elapsed}"
    );
    let mut buf = [0u8; 64];
    let r = sys::channel_read_txid(
        BOOTSTRAP,
        77,
        buf.as_mut_ptr(),
        64,
        core::ptr::null_mut(),
        0,
    );
    ensure_eq!(
        (r.status, r.a1, &buf[..7]),
        (0, 7, &m[..7]),
        "the echoed message"
    );
    Ok(())
}

fn channel_call_round_trip() -> Outcome {
    let (m, n) = request(1, b'E', b"x");
    let mut reply = [0u8; 32];
    let r = call(&m[..n], &mut reply, u64::MAX);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, n, 0),
        "call (status, nbytes, nhandles)"
    );
    ensure_eq!(&reply[..n], &m[..n], "the reply is the echo");
    Ok(())
}

fn channel_call_skips_unrelated_messages() -> Outcome {
    let (m, n) = request(2, b'N', b"y");
    let mut reply = [0u8; 32];
    let r = call(&m[..n], &mut reply, u64::MAX);
    ensure_eq!(
        (r.status, r.a1),
        (0, n),
        "the call returns the message with its transaction id"
    );
    ensure_eq!(&reply[..4], &2u32.to_le_bytes(), "txid 2");
    // the noise (txid 0xdead) the helper sent first is still queued for an ordinary read
    let mut buf = [0u8; 32];
    let r = sys::channel_read(BOOTSTRAP, buf.as_mut_ptr(), 32, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1, buf[4]),
        (0, 5, b'n'),
        "the unrelated message was left queued"
    );
    Ok(())
}

fn channel_call_times_out_but_the_request_was_sent() -> Outcome {
    let (m, n) = request(3, b'D', b"late"); // answered after 3 ticks
    let mut reply = [0u8; 32];
    let t0 = sys::clock_get();
    let r = call(&m[..n], &mut reply, t0 + 1);
    ensure_eq!(st(r), Some(S::TimedOut), "the deadline came first");
    ensure!(
        within(sys::clock_get() - t0, 1, 3),
        "it waited about 1 tick"
    );
    // the request was sent, so the reply still arrives and can be read later
    ensure_eq!(
        sys::object_wait(BOOTSTRAP, sig::READABLE, t0 + 100).status,
        0,
        "the late reply arrives"
    );
    let r = sys::channel_read_txid(
        BOOTSTRAP,
        3,
        reply.as_mut_ptr(),
        32,
        core::ptr::null_mut(),
        0,
    );
    ensure_eq!((r.status, r.a1), (0, n), "and reads");
    let (s, ns) = request(4, b'S', b""); // never answered
    let t1 = sys::clock_get();
    let r = call(&s[..ns], &mut reply, t1 + 4);
    let waited = sys::clock_get() - t1;
    ensure_eq!(st(r), Some(S::TimedOut), "a silent server");
    ensure!(within(waited, 4, 6), "waited {waited} ticks, wanted 4");
    Ok(())
}

fn channel_call_buffer_too_small() -> Outcome {
    let (m, n) = request(5, b'E', b"0123456789");
    let mut small = [0u8; 4];
    let r = call(&m[..n], &mut small, u64::MAX);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (S::BufferTooSmall.code(), n, 0),
        "sizes come back"
    );
    let mut buf = [0u8; 32];
    let r = sys::channel_read_txid(BOOTSTRAP, 5, buf.as_mut_ptr(), 32, core::ptr::null_mut(), 0);
    ensure_eq!((r.status, r.a1), (0, n), "the reply stayed queued");
    Ok(())
}

fn channel_call_argument_errors() -> Outcome {
    let (m, n) = request(6, b'E', b"z");
    let mut reply = [0u8; 32];
    let args = |req: &[u8], rep: u64| CallArgs {
        req_bytes: req.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: req.len() as u32,
        req_nhandles: 0,
        rep_bytes: rep,
        rep_handles: 0,
        rep_max_bytes: 32,
        rep_max_handles: 0,
        deadline: u64::MAX,
    };
    ensure_eq!(
        st(sys::channel_call(
            BAD,
            &args(&m[..n], reply.as_mut_ptr() as u64)
        )),
        Some(S::BadHandle),
        "bad handle"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::channel_call(
            ev,
            &args(&m[..n], reply.as_mut_ptr() as u64)
        )),
        Some(S::WrongType),
        "an event"
    );
    let read_only = ok(
        sys::handle_replace(
            ok(sys::handle_duplicate(BOOTSTRAP, Rights::SAME.0), "dup")?.a1 as u32,
            Rights::READ.0,
        ),
        "replace",
    )?
    .a1 as u32;
    ensure_eq!(
        st(sys::channel_call(
            read_only,
            &args(&m[..n], reply.as_mut_ptr() as u64)
        )),
        Some(S::AccessDenied),
        "no WRITE"
    );
    // WRITE without READ: refused before anything is sent. (If the request had gone out, the
    // helper would answer and the channel would become readable.)
    let write_only = ok(
        sys::handle_replace(
            ok(sys::handle_duplicate(BOOTSTRAP, Rights::SAME.0), "dup")?.a1 as u32,
            Rights::WRITE.0,
        ),
        "replace",
    )?
    .a1 as u32;
    let (e, en) = request(8, b'E', b"w");
    let r = sys::channel_call(write_only, &args(&e[..en], reply.as_mut_ptr() as u64));
    ensure_eq!(st(r), Some(S::AccessDenied), "no READ");
    ensure_eq!(
        sys::object_wait(BOOTSTRAP, sig::READABLE, sys::clock_get() + 3).status,
        S::TimedOut.code(),
        "no request was sent: nothing came back"
    );
    ensure_eq!(
        st(sys::channel_call(
            BOOTSTRAP,
            &args(&m[..3], reply.as_mut_ptr() as u64)
        )),
        Some(S::InvalidArgs),
        "a request under 4 bytes"
    );
    let bad_record = rt::syscall(
        rt::abi::syscall::CHANNEL_CALL,
        [BOOTSTRAP as usize, 0x10, 0, 0, 0, 0],
    );
    ensure_eq!(
        st(bad_record),
        Some(S::InvalidArgs),
        "a bad pointer to the argument record"
    );
    let mut a = args(&m[..n], reply.as_mut_ptr() as u64);
    a.req_nbytes = 70_000;
    ensure_eq!(
        st(sys::channel_call(BOOTSTRAP, &a)),
        Some(S::TooBig),
        "an oversized request"
    );
    // a bad reply buffer: the request goes out, the reply comes back, and is not lost
    let r = sys::channel_call(BOOTSTRAP, &args(&m[..n], 0x10));
    ensure_eq!(st(r), Some(S::InvalidArgs), "unmapped reply buffer");
    let mut buf = [0u8; 32];
    let r = sys::channel_read_txid(BOOTSTRAP, 6, buf.as_mut_ptr(), 32, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1),
        (0, n),
        "the reply survived the refused call"
    );
    Ok(())
}

fn wait_and_call_argument_edges() -> Outcome {
    let e = event()?;
    let wide = 1usize << 32;
    use rt::abi::syscall as nr;
    ensure_eq!(
        st(rt::syscall(
            nr::OBJECT_WAIT,
            [wide | e as usize, 1 << 24, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "object_wait with a wide handle"
    );
    ensure_eq!(
        st(rt::syscall(
            nr::CHANNEL_CALL,
            [wide | BOOTSTRAP as usize, 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "channel_call with a wide handle"
    );
    // a deadline that has already passed (but is not 0): no waiting, TIMED_OUT
    let t0 = sys::clock_get();
    let r = sys::object_wait(e, 1 << 24, 1);
    ensure_eq!(st(r), Some(S::TimedOut), "a deadline in the past");
    ensure!(sys::clock_get() - t0 <= 40, "it did not wait"); // loose: a loaded host can preempt between the clock reads (a real wait would be unbounded)
                                                             // a set signal beats a deadline in the past
    ensure_eq!(st(sys::event_signal(e, 0, 1 << 24)), Some(S::Ok), "signal");
    ensure_eq!(
        sys::object_wait(e, 1 << 24, 1).status,
        0,
        "an already true condition returns OK even with a past deadline"
    );
    // a call whose deadline has passed still sends the request. With a priority scheduler the
    // server answers at once, so the call may even succeed (the condition holds on the first
    // check); otherwise it is TIMED_OUT and the reply is readable later.
    let (m, n) = request(11, b'E', b"p");
    let mut reply = [0u8; 32];
    match st(call(&m[..n], &mut reply, 1)) {
        Some(S::Ok) => {}
        Some(S::TimedOut) => {
            ensure_eq!(
                sys::object_wait(BOOTSTRAP, sig::READABLE, sys::clock_get() + 100).status,
                0,
                "the reply still arrives"
            );
            let r = sys::channel_read_txid(
                BOOTSTRAP,
                11,
                reply.as_mut_ptr(),
                32,
                core::ptr::null_mut(),
                0,
            );
            ensure_eq!((r.status, r.a1), (0, n), "and reads");
        }
        other => ensure!(false, "channel_call with a deadline in the past: {other:?}"),
    }
    Ok(())
}

fn channel_call_carries_handles() -> Outcome {
    let h = event()?;
    let (m, n) = request(12, b'E', b"h");
    let mut reply = [0u8; 32];
    let handles = [h];
    let args = CallArgs {
        req_bytes: m.as_ptr() as u64,
        req_handles: handles.as_ptr() as u64,
        req_nbytes: n as u32,
        req_nhandles: 1,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: 0,
        rep_max_bytes: 32,
        rep_max_handles: 0,
        deadline: u64::MAX,
    };
    let r = sys::channel_call(BOOTSTRAP, &args);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, n, 0),
        "the request carried a handle; the echo carries none"
    );
    ensure_eq!(
        st(sys::handle_close(h)),
        Some(S::BadHandle),
        "the handle left this table with the request"
    );
    // a request handle without TRANSFER is refused and stays put
    let weak = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u32;
    let handles = [weak];
    let args = CallArgs {
        req_handles: handles.as_ptr() as u64,
        ..args
    };
    ensure_eq!(
        st(sys::channel_call(BOOTSTRAP, &args)),
        Some(S::AccessDenied),
        "no TRANSFER right"
    );
    ensure_eq!(
        st(sys::event_signal(weak, 0, 1 << 24)),
        Some(S::Ok),
        "it is still in the table"
    );
    Ok(())
}

fn kernel_ends_survive_a_sweep() -> Outcome {
    // More than 256 live objects makes the kernel sweep garbage before it creates the next
    // one. The kernel's end of the bootstrap channel is reachable from no process handle:
    // it must not be swept with the garbage, or the channel would look closed.
    let mut events = [0u32; 300];
    for slot in events.iter_mut() {
        *slot = event()?;
    }
    let (m, n) = request(13, b'E', b"s");
    let mut reply = [0u8; 32];
    let r = call(&m[..n], &mut reply, sys::clock_get() + 200);
    ensure_eq!(
        (r.status, r.a1),
        (0, n),
        "the helper still answers after the sweeps"
    );
    for h in events {
        ok(sys::handle_close(h), "close")?;
    }
    Ok(())
}

fn wait_is_woken_by_the_peer_closing() -> Outcome {
    // Last of the helper tests: op 'T' makes the helper close its end after 3 ticks.
    let (m, n) = request(9, b'T', b"");
    ensure_eq!(
        st(sys::channel_write(
            BOOTSTRAP,
            m.as_ptr(),
            n,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send"
    );
    let t0 = sys::clock_get();
    let r = sys::object_wait(BOOTSTRAP, sig::PEER_CLOSED, t0 + 200);
    ensure_eq!(
        (r.status, r.a1 as u32 & sig::PEER_CLOSED),
        (0, sig::PEER_CLOSED),
        "a blocked wait is woken by PEER_CLOSED"
    );
    ensure!(
        within(sys::clock_get() - t0, 2, 12),
        "the close came after about 3 ticks"
    );
    let mut reply = [0u8; 32];
    let (m, n) = request(10, b'E', b"");
    ensure_eq!(
        st(call(&m[..n], &mut reply, u64::MAX)),
        Some(S::PeerClosed),
        "calling a dead peer"
    );
    // a wait for readable that nothing can ever satisfy still ends: only because PEER_CLOSED
    // must be in the mask, the caller asks for both
    let r = sys::object_wait(BOOTSTRAP, sig::READABLE | sig::PEER_CLOSED, u64::MAX);
    ensure_eq!(
        r.status,
        0,
        "READABLE | PEER_CLOSED returns at once on a closed channel"
    );
    Ok(())
}

// --- VMOs, processes and the user-level launcher (S3c-2a) ---------------------------------

use core::sync::atomic::{AtomicU32, Ordering};

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;
const TOP: u64 = 0x40_0000_0000;

/// The handle of the program image VMO the kernel-side helper sent us (0 until fetched).
static IMAGE: AtomicU32 = AtomicU32::new(0);
/// The `hello` program image (S4a runtime tests).
static HELLO_IMAGE: AtomicU32 = AtomicU32::new(0);

fn vmo(pages: u64) -> Result<u32, Detail> {
    Ok(ok(sys::vmo_create(pages), "vmo_create")?.a1 as u32)
}

fn proc_create() -> Result<u32, Detail> {
    Ok(ok(sys::process_create(), "process_create")?.a1 as u32)
}

fn weaker(h: u32, rights: u32) -> Result<u32, Detail> {
    let d = ok(sys::handle_duplicate(h, Rights::SAME.0), "duplicate")?.a1 as u32;
    Ok(ok(sys::handle_replace(d, rights), "replace")?.a1 as u32)
}

fn vmo_basics() -> Outcome {
    let v = vmo(3)?;
    ensure_eq!(sys::vmo_get_size(v).a1, 3 * 4096, "size of a 3 page VMO");
    let data: [u8; 20] = core::array::from_fn(|i| i as u8 + 1);
    ensure_eq!(
        st(sys::vmo_write(v, 4090, data.as_ptr(), 20)),
        Some(S::Ok),
        "a write across a page boundary"
    );
    let mut back = [0u8; 20];
    ensure_eq!(
        st(sys::vmo_read(v, 4090, back.as_mut_ptr(), 20)),
        Some(S::Ok),
        "read it back"
    );
    ensure_eq!(back, data, "the same bytes");
    let mut zero = [9u8; 16];
    ensure_eq!(
        st(sys::vmo_read(v, 0, zero.as_mut_ptr(), 16)),
        Some(S::Ok),
        "an untouched area"
    );
    ensure_eq!(zero, [0u8; 16], "reads as zeros");
    ensure_eq!(
        st(sys::vmo_write(v, 3 * 4096 - 4, data.as_ptr(), 8)),
        Some(S::InvalidArgs),
        "past the end"
    );
    ensure_eq!(
        st(sys::vmo_write(v, u64::MAX, data.as_ptr(), 1)),
        Some(S::InvalidArgs),
        "offset overflow"
    );
    ensure_eq!(
        st(sys::vmo_read(v, 3 * 4096, zero.as_mut_ptr(), 0)),
        Some(S::Ok),
        "an empty read at the end"
    );
    ensure_eq!(
        st(sys::vmo_read(v, 3 * 4096 + 1, zero.as_mut_ptr(), 0)),
        Some(S::InvalidArgs),
        "an empty read past the end"
    );
    ensure_eq!(
        st(sys::vmo_write(v, 0, data.as_ptr(), 65537)),
        Some(S::TooBig),
        "over the per-call limit"
    );
    ensure_eq!(
        st(sys::vmo_create(0)),
        Some(S::InvalidArgs),
        "a VMO of no pages"
    );
    ensure_eq!(
        st(sys::vmo_create(16385)),
        Some(S::InvalidArgs),
        "over 64 MiB"
    );
    ensure_eq!(
        st(sys::vmo_write(v, 0, 0x10 as *const u8, 4)),
        Some(S::InvalidArgs),
        "a bad source pointer"
    );
    ensure_eq!(
        st(sys::vmo_read(v, 0, KERNEL_ADDR as *mut u8, 4)),
        Some(S::InvalidArgs),
        "a kernel destination"
    );
    let ro = weaker(v, Rights::READ.0)?;
    let wo = weaker(v, Rights::WRITE.0)?;
    ensure_eq!(
        st(sys::vmo_write(ro, 0, data.as_ptr(), 4)),
        Some(S::AccessDenied),
        "write with a read-only handle"
    );
    ensure_eq!(
        st(sys::vmo_read(wo, 0, zero.as_mut_ptr(), 4)),
        Some(S::AccessDenied),
        "read with a write-only handle"
    );
    let e = event()?;
    ensure_eq!(
        st(sys::vmo_read(e, 0, zero.as_mut_ptr(), 4)),
        Some(S::WrongType),
        "an event is no VMO"
    );
    ensure_eq!(st(sys::vmo_get_size(BAD)), Some(S::BadHandle), "bad handle");
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::VMO_GET_SIZE,
            [(1usize << 32) | v as usize, 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // two handles, one VMO: a write through one shows through the other
    let dup = ok(sys::handle_duplicate(v, Rights::SAME.0), "dup")?.a1 as u32;
    sys::vmo_write(dup, 100, b"shared".as_ptr(), 6);
    let mut s = [0u8; 6];
    sys::vmo_read(v, 100, s.as_mut_ptr(), 6);
    ensure_eq!(&s, b"shared", "one memory object");
    Ok(())
}

fn process_map_argument_checks() -> Outcome {
    let p = proc_create()?;
    let v = vmo(4)?;
    let map = |vaddr: u64, off: u64, len: u64, flags: u64| {
        st(sys::process_map(p, v, vaddr, off, len, flags))
    };
    ensure_eq!(
        map(0x1_0001, 0, 4096, PF_R),
        Some(S::InvalidArgs),
        "unaligned address"
    );
    ensure_eq!(
        map(0x1_0000, 1, 4096, PF_R),
        Some(S::InvalidArgs),
        "unaligned offset"
    );
    ensure_eq!(map(0x1_0000, 0, 0, PF_R), Some(S::InvalidArgs), "length 0");
    ensure_eq!(
        map(0x1_0000, 0, 100, PF_R),
        Some(S::InvalidArgs),
        "unaligned length"
    );
    ensure_eq!(
        map(0x1_0000, 0, 4096, 8),
        Some(S::InvalidArgs),
        "an unknown flag"
    );
    ensure_eq!(
        map(0x1_0000, 0, 4096, PF_W | PF_X),
        Some(S::InvalidArgs),
        "writable and executable"
    );
    ensure_eq!(map(0, 0, 4096, PF_R), Some(S::InvalidArgs), "page zero");
    ensure_eq!(
        map(TOP - 4096, 0, 8192, PF_R),
        Some(S::InvalidArgs),
        "over the top of the user half"
    );
    ensure_eq!(
        map(0xffff_ffff_8000_0000, 0, 4096, PF_R),
        Some(S::InvalidArgs),
        "a kernel address"
    );
    ensure_eq!(
        map(0x1_0000, 3 * 4096, 8192, PF_R),
        Some(S::InvalidArgs),
        "past the end of the VMO"
    );
    ensure_eq!(
        map(0x1_0000, u64::MAX - 4095, 4096, PF_R),
        Some(S::InvalidArgs),
        "offset overflow"
    );
    ensure_eq!(
        map(0x1_0000, 0, 4 * 4096, PF_R | PF_W),
        Some(S::Ok),
        "a good mapping"
    );
    ensure_eq!(
        map(0x1_0000, 0, 4096, PF_R),
        Some(S::InvalidArgs),
        "overlap"
    );
    ensure_eq!(
        map(0x1_3000, 0, 4096, PF_R),
        Some(S::InvalidArgs),
        "overlap with the last page"
    );
    ensure_eq!(
        map(0x1_4000, 0, 4096, PF_R),
        Some(S::Ok),
        "right after is fine"
    );
    // rights
    let no_map = weaker(v, Rights::READ.0)?;
    ensure_eq!(
        st(sys::process_map(p, no_map, 0x8_0000, 0, 4096, PF_R)),
        Some(S::AccessDenied),
        "no MAP right"
    );
    let ro_map = weaker(v, Rights::READ.0 | Rights::MAP.0)?;
    ensure_eq!(
        st(sys::process_map(p, ro_map, 0x8_0000, 0, 4096, PF_W)),
        Some(S::AccessDenied),
        "W without WRITE"
    );
    ensure_eq!(
        st(sys::process_map(p, ro_map, 0x8_0000, 0, 4096, PF_X)),
        Some(S::AccessDenied),
        "X without EXECUTE"
    );
    ensure_eq!(
        st(sys::process_map(p, ro_map, 0x8_0000, 0, 4096, PF_R)),
        Some(S::Ok),
        "R with READ|MAP"
    );
    let no_manage = weaker(p, Rights::WAIT.0)?;
    ensure_eq!(
        st(sys::process_map(no_manage, v, 0x9_0000, 0, 4096, PF_R)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    let e = event()?;
    ensure_eq!(
        st(sys::process_map(e, v, 0x9_0000, 0, 4096, PF_R)),
        Some(S::WrongType),
        "an event as the process"
    );
    ensure_eq!(
        st(sys::process_map(p, e, 0x9_0000, 0, 4096, PF_R)),
        Some(S::WrongType),
        "an event as the VMO"
    );
    ensure_eq!(
        st(sys::process_map(BAD, v, 0x9_0000, 0, 4096, PF_R)),
        Some(S::BadHandle),
        "bad process handle"
    );
    ensure_eq!(
        st(sys::process_map(p, BAD, 0x9_0000, 0, 4096, PF_R)),
        Some(S::BadHandle),
        "bad VMO handle"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::PROCESS_MAP,
            [
                (1usize << 32) | p as usize,
                v as usize,
                0x9_0000,
                0,
                4096,
                1
            ]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // unmap
    ensure_eq!(
        st(sys::process_unmap(p, 0x1_1000)),
        Some(S::InvalidArgs),
        "no mapping starts there"
    );
    ensure_eq!(
        st(sys::process_unmap(no_manage, 0x1_0000)),
        Some(S::AccessDenied),
        "unmap needs MANAGE"
    );
    ensure_eq!(st(sys::process_unmap(p, 0x1_0000)), Some(S::Ok), "unmap");
    ensure_eq!(
        st(sys::process_unmap(p, 0x1_0000)),
        Some(S::InvalidArgs),
        "unmap again"
    );
    ensure_eq!(
        st(sys::process_map(p, v, 0x1_0000, 0, 4096, PF_R)),
        Some(S::Ok),
        "the range is free again"
    );
    Ok(())
}

fn process_start_argument_checks() -> Outcome {
    let p = proc_create()?;
    let code = vmo(2)?;
    ensure_eq!(
        st(sys::process_map(p, code, 0x2_0000, 0, 8192, PF_R | PF_X)),
        Some(S::Ok),
        "map some code"
    );
    let data = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(p, data, 0x4_0000, 0, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "and some data"
    );
    let (mine, theirs) = channel()?;
    let go = |entry: u64, sp: u64, bs: u32| st(sys::process_start(p, entry, sp, bs, 0));
    ensure_eq!(
        go(0x3_0000, 0x4_1000, theirs),
        Some(S::InvalidArgs),
        "entry in no mapping"
    );
    ensure_eq!(
        go(0x4_0000, 0x4_1000, theirs),
        Some(S::InvalidArgs),
        "entry in a mapping that is not executable"
    );
    ensure_eq!(
        go(0x2_0000, 0x4_0ff8, theirs),
        Some(S::InvalidArgs),
        "sp not 16-byte aligned"
    );
    ensure_eq!(
        go(0x2_0000, TOP + 16, theirs),
        Some(S::InvalidArgs),
        "sp above the user half"
    );
    let ev = event()?;
    ensure_eq!(
        go(0x2_0000, 0x4_1000, ev),
        Some(S::WrongType),
        "an event as the bootstrap handle"
    );
    let no_transfer = weaker(theirs, Rights::READ.0 | Rights::WRITE.0)?;
    ensure_eq!(
        go(0x2_0000, 0x4_1000, no_transfer),
        Some(S::AccessDenied),
        "no TRANSFER on the bootstrap handle"
    );
    ensure_eq!(
        go(0x2_0000, 0x4_1000, BAD),
        Some(S::BadHandle),
        "bad bootstrap handle"
    );
    ensure_eq!(
        st(sys::process_start(BAD, 0x2_0000, 0x4_1000, theirs, 0)),
        Some(S::BadHandle),
        "bad process handle"
    );
    ensure_eq!(
        st(sys::process_start(ev, 0x2_0000, 0x4_1000, theirs, 0)),
        Some(S::WrongType),
        "an event as the process"
    );
    let no_manage = weaker(p, Rights::INSPECT.0)?;
    ensure_eq!(
        st(sys::process_start(no_manage, 0x2_0000, 0x4_1000, theirs, 0)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    ensure!(
        sys::object_signals(theirs).status == 0 && sys::object_signals(mine).status == 0,
        "a refused start left the handle with us"
    );
    // process_kill / process_info argument checks on the same process
    ensure_eq!(st(sys::process_info(p)), Some(S::BadState), "not ended yet");
    ensure_eq!(
        st(sys::process_info(no_manage)),
        Some(S::BadState),
        "INSPECT is enough"
    );
    let weak = weaker(p, Rights::WAIT.0)?;
    ensure_eq!(
        st(sys::process_info(weak)),
        Some(S::AccessDenied),
        "info needs INSPECT"
    );
    ensure_eq!(
        st(sys::process_kill(weak, 1)),
        Some(S::AccessDenied),
        "kill needs KILL"
    );
    ensure_eq!(
        st(sys::process_kill(ev, 1)),
        Some(S::WrongType),
        "an event as the process"
    );
    ensure_eq!(
        st(sys::process_kill(BAD, 1)),
        Some(S::BadHandle),
        "bad handle"
    );
    Ok(())
}

fn fetch_the_program_image() -> Outcome {
    let (m, n) = request(21, b'I', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u32; 2];
    let args = CallArgs {
        req_bytes: m.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: n as u32,
        req_nhandles: 0,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: got.as_mut_ptr() as u64,
        rep_max_bytes: 16,
        rep_max_handles: 2,
        deadline: sys::clock_get() + 500,
    };
    let r = sys::channel_call(BOOTSTRAP, &args);
    ensure_eq!(
        (r.status, r.a1, r.a2),
        (0, 5, 1),
        "the reply carries a handle (call: status, nbytes, nhandles)"
    );
    let image = got[0];
    let size = sys::vmo_get_size(image).a1;
    ensure!(size > 1000, "the image VMO is {size} bytes");
    let mut head = [0u8; 4];
    sys::vmo_read(image, 0, head.as_mut_ptr(), 4);
    ensure_eq!(&head, b"\x7fELF", "it holds an ELF file");
    IMAGE.store(image, Ordering::Relaxed);
    Ok(())
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..n {
        v |= (b[at + i] as u64) << (8 * i);
    }
    v
}

/// The launcher: read the ELF in `image`, build a process from it (one VMO and one mapping
/// per PT_LOAD segment, plus a stack), and start it with start argument `arg`. Returns our
/// handle to the process and our end of its bootstrap channel. This is what a real launcher
/// does (OS-ARCHITECTURE 9): the kernel never parses an executable for it.
fn launch(arg: u64) -> Result<(u32, u32), Detail> {
    launch_image(IMAGE.load(Ordering::Relaxed), arg)
}

fn launch_image(image: u32, arg: u64) -> Result<(u32, u32), Detail> {
    launch_image_in(image, arg, 0)
}

/// Build and start a child from `image` charged to `job` (a handle with MANAGE; 0: our own job).
/// With a job, our handles to the child's memory are closed after mapping, so that the job's
/// usage can be seen going back to zero when the child ends.
fn launch_image_in(image: u32, arg: u64, job: u32) -> Result<(u32, u32), Detail> {
    ensure!(image != 0, "no image was fetched");
    let mut eh = [0u8; 64];
    ok(
        sys::vmo_read(image, 0, eh.as_mut_ptr(), 64),
        "read the ELF header",
    )?;
    let (entry, phoff, phnum) = (le(&eh, 24, 8), le(&eh, 32, 8), le(&eh, 56, 2) as usize);
    ensure!(phnum <= 16, "{phnum} program headers");
    let mut ph = [0u8; 56 * 16];
    ok(
        sys::vmo_read(image, phoff, ph.as_mut_ptr(), 56 * phnum),
        "read the program headers",
    )?;
    let proc = if job == 0 {
        proc_create()?
    } else {
        ok(sys::process_create_in(job), "process_create_in")?.a1 as u32
    };
    for i in 0..phnum {
        let h = &ph[i * 56..(i + 1) * 56];
        if le(h, 0, 4) != 1 {
            continue; // not PT_LOAD
        }
        let (flags, offset, vaddr, filesz, memsz) = (
            le(h, 4, 4),
            le(h, 8, 8),
            le(h, 16, 8),
            le(h, 32, 8),
            le(h, 40, 8),
        );
        let lead = vaddr % 4096;
        let pages = (lead + memsz).div_ceil(4096);
        let seg = if job == 0 {
            vmo(pages)?
        } else {
            ok(sys::vmo_create_in(pages, job), "vmo_create_in")?.a1 as u32
        };
        let mut done = 0u64;
        let mut chunk = [0u8; 2048];
        while done < filesz {
            let n = (filesz - done).min(2048);
            ok(
                sys::vmo_read(image, offset + done, chunk.as_mut_ptr(), n as usize),
                "read the image",
            )?;
            ok(
                sys::vmo_write(seg, lead + done, chunk.as_ptr(), n as usize),
                "fill the segment",
            )?;
            done += n;
        }
        let perms =
            PF_R | if flags & 2 != 0 { PF_W } else { 0 } | if flags & 1 != 0 { PF_X } else { 0 };
        ok(
            sys::process_map(proc, seg, vaddr - lead, 0, pages * 4096, perms),
            "map a segment",
        )?;
        if job != 0 {
            sys::handle_close(seg);
        }
    }
    let stack = if job == 0 {
        vmo(16)?
    } else {
        ok(sys::vmo_create_in(16, job), "vmo_create_in")?.a1 as u32
    };
    ok(
        sys::process_map(proc, stack, 0x10_0000_0000, 0, 16 * 4096, PF_R | PF_W),
        "map the stack",
    )?;
    if job != 0 {
        sys::handle_close(stack);
    }
    let (mine, theirs) = channel()?;
    ok(
        sys::process_start(proc, entry, 0x10_0000_0000 + 16 * 4096, theirs, arg),
        "start",
    )?;
    Ok((proc, mine))
}

fn wait_ended(proc: u32, ticks: u64) -> Result<(), Detail> {
    let r = sys::object_wait(proc, sig::TERMINATED, sys::clock_get() + ticks);
    ensure_eq!(r.status, 0, "the process ended");
    ensure!(r.a1 as u32 & sig::TERMINATED != 0, "TERMINATED is set");
    Ok(())
}

fn child_exit_code_and_terminated_signal() -> Outcome {
    let (p, mine) = launch(8)?;
    wait_ended(p, 500)?;
    let r = sys::process_info(p);
    ensure_eq!((r.status, r.a1 as i64), (0, 42), "the child's exit code");
    ensure!(
        sys::object_signals(mine).a1 as u32 & sig::PEER_CLOSED != 0,
        "its handles were closed: our end of the bootstrap channel sees it"
    );
    Ok(())
}

fn child_faults_are_contained() -> Outcome {
    for (variant, want) in [(1u64, -1015i64), (7, -1013), (5, -1002)] {
        let (p, _mine) = launch(variant)?;
        wait_ended(p, 500)?;
        let r = sys::process_info(p);
        ensure_eq!(
            (r.status, r.a1 as i64),
            (0, want),
            "exit code of variant {variant}"
        );
    }
    Ok(())
}

fn parent_and_child_talk() -> Outcome {
    let (p, mine) = launch(14)?;
    ensure_eq!(
        st(sys::channel_write(
            mine,
            b"ping".as_ptr(),
            4,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send to the child"
    );
    let r = sys::object_wait(
        mine,
        sig::READABLE | sig::PEER_CLOSED,
        sys::clock_get() + 500,
    );
    ensure!(
        r.status == 0 && r.a1 as u32 & sig::READABLE != 0,
        "the child answered"
    );
    let mut buf = [0u8; 8];
    let r = sys::channel_read(mine, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1, &buf[..4]),
        (0, 4, &b"gnip"[..]),
        "the reversed message"
    );
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 3, "its exit code");
    Ok(())
}

fn kill_a_spinning_child() -> Outcome {
    let (p, _mine) = launch(15)?;
    ensure_eq!(st(sys::process_info(p)), Some(S::BadState), "still running");
    let r = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 4);
    ensure_eq!(
        st(r),
        Some(S::TimedOut),
        "a spinning child does not end by itself"
    );
    ensure_eq!(st(sys::process_kill(p, 77)), Some(S::Ok), "kill it");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 77, "the exit code we gave");
    ensure_eq!(
        st(sys::process_kill(p, 1)),
        Some(S::BadState),
        "kill it again"
    );
    Ok(())
}

fn kill_a_blocked_child() -> Outcome {
    let (p, _mine) = launch(16)?;
    let r = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 4);
    ensure_eq!(st(r), Some(S::TimedOut), "the child is blocked in a wait");
    ensure_eq!(st(sys::process_kill(p, -5)), Some(S::Ok), "kill it");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -5, "its exit code");
    Ok(())
}

fn closing_the_last_process_handle_kills_the_child() -> Outcome {
    let (p, mine) = launch(15)?;
    sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 3);
    ensure_eq!(
        st(sys::handle_close(p)),
        Some(S::Ok),
        "drop the only handle to the child"
    );
    let r = sys::object_wait(mine, sig::PEER_CLOSED, sys::clock_get() + 500);
    ensure_eq!(
        r.status,
        0,
        "the child is gone: its end of the channel closed"
    );
    Ok(())
}

fn a_process_that_never_started_can_be_killed() -> Outcome {
    let p = proc_create()?;
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(p, v, 0x1_0000, 0, 4096, PF_R)),
        Some(S::Ok),
        "give it some memory"
    );
    ensure_eq!(
        st(sys::process_kill(p, 5)),
        Some(S::Ok),
        "kill it before it ran"
    );
    ensure_eq!(
        sys::object_signals(p).a1 as u32 & sig::TERMINATED,
        sig::TERMINATED,
        "TERMINATED at once"
    );
    ensure_eq!(sys::process_info(p).a1 as i64, 5, "exit code");
    ensure_eq!(
        st(sys::process_map(p, v, 0x2_0000, 0, 4096, PF_R)),
        Some(S::BadState),
        "an ended process takes no mappings"
    );
    let (_mine, theirs) = channel()?;
    ensure_eq!(
        st(sys::process_start(p, 0x1_0000, 0x1_1000, theirs, 0)),
        Some(S::BadState),
        "and cannot be started"
    );
    Ok(())
}

fn process_self_and_mapping_into_ourselves() -> Outcome {
    let me = ok(sys::process_self(), "process_self")?.a1 as u32;
    ensure_eq!(
        st(sys::process_info(me)),
        Some(S::BadState),
        "we have not ended"
    );
    let list = [me];
    let (a, _b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            core::ptr::null(),
            0,
            list.as_ptr(),
            1
        )),
        Some(S::AccessDenied),
        "a self handle cannot be sent away"
    );
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(me, v, 0x400_0000, 0, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map a VMO into ourselves"
    );
    // SAFETY: 0x400_0000 was just mapped read-write in this process.
    unsafe { core::ptr::write_volatile(0x400_0010 as *mut u32, 0xdead_beef) };
    let mut b = [0u8; 4];
    sys::vmo_read(v, 0x10, b.as_mut_ptr(), 4);
    ensure_eq!(
        u32::from_le_bytes(b),
        0xdead_beef,
        "the store through the mapping is in the VMO"
    );
    sys::vmo_write(v, 0x20, 0x1234_5678u32.to_le_bytes().as_ptr(), 4);
    // SAFETY: as above.
    let seen = unsafe { core::ptr::read_volatile(0x400_0020 as *const u32) };
    ensure_eq!(
        seen,
        0x1234_5678,
        "and a write to the VMO is visible through the mapping"
    );
    ensure_eq!(st(sys::process_unmap(me, 0x400_0000)), Some(S::Ok), "unmap");
    let again = ok(sys::process_self(), "a second self handle")?.a1 as u32;
    ensure_eq!(
        st(sys::handle_close(again)),
        Some(S::Ok),
        "closing a self handle does not end us"
    );
    ensure_eq!(
        st(sys::handle_close(me)),
        Some(S::Ok),
        "not even the last one"
    );
    Ok(())
}

fn vmo_closed_while_mapped_keeps_its_pages() -> Outcome {
    // SYSCALLS.md: closing the last handle to a VMO frees it "once nothing maps it". A
    // mapping must keep the VMO alive, including pages not yet touched.
    let me = ok(sys::process_self(), "process_self")?.a1 as u32;
    let v = vmo(2)?;
    sys::vmo_write(v, 0, 0xfeed_f00du32.to_le_bytes().as_ptr(), 4);
    ensure_eq!(
        st(sys::process_map(me, v, 0x500_0000, 0, 8192, PF_R | PF_W)),
        Some(S::Ok),
        "map both pages"
    );
    ensure_eq!(
        st(sys::handle_close(v)),
        Some(S::Ok),
        "close the only VMO handle"
    );
    // SAFETY: 0x500_0000..0x500_2000 is mapped read-write in this process.
    let (first, second) = unsafe {
        (
            core::ptr::read_volatile(0x500_0000 as *const u32),
            core::ptr::read_volatile(0x500_1000 as *const u32),
        )
    };
    ensure_eq!(first, 0xfeed_f00d, "the committed page survives");
    ensure_eq!(second, 0, "an untouched page still faults in as zeros");
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(0x500_1000 as *mut u32, 1) };
    ensure_eq!(st(sys::process_unmap(me, 0x500_0000)), Some(S::Ok), "unmap");
    Ok(())
}

fn many_children_do_not_leak() -> Outcome {
    // The kernel checks after we exit that no process, VMO, object or frame is left.
    for _ in 0..6 {
        let (p, _m) = launch(8)?;
        wait_ended(p, 500)?;
    }
    for _ in 0..4 {
        let (p, _m) = launch(15)?;
        ensure_eq!(st(sys::process_kill(p, 1)), Some(S::Ok), "kill");
        wait_ended(p, 500)?;
    }
    for _ in 0..3 {
        let (p, _m) = launch(16)?;
        sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 2);
        sys::handle_close(p);
    }
    Ok(())
}

// --- Threads (S3c-2b) -----------------------------------------------------------------------

use core::sync::atomic::AtomicU64;

static NEXT_STACK: AtomicU64 = AtomicU64::new(0x30_0000_0000);
static COUNTER: AtomicU32 = AtomicU32::new(0);
static WOKEN_AT: AtomicU64 = AtomicU64::new(0);
static WOKEN_STATUS: AtomicU32 = AtomicU32::new(99);

fn me() -> Result<u32, Detail> {
    Ok(ok(sys::process_self(), "process_self")?.a1 as u32)
}

/// Map a fresh 4-page stack into our own address space; returns its top.
fn own_stack() -> Result<u64, Detail> {
    let base = NEXT_STACK.fetch_add(0x10_0000, Ordering::SeqCst);
    let v = vmo(4)?;
    ok(
        sys::process_map(me()?, v, base, 0, 4 * 4096, PF_R | PF_W),
        "map a stack",
    )?;
    Ok(base + 4 * 4096)
}

fn new_thread(f: extern "C" fn(usize) -> !, arg: usize) -> Result<u32, Detail> {
    let top = own_stack()?;
    Ok(ok(
        sys::thread_create(me()?, f as *const () as usize as u64, top, arg as u64),
        "thread_create",
    )?
    .a1 as u32)
}

extern "C" fn worker_set(arg: usize) -> ! {
    COUNTER.store(arg as u32, Ordering::SeqCst);
    sys::thread_exit(7)
}

/// `arg` is an event handle: wait for user signal 0 and record when and how the wait ended.
extern "C" fn worker_wait_event(arg: usize) -> ! {
    let r = sys::object_wait(arg as u32, 1 << 24, sys::clock_get() + 2000);
    WOKEN_AT.store(sys::clock_get(), Ordering::SeqCst);
    WOKEN_STATUS.store(r.status as u32, Ordering::SeqCst);
    sys::thread_exit(if r.status == 0 { 0 } else { 1 })
}

fn thread_create_argument_checks() -> Outcome {
    let top = own_stack()?;
    let me = me()?;
    let entry = worker_set as *const () as usize as u64;
    let go = |p: u32, entry: u64, sp: u64| st(sys::thread_create(p, entry, sp, 0));
    ensure_eq!(go(BAD, entry, top), Some(S::BadHandle), "bad handle");
    let ev = event()?;
    ensure_eq!(
        go(ev, entry, top),
        Some(S::WrongType),
        "an event as the process"
    );
    let weak = weaker(me, Rights::WAIT.0)?;
    ensure_eq!(go(weak, entry, top), Some(S::AccessDenied), "no MANAGE");
    static DATA: u64 = 5;
    ensure_eq!(
        go(me, &DATA as *const u64 as u64, top),
        Some(S::InvalidArgs),
        "an entry that is not executable"
    );
    ensure_eq!(
        go(me, 0x2_0000_0000, top),
        Some(S::InvalidArgs),
        "an unmapped entry"
    );
    ensure_eq!(
        go(me, entry, top - 8),
        Some(S::InvalidArgs),
        "sp not 16-byte aligned"
    );
    ensure_eq!(
        go(me, entry, TOP + 16),
        Some(S::InvalidArgs),
        "sp above the user half"
    );
    ensure_eq!(go(me, entry, 0), Some(S::InvalidArgs), "sp 0");
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::THREAD_CREATE,
            [
                (1usize << 32) | me as usize,
                entry as usize,
                top as usize,
                0,
                0,
                0
            ]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // a process that has not started, and one that has ended
    let p = proc_create()?;
    ensure_eq!(
        go(p, entry, top),
        Some(S::BadState),
        "a process that was never started"
    );
    ensure_eq!(st(sys::process_kill(p, 1)), Some(S::Ok), "end it");
    ensure_eq!(go(p, entry, top), Some(S::BadState), "an ended process");
    // thread_info
    ensure_eq!(
        st(sys::thread_info(BAD)),
        Some(S::BadHandle),
        "info: bad handle"
    );
    ensure_eq!(
        st(sys::thread_info(ev)),
        Some(S::WrongType),
        "info: an event"
    );
    ensure_eq!(
        st(sys::thread_info(me)),
        Some(S::WrongType),
        "info: a process"
    );
    Ok(())
}

fn threads_share_memory_and_report_exit_codes() -> Outcome {
    COUNTER.store(0, Ordering::SeqCst);
    let t = new_thread(worker_set, 42)?;
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "the thread ended");
    ensure_eq!(COUNTER.load(Ordering::SeqCst), 42, "it wrote our memory");
    let r = sys::thread_info(t);
    ensure_eq!((r.status, r.a1 as i64), (0, 7), "its exit code");
    let weak = weaker(t, Rights::WAIT.0)?;
    ensure_eq!(
        st(sys::thread_info(weak)),
        Some(S::AccessDenied),
        "info needs INSPECT"
    );
    let ev = event()?;
    ensure_eq!(
        sys::object_signals(t).a1 as u32 & sig::TERMINATED,
        sig::TERMINATED,
        "TERMINATED stays set"
    );
    let _ = ev;
    Ok(())
}

fn a_user_signal_wakes_a_blocked_thread() -> Outcome {
    let ev = event()?;
    let t = new_thread(worker_wait_event, ev as usize)?;
    // the thread blocks; nothing wakes it for a while
    let gate = event()?;
    let r = sys::object_wait(gate, 1 << 24, sys::clock_get() + 6);
    ensure_eq!(st(r), Some(S::TimedOut), "we wait a few ticks");
    ensure_eq!(
        sys::object_signals(t).a1 as u32 & sig::TERMINATED,
        0,
        "the thread is still blocked"
    );
    ensure_eq!(st(sys::thread_info(t)), Some(S::BadState), "not ended yet");
    let t0 = sys::clock_get();
    ensure_eq!(
        st(sys::event_signal(ev, 0, 1 << 24)),
        Some(S::Ok),
        "signal the event"
    );
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "the thread ended");
    ensure_eq!(WOKEN_STATUS.load(Ordering::SeqCst), 0, "its wait succeeded");
    let woke = WOKEN_AT.load(Ordering::SeqCst);
    ensure!(
        within(woke.saturating_sub(t0), 0, 5),
        "it woke {} ticks after the signal",
        woke.saturating_sub(t0)
    );
    ensure_eq!(sys::thread_info(t).a1, 0, "exit code 0");
    Ok(())
}

fn thread_limit_and_slots_are_reused() -> Outcome {
    let ev = event()?;
    let mut ts = [0u32; 8];
    let mut n = 0;
    loop {
        let top = own_stack()?;
        let r = sys::thread_create(
            me()?,
            worker_wait_event as *const () as usize as u64,
            top,
            ev as u64,
        );
        if r.status != 0 {
            ensure_eq!(st(r), Some(S::NoMemory), "the limit is NO_MEMORY");
            break;
        }
        ensure!(n < ts.len(), "more than 8 threads were allowed");
        ts[n] = r.a1 as u32;
        n += 1;
    }
    // this thread plus `n` others: the limit is 8 in all
    ensure_eq!(n, 7, "threads created on top of the one we are");
    ensure_eq!(
        st(sys::event_signal(ev, 0, 1 << 24)),
        Some(S::Ok),
        "wake them all"
    );
    for &t in &ts[..n] {
        let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
        ensure_eq!(r.status, 0, "a thread ended");
    }
    ensure_eq!(
        st(sys::event_signal(ev, 1 << 24, 0)),
        Some(S::Ok),
        "clear the signal"
    );
    let t = new_thread(worker_set, 1)?;
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "after they ended, there is room again"
    );
    Ok(())
}

fn second_thread_outlives_the_first() -> Outcome {
    let (p, _m) = launch(17)?;
    let r = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 6);
    ensure_eq!(
        st(r),
        Some(S::TimedOut),
        "the first thread left; the process lives on"
    );
    ensure_eq!(st(sys::process_kill(p, 77)), Some(S::Ok), "kill it");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 77, "the kill's exit code");
    Ok(())
}

fn a_thread_can_end_the_whole_process() -> Outcome {
    let (p, _m) = launch(18)?;
    wait_ended(p, 500)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        9,
        "exit(9) from the second thread"
    );
    Ok(())
}

fn a_fault_in_any_thread_ends_the_process() -> Outcome {
    let (p, _m) = launch(19)?;
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -1013, "the load fault");
    Ok(())
}

fn the_last_thread_sets_the_exit_code() -> Outcome {
    let (p, _m) = launch(20)?;
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 6, "the last thread's code");
    Ok(())
}

fn many_threaded_children_do_not_leak() -> Outcome {
    for _ in 0..3 {
        for variant in [18u64, 19, 20] {
            let (p, _m) = launch(variant)?;
            wait_ended(p, 500)?;
        }
        let (p, _m) = launch(17)?;
        sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 2);
        ensure_eq!(st(sys::process_kill(p, 1)), Some(S::Ok), "kill");
        wait_ended(p, 500)?;
    }
    Ok(())
}

// Regression tests from the independent review of S3c-2b.

fn threads_created_but_not_run_when_the_process_ends() -> Outcome {
    for _ in 0..5 {
        let (p, _m) = launch(22)?;
        wait_ended(p, 500)?;
        ensure_eq!(
            sys::process_info(p).a1 as i64,
            3,
            "exit(3) with unstarted threads"
        );
        let (p, _m) = launch(23)?;
        wait_ended(p, 500)?;
        ensure_eq!(sys::process_info(p).a1 as i64, 8, "the last thread's code");
    }
    Ok(())
}

fn kill_a_process_that_is_creating_threads() -> Outcome {
    for d in 0..12u64 {
        let (p, _m) = launch(24)?;
        let g = event()?;
        sys::object_wait(g, 1 << 24, sys::clock_get() + d % 4 + 1);
        ensure_eq!(st(sys::process_kill(p, 5)), Some(S::Ok), "kill");
        wait_ended(p, 500)?;
        ensure_eq!(sys::process_info(p).a1 as i64, 5, "the kill's code");
    }
    Ok(())
}

fn thread_create_with_a_full_handle_table() -> Outcome {
    let me = me()?;
    let top = own_stack()?;
    let entry = worker_set as *const () as usize as u64;
    static HS: [AtomicU32; 4200] = [const { AtomicU32::new(0) }; 4200];
    let mut n = 0;
    while n < HS.len() {
        let r = sys::event_create();
        if r.status != 0 {
            break;
        }
        HS[n].store(r.a1 as u32, Ordering::Relaxed);
        n += 1;
    }
    ensure_eq!(
        st(sys::thread_create(me, entry, top, 0)),
        Some(S::NoMemory),
        "table full"
    );
    for h in &HS[..n] {
        sys::handle_close(h.load(Ordering::Relaxed));
    }
    for _ in 0..3 {
        let t = new_thread(worker_set, 1)?;
        ensure_eq!(
            sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
            0,
            "threads work again"
        );
    }
    Ok(())
}

fn a_thread_handle_closed_early_is_harmless() -> Outcome {
    let ev = event()?;
    let t = new_thread(worker_wait_event, ev as usize)?;
    ensure_eq!(
        st(sys::handle_close(t)),
        Some(S::Ok),
        "close the thread handle"
    );
    for _ in 0..50 {
        let e = event()?;
        sys::handle_close(e);
    }
    ensure_eq!(
        st(sys::event_signal(ev, 0, 1 << 24)),
        Some(S::Ok),
        "wake it"
    );
    let gate = event()?;
    sys::object_wait(gate, 1 << 24, sys::clock_get() + 10);
    Ok(())
}

/// Many children each trying to start seven threads: the system-wide limit must turn into
/// NO_MEMORY for the children, never a kernel out-of-memory panic.
fn a_thread_hog_cannot_exhaust_the_kernel() -> Outcome {
    let mut ps = [0u32; 40];
    let mut n = 0;
    while n < ps.len() {
        // once the system-wide thread limit is reached a start is refused with NO_MEMORY
        match launch(21) {
            Ok((h, _m)) => ps[n] = h,
            Err(_) => break,
        }
        n += 1;
    }
    for &p in &ps[..n] {
        sys::process_kill(p, 1);
    }
    for &p in &ps[..n] {
        wait_ended(p, 2000)?;
    }
    ensure!(n >= 10, "only {n} children could be started");
    Ok(())
}

// Tests from the independent (Fable) review of S3c-2b.

fn child_entry() -> Result<u64, Detail> {
    let image = IMAGE.load(Ordering::Relaxed);
    let mut eh = [0u8; 64];
    ok(
        sys::vmo_read(image, 0, eh.as_mut_ptr(), 64),
        "read the ELF header",
    )?;
    Ok(le(&eh, 24, 8))
}

/// Another thread in child `p` at the program entry with start arg `arg`, on its own stack.
fn child_thread(p: u32, slot: u64, arg: u64) -> Result<u32, Detail> {
    let base = 0x20_0000_0000 + slot * 0x10_0000;
    let v = vmo(4)?;
    ok(
        sys::process_map(p, v, base, 0, 4 * 4096, PF_R | PF_W),
        "map a child stack",
    )?;
    sys::handle_close(v);
    Ok(ok(
        sys::thread_create(p, child_entry()?, base + 4 * 4096, arg),
        "thread_create in a child",
    )?
    .a1 as u32)
}

fn thread_ended_with(t: u32, code: i64) -> Outcome {
    ensure_eq!(
        sys::object_signals(t).a1 as u32 & sig::TERMINATED,
        sig::TERMINATED,
        "a thread is TERMINATED once its process is"
    );
    let r = sys::thread_info(t);
    ensure_eq!((r.status, r.a1 as i64), (0, code), "thread exit code");
    Ok(())
}

fn nap(ticks: u64) -> Outcome {
    let gate = event()?;
    sys::object_wait(gate, 1 << 24, sys::clock_get() + ticks);
    sys::handle_close(gate);
    Ok(())
}

extern "C" fn worker_spin(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn worker_block_forever(arg: usize) -> ! {
    sys::object_wait(arg as u32, 1 << 24, u64::MAX);
    sys::thread_exit(1)
}

fn replacing_a_handle_ends_a_siblings_wait() -> Outcome {
    let ev = event()?;
    WOKEN_STATUS.store(99, Ordering::SeqCst);
    let t = new_thread(worker_wait_event, ev as usize)?; // waits on `ev` up to 2000 ticks
    nap(4)?;
    // nap's close woke every waiter: let the worker re-check and sleep again
    for _ in 0..3 {
        sys::thread_yield();
    }
    let _ev2 = ok(sys::handle_replace(ev, Rights::SAME.0), "replace")?.a1 as u32;
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 100);
    let prompt = r.status == 0;
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 3000); // leave nothing behind
    ensure!(prompt, "the waiter slept on after its handle was replaced");
    ensure_eq!(
        WOKEN_STATUS.load(Ordering::SeqCst) as i32,
        S::BadHandle.code(),
        "the wait reports BAD_HANDLE"
    );
    Ok(())
}

fn child_with_a_ready_exit_thread() -> Result<(u32, u32, u32), Detail> {
    let (p, m) = launch(25)?;
    let base = 0x20_0000_0000u64;
    let v = vmo(4)?;
    ok(
        sys::process_map(p, v, base, 0, 4 * 4096, PF_R | PF_W),
        "map the stack before the child runs",
    )?;
    nap(3)?;
    // a thread that would exit(42) at once
    let t = ok(
        sys::thread_create(p, child_entry()?, base + 4 * 4096, 8),
        "thread_create",
    )?
    .a1 as u32;
    Ok((p, m, t))
}

fn a_thread_killed_before_it_ran_runs_no_user_code() -> Outcome {
    let (p, _m, t) = child_with_a_ready_exit_thread()?;
    ensure_eq!(
        st(sys::process_kill(p, 55)),
        Some(S::Ok),
        "kill before it ran"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 55, "the kill's exit code");
    thread_ended_with(t, 55)
}

fn kill_a_child_whose_threads_never_ran() -> Outcome {
    let (p, _m) = launch(15)?;
    let mut ts = [0u32; 7];
    for (i, t) in ts.iter_mut().enumerate() {
        *t = child_thread(p, i as u64, 15)?;
    }
    ensure_eq!(
        st(sys::thread_create(
            p,
            child_entry()?,
            0x20_0000_0000 + 0x4000,
            15
        )),
        Some(S::NoMemory),
        "a ninth thread"
    );
    ensure_eq!(st(sys::process_kill(p, 55)), Some(S::Ok), "kill");
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 55, "exit code");
    for t in ts {
        thread_ended_with(t, 55)?;
    }
    Ok(())
}

fn kill_a_child_with_blocked_and_spinning_threads() -> Outcome {
    let (p, _m) = launch(16)?;
    nap(3)?;
    let mut ts = [0u32; 6];
    for (i, t) in ts.iter_mut().enumerate().take(3) {
        *t = child_thread(p, i as u64, 16)?;
    }
    nap(3)?;
    for (i, t) in ts.iter_mut().enumerate().skip(3) {
        *t = child_thread(p, i as u64, 15)?;
    }
    nap(6)?;
    ensure_eq!(st(sys::process_info(p)), Some(S::BadState), "still running");
    ensure_eq!(st(sys::process_kill(p, -9)), Some(S::Ok), "kill");
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -9, "exit code");
    for t in ts {
        thread_ended_with(t, -9)?;
    }
    Ok(())
}

fn closing_handles_early_and_kill_on_close() -> Outcome {
    let (p, mine) = launch(15)?;
    let mut ts = [0u32; 7];
    for (i, t) in ts.iter_mut().enumerate() {
        *t = child_thread(p, i as u64, 15)?;
    }
    for &t in &ts[..4] {
        ensure_eq!(
            st(sys::handle_close(t)),
            Some(S::Ok),
            "close a thread handle"
        );
    }
    ensure_eq!(st(sys::handle_close(p)), Some(S::Ok), "drop the process");
    let r = sys::object_wait(mine, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(r.status, 0, "the child is gone");
    for &t in &ts[4..] {
        thread_ended_with(t, -2000)?;
    }
    Ok(())
}

fn a_thread_made_by_the_parent_faults_or_exits() -> Outcome {
    let (p, _m) = launch(16)?;
    nap(3)?;
    let b = child_thread(p, 0, 16)?;
    nap(3)?;
    let t = child_thread(p, 1, 7)?; // null load
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -1013, "the fault code");
    thread_ended_with(t, -1013)?;
    thread_ended_with(b, -1013)?;
    let (p, _m) = launch(16)?;
    nap(3)?;
    let b = child_thread(p, 0, 16)?;
    nap(3)?;
    let t = child_thread(p, 1, 8)?; // exit(42)
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 42, "exit(42) from a thread");
    thread_ended_with(t, 42)?;
    thread_ended_with(b, 42)?;
    ensure_eq!(
        st(sys::thread_create(
            p,
            child_entry()?,
            0x20_0000_0000 + 0x4000,
            15
        )),
        Some(S::BadState),
        "no thread in an ended process"
    );
    Ok(())
}

/// Last in the table: init exits with a blocked, a spinning and a not-yet-run thread alive.
fn threads_left_behind_when_init_exits() -> Outcome {
    let ev = event()?;
    new_thread(worker_block_forever, ev as usize)?;
    new_thread(worker_spin, 0)?;
    nap(3)?;
    new_thread(worker_spin, 0)?;
    Ok(())
}

// --- Sockets and ports (S3c-3) --------------------------------------------------------------

const SOCKET_BUFFER: usize = 65536;

fn socket() -> Result<(u32, u32), Detail> {
    let r = ok(sys::socket_create(), "socket_create")?;
    Ok((r.a1 as u32, r.a2 as u32))
}

fn port() -> Result<u32, Detail> {
    Ok(ok(sys::port_create(), "port_create")?.a1 as u32)
}

fn sleep_in_thread(ticks: u64) {
    let gate = sys::event_create().a1 as u32;
    sys::object_wait(gate, 1 << 24, sys::clock_get() + ticks);
    sys::handle_close(gate);
}

extern "C" fn worker_signal_later(arg: usize) -> ! {
    sleep_in_thread(6);
    sys::event_signal(arg as u32, 0, 1 << 24);
    sys::thread_exit(0)
}

extern "C" fn worker_write_later(arg: usize) -> ! {
    sleep_in_thread(6);
    // Let the waiter that the set-up calls woke go back to sleep: with one global wait queue a
    // missing wake-up is otherwise masked by an earlier one (found by the wake-up audit).
    for _ in 0..3 {
        sys::thread_yield();
    }
    let r = sys::socket_write(arg as u32, b"ping".as_ptr(), 4);
    // Linger without making any syscall that wakes waiters, so that only the write's own
    // wake-up can end a waiter's sleep (an exit would wake everyone and hide a missing one).
    let until = sys::clock_get() + 40;
    while sys::clock_get() < until {
        core::hint::spin_loop();
    }
    sys::thread_exit(r.status as i32)
}

/// Wait (up to 2000 ticks) until the socket can take a write, then write 3 bytes.
extern "C" fn worker_write_when_writable(arg: usize) -> ! {
    let r = sys::object_wait(arg as u32, sig::WRITABLE, sys::clock_get() + 2000);
    if r.status != 0 {
        sys::thread_exit(1);
    }
    let w = sys::socket_write(arg as u32, b"abc".as_ptr(), 3);
    sys::thread_exit(if w.status == 0 { w.a1 as i32 } else { 2 })
}

fn socket_syscalls() -> Outcome {
    let (a, b) = socket()?;
    let mut buf = [0u8; 16];
    ensure_eq!(
        st(sys::socket_read(b, buf.as_mut_ptr(), 4)),
        Some(S::ShouldWait),
        "empty and open"
    );
    let w = sys::socket_write(a, b"hello".as_ptr(), 5);
    ensure_eq!((w.status, w.a1), (0, 5), "write (status, written)");
    let r = sys::socket_read(b, buf.as_mut_ptr(), 3);
    ensure_eq!(
        (r.status, r.a1, &buf[..3]),
        (0, 3, &b"hel"[..]),
        "a short read"
    );
    let r = sys::socket_read(b, buf.as_mut_ptr(), 16);
    ensure_eq!((r.status, r.a1, &buf[..2]), (0, 2, &b"lo"[..]), "the rest");
    ensure_eq!(
        st(sys::socket_read(b, buf.as_mut_ptr(), 0)),
        Some(S::Ok),
        "max 0 on an empty socket"
    );
    // full buffer: short write, then SHOULD_WAIT
    let big = [9u8; 4096];
    let mut total = 0usize;
    while total < SOCKET_BUFFER {
        let w = sys::socket_write(a, big.as_ptr(), 4096);
        ensure_eq!(w.status, 0, "filling the buffer");
        total += w.a1;
    }
    ensure_eq!(total, SOCKET_BUFFER, "exactly the buffer size fits");
    ensure_eq!(
        st(sys::socket_write(a, big.as_ptr(), 1)),
        Some(S::ShouldWait),
        "full"
    );
    ensure_eq!(
        sys::object_signals(a).a1 as u32 & sig::WRITABLE,
        0,
        "not WRITABLE when full"
    );
    ensure_eq!(
        st(sys::socket_write(a, big.as_ptr(), 0)),
        Some(S::Ok),
        "an empty write is fine"
    );
    let r = sys::socket_read(b, buf.as_mut_ptr(), 10);
    ensure_eq!((r.status, r.a1), (0, 10), "make room");
    ensure_eq!(
        sys::object_signals(a).a1 as u32 & sig::WRITABLE,
        sig::WRITABLE,
        "WRITABLE again"
    );
    let w = sys::socket_write(a, big.as_ptr(), 100);
    ensure_eq!(
        (w.status, w.a1),
        (0, 10),
        "a short write of the room that was left"
    );
    // errors
    ensure_eq!(
        st(sys::socket_write(a, big.as_ptr(), SOCKET_BUFFER + 1)),
        Some(S::TooBig),
        "write too big"
    );
    ensure_eq!(
        st(sys::socket_read(b, buf.as_mut_ptr(), SOCKET_BUFFER + 1)),
        Some(S::TooBig),
        "read too big"
    );
    ensure_eq!(
        st(sys::socket_write(BAD, big.as_ptr(), 1)),
        Some(S::BadHandle),
        "bad handle"
    );
    let e = event()?;
    ensure_eq!(
        st(sys::socket_write(e, big.as_ptr(), 1)),
        Some(S::WrongType),
        "an event is no socket"
    );
    ensure_eq!(
        st(sys::socket_read(e, buf.as_mut_ptr(), 1)),
        Some(S::WrongType),
        "an event is no socket (read)"
    );
    let ro = weaker(a, Rights::READ.0)?;
    let wo = weaker(a, Rights::WRITE.0)?;
    ensure_eq!(
        st(sys::socket_write(ro, big.as_ptr(), 1)),
        Some(S::AccessDenied),
        "no WRITE"
    );
    ensure_eq!(
        st(sys::socket_read(wo, buf.as_mut_ptr(), 1)),
        Some(S::AccessDenied),
        "no READ"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::SOCKET_WRITE,
            [
                (1usize << 32) | a as usize,
                big.as_ptr() as usize,
                1,
                0,
                0,
                0
            ]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // bad pointers lose no data
    ensure_eq!(
        st(sys::socket_write(wo, 0x10 as *const u8, 4)),
        Some(S::InvalidArgs),
        "bad source"
    );
    ensure_eq!(
        st(sys::socket_read(b, KERNEL_ADDR as *mut u8, 4)),
        Some(S::InvalidArgs),
        "kernel destination"
    );
    let n = sys::socket_read(b, buf.as_mut_ptr(), 8);
    ensure_eq!((n.status, n.a1), (0, 8), "the data is still there");
    Ok(())
}

fn socket_end_of_stream() -> Outcome {
    let (a, b) = socket()?;
    sys::socket_write(a, b"tail".as_ptr(), 4);
    ensure_eq!(st(sys::handle_close(a)), Some(S::Ok), "close the writer");
    let s = sys::object_signals(b).a1 as u32;
    ensure_eq!(
        s & (sig::READABLE | sig::PEER_CLOSED),
        sig::READABLE | sig::PEER_CLOSED,
        "data and PEER_CLOSED"
    );
    let mut buf = [0u8; 8];
    let r = sys::socket_read(b, buf.as_mut_ptr(), 8);
    ensure_eq!(
        (r.status, r.a1, &buf[..4]),
        (0, 4, &b"tail"[..]),
        "the remaining data first"
    );
    ensure_eq!(
        st(sys::socket_read(b, buf.as_mut_ptr(), 8)),
        Some(S::PeerClosed),
        "then the end of the stream"
    );
    ensure_eq!(
        st(sys::socket_write(b, b"x".as_ptr(), 1)),
        Some(S::PeerClosed),
        "writing to a closed peer"
    );
    Ok(())
}

fn socket_ends_travel_over_channels() -> Outcome {
    let (a, b) = socket()?;
    let (c, d) = channel()?;
    ensure_eq!(
        st(sys::channel_write(c, b"s".as_ptr(), 1, [b].as_ptr(), 1)),
        Some(S::Ok),
        "send a socket end"
    );
    let mut hb = [0u32; 1];
    let mut mb = [0u8; 4];
    let r = sys::channel_read(d, mb.as_mut_ptr(), 4, hb.as_mut_ptr(), 1);
    ensure_eq!((r.status, r.a2), (0, 1), "received it");
    sys::socket_write(a, b"via".as_ptr(), 3);
    let mut buf = [0u8; 4];
    let r = sys::socket_read(hb[0], buf.as_mut_ptr(), 4);
    ensure_eq!(
        (r.status, r.a1, &buf[..3]),
        (0, 3, &b"via"[..]),
        "the received end works"
    );
    Ok(())
}

fn a_child_reads_a_socket_to_the_end() -> Outcome {
    let (p, mine) = launch(26)?;
    let (a, b) = socket()?;
    ensure_eq!(
        st(sys::channel_write(mine, b"s".as_ptr(), 1, [b].as_ptr(), 1)),
        Some(S::Ok),
        "give the child an end"
    );
    let data = [5u8; 100];
    ensure_eq!(
        sys::socket_write(a, data.as_ptr(), 100).a1,
        100,
        "write 100 bytes"
    );
    sys::socket_write(a, data.as_ptr(), 28);
    ensure_eq!(
        st(sys::handle_close(a)),
        Some(S::Ok),
        "close our end: the end of the stream"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        128,
        "the child counted every byte"
    );
    Ok(())
}

fn a_blocked_reader_wakes_when_data_arrives() -> Outcome {
    let (a, b) = socket()?;
    let t = new_thread(worker_write_later, a as usize)?;
    let t0 = sys::clock_get();
    let r = sys::object_wait(b, sig::READABLE, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "woken by the write");
    // Strictly: the writer lingers 40 ticks after its write without waking anyone, so a
    // reader woken only by the writer's exit would be about 45 ticks late.
    let took = sys::clock_get() - t0;
    ensure!(
        took >= 5 && (loose() || took <= 25),
        "after about 6 ticks, not {took}"
    );
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    Ok(())
}

fn a_blocked_writer_wakes_when_space_appears() -> Outcome {
    let (a, b) = socket()?;
    let big = [1u8; 4096];
    for _ in 0..16 {
        sys::socket_write(a, big.as_ptr(), 4096);
    }
    let t = new_thread(worker_write_when_writable, a as usize)?;
    sleep_in_thread(5);
    ensure_eq!(
        st(sys::thread_info(t)),
        Some(S::BadState),
        "the writer is still blocked"
    );
    let mut buf = [0u8; 8];
    sys::socket_read(b, buf.as_mut_ptr(), 8);
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "the writer was woken by our read");
    ensure_eq!(sys::thread_info(t).a1, 3, "and wrote 3 bytes");
    Ok(())
}

fn port_argument_checks() -> Outcome {
    let p = port()?;
    ensure_eq!(sys::object_signals(p).a1, 0, "a fresh port has no signals");
    let e = event()?;
    let (a, _b) = socket()?;
    let m = 1u32 << 24;
    ensure_eq!(
        st(sys::port_bind(BAD, e, m, 1)),
        Some(S::BadHandle),
        "bad port"
    );
    ensure_eq!(
        st(sys::port_bind(p, BAD, m, 1)),
        Some(S::BadHandle),
        "bad object"
    );
    ensure_eq!(
        st(sys::port_bind(e, e, m, 1)),
        Some(S::WrongType),
        "an event is no port"
    );
    let nomanage = weaker(p, Rights::WAIT.0)?;
    ensure_eq!(
        st(sys::port_bind(nomanage, e, m, 1)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    let nowait = weaker(e, Rights::SIGNAL.0)?;
    ensure_eq!(
        st(sys::port_bind(p, nowait, m, 1)),
        Some(S::AccessDenied),
        "object without WAIT"
    );
    ensure_eq!(
        st(sys::port_bind(p, e, 0, 1)),
        Some(S::InvalidArgs),
        "empty mask"
    );
    ensure_eq!(
        st(sys::port_bind(p, e, 1 << 10, 1)),
        Some(S::InvalidArgs),
        "not a signal"
    );
    ensure_eq!(
        st(sys::port_bind(p, p, sig::READABLE, 1)),
        Some(S::InvalidArgs),
        "a port in a port"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::PORT_BIND,
            [p as usize, e as usize, 1 << 32, 1, 0, 0]
        )),
        Some(S::InvalidArgs),
        "a mask wider than 32 bits"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::PORT_BIND,
            [(1usize << 32) | p as usize, e as usize, m as usize, 1, 0, 0]
        )),
        Some(S::BadHandle),
        "a wide port handle"
    );
    ensure_eq!(
        st(sys::port_bind(p, e, m, 1)),
        Some(S::Ok),
        "a good binding"
    );
    ensure_eq!(
        st(sys::port_bind(p, a, sig::READABLE, 1)),
        Some(S::InvalidArgs),
        "key already bound"
    );
    ensure_eq!(
        st(sys::port_unbind(p, 9)),
        Some(S::InvalidArgs),
        "unknown key"
    );
    ensure_eq!(
        st(sys::port_unbind(nomanage, 1)),
        Some(S::AccessDenied),
        "unbind needs MANAGE"
    );
    ensure_eq!(
        st(sys::port_unbind(e, 1)),
        Some(S::WrongType),
        "unbind on an event"
    );
    ensure_eq!(
        st(sys::port_wait(BAD, 0)),
        Some(S::BadHandle),
        "wait: bad handle"
    );
    ensure_eq!(
        st(sys::port_wait(e, 0)),
        Some(S::WrongType),
        "wait: not a port"
    );
    let nowait_port = weaker(p, Rights::MANAGE.0)?;
    ensure_eq!(
        st(sys::port_wait(nowait_port, 0)),
        Some(S::AccessDenied),
        "wait needs WAIT"
    );
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::PORT_WAIT,
            [(1usize << 32) | p as usize, 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "wide handle"
    );
    ensure_eq!(st(sys::port_unbind(p, 1)), Some(S::Ok), "unbind");
    // the binding limit
    for k in 0..256u64 {
        ensure_eq!(st(sys::port_bind(p, e, m, k)), Some(S::Ok), "binding {k}");
    }
    ensure_eq!(
        st(sys::port_bind(p, e, m, 1000)),
        Some(S::NoMemory),
        "257th binding"
    );
    Ok(())
}

fn port_wait_levels_timeouts_and_fairness() -> Outcome {
    let p = port()?;
    let (e1, e2) = (event()?, event()?);
    let m = 1u32 << 24;
    sys::port_bind(p, e1, m, 11);
    sys::port_bind(p, e2, m, 22);
    let t0 = sys::clock_get();
    ensure_eq!(
        st(sys::port_wait(p, 0)),
        Some(S::TimedOut),
        "a poll with nothing ready"
    );
    ensure_eq!(
        st(sys::port_wait(p, sys::clock_get() + 4)),
        Some(S::TimedOut),
        "a short wait times out"
    );
    ensure!(
        within(sys::clock_get() - t0, 4, 10),
        "the timeout took {} ticks",
        sys::clock_get() - t0
    );
    sys::event_signal(e1, 0, m);
    sys::event_signal(e2, 0, m);
    ensure_eq!(
        sys::object_signals(p).a1 as u32,
        sig::READABLE,
        "the port is READABLE"
    );
    let mut seen = [0u32; 2];
    for _ in 0..6 {
        let r = sys::port_wait(p, 0);
        ensure_eq!(r.status, 0, "a ready binding");
        ensure_eq!(r.a2 as u32, m, "observed is signals & mask");
        match r.a1 {
            11 => seen[0] += 1,
            22 => seen[1] += 1,
            k => ensure!(false, "unexpected key {k}"),
        }
    }
    ensure_eq!(seen, [3, 3], "both ready bindings are served in turn");
    sys::event_signal(e1, m, 0);
    for _ in 0..3 {
        ensure_eq!(sys::port_wait(p, 0).a1, 22, "only the one still asserted");
    }
    sys::event_signal(e2, m, 0);
    ensure_eq!(
        st(sys::port_wait(p, 0)),
        Some(S::TimedOut),
        "deasserted: nothing ready"
    );
    // closing the object drops its binding
    sys::event_signal(e1, 0, m);
    sys::handle_close(e1);
    ensure_eq!(
        st(sys::port_wait(p, 0)),
        Some(S::TimedOut),
        "the destroyed object's binding is gone"
    );
    ensure_eq!(
        st(sys::port_bind(p, e2, m, 11)),
        Some(S::Ok),
        "and its key is free"
    );
    Ok(())
}

fn port_wait_blocks_until_an_object_is_ready() -> Outcome {
    let p = port()?;
    let ev = event()?;
    sys::port_bind(p, ev, 1 << 24, 77);
    let t = new_thread(worker_signal_later, ev as usize)?;
    let t0 = sys::clock_get();
    let r = sys::port_wait(p, sys::clock_get() + 500);
    ensure_eq!((r.status, r.a1), (0, 77), "woken with the key");
    ensure!(
        within(sys::clock_get() - t0, 5, 12),
        "after about 6 ticks, not {}",
        sys::clock_get() - t0
    );
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    Ok(())
}

fn port_wait_sees_socket_data_and_object_wait_works_on_a_port() -> Outcome {
    let p = port()?;
    let (a, b) = socket()?;
    sys::port_bind(p, b, sig::READABLE | sig::PEER_CLOSED, 5);
    let t = new_thread(worker_write_later, a as usize)?;
    let r = sys::object_wait(p, sig::READABLE, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "object_wait on the port itself");
    let r = sys::port_wait(p, 0);
    ensure_eq!(
        (r.status, r.a1, r.a2 as u32),
        (0, 5, sig::READABLE),
        "the socket is readable"
    );
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    sys::handle_close(a);
    let r = sys::port_wait(p, 0);
    ensure_eq!(
        r.a2 as u32,
        sig::READABLE | sig::PEER_CLOSED,
        "peer closed too"
    );
    Ok(())
}

fn kill_a_child_blocked_in_port_wait() -> Outcome {
    let (p, _m) = launch(27)?;
    let r = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 5);
    ensure_eq!(st(r), Some(S::TimedOut), "the child waits on its port");
    ensure_eq!(st(sys::process_kill(p, 31)), Some(S::Ok), "kill it");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 31, "exit code");
    Ok(())
}

// Regression tests from the independent review of S3c-3.

fn socket_memory_is_bounded() -> Outcome {
    let chunk = [7u8; SOCKET_BUFFER];
    let mut pairs = [(0u32, 0u32); 64];
    let mut n = 0;
    let mut hit_the_limit = false;
    while n < pairs.len() {
        let (a, b) = socket()?;
        pairs[n] = (a, b);
        n += 1;
        let r = sys::socket_write(a, chunk.as_ptr(), chunk.len());
        if r.status != 0 {
            ensure_eq!(
                st(r),
                Some(S::NoMemory),
                "the system-wide budget is NO_MEMORY"
            );
            hit_the_limit = true;
            break;
        }
    }
    ensure!(hit_the_limit, "300 sockets of 64 KiB were accepted");
    // closing the sockets gives the budget back
    for &(a, b) in &pairs[..n] {
        sys::handle_close(a);
        sys::handle_close(b);
    }
    let (a, _b) = socket()?;
    ensure_eq!(
        sys::socket_write(a, chunk.as_ptr(), chunk.len()).status,
        0,
        "room again"
    );
    Ok(())
}

fn dead_port_bindings_vanish_at_once() -> Outcome {
    let p = port()?;
    let m = sig::READABLE | sig::PEER_CLOSED;
    let (a, b) = socket()?;
    ensure_eq!(st(sys::port_bind(p, b, m, 1)), Some(S::Ok), "bind");
    sys::handle_close(b);
    sys::handle_close(a);
    ensure_eq!(
        st(sys::port_unbind(p, 1)),
        Some(S::InvalidArgs),
        "unbind of a vanished binding"
    );
    for k in 100..356u64 {
        let (x, y) = socket()?;
        sys::port_bind(p, y, m, k);
        sys::handle_close(y);
        sys::handle_close(x);
    }
    let (_x, y) = socket()?;
    ensure_eq!(
        st(sys::port_bind(p, y, m, 9999)),
        Some(S::Ok),
        "a port full of dead bindings"
    );
    let e = event()?;
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::PORT_BIND,
            [e as usize, e as usize, 1 << 32, 1, 0, 0]
        )),
        Some(S::WrongType),
        "a wide mask comes after the type check"
    );
    Ok(())
}

// Regression tests from the trust-boundary audit (kernel memory caps must end in NO_MEMORY,
// never a kernel panic).

/// Park events 64 at a time in channel messages until something refuses: the refusal must be
/// NO_MEMORY, and everything must work again once the channels are closed.
fn the_object_cap_is_reached_without_a_kernel_panic() -> Outcome {
    let mut chans = [0u32; 10];
    for i in 0..5 {
        let (a, b) = channel()?;
        chans[2 * i] = a;
        chans[2 * i + 1] = b;
    }
    let mut made = 0u32;
    let mut refused = None;
    'outer: for &ch in chans.iter() {
        for _ in 0..128 {
            let mut hs = [0u32; 64];
            let mut got = 0;
            for h in hs.iter_mut() {
                let r = sys::event_create();
                if r.status != 0 {
                    refused = st(r);
                    break;
                }
                *h = r.a1 as u32;
                got += 1;
                made += 1;
            }
            if got > 0 {
                let w = sys::channel_write(ch, core::ptr::null(), 0, hs.as_ptr(), got);
                if w.status != 0 {
                    refused = st(w);
                    break 'outer;
                }
            }
            if refused.is_some() {
                break 'outer;
            }
        }
    }
    ensure_eq!(
        refused,
        Some(S::NoMemory),
        "after {made} events the refusal is NO_MEMORY"
    );
    for ch in chans {
        sys::handle_close(ch);
    }
    ensure!(
        event().is_ok(),
        "objects can be made again after the channels are closed"
    );
    Ok(())
}

fn mappings_per_process_are_bounded() -> Outcome {
    let me = me()?;
    let v = vmo(1)?;
    let base = 0x10_0000_0000u64;
    let mut n = 0u64;
    let mut refused = None;
    while n < 3000 {
        let r = sys::process_map(me, v, base + n * 8192, 0, 4096, PF_R);
        if r.status != 0 {
            refused = st(r);
            break;
        }
        n += 1;
    }
    ensure_eq!(
        refused,
        Some(S::NoMemory),
        "a limit on regions (reached at {n})"
    );
    ensure!(n >= 512, "the limit is not absurdly low ({n})");
    for i in 0..n {
        sys::process_unmap(me, base + i * 8192);
    }
    ensure_eq!(
        st(sys::process_map(me, v, base, 0, 4096, PF_R)),
        Some(S::Ok),
        "room again"
    );
    Ok(())
}

/// Queued handles are kernel memory: they are charged to the IPC budget.
fn queued_handles_are_charged_to_the_budget() -> Outcome {
    let e = event()?;
    let mut sent = 0u32;
    let mut refused = None;
    let mut chans = [0u32; 40];
    let mut nch = 0;
    'outer: for _ in 0..20 {
        let (a, b) = channel()?;
        chans[nch] = a;
        chans[nch + 1] = b;
        nch += 2;
        for side in [a, b] {
            for _ in 0..128 {
                let mut hs = [0u32; 64];
                for h in hs.iter_mut() {
                    let r = sys::handle_duplicate(e, Rights::SAME.0);
                    if r.status != 0 {
                        refused = st(r);
                        break 'outer;
                    }
                    *h = r.a1 as u32;
                }
                let w = sys::channel_write(side, core::ptr::null(), 0, hs.as_ptr(), 64);
                if w.status != 0 {
                    refused = st(w);
                    break 'outer;
                }
                sent += 1;
            }
        }
    }
    ensure!(
        matches!(refused, Some(S::NoMemory) | Some(S::ShouldWait)),
        "refused after {sent} messages with {refused:?}"
    );
    ensure!(sent < 4000, "{sent} messages of 64 handles were accepted");
    for &c in &chans[..nch] {
        sys::handle_close(c);
    }
    let (a, _b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"ok".as_ptr(),
            2,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "usable again"
    );
    Ok(())
}

// --- The KILL right (S4a) ---------------------------------------------------------------------

fn kill_needs_the_kill_right_and_nothing_else() -> Outcome {
    let (p, _m) = launch(15)?;
    // MANAGE (and everything but KILL) is not enough to end a process
    let all_but_kill = weaker(p, Rights::PROCESS.0 & !Rights::KILL.0)?;
    ensure_eq!(
        st(sys::process_kill(all_but_kill, 9)),
        Some(S::AccessDenied),
        "kill needs KILL, not MANAGE"
    );
    // KILL alone cannot do what MANAGE does: no mapping, no new threads
    let kill_only = weaker(p, Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(kill_only, v, 0x7_0000, 0, 4096, PF_R)),
        Some(S::AccessDenied),
        "KILL does not allow mapping into the process"
    );
    ensure_eq!(
        st(sys::thread_create(
            kill_only,
            child_entry()?,
            0x20_0000_0000 + 0x4000,
            0
        )),
        Some(S::AccessDenied),
        "KILL does not allow adding threads"
    );
    // ... and it can be handed to another process (here: sent through a channel and back)
    let (a, b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"k".as_ptr(),
            1,
            [kill_only].as_ptr(),
            1
        )),
        Some(S::Ok),
        "a KILL|WAIT|TRANSFER handle can be sent"
    );
    let mut hb = [0u32; 1];
    let mut mb = [0u8; 4];
    let r = sys::channel_read(b, mb.as_mut_ptr(), 4, hb.as_mut_ptr(), 1);
    ensure_eq!((r.status, r.a2), (0, 1), "received it");
    ensure_eq!(
        st(sys::process_info(hb[0])),
        Some(S::AccessDenied),
        "no INSPECT on the narrowed handle"
    );
    ensure_eq!(
        st(sys::process_kill(hb[0], 9)),
        Some(S::Ok),
        "the KILL-only handle kills"
    );
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 9, "with the code it gave");
    // process_self handles carry no KILL: a process ends itself with process_exit
    let me = me()?;
    ensure_eq!(
        st(sys::process_kill(me, 1)),
        Some(S::AccessDenied),
        "no KILL on a self handle"
    );
    Ok(())
}

// --- The S4a runtime: startup message, heap, stdio -------------------------------------------

use proto::startup::{Role, Startup};

fn fetch_the_hello_image() -> Outcome {
    let (m, n) = request(22, b'H', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u32; 2];
    let args = CallArgs {
        req_bytes: m.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: n as u32,
        req_nhandles: 0,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: got.as_mut_ptr() as u64,
        rep_max_bytes: 16,
        rep_max_handles: 2,
        deadline: sys::clock_get() + 500,
    };
    let r = sys::channel_call(BOOTSTRAP, &args);
    ensure_eq!((r.status, r.a2), (0, 1), "the reply carries the image");
    HELLO_IMAGE.store(got[0], Ordering::Relaxed);
    Ok(())
}

fn send_startup(mine: u32, s: &Startup, handles: &[u32]) -> Outcome {
    let bytes = s.encode().map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("encode: {e:?}"));
        d
    })?;
    ensure_eq!(
        st(sys::channel_write(
            mine,
            bytes.as_ptr(),
            bytes.len(),
            handles.as_ptr(),
            handles.len()
        )),
        Some(S::Ok),
        "send the startup message"
    );
    Ok(())
}

/// Start `hello` with `args`, a stdin pipe and a stdout pipe; returns (process, our write end of
/// stdin, our read end of stdout).
fn start_hello(args: &[&[u8]]) -> Result<(u32, u32, u32), Detail> {
    let (p, mine) = launch_image(HELLO_IMAGE.load(Ordering::Relaxed), 0)?;
    let (in_w, in_r) = socket()?;
    let (out_r, out_w) = socket()?;
    // Narrow the ends we give away, then close our originals: the stream only ends for the
    // reader when every handle to the other end is gone.
    let in_r_child = weaker(in_r, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    let out_w_child = weaker(out_w, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    sys::handle_close(in_r);
    sys::handle_close(out_w);
    let s = Startup {
        args: args.iter().map(|a| a.to_vec()).collect(),
        env: vec![],
        namespace: vec![],
        roles: vec![(Role::Stdin, 0), (Role::Stdout, 1)],
    };
    send_startup(mine, &s, &[in_r_child, out_w_child])?;
    Ok((p, in_w, out_r))
}

/// Read a socket to the end of the stream.
fn read_to_end(h: u32) -> Result<Vec<u8>, Detail> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let r = sys::socket_read(h, buf.as_mut_ptr(), buf.len());
        match st(r) {
            Some(S::Ok) => out.extend_from_slice(&buf[..r.a1]),
            Some(S::PeerClosed) => return Ok(out),
            Some(S::ShouldWait) => {
                let w = sys::object_wait(
                    h,
                    sig::READABLE | sig::PEER_CLOSED,
                    sys::clock_get() + 10_000,
                );
                ensure!(w.status == 0, "the program stopped talking");
            }
            other => ensure!(false, "read: {other:?}"),
        }
    }
}

fn runtime_startup_heap_and_stdio() -> Outcome {
    let (p, in_w, out_r) = start_hello(&[b"hello", b"world"])?;
    ensure_eq!(
        sys::socket_write(in_w, b"abc".as_ptr(), 3).status,
        0,
        "feed its stdin"
    );
    ensure_eq!(st(sys::handle_close(in_w)), Some(S::Ok), "end of its stdin");
    let out = read_to_end(out_r)?;
    ensure_eq!(&out[..], &b"args=hello world\nabc"[..], "what it printed");
    wait_ended(p, 1000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        2,
        "exit code: the number of arguments"
    );
    Ok(())
}

fn runtime_heap_stops_at_its_cap() -> Outcome {
    let (p, in_w, out_r) = start_hello(&[b"hello", b"hog"])?;
    let _ = out_r;
    wait_ended(p, 30_000)?;
    let blocks = sys::process_info(p).a1 as i64;
    ensure!(
        (10..=16).contains(&blocks),
        "the heap gave {blocks} MiB before refusing (cap 16 MiB, some in use)"
    );
    let _ = in_w;
    Ok(())
}

fn runtime_refuses_a_bad_startup_message() -> Outcome {
    let (p, mine) = launch_image(HELLO_IMAGE.load(Ordering::Relaxed), 0)?;
    let ev = event()?;
    ensure_eq!(
        st(sys::channel_write(
            mine,
            b"nonsense".as_ptr(),
            8,
            [ev].as_ptr(),
            1
        )),
        Some(S::Ok),
        "send garbage with a handle"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        -2001,
        "the exit code for a malformed message"
    );
    Ok(())
}

fn runtime_without_a_startup_message() -> Outcome {
    let (p, mine) = launch_image(HELLO_IMAGE.load(Ordering::Relaxed), 0)?;
    ensure_eq!(
        st(sys::handle_close(mine)),
        Some(S::Ok),
        "close our end: no message will come"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        -2002,
        "the exit code when nothing arrives"
    );
    Ok(())
}

fn runtime_io_fails_instead_of_spinning_without_wait() -> Outcome {
    // A pipe end without WAIT cannot be waited on: a blocked read must fail with the kernel's
    // error, not loop (found by the S4a review: it spun at full speed).
    let (a, b) = socket()?;
    let _ = a;
    let ro = weaker(b, Rights::READ.0)?;
    let t0 = sys::clock_get();
    let mut buf = [0u8; 4];
    let r = rt::io::read(ro, &mut buf);
    ensure_eq!(
        r,
        Err(rt::io::IoError::Other(S::AccessDenied.code())),
        "read without WAIT"
    );
    ensure!(sys::clock_get() - t0 < 50, "it must fail at once, not wait");
    let wo = weaker(a, Rights::WRITE.0)?;
    let full = alloc::vec![0u8; SOCKET_BUFFER];
    ensure_eq!(rt::io::write_all(wo, &full), Ok(()), "fill the buffer");
    ensure_eq!(
        rt::io::write_all(wo, b"more"),
        Err(rt::io::IoError::Other(S::AccessDenied.code())),
        "write without WAIT on a full socket"
    );
    // a write larger than one socket buffer is split by the helper
    let (c, d) = socket()?;
    let big = alloc::vec![7u8; 70_000];
    let reader = new_thread(drain_for_big_write, d as usize)?;
    ensure_eq!(rt::io::write_all(c, &big), Ok(()), "a 70000-byte write_all");
    let r = sys::object_wait(reader, sig::TERMINATED, sys::clock_get() + 5000);
    ensure_eq!(r.status, 0, "the reader finished");
    ensure_eq!(sys::thread_info(reader).a1, 70_000, "and saw every byte");
    Ok(())
}

extern "C" fn drain_for_big_write(arg: usize) -> ! {
    let mut total = 0usize;
    let mut buf = [0u8; 4096];
    while total < 70_000 {
        match rt::io::read(arg as u32, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    sys::thread_exit(total as i32)
}

/// A failure detail that names the source line (for `?` on an `Option` or an opaque error).
/// A failure detail for a protocol status that came back where it should not have.
fn msg_e(status: i32, line: u32) -> Detail {
    let mut d = Detail::new();
    let _ = core::fmt::Write::write_fmt(
        &mut d,
        format_args!("unexpected status {status} at suite.rs:{line}"),
    );
    d
}

fn msg_at(line: u32) -> Detail {
    let mut d = Detail::new();
    let _ = core::fmt::Write::write_fmt(
        &mut d,
        format_args!("unexpected None/Err at suite.rs:{line}"),
    );
    d
}

// --- The launcher service (S4b) ----------------------------------------------------------------

use proto::bootimg;
use proto::launch;

/// A read-only VMO of the boot image, from the kernel-side helper.
/// The boot image VMO, fetched once: the kernel makes a fresh copy of the whole image (about a
/// thousand pages) for every request, so asking in every test left a dozen copies behind and
/// the suite ran the machine out of memory on a runner whose image was a little larger.
fn boot_image_vmo() -> Result<u32, Detail> {
    static CACHED: AtomicU32 = AtomicU32::new(0);
    let have = CACHED.load(Ordering::Relaxed);
    if have != 0 {
        return Ok(have);
    }
    let h = fetch_boot_image_vmo()?;
    CACHED.store(h, Ordering::Relaxed);
    Ok(h)
}

fn fetch_boot_image_vmo() -> Result<u32, Detail> {
    let (m, n) = request(23, b'B', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u32; 2];
    let args = CallArgs {
        req_bytes: m.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: n as u32,
        req_nhandles: 0,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: got.as_mut_ptr() as u64,
        rep_max_bytes: 16,
        rep_max_handles: 2,
        deadline: sys::clock_get() + 500,
    };
    let r = sys::channel_call(BOOTSTRAP, &args);
    ensure_eq!((r.status, r.a2), (0, 1), "the reply carries the boot image");
    Ok(got[0])
}

/// The whole VMO into memory. (`rt::spawn::read_image` refuses more than one program image's
/// worth, 4 MiB; the whole boot image may be larger.)
fn read_whole_vmo(vmo: u32, size: u64) -> Option<Vec<u8>> {
    let mut img: Vec<u8> = Vec::new();
    img.try_reserve_exact(size as usize).ok()?;
    img.resize(size as usize, 0);
    for (i, chunk) in img.chunks_mut(32 * 1024).enumerate() {
        let r = sys::vmo_read(vmo, (i * 32 * 1024) as u64, chunk.as_mut_ptr(), chunk.len());
        if r.status != 0 {
            return None;
        }
    }
    Some(img)
}

/// (offset, length) of a boot image entry.
fn image_entry(vmo: u32, name: &[u8]) -> Result<(u64, u64), Detail> {
    let size = sys::vmo_get_size(vmo).a1 as u64;
    let img = read_whole_vmo(vmo, size).ok_or_else(|| {
        let mut d = Detail::new();
        let _ =
            core::fmt::Write::write_fmt(&mut d, format_args!("read the boot image (size {size})"));
        d
    })?;
    let entries = bootimg::parse(&img).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("parse: {e:?}"));
        d
    })?;
    let e = entries.iter().find(|e| e.name == name);
    ensure!(e.is_some(), "no entry {:?}", core::str::from_utf8(name));
    let e = e.unwrap();
    Ok((u64::from(e.offset), u64::from(e.size)))
}

/// Start the launcher from the boot image through `rt::spawn`: (its process handle, our end of
/// its service channel, the boot image VMO).
fn start_launcher() -> Result<(u32, u32, u32), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/launcher")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let s = Startup {
        args: vec![b"launcher".to_vec()],
        roles: vec![(Role::ServiceLaunch, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[theirs]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn the launcher: {e:?}"));
        d
    })?;
    Ok((p, mine, bi))
}

/// Ask the launcher to spawn the image at (offset, len) of `bi`, with `startup` and `forward`
/// handles. Returns (status, process handle if any).
fn ask_launcher(
    ch: u32,
    bi: u32,
    txid: u32,
    off: u64,
    len: u64,
    startup: &[u8],
    forward: &[u32],
) -> Result<(i32, Option<u32>), Detail> {
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u32;
    // Built by hand: the encoder refuses the invalid extents some tests need to send.
    let mut req = Vec::new();
    req.extend_from_slice(&txid.to_le_bytes());
    req.extend_from_slice(&1u16.to_le_bytes());
    req.extend_from_slice(&0u16.to_le_bytes());
    req.extend_from_slice(&off.to_le_bytes());
    req.extend_from_slice(&len.to_le_bytes());
    req.extend_from_slice(&(startup.len() as u32).to_le_bytes());
    req.extend_from_slice(startup);
    let mut hs = vec![image_dup];
    hs.extend_from_slice(forward);
    ensure_eq!(
        st(sys::channel_write(
            ch,
            req.as_ptr(),
            req.len(),
            hs.as_ptr(),
            hs.len()
        )),
        Some(S::Ok),
        "send the request"
    );
    let w = sys::object_wait(
        ch,
        sig::READABLE | sig::PEER_CLOSED,
        sys::clock_get() + 10_000,
    );
    ensure_eq!(w.status, 0, "the launcher answered");
    let mut buf = [0u8; 16];
    let mut h = [0u32; 1];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), 16, h.as_mut_ptr(), 1);
    ensure_eq!(r.status, 0, "read the reply");
    let (id, status) = launch::decode_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(id, txid, "the reply is for this request");
    Ok((status, if r.a2 == 1 { Some(h[0]) } else { None }))
}

fn hello_startup(args: &[&[u8]]) -> Result<(Vec<u8>, u32, u32, u32, u32), Detail> {
    // returns (startup bytes, in_w (ours), in_r_child, out_r (ours), out_w_child)
    let (in_w, in_r) = socket()?;
    let (out_r, out_w) = socket()?;
    let in_r_child = weaker(in_r, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    let out_w_child = weaker(out_w, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    sys::handle_close(in_r);
    sys::handle_close(out_w);
    let s = Startup {
        args: args.iter().map(|a| a.to_vec()).collect(),
        roles: vec![(Role::Stdin, 0), (Role::Stdout, 1)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    Ok((bytes, in_w, in_r_child, out_r, out_w_child))
}

fn launcher_spawns_a_program_from_the_boot_image() -> Outcome {
    let (_lp, ch, bi) = start_launcher()?;
    let (off, len) = image_entry(bi, b"bin/hello")?;
    let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"hello", b"x"])?;
    let (status, proc) = ask_launcher(ch, bi, 1, off, len, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, 0, "spawn status");
    let proc = proc.ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(
        sys::socket_write(in_w, b"abc".as_ptr(), 3).status,
        0,
        "feed its stdin"
    );
    sys::handle_close(in_w);
    let out = read_to_end(out_r)?;
    ensure_eq!(&out[..], &b"args=hello x\nabc"[..], "what it printed");
    wait_ended(proc, 2000)?;
    ensure_eq!(
        sys::process_info(proc).a1 as i64,
        2,
        "exit code: the number of arguments"
    );
    // a second spawn through the same launcher
    let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"hello"])?;
    let (status, proc) = ask_launcher(ch, bi, 2, off, len, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, 0, "second spawn");
    sys::handle_close(in_w);
    ensure_eq!(
        &read_to_end(out_r)?[..],
        &b"args=hello\n"[..],
        "and its output"
    );
    wait_ended(proc.ok_or_else(|| msg_at(line!()))?, 2000)?;
    Ok(())
}

fn the_process_handle_the_launcher_returns_cannot_manage() -> Outcome {
    let (_lp, ch, bi) = start_launcher()?;
    let (off, len) = image_entry(bi, b"bin/hello")?;
    let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"hello"])?;
    let (status, proc) = ask_launcher(ch, bi, 1, off, len, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, 0, "spawn");
    let proc = proc.ok_or_else(|| msg_at(line!()))?;
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(proc, v, 0x7_0000, 0, 4096, PF_R)),
        Some(S::AccessDenied),
        "no MANAGE: the caller cannot map into the child"
    );
    ensure_eq!(
        st(sys::thread_create(proc, 0x10000, 0x20_0000_0000, 0)),
        Some(S::AccessDenied),
        "nor add threads"
    );
    // the child waits for stdin; the caller can still stop it (KILL) and see it end (WAIT, INSPECT)
    let _ = (in_w, out_r);
    ensure_eq!(st(sys::process_kill(proc, 55)), Some(S::Ok), "KILL works");
    wait_ended(proc, 2000)?;
    ensure_eq!(sys::process_info(proc).a1 as i64, 55, "its exit code");
    Ok(())
}

fn the_launcher_refuses_bad_images_and_requests_without_leaks() -> Outcome {
    let (lp, ch, bi) = start_launcher()?;
    let (hoff, hlen) = image_entry(bi, b"bin/hello")?;
    // not an ELF: a fixture text file
    let (foff, flen) = image_entry(bi, b"home/hello.txt")?;
    let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"x"])?;
    let (status, proc) = ask_launcher(ch, bi, 1, foff, flen, &bytes, &[in_r, out_w])?;
    ensure_eq!(
        (status, proc),
        (proto::file::status::BAD_ELF, None),
        "a text file is BAD_ELF"
    );
    // the forwarded handles were closed: our ends now see the peers gone
    let w = sys::object_wait(in_w, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(w.status, 0, "the stdin end was released");
    let w = sys::object_wait(out_r, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(w.status, 0, "the stdout end was released");
    // extents outside the VMO, or empty
    let size = sys::vmo_get_size(bi).a1 as u64;
    let (bytes, _a, in_r, _b, out_w) = hello_startup(&[b"x"])?;
    let (status, _) = ask_launcher(ch, bi, 2, size - 4, 100, &bytes, &[in_r, out_w])?;
    ensure_eq!(
        status,
        proto::file::status::BAD_REQUEST,
        "an extent past the end of the VMO"
    );
    let (bytes, _a, in_r, _b, out_w) = hello_startup(&[b"x"])?;
    let (status, _) = ask_launcher(ch, bi, 3, hoff, 0, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, proto::file::status::BAD_REQUEST, "an empty image");
    // a startup message that does not name every handle that came with it
    let (bytes, _a, in_r, _b, out_w) = hello_startup(&[b"x"])?;
    let extra = event()?;
    let (status, _) = ask_launcher(ch, bi, 4, hoff, hlen, &bytes, &[in_r, out_w, extra])?;
    ensure_eq!(
        status,
        proto::file::status::BAD_REQUEST,
        "an unreferenced handle"
    );
    // garbage bytes
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup")?.a1 as u32;
    ensure_eq!(
        st(sys::channel_write(
            ch,
            b"garbage garbage garbage garbage".as_ptr(),
            31,
            [image_dup].as_ptr(),
            1
        )),
        Some(S::Ok),
        "send garbage"
    );
    let w = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(w.status, 0, "the launcher answers garbage too");
    let mut buf = [0u8; 16];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    ensure_eq!(r.status, 0, "a reply");
    let (_, status) = launch::decode_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(
        status,
        proto::file::status::BAD_REQUEST,
        "garbage is BAD_REQUEST"
    );
    // a message larger than the launcher reads is a protocol violation: it drops the client
    let big = alloc::vec![0u8; 20 * 1024];
    ensure_eq!(
        st(sys::channel_write(
            ch,
            big.as_ptr(),
            big.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send an oversized request"
    );
    wait_ended(lp, 5000)?;
    ensure_eq!(
        sys::process_info(lp).a1 as i64,
        0,
        "the launcher ended cleanly"
    );
    ensure!(
        sys::object_signals(ch).a1 as u32 & sig::PEER_CLOSED != 0,
        "and our channel to it is closed"
    );
    Ok(())
}

fn many_launcher_spawns_do_not_leak() -> Outcome {
    let (_lp, ch, bi) = start_launcher()?;
    let (off, len) = image_entry(bi, b"bin/hello")?;
    for i in 0..12u32 {
        let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"hello"])?;
        let (status, proc) = ask_launcher(ch, bi, 10 + i, off, len, &bytes, &[in_r, out_w])?;
        ensure_eq!(status, 0, "spawn {i}");
        sys::handle_close(in_w);
        ensure_eq!(&read_to_end(out_r)?[..], &b"args=hello\n"[..], "output {i}");
        wait_ended(proc.ok_or_else(|| msg_at(line!()))?, 2000)?;
    }
    // a failing spawn in the middle must not disturb later ones
    let (foff, flen) = image_entry(bi, b"home/lines.txt")?;
    let (bytes, _a, in_r, _b, out_w) = hello_startup(&[b"x"])?;
    let (status, _) = ask_launcher(ch, bi, 99, foff, flen, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, proto::file::status::BAD_ELF, "bad image");
    Ok(())
}

// --- The memory filesystem server (S4c) --------------------------------------------------------

use proto::file::{flags as pfl, op as pop, rights as prt, status as pst};
use rt::fs::Fs;

const RW_DIR: u32 = prt::READ | prt::ENUMERATE | prt::CREATE | prt::WRITE;
const RO_DIR: u32 = prt::READ | prt::ENUMERATE;
const DENIED: i32 = -3; // the kernel's ACCESS_DENIED, which memfsd uses for rights and read-only
const TOO_BIG: i32 = -9;

fn code<T>(r: Result<T, i32>) -> Option<i32> {
    r.err()
}

/// Start memfsd from the boot image: (process handle, a root connection with every right, the
/// boot image VMO).
fn start_memfsd() -> Result<(u32, Fs, u32), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/memfsd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup")?.a1 as u32;
    let s = Startup {
        args: vec![b"memfsd".to_vec()],
        roles: vec![(Role::ServiceFs, 0), (Role::BootImage, 1)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[theirs, image_dup]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn memfsd: {e:?}"));
        d
    })?;
    Ok((p, Fs::new(mine), bi))
}

fn names(es: &[proto::file::DirEntry]) -> Vec<Vec<u8>> {
    es.iter().map(|e| e.name.clone()).collect()
}

fn memfsd_serves_the_boot_image() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let top = root.read_dir().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&top),
        vec![
            b"bin".to_vec(),
            b"etc".to_vec(),
            b"home".to_vec(),
            b"tmp".to_vec()
        ],
        "the top level"
    );
    ensure!(top[0].is_dir && top[3].is_dir, "they are directories");
    let mut bin = root
        .open(b"bin", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let progs = names(&bin.read_dir().map_err(|e| msg_e(e, line!()))?);
    ensure_eq!(
        progs,
        vec![
            b"bomb".to_vec(),
            b"cat".to_vec(),
            b"consoled".to_vec(),
            b"crash".to_vec(),
            b"echo".to_vec(),
            b"hello".to_vec(),
            b"hog".to_vec(),
            b"init".to_vec(),
            b"ipchog".to_vec(),
            b"launcher".to_vec(),
            b"ls".to_vec(),
            b"memfsd".to_vec(),
            b"probe".to_vec(),
            b"sh".to_vec(),
            b"spin".to_vec()
        ],
        "the programs"
    );
    let mut f = root
        .open(b"home/hello.txt", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        f.read_all().map_err(|e| msg_e(e, line!()))?,
        b"hello, world\n".to_vec(),
        "a fixture"
    );
    ensure_eq!(f.size(), Ok(13), "its size");
    ensure_eq!(
        f.read(7, 100),
        Ok(b"world\n".to_vec()),
        "a read from the middle"
    );
    ensure_eq!(f.read(13, 10), Ok(vec![]), "a read at the end");
    ensure_eq!(f.read(u64::MAX, 10), Ok(vec![]), "a read far past the end");
    let mut g = root
        .open(b"home/sub/inner.txt", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(g.read_all(), Ok(b"inner\n".to_vec()), "a nested fixture");
    let mut t = root
        .open(b"etc/trusted", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(t.read_all(), Ok(b"ls\ncat\n".to_vec()), "the trust list");
    let mut prog = root
        .open(b"bin/hello", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    let head = prog.read(0, 4).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        head,
        b"\x7fELF".to_vec(),
        "a program reads as its ELF bytes"
    );
    ensure_eq!(
        root.stat(b"home/lines.txt"),
        Ok(rt::fs::Stat {
            is_dir: false,
            size: 14
        }),
        "stat by path"
    );
    ensure_eq!(
        root.stat(b"home/sub"),
        Ok(rt::fs::Stat {
            is_dir: true,
            size: 1
        }),
        "stat a directory"
    );
    ensure_eq!(
        code(root.stat(b"home/nope")),
        Some(pst::NOT_FOUND),
        "stat of a missing path"
    );
    Ok(())
}

fn memfsd_tmp_is_the_only_writable_place() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let mut f = tmp
        .open(b"note", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(f.write(0, b"hello"), Ok(5), "write");
    ensure_eq!(f.read_all(), Ok(b"hello".to_vec()), "read it back");
    ensure_eq!(f.write(8, b"!"), Ok(1), "a write past the end");
    ensure_eq!(
        f.read_all(),
        Ok(b"hello\0\0\0!".to_vec()),
        "leaves a zero gap"
    );
    ensure_eq!(f.set_size(2), Ok(()), "truncate");
    ensure_eq!(f.read_all(), Ok(b"he".to_vec()), "and the data is cut");
    ensure_eq!(tmp.mkdir(b"d"), Ok(()), "mkdir");
    ensure_eq!(code(tmp.mkdir(b"d")), Some(pst::EXISTS), "mkdir twice");
    ensure_eq!(
        code(tmp.unlink(b"d").and(tmp.unlink(b"d"))),
        Some(pst::NOT_FOUND),
        "unlink twice"
    );
    let names_now = names(&tmp.read_dir().map_err(|e| msg_e(e, line!()))?);
    ensure_eq!(names_now, vec![b"note".to_vec()], "what is left");
    ensure_eq!(
        code(tmp.open(b"note", pfl::CREATE | pfl::EXCLUSIVE, prt::READ)),
        Some(pst::EXISTS),
        "exclusive create"
    );
    ensure_eq!(
        code(tmp.open(b"gone", 0, prt::READ)),
        Some(pst::NOT_FOUND),
        "open a missing file"
    );
    // everything else is read-only
    ensure_eq!(
        code(root.mkdir(b"x")),
        Some(DENIED),
        "no new top-level names"
    );
    let mut bin = root
        .open(b"bin", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(bin.unlink(b"hello")),
        Some(DENIED),
        "no unlink in /bin (no WRITE on this connection)"
    );
    ensure_eq!(
        code(root.open(b"bin/hello", 0, prt::READ | prt::WRITE)),
        Some(DENIED),
        "no write access to a program"
    );
    ensure_eq!(
        code(root.open(b"bin/new", pfl::CREATE, prt::READ | prt::WRITE)),
        Some(DENIED),
        "no creating in /bin"
    );
    ensure_eq!(
        code(root.open(b"home/hello.txt", 0, prt::READ | prt::WRITE)),
        Some(DENIED),
        "fixtures are read-only"
    );
    ensure_eq!(
        code(root.open(b"home", pfl::DIRECTORY, RW_DIR)),
        Some(DENIED),
        "no writable handle to /home"
    );
    ensure!(
        root.open(b"tmp", pfl::DIRECTORY, RW_DIR).is_ok(),
        "but /tmp is fine"
    );
    Ok(())
}

fn memfsd_paths_cannot_escape_a_directory_handle() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    for bad in [
        &b".."[..],
        b"/etc",
        b"a//b",
        b"home/../etc",
        b"./home",
        b"home/",
        &[b'x'; 256][..],
        b"a\0b",
    ] {
        let c = code(root.open(bad, 0, prt::READ));
        ensure!(
            c == Some(pst::BAD_REQUEST) || c == Some(pst::NAME_TOO_LONG),
            "{bad:?} gave {c:?}"
        );
    }
    let deep = vec!["a"; 33].join("/");
    ensure_eq!(
        code(root.open(deep.as_bytes(), 0, prt::READ)),
        Some(pst::BAD_REQUEST),
        "too deep"
    );
    // from a directory handle nothing outside it can be named
    let mut home = root
        .open(b"home", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(home.open(b"../etc/trusted", 0, prt::READ)),
        Some(pst::BAD_REQUEST),
        "no parent"
    );
    ensure_eq!(
        code(home.open(b"etc/trusted", 0, prt::READ)),
        Some(pst::NOT_FOUND),
        "not inside /home"
    );
    ensure_eq!(
        code(home.stat(b"..")),
        Some(pst::BAD_REQUEST),
        "stat has the same rules"
    );
    ensure!(
        home.open(b"sub/inner.txt", 0, prt::READ).is_ok(),
        "a path below it works"
    );
    // a directory needs the DIRECTORY flag, a file refuses it
    ensure_eq!(
        code(root.open(b"home", 0, prt::READ)),
        Some(pst::IS_DIRECTORY),
        "a directory without the flag"
    );
    ensure_eq!(
        code(root.open(b"home/hello.txt", pfl::DIRECTORY, prt::READ)),
        Some(pst::NOT_DIRECTORY),
        "a file with it"
    );
    Ok(())
}

fn memfsd_rights_only_shrink() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut ro = root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(ro.clone_conn(prt::ALL)),
        Some(DENIED),
        "a clone cannot ask for more"
    );
    ensure_eq!(
        code(ro.open(b"tmp", pfl::DIRECTORY, RW_DIR)),
        Some(DENIED),
        "an open cannot ask for more"
    );
    ensure_eq!(code(ro.mkdir(b"x")), Some(DENIED), "no WRITE: no mkdir");
    ensure_eq!(code(ro.unlink(b"tmp")), Some(DENIED), "no WRITE: no unlink");
    ensure_eq!(
        code(ro.open(b"tmp/f", pfl::CREATE, prt::READ)),
        Some(DENIED),
        "no CREATE"
    );
    let mut small = ro.clone_conn(prt::READ).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(small.read_dir()),
        Some(DENIED),
        "no ENUMERATE: no listing"
    );
    ensure!(
        small.open(b"home/hello.txt", 0, prt::READ).is_ok(),
        "READ is enough to open"
    );
    let mut none = ro.clone_conn(0).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(none.open(b"home", pfl::DIRECTORY, 0)),
        Some(DENIED),
        "no READ: no opening"
    );
    // a file connection with READ only cannot write, one with WRITE only cannot read
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let mut rw = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    rw.write(0, b"abc").map_err(|e| msg_e(e, line!()))?;
    let mut r = rw.clone_conn(prt::READ).map_err(|e| msg_e(e, line!()))?;
    let mut w = rw.clone_conn(prt::WRITE).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(code(r.write(0, b"x")), Some(DENIED), "read-only connection");
    ensure_eq!(
        code(r.set_size(0)),
        Some(DENIED),
        "read-only connection, set_size"
    );
    ensure_eq!(code(w.read(0, 1)), Some(DENIED), "write-only connection");
    ensure_eq!(code(w.size()), Some(DENIED), "write-only connection, size");
    ensure_eq!(w.write(3, b"d"), Ok(1), "but it writes");
    ensure_eq!(r.read_all(), Ok(b"abcd".to_vec()), "and the reader sees it");
    // a file connection cannot be given directory rights
    ensure_eq!(
        code(rw.clone_conn(prt::CREATE)),
        Some(DENIED),
        "CREATE is more than a file connection has"
    );
    Ok(())
}

fn raw_request(ch: u32, bytes: &[u8], handles: &[u32]) -> Outcome {
    ensure_eq!(
        st(sys::channel_write(
            ch,
            bytes.as_ptr(),
            bytes.len(),
            handles.as_ptr(),
            handles.len()
        )),
        Some(S::Ok),
        "send"
    );
    Ok(())
}

fn reply_status(ch: u32) -> Result<i32, Detail> {
    let w = sys::object_wait(
        ch,
        sig::READABLE | sig::PEER_CLOSED,
        sys::clock_get() + 10_000,
    );
    ensure_eq!(w.status, 0, "an answer or a close");
    let mut buf = alloc::vec![0u8; 4096];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
    if r.status != 0 {
        return Ok(i32::MIN); // closed with nothing to read
    }
    let rep = proto::file::decode_reply(&buf[..r.a1]).map_err(|_| msg_at(line!()))?;
    Ok(rep.status)
}

fn memfsd_protocol_violations() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let req = |txid: u32, opc: u16, a: u64, b: u64, payload: &[u8]| {
        proto::file::Request {
            txid,
            op: opc,
            flags: 0,
            rights: prt::READ,
            a,
            b,
            payload,
        }
        .encode()
        .unwrap()
    };
    // each case gets its own connection so a closed one does not hide the next
    let fresh = |root: &mut Fs| -> Result<Fs, Detail> {
        root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))
    };
    // unknown op
    let c = fresh(&mut root)?;
    raw_request(c.ch, &req(1, 99, 0, 0, &[]), &[])?;
    ensure_eq!(reply_status(c.ch)?, pst::BAD_REQUEST, "an unknown op");
    // a directory op on a file connection and the other way round
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let f = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    raw_request(f.ch, &req(2, pop::READDIR, 0, 0, &[]), &[])?;
    ensure_eq!(reply_status(f.ch)?, pst::NOT_DIRECTORY, "READDIR on a file");
    raw_request(tmp.ch, &req(3, pop::READ, 0, 1, &[]), &[])?;
    ensure_eq!(
        reply_status(tmp.ch)?,
        pst::IS_DIRECTORY,
        "READ on a directory"
    );
    // OPEN without its endpoint, READ with a stray handle (which the server must close)
    let c = fresh(&mut root)?;
    raw_request(c.ch, &req(4, pop::OPEN, 0, 0, b"home"), &[])?;
    ensure_eq!(
        reply_status(c.ch)?,
        pst::BAD_REQUEST,
        "OPEN without a handle"
    );
    let (stray_mine, stray) = channel()?;
    raw_request(f.ch, &req(5, pop::READ, 0, 1, &[]), &[stray])?;
    ensure_eq!(reply_status(f.ch)?, pst::BAD_REQUEST, "READ with a handle");
    let w = sys::object_wait(stray_mine, sig::PEER_CLOSED, sys::clock_get() + 2000);
    ensure_eq!(w.status, 0, "the stray handle was closed");
    // a bad length field, bad bits, a read longer than the limit
    let mut bad = req(6, pop::READ, 0, 1, &[]);
    bad[28] = 5;
    let c = fresh(&mut root)?;
    raw_request(c.ch, &bad, &[])?;
    ensure_eq!(
        reply_status(c.ch)?,
        pst::BAD_REQUEST,
        "a lying length field"
    );
    raw_request(f.ch, &req(7, pop::READ, 0, 40_000, &[]), &[])?;
    ensure_eq!(reply_status(f.ch)?, pst::BAD_REQUEST, "a READ above 32 KiB");
    // too short to carry a txid: the connection is dropped
    let c = fresh(&mut root)?;
    raw_request(c.ch, b"xy", &[])?;
    ensure_eq!(
        reply_status(c.ch)?,
        i32::MIN,
        "a 2-byte message closes the connection"
    );
    // too many handles (a request carries at most one): dropped, all handles closed
    let c = fresh(&mut root)?;
    let (m1, h1) = channel()?;
    let (m2, h2) = channel()?;
    raw_request(c.ch, &req(8, pop::OPEN, 0, 0, b"home"), &[h1, h2])?;
    ensure_eq!(
        reply_status(c.ch)?,
        i32::MIN,
        "two handles close the connection"
    );
    for m in [m1, m2] {
        ensure_eq!(
            sys::object_wait(m, sig::PEER_CLOSED, sys::clock_get() + 2000).status,
            0,
            "handles closed"
        );
    }
    // an oversized message
    let c = fresh(&mut root)?;
    let big = alloc::vec![0u8; 40 * 1024];
    raw_request(c.ch, &big, &[])?;
    ensure_eq!(
        reply_status(c.ch)?,
        i32::MIN,
        "an oversized message closes the connection"
    );
    // the server is still fine
    ensure!(root.stat(b"home").is_ok(), "memfsd carried on");
    Ok(())
}

fn memfsd_quotas_and_connection_limit() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let chunk = alloc::vec![0xabu8; 32 * 1024];
    let mut a = tmp
        .open(b"a", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    for i in 0..128u64 {
        ensure_eq!(
            a.write(i * 32 * 1024, &chunk),
            Ok(32 * 1024),
            "fill a: chunk {i}"
        );
    }
    ensure_eq!(
        code(a.write(4 * 1024 * 1024, b"x")),
        Some(TOO_BIG),
        "a file is at most 4 MiB"
    );
    ensure_eq!(
        code(a.set_size(4 * 1024 * 1024 + 1)),
        Some(TOO_BIG),
        "set_size past the file limit"
    );
    let mut b = tmp
        .open(b"b", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    for i in 0..128u64 {
        ensure_eq!(
            b.write(i * 32 * 1024, &chunk),
            Ok(32 * 1024),
            "fill b: chunk {i}"
        );
    }
    let mut c = tmp
        .open(b"c", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(code(c.write(0, b"x")), Some(pst::QUOTA), "8 MiB in all");
    ensure_eq!(c.size(), Ok(0), "nothing was written");
    ensure_eq!(code(c.set_size(10)), Some(pst::QUOTA), "holes count");
    ensure_eq!(tmp.unlink(b"a"), Ok(()), "delete one");
    ensure_eq!(
        a.size(),
        Ok(4 * 1024 * 1024),
        "it is still open, so still there"
    );
    ensure_eq!(
        code(c.write(0, b"x")),
        Some(pst::QUOTA),
        "and still counted"
    );
    a.close();
    // the server frees a's bytes when it sees the close: give it a moment
    let mut freed = false;
    for _ in 0..50 {
        if c.write(0, b"x") == Ok(1) {
            freed = true;
            break;
        }
        rt::io::nap(10);
    }
    ensure!(freed, "closing the last connection frees its bytes");
    // connections: at most 200 live ones in all
    let mut conns: Vec<Fs> = Vec::new();
    let mut refused = None;
    for _ in 0..205 {
        match root.clone_conn(RO_DIR) {
            Ok(x) => conns.push(x),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    ensure_eq!(refused, Some(pst::QUOTA), "the connection limit");
    // 200 in all; four are ours (root, /tmp, b, c), `a` just closed (the server may not have seen
    // that yet): so 196, or 195 if it has not
    ensure!(
        conns.len() == 196 || conns.len() == 195,
        "{} clones were accepted",
        conns.len()
    );
    // a refused open must not have done anything: not even the truncation it asked for
    ensure_eq!(
        code(tmp.open(b"c", pfl::TRUNCATE, prt::READ | prt::WRITE)),
        Some(pst::QUOTA),
        "an open refused at the limit"
    );
    ensure_eq!(c.read_all(), Ok(b"x".to_vec()), "left the file alone");
    for x in conns {
        x.close();
    }
    ensure!(
        root.clone_conn(RO_DIR).is_ok(),
        "room again after they close"
    );
    Ok(())
}

fn memfsd_refuses_endpoints_it_cannot_serve_and_orphans() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    // an event or a VMO passed as the "endpoint" is refused, and does not use up a connection
    for _ in 0..250 {
        let ev = event()?;
        let r = root
            .call(pop::OPEN, pfl::DIRECTORY, RO_DIR, 0, 0, b"home", Some(ev))
            .map_err(|e| msg_e(e, line!()))?;
        ensure_eq!(r.status, pst::BAD_REQUEST, "a non-channel endpoint");
        sys::handle_close(ev);
    }
    ensure!(
        root.clone_conn(RO_DIR).is_ok(),
        "no connection was consumed"
    );
    // a directory unlinked while open takes no new children (they would leak the quota for good)
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    tmp.mkdir(b"d").map_err(|e| msg_e(e, line!()))?;
    let mut d = tmp
        .open(b"d", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    tmp.unlink(b"d").map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(d.open(b"f", pfl::CREATE, prt::READ | prt::WRITE)),
        Some(pst::NOT_FOUND),
        "create in an orphan"
    );
    ensure_eq!(
        code(d.mkdir(b"g")),
        Some(pst::NOT_FOUND),
        "mkdir in an orphan"
    );
    d.close();
    // a reply that arrives after its caller gave up is not taken for the next answer
    let mut f = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    f.write(0, b"12345").map_err(|e| msg_e(e, line!()))?;
    let stale = proto::file::Request {
        txid: 0xffff_ffff,
        op: pop::GETSIZE,
        flags: 0,
        rights: 0,
        a: 0,
        b: 0,
        payload: &[],
    }
    .encode()
    .unwrap();
    raw_request(f.ch, &stale, &[])?;
    ensure_eq!(
        f.size(),
        Ok(5),
        "the stale reply was discarded, not returned"
    );
    Ok(())
}

fn clone_request(txid: u32) -> Vec<u8> {
    proto::file::Request {
        txid,
        op: pop::CLONE,
        flags: 0,
        rights: RO_DIR,
        a: 0,
        b: 0,
        payload: &[],
    }
    .encode()
    .unwrap()
}

fn memfsd_refuses_both_ends_of_one_channel() -> Outcome {
    // A client that hands the server both ends of a channel it made would leave connections
    // nobody can close (neither end ever sees PEER_CLOSED), and a reply sent on one end would be
    // read back as a request on the other, for ever (found by the S4b/S4c review).
    let (p, mut root, _bi) = start_memfsd()?;
    for i in 0..120u32 {
        let (x, y) = channel()?;
        raw_request(root.ch, &clone_request(2 * i), &[x])?;
        let s1 = reply_status(root.ch)?;
        raw_request(root.ch, &clone_request(2 * i + 1), &[y])?;
        let s2 = reply_status(root.ch)?;
        ensure_eq!(s1, 0, "the first end of pair {i} is an ordinary client");
        ensure_eq!(
            s2,
            pst::BAD_REQUEST,
            "the second end of pair {i} is refused"
        );
    }
    rt::io::nap(50);
    match root.clone_conn(RO_DIR) {
        Ok(c) => c.close(),
        Err(e) => return Err(msg_e(e, line!())),
    }
    root.close();
    let ended = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 5000).status == 0;
    ensure!(ended, "memfsd ends when its last real client is gone");
    Ok(())
}

fn memfsd_survives_memory_pressure_from_quota_legal_clients() -> Outcome {
    // Files use the whole byte quota, names the whole name budget, and 197 clients each ask for a
    // 32 KiB read while the shared IPC budget is full, so every reply has to be held. The server
    // must stay up (it drops the connections it cannot serve).
    let (p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let chunk = alloc::vec![0xabu8; 32 * 1024];
    // Under the round-robin class every round trip waits for a scheduling slice (this test took
    // 77 s there), so it uses a lighter load; the exact budgets are host tests anyway.
    let (nfiles, nnames, nconns) = if loose() {
        (8u32, 100u32, 100usize)
    } else {
        (64, 700, 200)
    };
    // 64 files of 128 KiB: the whole 8 MiB byte quota in few round trips (every round trip is a
    // scheduling slice under the round-robin class)
    for i in 0..nfiles {
        let name = alloc::format!("f{i:03}").into_bytes();
        let mut f = tmp
            .open(&name, pfl::CREATE, prt::READ | prt::WRITE)
            .map_err(|e| msg_e(e, line!()))?;
        for part in 0..4u64 {
            let r = f.write(part * 32 * 1024, &chunk);
            ensure!(r == Ok(32 * 1024), "file {i} part {part}: {r:?}");
        }
        f.close();
    }
    // names are charged too (the exact budget is a host test; here a good share of it)
    let mut made = 0u32;
    for i in 0..nnames {
        let mut name = alloc::format!("{i:05}").into_bytes();
        name.extend_from_slice(&[b'n'; 250]);
        match tmp.mkdir(&name) {
            Ok(()) => made += 1,
            Err(_) => break,
        }
    }
    ensure!(made == nnames, "{made} long-named directories");
    let mut one = tmp
        .open(b"f000", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    let mut conns: Vec<Fs> = Vec::new();
    for _ in 0..nconns {
        match one.clone_conn(prt::READ) {
            Ok(c) => conns.push(c),
            Err(_) => break,
        }
    }
    // fill the shared IPC budget with our own junk, so no reply can be queued
    let mut junk: Vec<(u32, u32)> = Vec::new();
    'fill: loop {
        let (m, t) = channel()?;
        junk.push((m, t));
        let mut n = 0;
        loop {
            match st(sys::channel_write(
                m,
                chunk.as_ptr(),
                chunk.len(),
                core::ptr::null(),
                0,
            )) {
                Some(S::Ok) => n += 1,
                Some(S::ShouldWait) => break,
                _ => break 'fill,
            }
        }
        if n == 0 || junk.len() > 80 {
            break;
        }
    }
    let rq = proto::file::Request {
        txid: 7,
        op: pop::READ,
        flags: 0,
        rights: 0,
        a: 0,
        b: 32 * 1024,
        payload: &[],
    }
    .encode()
    .unwrap();
    for c in conns.iter() {
        for _ in 0..50 {
            if st(sys::channel_write(
                c.ch,
                rq.as_ptr(),
                rq.len(),
                core::ptr::null(),
                0,
            )) == Some(S::Ok)
            {
                break;
            }
            rt::io::nap(2);
        }
    }
    rt::io::nap(200);
    let died = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 10).status == 0;
    let exit = if died {
        sys::process_info(p).a1 as i64
    } else {
        0
    };
    for (m, t) in junk {
        sys::handle_close(m);
        sys::handle_close(t);
    }
    ensure!(
        !died,
        "memfsd died (exit {exit}) under {} connections and {made} directories",
        conns.len()
    );
    let answers = root.stat(b"home").is_ok();
    for c in conns {
        c.close(); // their unread replies count against the shared IPC budget until then
    }
    ensure!(answers, "and it still answers");
    Ok(())
}

fn memfsd_rights_are_checked_op_by_op() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp_rw = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    tmp_rw.mkdir(b"keep").map_err(|e| msg_e(e, line!()))?;
    // a read-only connection to the writable directory cannot change it
    let mut tmp_ro = root
        .open(b"tmp", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(tmp_ro.mkdir(b"x")),
        Some(DENIED),
        "mkdir without WRITE"
    );
    ensure_eq!(
        code(tmp_ro.unlink(b"keep")),
        Some(DENIED),
        "unlink without WRITE"
    );
    // a connection with no rights at all can do nothing
    let mut none = tmp_rw.clone_conn(0).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(code(none.stat(b"keep")), Some(DENIED), "stat without READ");
    // a write-only file connection can neither stat nor ask for a VMO
    let mut f = tmp_rw
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    let mut wo = f.clone_conn(prt::WRITE).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(code(wo.fstat()), Some(DENIED), "fstat without READ");
    ensure_eq!(code(wo.get_vmo()), Some(DENIED), "get_vmo without READ");
    ensure_eq!(code(wo.size()), Some(DENIED), "size without READ");
    // a file connection never gets directory rights
    ensure_eq!(
        code(tmp_rw.open(b"f", 0, prt::READ | prt::ENUMERATE)),
        Some(pst::BAD_REQUEST),
        "ENUMERATE on a file"
    );
    Ok(())
}

fn memfsd_unlinked_open_files_live_until_closed() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let mut f = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    f.write(0, b"data").map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(tmp.unlink(b"f"), Ok(()), "unlink while open");
    ensure_eq!(
        code(tmp.open(b"f", 0, prt::READ)),
        Some(pst::NOT_FOUND),
        "the name is gone"
    );
    ensure_eq!(
        f.read_all(),
        Ok(b"data".to_vec()),
        "the open connection still reads"
    );
    let mut f2 = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        f2.read_all(),
        Ok(vec![]),
        "the name now means a new, empty file"
    );
    // truncate on open
    f2.write(0, b"zzzz").map_err(|e| msg_e(e, line!()))?;
    let mut f3 = tmp
        .open(b"f", pfl::TRUNCATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(f3.read_all(), Ok(vec![]), "TRUNCATE empties it");
    ensure_eq!(
        code(tmp.open(b"f", pfl::TRUNCATE, prt::READ)),
        Some(pst::BAD_REQUEST),
        "TRUNCATE needs WRITE"
    );
    Ok(())
}

fn memfsd_hands_out_programs_the_launcher_can_run() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let (_lp, lch, lbi) = start_launcher()?;
    let _ = lbi;
    let mut prog = root
        .open(b"bin/hello", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    let (vmo, off, len) = prog.get_vmo().map_err(|e| msg_e(e, line!()))?;
    ensure!(len > 1000, "the program is {len} bytes");
    // the VMO handle is read-only
    ensure_eq!(
        st(sys::vmo_write(vmo, 0, b"X".as_ptr(), 1)),
        Some(S::AccessDenied),
        "the image handle is read-only"
    );
    let (bytes, in_w, in_r, out_r, out_w) = hello_startup(&[b"hello", b"from", b"memfsd"])?;
    let (status, proc) = ask_launcher(lch, vmo, 1, off, len, &bytes, &[in_r, out_w])?;
    ensure_eq!(status, 0, "the launcher accepts what memfsd handed out");
    sys::handle_close(in_w);
    ensure_eq!(
        &read_to_end(out_r)?[..],
        &b"args=hello from memfsd\n"[..],
        "and it runs"
    );
    wait_ended(proc.ok_or_else(|| msg_at(line!()))?, 2000)?;
    // only image files have a VMO
    let mut text = root
        .open(b"home/hello.txt", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    let (fv, foff, flen) = text.get_vmo().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        flen,
        13,
        "every boot-image file has a VMO extent, fixtures included"
    );
    let mut word = [0u8; 5];
    sys::vmo_read(fv, foff, word.as_mut_ptr(), 5);
    ensure_eq!(&word, b"hello", "and it is the file's bytes");
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let mut mine = tmp
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(mine.get_vmo()),
        Some(pst::BAD_REQUEST),
        "nor is a writable file"
    );
    Ok(())
}

fn memfsd_listing_pages_and_a_slow_client_stalls_only_itself() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    // 300 long names: more than one 32 KiB page of listing
    for i in 0..300u32 {
        let mut name = alloc::format!("{i:05}").into_bytes();
        name.extend_from_slice(&[b'n'; 200]);
        tmp.open(&name, pfl::CREATE, prt::READ)
            .map_err(|e| msg_e(e, line!()))?
            .close();
    }
    let all = tmp.read_dir().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(all.len(), 300, "every entry arrives over several pages");
    ensure!(
        all.windows(2).all(|w| w[0].name < w[1].name),
        "in name order"
    );
    // a client that sends many reads and never reads a reply
    let mut big = tmp
        .open(b"big", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    big.write(0, &alloc::vec![7u8; 32 * 1024])
        .map_err(|e| msg_e(e, line!()))?;
    let slow = big.clone_conn(prt::READ).map_err(|e| msg_e(e, line!()))?;
    for i in 0..12u32 {
        let rq = proto::file::Request {
            txid: 1000 + i,
            op: pop::READ,
            flags: 0,
            rights: 0,
            a: 0,
            b: 32 * 1024,
            payload: &[],
        }
        .encode()
        .unwrap();
        ensure_eq!(
            st(sys::channel_write(
                slow.ch,
                rq.as_ptr(),
                rq.len(),
                core::ptr::null(),
                0
            )),
            Some(S::Ok),
            "queue request {i}"
        );
    }
    // another client is served promptly meanwhile
    let t0 = sys::clock_get();
    ensure_eq!(
        root.stat(b"home").map(|s| s.is_dir),
        Ok(true),
        "another client"
    );
    ensure!(
        sys::clock_get() - t0 < 2000,
        "another client is not stalled ({} ticks)",
        sys::clock_get() - t0
    );
    // the slow client eventually reads every reply, in order
    let mut got = 0u32;
    let mut slow = slow;
    while got < 12 {
        let a = slow.read_next_reply().map_err(|e| msg_e(e, line!()))?;
        ensure_eq!(a.status, 0, "reply {got}");
        ensure_eq!(a.payload.len(), 32 * 1024, "its length");
        got += 1;
    }
    Ok(())
}

fn memfsd_exits_when_every_client_is_gone() -> Outcome {
    let (p, root, _bi) = start_memfsd()?;
    root.close();
    wait_ended(p, 5000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 0, "a clean exit");
    Ok(())
}

fn syscalls_stay_fast_with_many_live_processes() -> Outcome {
    // Every VMO a process closes after mapping it stays pending until the process dies; the
    // kernel must not rescan all of them on every wake-up (it once cost ~1 ms per syscall with a
    // dozen service processes alive, found when the filesystem tests were added). 3000 syscalls
    // took about 3000 ticks then, 150 to 700 now (a fast machine, a loaded CI runner): the bound
    // sits between. (A bound relative to a run "without" the processes does not work: earlier
    // tests leave processes alive, which slows both runs the same.)
    let mut kids = [0u32; 12];
    for k in kids.iter_mut() {
        let (p, _mine) = launch_image(HELLO_IMAGE.load(Ordering::Relaxed), 0)?;
        *k = p;
    }
    let t0 = sys::clock_get();
    for _ in 0..1500 {
        let e = event()?;
        sys::handle_close(e);
    }
    let took = sys::clock_get() - t0;
    for k in kids {
        sys::process_kill(k, 1);
    }
    ensure!(
        took < 1800,
        "3000 syscalls took {took} ticks with live processes around"
    );
    Ok(())
}

fn object_info_names_objects_and_peers() -> Outcome {
    let (a, b) = channel()?;
    let ia = ok(sys::object_info(a), "info a")?;
    let ib = ok(sys::object_info(b), "info b")?;
    ensure!(
        ia.a1 != 0 && ib.a1 != 0 && ia.a1 != ib.a1,
        "two ends, two ids"
    );
    ensure_eq!(ia.a2, ib.a1, "a's peer is b");
    ensure_eq!(ib.a2, ia.a1, "b's peer is a");
    let dup = ok(sys::handle_duplicate(a, Rights::SAME.0), "dup")?.a1 as u32;
    ensure_eq!(
        ok(sys::object_info(dup), "info dup")?.a1,
        ia.a1,
        "a duplicate names the same object"
    );
    let e = event()?;
    let ie = ok(sys::object_info(e), "info e")?;
    ensure_eq!((ie.a1 != 0, ie.a2), (true, 0), "an event has no peer");
    sys::handle_close(b);
    ensure_eq!(
        ok(sys::object_info(a), "info a again")?.a2,
        0,
        "once the peer is gone it reads 0"
    );
    ensure_eq!(st(sys::object_info(BAD)), Some(S::BadHandle), "bad handle");
    ensure_eq!(
        st(rt::syscall(
            rt::abi::syscall::OBJECT_INFO,
            [(1usize << 32) | a as usize, 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // ids are not reused: a new object gets a new id even in the same slot
    sys::handle_close(e);
    let e2 = event()?;
    ensure!(
        ok(sys::object_info(e2), "info e2")?.a1 != ie.a1,
        "a freed object's id is never reused"
    );
    Ok(())
}

fn user_cannot_forge_kernel_lines() -> Outcome {
    // The boot harness trusts `##DONE`, `##PANIC` and `##BOOT` lines only from the kernel;
    // the kernel prefixes any such line a user program writes. If these leaked, the whole
    // run would be reported as malformed or contradictory.
    println!("##DONE pass=999 fail=0");
    println!("##PANIC forged by user mode");
    println!("##BOOT forged 0");
    println!("##TEST forged_by_user_mode_with_no_status");
    Ok(())
}

fn registers_survive_a_syscall() -> Outcome {
    // SYSCALLS.md: every register except a0-a2 is preserved across `ecall`.
    let (mut t0, mut t1, mut t2, mut t3, mut t4, mut t5, mut t6) =
        (0x11usize, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77);
    let (mut a3, mut a4, mut a5, mut a6) = (0x88usize, 0x99, 0xaa, 0xbb);
    let (mut s2, mut s3, mut s4, mut s5) = (0xc1usize, 0xc2, 0xc3, 0xc4);
    // SAFETY: a CLOCK_GET syscall; the registers named are inputs and outputs, a0-a2 are
    // clobbered by the results as the ABI says.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") rt::abi::syscall::CLOCK_GET,
            inout("t0") t0, inout("t1") t1, inout("t2") t2, inout("t3") t3, inout("t4") t4,
            inout("t5") t5, inout("t6") t6,
            inout("a3") a3, inout("a4") a4, inout("a5") a5, inout("a6") a6,
            inout("s2") s2, inout("s3") s3, inout("s4") s4, inout("s5") s5,
            lateout("a0") _, lateout("a1") _, lateout("a2") _,
        );
    }
    ensure_eq!(
        [t0, t1, t2, t3, t4, t5, t6],
        [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
        "temporaries"
    );
    ensure_eq!(
        [a3, a4, a5, a6],
        [0x88, 0x99, 0xaa, 0xbb],
        "argument registers a3-a6"
    );
    ensure_eq!(
        [s2, s3, s4, s5],
        [0xc1, 0xc2, 0xc3, 0xc4],
        "saved registers"
    );
    Ok(())
}

fn wide_arguments_are_not_truncated() -> Outcome {
    let e = event()?;
    let wide = |h: u32| (1usize << 32) | h as usize; // a valid handle with a high bit added
    let st = |n: usize, a: [usize; 6]| st(rt::syscall(n, a));
    use rt::abi::syscall as nr;
    ensure_eq!(
        st(nr::HANDLE_CLOSE, [wide(e), 0, 0, 0, 0, 0]),
        Some(S::BadHandle),
        "close with a high bit set"
    );
    ensure_eq!(
        st(nr::OBJECT_SIGNALS, [wide(e), 0, 0, 0, 0, 0]),
        Some(S::BadHandle),
        "object_signals"
    );
    ensure_eq!(
        st(
            nr::HANDLE_DUPLICATE,
            [wide(e), Rights::SAME.0 as usize, 0, 0, 0, 0]
        ),
        Some(S::BadHandle),
        "duplicate handle"
    );
    ensure_eq!(
        st(
            nr::HANDLE_DUPLICATE,
            [
                e as usize,
                (1usize << 32) | Rights::SIGNAL.0 as usize,
                0,
                0,
                0,
                0
            ]
        ),
        Some(S::AccessDenied),
        "duplicate rights with bit 32"
    );
    ensure_eq!(
        st(nr::EVENT_SIGNAL, [e as usize, 0, 1 << 32, 0, 0, 0]),
        Some(S::InvalidArgs),
        "signal mask with bit 32"
    );
    ensure_eq!(
        st(nr::EVENT_SIGNAL, [wide(e), 0, 1 << 24, 0, 0, 0]),
        Some(S::BadHandle),
        "event_signal handle"
    );
    ensure_eq!(
        st(nr::EVENT_SIGNAL, [e as usize, 0, 1 << 24, 0, 0, 0]),
        Some(S::Ok),
        "the same call without the high bit works"
    );
    Ok(())
}

fn channel_read_checks_the_message_not_the_capacity() -> Outcome {
    let (a, b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"small".as_ptr(),
            5,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write"
    );
    // A buffer that is valid for only 8 bytes (the top of my stack) with a declared capacity
    // of a million: the message is 5 bytes, so only 5 bytes need to be writable.
    let top = 0x20_0000_0000usize;
    let r = sys::channel_read(b, (top - 8) as *mut u8, 1_000_000, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1),
        (0, 5),
        "a large declared capacity over a small valid buffer"
    );
    ensure_eq!(
        st(sys::channel_write(
            a,
            b"again".as_ptr(),
            5,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write again"
    );
    // usize::MAX capacity is clamped to the limit; the (5 byte) message needs a 5 byte
    // destination, and 0x10 is not mapped: a pointer error, and the message is not lost.
    let r = sys::channel_read(b, 0x10 as *mut u8, usize::MAX, core::ptr::null_mut(), 0);
    ensure_eq!(
        st(r),
        Some(S::InvalidArgs),
        "unmapped destination, huge capacity"
    );
    let mut buf = [0u8; 8];
    let r = sys::channel_read(b, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, r.a1),
        (0, 5),
        "the message survived the refused read"
    );
    Ok(())
}

fn rights_cannot_be_widened() -> Outcome {
    let e = event()?; // DUPLICATE TRANSFER SIGNAL WAIT
    ensure_eq!(
        st(sys::handle_duplicate(e, Rights::READ.0)),
        Some(S::AccessDenied),
        "a right the source lacks"
    );
    ensure_eq!(
        st(sys::handle_duplicate(e, 1 << 20)),
        Some(S::AccessDenied),
        "an unassigned bit"
    );
    let dup = ok(
        sys::handle_duplicate(e, Rights::SIGNAL.0),
        "duplicate with fewer rights",
    )?
    .a1 as u32;
    ensure_eq!(
        st(sys::handle_duplicate(dup, Rights::SAME.0)),
        Some(S::AccessDenied),
        "the weaker handle cannot duplicate"
    );
    ensure_eq!(
        st(sys::handle_replace(dup, Rights::EVENT.0)),
        Some(S::AccessDenied),
        "replace cannot widen"
    );
    ensure_eq!(
        st(sys::event_signal(dup, 0, 1 << 24)),
        Some(S::Ok),
        "a refused replace left the handle valid"
    );
    let list = [dup];
    let (a, _b) = channel()?;
    ensure_eq!(
        st(sys::channel_write(
            a,
            core::ptr::null(),
            0,
            list.as_ptr(),
            1
        )),
        Some(S::AccessDenied),
        "no TRANSFER right"
    );
    let nr = ok(
        sys::handle_replace(dup, Rights::NONE.0),
        "replace with no rights",
    )?
    .a1 as u32;
    ensure_eq!(
        st(sys::event_signal(dup, 0, 0)),
        Some(S::BadHandle),
        "the old value is dead after a replace"
    );
    ensure_eq!(
        st(sys::event_signal(nr, 0, 0)),
        Some(S::AccessDenied),
        "and the new one has nothing"
    );
    Ok(())
}

fn deep(n: u32) -> u32 {
    let mut pad = [0u8; 2048];
    pad[(n as usize) % 2048] = n as u8;
    if n == 0 {
        return pad[0] as u32;
    }
    deep(n - 1).wrapping_add(core::hint::black_box(pad[(n as usize) % 2048]) as u32)
}

fn stack_grows_lazily() -> Outcome {
    // 100 frames of 2 KiB: about 200 KiB of the 256 KiB stack, touched one page fault at a time.
    let v = deep(100);
    ensure!(v > 0, "the recursion ran ({v})");
    Ok(())
}

fn syscall_layer_bounds() -> Outcome {
    // nbytes and nhandles are bounded before anything is read from user memory
    ensure_eq!(
        st(sys::channel_write(
            BAD,
            core::ptr::null(),
            usize::MAX,
            core::ptr::null(),
            0
        )),
        Some(S::TooBig),
        "nbytes = usize::MAX"
    );
    ensure_eq!(
        st(sys::channel_write(
            BAD,
            core::ptr::null(),
            0,
            core::ptr::null(),
            usize::MAX
        )),
        Some(S::TooBig),
        "nhandles = usize::MAX"
    );
    ensure_eq!(
        st(sys::debug_write(&[0u8; 4096])),
        Some(S::Ok),
        "exactly 4096 is allowed"
    );
    Ok(())
}

fn cycles_do_not_grow_memory() -> Outcome {
    // The OD39 loop: make two channels hold each other and drop every handle. 9,000 rounds
    // leave 18,000 unreachable objects (two per round: the pair that holds each other), more
    // than the kernel's cap of 16,384 (`MAX_OBJECTS`): it must keep sweeping or this ends in
    // NoMemory.
    for i in 0..9_000u32 {
        let (x, w) = channel().map_err(|mut d| {
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!(" at round {i}"));
            d
        })?;
        let (z, y) = channel()?;
        ok(
            sys::channel_write(y, core::ptr::null(), 0, [x].as_ptr(), 1),
            "write x",
        )?;
        ok(
            sys::channel_write(w, core::ptr::null(), 0, [z].as_ptr(), 1),
            "write z",
        )?;
        ok(sys::handle_close(w), "close w")?;
        ok(sys::handle_close(y), "close y")?;
    }
    Ok(())
}

// ---- S4.5: jobs and limits ----

fn root_job() -> Result<u32, Detail> {
    Ok(ok(sys::job_root(), "job_root")?.a1 as u32)
}

fn job(parent: u32, procs: u64, threads: u64, pages: u64) -> Result<u32, Detail> {
    Ok(ok(
        sys::job_create(
            parent,
            &[procs, threads, pages, u64::MAX, u64::MAX, u64::MAX],
        ),
        "job_create",
    )?
    .a1 as u32)
}

/// (used, limit) of a job's resource: 0 processes, 1 threads, 2 pages.
fn job_use(j: u32, which: u64) -> Result<(u64, u64), Detail> {
    let r = ok(sys::job_info(j, which), "job_info")?;
    Ok((r.a1 as u64, r.a2 as u64))
}

/// Wait (up to `ticks`) for a job's usage of `which` to reach `want`.
fn job_use_settles(j: u32, which: u64, want: u64, ticks: u64) -> Outcome {
    let until = sys::clock_get() + ticks;
    loop {
        let (used, _) = job_use(j, which)?;
        if used == want {
            return Ok(());
        }
        ensure!(
            sys::clock_get() < until,
            "job usage {which} stayed at {used}, wanted {want}"
        );
        rt::io::nap(2);
    }
}

fn job_syscalls_check_handles_rights_and_arguments() -> Outcome {
    let root = root_job()?;
    // a job is a job: the other kinds of handle are the wrong type
    let ev = ok(sys::event_create(), "event")?.a1 as u32;
    ensure_eq!(
        st(sys::job_create(
            ev,
            &[u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::WrongType),
        "job_create on an event"
    );
    ensure_eq!(
        st(sys::job_info(ev, 0)),
        Some(S::WrongType),
        "job_info on an event"
    );
    ensure_eq!(
        st(sys::job_create(0, &[1, 1, 1, u64::MAX, u64::MAX, u64::MAX])),
        Some(S::BadHandle),
        "job_create on handle 0"
    );
    let gone = job(root, 1, 1, 1)?;
    sys::handle_close(gone);
    ensure_eq!(
        st(sys::job_create(
            gone,
            &[1, 1, 1, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::BadHandle),
        "job_create on a closed handle"
    );
    ensure_eq!(
        st(sys::job_info(root, 6)),
        Some(S::InvalidArgs),
        "an unknown resource"
    );
    // each operation needs its own right
    let only = |r: Rights| -> Result<u32, Detail> {
        Ok(ok(sys::handle_duplicate(root, r.0), "duplicate")?.a1 as u32)
    };
    let inspect = only(Rights::INSPECT)?;
    let manage = only(Rights::MANAGE)?;
    let kill = only(Rights::KILL)?;
    ensure_eq!(
        st(sys::job_info(inspect, 0)),
        Some(S::Ok),
        "inspect may inspect"
    );
    ensure_eq!(
        st(sys::job_info(manage, 0)),
        Some(S::AccessDenied),
        "manage may not inspect"
    );
    ensure_eq!(
        st(sys::job_create(
            inspect,
            &[1, 1, 1, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::AccessDenied),
        "inspect may not create"
    );
    ensure_eq!(
        st(sys::job_set_limits(
            kill,
            &[1, 1, 1, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::AccessDenied),
        "kill may not set limits"
    );
    ensure_eq!(
        st(sys::job_set_limits(
            manage,
            &[1, 1, 1, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::AccessDenied),
        "MANAGE alone may not lift or change limits (T36)"
    );
    let setter = only(Rights::SET_LIMITS)?;
    ensure_eq!(
        st(sys::job_set_limits(
            setter,
            &[u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX]
        )),
        Some(S::Ok),
        "SET_LIMITS may set limits"
    );
    ensure_eq!(
        st(sys::job_create(setter, &[1, 1, 1, 1, u64::MAX, u64::MAX])),
        Some(S::AccessDenied),
        "SET_LIMITS alone may not create jobs"
    );
    ensure_eq!(
        st(sys::job_kill(manage, 0)),
        Some(S::AccessDenied),
        "manage may not kill"
    );
    ensure_eq!(
        st(sys::process_create_in(inspect)),
        Some(S::AccessDenied),
        "a process needs MANAGE on its job"
    );
    ensure_eq!(
        st(sys::vmo_create_in(1, inspect)),
        Some(S::AccessDenied),
        "a VMO needs MANAGE on its job"
    );
    // rights only shrink: the root handle's rights cannot be widened by duplicating
    ensure_eq!(
        st(sys::handle_duplicate(inspect, Rights::JOB.0)),
        Some(S::AccessDenied),
        "no widening"
    );
    // job_root is for the kernel's first process only: a child is refused, and so is a
    // child's attempt to name a job it was not given (variant 31 checks bits 0 and 1)
    let j = job(root, 1, u64::MAX, u64::MAX)?;
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 31, j)?;
    wait_ended(p, 500)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        7,
        "the child's three refusals (job_root, a made-up job, a second process)"
    );
    for h in [root, ev, inspect, manage, kill, setter, j, p] {
        sys::handle_close(h);
    }
    Ok(())
}

fn job_limits_stop_a_process_bomb_and_usage_comes_back() -> Outcome {
    let root = root_job()?;
    let (base, _) = job_use(root, 0)?;
    let j = job(root, 4, u64::MAX, u64::MAX)?;
    // the child is one process of the four and makes the other three, then is refused
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 32, j)?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        3,
        "processes the bomb managed to make"
    );
    // everything it made dies with it (never started: closing their last handles ends them)
    job_use_settles(j, 0, 0, 500)?;
    ensure_eq!(
        job_use(root, 0)?.0,
        base,
        "the root job's count is back where it was"
    );
    // set_limits works on a live job: raise it and a parent can make processes again
    ok(
        sys::job_set_limits(j, &[2, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        "raise",
    )?;
    let a = ok(sys::process_create_in(j), "first")?.a1 as u32;
    let b = ok(sys::process_create_in(j), "second")?.a1 as u32;
    ensure_eq!(
        st(sys::process_create_in(j)),
        Some(S::NoMemory),
        "a third process is over the limit"
    );
    // lowering the limit below usage keeps what exists and refuses more
    ok(
        sys::job_set_limits(j, &[1, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        "lower",
    )?;
    ensure_eq!(job_use(j, 0)?, (2, 1), "usage above a lowered limit stays");
    sys::handle_close(a);
    sys::handle_close(b);
    job_use_settles(j, 0, 0, 500)?;
    ok(sys::process_create_in(j), "room again once usage fell")?;
    sys::handle_close(p);
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

fn job_limits_stop_a_thread_bomb() -> Outcome {
    let root = root_job()?;
    let j = job(root, u64::MAX, 2, u64::MAX)?;
    // two threads in all: the child's own and one more; the second extra is refused
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 33, j)?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        3,
        "one extra thread allowed, the next refused"
    );
    job_use_settles(j, 1, 0, 500)?;
    // with one thread allowed, a second running process cannot be started
    ok(
        sys::job_set_limits(j, &[u64::MAX, 1, u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        "one thread",
    )?;
    let (q, _mq) = launch_image_in(IMAGE.load(Ordering::Relaxed), 15, j)?; // spins
    let second = launch_image_in(IMAGE.load(Ordering::Relaxed), 15, j);
    ensure!(
        second.is_err(),
        "a second process started although the job allows one thread"
    );
    ok(sys::process_kill(q, -1), "end the spinner")?;
    wait_ended(q, 500)?;
    job_use_settles(j, 1, 0, 500)?;
    for h in [p, q, j, root] {
        sys::handle_close(h);
    }
    Ok(())
}

fn proc_create_in(job: u32) -> Result<u32, Detail> {
    Ok(ok(sys::process_create_in(job), "process_create_in")?.a1 as u32)
}

fn job_memory_limits_stop_a_hog_and_are_credited_back() -> Outcome {
    let root = root_job()?;
    // the child's own image and stack count against its job too: 64 pages leave it room for
    // some of its own, but not for a hog
    let j = job(root, u64::MAX, u64::MAX, 64)?;
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 30, j)?;
    wait_ended(p, 3000)?;
    let got = sys::process_info(p).a1 as i64;
    ensure!(
        (1..64).contains(&got),
        "the hog got {got} pages of a 64-page job (and was told no)"
    );
    // all of it is given back when the child is gone
    job_use_settles(j, 2, 0, 1000)?;
    // (the root job's own page count is not stable: this program's heap and stack grow too)
    sys::handle_close(p);
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

fn a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling() -> Outcome {
    let root = root_job()?;
    let parent = job(root, u64::MAX, u64::MAX, 8)?;
    let a = job(parent, u64::MAX, u64::MAX, u64::MAX)?; // its own limit allows everything
    let b = job(parent, u64::MAX, u64::MAX, u64::MAX)?;
    let big = ok(sys::vmo_create_in(100, a), "a VMO in a")?.a1 as u32;
    let page = [7u8; 4096];
    let mut got = 0;
    while sys::vmo_write(big, got * 4096, page.as_ptr(), 4096).status == 0 {
        got += 1;
        ensure!(got < 100, "the parent's limit never applied");
    }
    ensure_eq!(got, 8, "pages a child could commit under an 8-page parent");
    ensure_eq!(job_use(parent, 2)?.0, 8, "the parent sees them");
    ensure_eq!(job_use(a, 2)?.0, 8, "and so does the child");
    // the sibling shares the same 8 pages: none left
    let other = ok(sys::vmo_create_in(4, b), "a VMO in b")?.a1 as u32;
    ensure_eq!(
        st(sys::vmo_write(other, 0, page.as_ptr(), 4096)),
        Some(S::NoMemory),
        "no pages left for the sibling"
    );
    ensure_eq!(job_use(b, 2)?.0, 0, "a refused page is not charged");
    // closing the VMO frees its pages for the sibling
    sys::handle_close(big);
    job_use_settles(a, 2, 0, 500)?;
    ok(
        sys::vmo_write(other, 0, page.as_ptr(), 4096),
        "the sibling after the first VMO went",
    )?;
    // a limit set after the fact applies to what is committed next
    ok(
        sys::job_set_limits(
            parent,
            &[u64::MAX, u64::MAX, 1, u64::MAX, u64::MAX, u64::MAX],
        ),
        "lower the parent",
    )?;
    ensure_eq!(
        st(sys::vmo_write(other, 4096, page.as_ptr(), 4096)),
        Some(S::NoMemory),
        "over the lowered limit"
    );
    for h in [other, a, b, parent, root] {
        sys::handle_close(h);
    }
    Ok(())
}

fn job_kill_ends_every_process_below_and_forbids_more() -> Outcome {
    let root = root_job()?;
    let top = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let low = job(top, u64::MAX, u64::MAX, u64::MAX)?;
    let image = IMAGE.load(Ordering::Relaxed);
    let (p1, _m1) = launch_image_in(image, 15, top)?; // spins
    let (p2, _m2) = launch_image_in(image, 15, low)?;
    let (p3, _m3) = launch_image_in(image, 16, low)?; // blocked in a wait
    let outside = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let (p4, _m4) = launch_image_in(image, 15, outside)?;
    ensure_eq!(job_use(top, 0)?.0, 3, "three processes at or below top");
    ok(sys::job_kill(top, -9), "kill top")?;
    for p in [p1, p2, p3] {
        wait_ended(p, 500)?;
        ensure_eq!(sys::process_info(p).a1 as i64, -9, "the code job_kill gave");
    }
    ensure_eq!(
        st(sys::process_info(p4)),
        Some(S::BadState),
        "a process in another job is untouched"
    );
    // nothing new may start in the killed subtree
    ensure_eq!(
        st(sys::process_create_in(top)),
        Some(S::NoMemory),
        "no process in the killed job"
    );
    ensure_eq!(
        st(sys::process_create_in(low)),
        Some(S::NoMemory),
        "none below it either"
    );
    job_use_settles(top, 0, 0, 500)?;
    ok(sys::process_kill(p4, -1), "end the bystander")?;
    wait_ended(p4, 500)?;
    for h in [p1, p2, p3, p4, top, low, outside, root] {
        sys::handle_close(h);
    }
    Ok(())
}

fn jobs_do_not_leak() -> Outcome {
    let root = root_job()?;
    let (procs0, _) = job_use(root, 0)?;
    // 1,200 jobs, each with a process and some memory, more than ten times the 256 the ledger
    // can hold at once: every record must be freed for this to get through
    for i in 0..1_200u32 {
        let j = job(root, 2, 2, 8).map_err(|mut d| {
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!(" at round {i}"));
            d
        })?;
        let p = proc_create_in(j)?;
        let v = ok(sys::vmo_create_in(2, j), "vmo")?.a1 as u32;
        ok(sys::vmo_write(v, 0, b"x".as_ptr(), 1), "commit a page")?;
        for h in [v, p, j] {
            sys::handle_close(h);
        }
    }
    job_use_settles(root, 0, procs0, 500)?;
    sys::handle_close(root);
    Ok(())
}

fn a_refused_process_start_changes_nothing_and_can_be_retried() -> Outcome {
    // (found by the independent review of S4.5a: the refusal came after the bootstrap handle had
    // moved, and a second start found handle 1 taken and panicked the kernel)
    let root = root_job()?;
    let j = job(root, u64::MAX, 0, u64::MAX)?; // no threads at all
    let p = proc_create_in(j)?;
    let seg = ok(sys::vmo_create_in(1, j), "segment")?.a1 as u32;
    ok(
        sys::process_map(p, seg, 0x1_0000, 0, 4096, PF_R | PF_X),
        "map some code",
    )?;
    let (mine, theirs) = channel()?;
    for attempt in 1..=3 {
        ensure_eq!(
            st(sys::process_start(p, 0x1_0000, 0x1_0000 + 4096, theirs, 0)),
            Some(S::NoMemory),
            "start {attempt} in a job with no threads"
        );
        // the bootstrap handle is still ours: nothing moved
        ensure_eq!(
            st(sys::object_signals(theirs)),
            Some(S::Ok),
            "our handle after refusal {attempt}"
        );
    }
    // once a thread is allowed the same start goes through (the code is one `jr` to itself)
    ok(
        sys::job_set_limits(j, &[u64::MAX, 1, u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        "allow one thread",
    )?;
    ok(
        sys::process_start(p, 0x1_0000, 0x1_0000 + 4096, theirs, 0),
        "the retry",
    )?;
    ensure_eq!(job_use(j, 1)?.0, 1, "one thread running");
    ok(sys::process_kill(p, -3), "end it")?;
    wait_ended(p, 500)?;
    job_use_settles(j, 1, 0, 500)?;
    for h in [mine, seg, p, j, root] {
        sys::handle_close(h);
    }
    Ok(())
}

fn page_tables_are_charged_to_the_job() -> Outcome {
    // Page-table frames are memory too: a program must not be able to spend its machine's frames
    // on tables for scattered mappings while its page count stays tiny (found by the review).
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, 40)?;
    let p = proc_create_in(j)?;
    let (base, _) = job_use(j, 2)?;
    ensure!(base >= 1, "the process's root table is charged ({base})");
    let v = ok(sys::vmo_create_in(1, j), "a VMO")?.a1 as u32;
    let mut mapped = 0u64;
    loop {
        let r = sys::process_map(p, v, 0x1_0000_0000 + mapped * 0x20_0000, 0, 4096, PF_R);
        if r.status != 0 {
            ensure_eq!(st(r), Some(S::NoMemory), "the mapping that did not fit");
            break;
        }
        mapped += 1;
        ensure!(mapped < 40, "mappings were never refused");
    }
    ensure!(
        (1..40).contains(&mapped),
        "{mapped} scattered mappings fit in a 40-page job"
    );
    let (used, _) = job_use(j, 2)?;
    ensure!(
        used > mapped,
        "each mapping costs more than nothing ({used} pages for {mapped})"
    );
    // unmapping gives the charge back
    ok(sys::process_unmap(p, 0x1_0000_0000), "unmap the first")?;
    let (after, _) = job_use(j, 2)?;
    ensure!(
        after < used,
        "unmapping credited its table pages ({used} -> {after})"
    );
    ok(
        sys::process_map(p, v, 0x1_0000_0000, 0, 4096, PF_R),
        "map again where there is room",
    )?;
    sys::handle_close(p); // never started: gone at once, everything it was charged is credited
    sys::handle_close(v);
    job_use_settles(j, 2, 0, 500)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

fn a_job_limits_how_many_jobs_may_exist_below_it() -> Outcome {
    let root = root_job()?;
    let top = ok(
        sys::job_create(root, &[u64::MAX, u64::MAX, u64::MAX, 3, u64::MAX, u64::MAX]),
        "top",
    )?
    .a1 as u32;
    let a = job(top, u64::MAX, u64::MAX, u64::MAX)?;
    let b = job(a, u64::MAX, u64::MAX, u64::MAX)?; // a grandchild counts against top too
    let c = job(top, u64::MAX, u64::MAX, u64::MAX)?;
    ensure_eq!(job_use(top, 3)?, (3, 3), "three jobs below top");
    ensure_eq!(
        st(sys::job_create(a, &[1, 1, 1, 0, u64::MAX, u64::MAX])),
        Some(S::NoMemory),
        "a fourth, anywhere below top"
    );
    sys::handle_close(c);
    ok(
        sys::job_create(a, &[1, 1, 1, 0, u64::MAX, u64::MAX]),
        "room after one went",
    )?;
    // (the 256-job cap is no longer the only bound: a program given a job cannot use it up)
    for h in [b, a, top, root] {
        sys::handle_close(h);
    }
    Ok(())
}

fn job_quotas_stop_an_object_and_an_ipc_hog_and_everything_comes_back() -> Outcome {
    let root = root_job()?;
    let image = IMAGE.load(Ordering::Relaxed);
    // objects: the child's own process object counts, so a limit of 11 leaves ten events
    let j = ok(
        sys::job_create(
            root,
            &[u64::MAX, u64::MAX, u64::MAX, u64::MAX, 11, u64::MAX],
        ),
        "a job with 11 objects",
    )?
    .a1 as u32;
    let (p, _m) = launch_image_in(image, 35, j)?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        10,
        "events the object bomb made"
    );
    sys::handle_close(p); // (the process object lives while we hold a handle to it)
    job_use_settles(j, 4, 0, 500)?;
    // bytes: a 40 KiB queue budget; the hog gets about that much in and is then refused
    ok(
        sys::job_set_limits(j, &[u64::MAX, u64::MAX, u64::MAX, u64::MAX, 100, 40 * 1024]),
        "a 40 KiB budget",
    )?;
    let (p, _m) = launch_image_in(image, 34, j)?;
    wait_ended(p, 3000)?;
    let kib = sys::process_info(p).a1 as i64;
    ensure!(
        (30..=40).contains(&kib),
        "the hog queued {kib} KiB of a 40 KiB budget"
    );
    // meanwhile this program (in the root job) was never short: the shared budget is not the
    // hog's to use up
    let (a, b) =
        ok(sys::socket_create(), "a socket in the root job").map(|r| (r.a1 as u32, r.a2 as u32))?;
    ok(
        sys::socket_write(a, [1u8; 4096].as_ptr(), 4096),
        "write while the hog is dead",
    )?;
    sys::handle_close(a);
    sys::handle_close(b);
    // everything the child held comes back when it is gone
    sys::handle_close(p);
    job_use_settles(j, 5, 0, 1000)?;
    job_use_settles(j, 4, 0, 1000)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

#[no_mangle]
pub fn app_main() -> i32 {
    let mut failed = 0;
    let tests: &[Test] = &[
        ("user_mode_runs_and_prints", user_mode_runs_and_prints),
        ("clock_advances_in_user_mode", clock_advances_in_user_mode),
        ("unknown_syscalls_are_refused", unknown_syscalls_are_refused),
        ("debug_write_pointer_rules", debug_write_pointer_rules),
        (
            "handle_syscalls_reject_bad_values",
            handle_syscalls_reject_bad_values,
        ),
        ("event_syscalls", event_syscalls),
        ("channel_round_trip", channel_round_trip),
        ("channel_sends_handles", channel_sends_handles),
        ("channel_syscall_errors", channel_syscall_errors),
        (
            "channel_buffer_too_small_keeps_message",
            channel_buffer_too_small_keeps_message,
        ),
        (
            "buffers_are_validated_before_a_read",
            buffers_are_validated_before_a_read,
        ),
        ("channel_txid_read", channel_txid_read),
        ("channel_peer_closed", channel_peer_closed),
        ("registers_survive_a_syscall", registers_survive_a_syscall),
        (
            "wide_arguments_are_not_truncated",
            wide_arguments_are_not_truncated,
        ),
        (
            "channel_read_checks_the_message_not_the_capacity",
            channel_read_checks_the_message_not_the_capacity,
        ),
        ("wait_arguments_are_checked", wait_arguments_are_checked),
        (
            "wait_returns_at_once_when_the_condition_holds",
            wait_returns_at_once_when_the_condition_holds,
        ),
        (
            "wait_with_deadline_zero_polls",
            wait_with_deadline_zero_polls,
        ),
        (
            "wait_times_out_at_the_deadline",
            wait_times_out_at_the_deadline,
        ),
        ("wait_is_woken_by_a_message", wait_is_woken_by_a_message),
        ("channel_call_round_trip", channel_call_round_trip),
        (
            "channel_call_skips_unrelated_messages",
            channel_call_skips_unrelated_messages,
        ),
        (
            "channel_call_times_out_but_the_request_was_sent",
            channel_call_times_out_but_the_request_was_sent,
        ),
        (
            "channel_call_buffer_too_small",
            channel_call_buffer_too_small,
        ),
        ("channel_call_argument_errors", channel_call_argument_errors),
        ("wait_and_call_argument_edges", wait_and_call_argument_edges),
        ("channel_call_carries_handles", channel_call_carries_handles),
        ("kernel_ends_survive_a_sweep", kernel_ends_survive_a_sweep),
        ("vmo_basics", vmo_basics),
        ("process_map_argument_checks", process_map_argument_checks),
        (
            "process_start_argument_checks",
            process_start_argument_checks,
        ),
        ("fetch_the_program_image", fetch_the_program_image),
        (
            "child_exit_code_and_terminated_signal",
            child_exit_code_and_terminated_signal,
        ),
        ("child_faults_are_contained", child_faults_are_contained),
        ("parent_and_child_talk", parent_and_child_talk),
        ("kill_a_spinning_child", kill_a_spinning_child),
        ("kill_a_blocked_child", kill_a_blocked_child),
        (
            "closing_the_last_process_handle_kills_the_child",
            closing_the_last_process_handle_kills_the_child,
        ),
        (
            "a_process_that_never_started_can_be_killed",
            a_process_that_never_started_can_be_killed,
        ),
        (
            "process_self_and_mapping_into_ourselves",
            process_self_and_mapping_into_ourselves,
        ),
        (
            "vmo_closed_while_mapped_keeps_its_pages",
            vmo_closed_while_mapped_keeps_its_pages,
        ),
        ("many_children_do_not_leak", many_children_do_not_leak),
        (
            "thread_create_argument_checks",
            thread_create_argument_checks,
        ),
        (
            "threads_share_memory_and_report_exit_codes",
            threads_share_memory_and_report_exit_codes,
        ),
        (
            "a_user_signal_wakes_a_blocked_thread",
            a_user_signal_wakes_a_blocked_thread,
        ),
        (
            "thread_limit_and_slots_are_reused",
            thread_limit_and_slots_are_reused,
        ),
        (
            "second_thread_outlives_the_first",
            second_thread_outlives_the_first,
        ),
        (
            "a_thread_can_end_the_whole_process",
            a_thread_can_end_the_whole_process,
        ),
        (
            "a_fault_in_any_thread_ends_the_process",
            a_fault_in_any_thread_ends_the_process,
        ),
        (
            "the_last_thread_sets_the_exit_code",
            the_last_thread_sets_the_exit_code,
        ),
        (
            "many_threaded_children_do_not_leak",
            many_threaded_children_do_not_leak,
        ),
        (
            "threads_created_but_not_run_when_the_process_ends",
            threads_created_but_not_run_when_the_process_ends,
        ),
        (
            "kill_a_process_that_is_creating_threads",
            kill_a_process_that_is_creating_threads,
        ),
        (
            "thread_create_with_a_full_handle_table",
            thread_create_with_a_full_handle_table,
        ),
        (
            "a_thread_handle_closed_early_is_harmless",
            a_thread_handle_closed_early_is_harmless,
        ),
        (
            "a_thread_hog_cannot_exhaust_the_kernel",
            a_thread_hog_cannot_exhaust_the_kernel,
        ),
        (
            "replacing_a_handle_ends_a_siblings_wait",
            replacing_a_handle_ends_a_siblings_wait,
        ),
        (
            "a_thread_killed_before_it_ran_runs_no_user_code",
            a_thread_killed_before_it_ran_runs_no_user_code,
        ),
        (
            "kill_a_child_whose_threads_never_ran",
            kill_a_child_whose_threads_never_ran,
        ),
        (
            "kill_a_child_with_blocked_and_spinning_threads",
            kill_a_child_with_blocked_and_spinning_threads,
        ),
        (
            "closing_handles_early_and_kill_on_close",
            closing_handles_early_and_kill_on_close,
        ),
        (
            "a_thread_made_by_the_parent_faults_or_exits",
            a_thread_made_by_the_parent_faults_or_exits,
        ),
        ("socket_syscalls", socket_syscalls),
        ("socket_end_of_stream", socket_end_of_stream),
        (
            "socket_ends_travel_over_channels",
            socket_ends_travel_over_channels,
        ),
        (
            "a_child_reads_a_socket_to_the_end",
            a_child_reads_a_socket_to_the_end,
        ),
        (
            "a_blocked_reader_wakes_when_data_arrives",
            a_blocked_reader_wakes_when_data_arrives,
        ),
        (
            "a_blocked_writer_wakes_when_space_appears",
            a_blocked_writer_wakes_when_space_appears,
        ),
        ("socket_memory_is_bounded", socket_memory_is_bounded),
        (
            "dead_port_bindings_vanish_at_once",
            dead_port_bindings_vanish_at_once,
        ),
        ("port_argument_checks", port_argument_checks),
        (
            "port_wait_levels_timeouts_and_fairness",
            port_wait_levels_timeouts_and_fairness,
        ),
        (
            "port_wait_blocks_until_an_object_is_ready",
            port_wait_blocks_until_an_object_is_ready,
        ),
        (
            "port_wait_sees_socket_data_and_object_wait_works_on_a_port",
            port_wait_sees_socket_data_and_object_wait_works_on_a_port,
        ),
        (
            "kill_a_child_blocked_in_port_wait",
            kill_a_child_blocked_in_port_wait,
        ),
        (
            "the_object_cap_is_reached_without_a_kernel_panic",
            the_object_cap_is_reached_without_a_kernel_panic,
        ),
        (
            "mappings_per_process_are_bounded",
            mappings_per_process_are_bounded,
        ),
        (
            "queued_handles_are_charged_to_the_budget",
            queued_handles_are_charged_to_the_budget,
        ),
        (
            "kill_needs_the_kill_right_and_nothing_else",
            kill_needs_the_kill_right_and_nothing_else,
        ),
        ("fetch_the_hello_image", fetch_the_hello_image),
        (
            "runtime_startup_heap_and_stdio",
            runtime_startup_heap_and_stdio,
        ),
        (
            "runtime_heap_stops_at_its_cap",
            runtime_heap_stops_at_its_cap,
        ),
        (
            "runtime_refuses_a_bad_startup_message",
            runtime_refuses_a_bad_startup_message,
        ),
        (
            "runtime_without_a_startup_message",
            runtime_without_a_startup_message,
        ),
        (
            "runtime_io_fails_instead_of_spinning_without_wait",
            runtime_io_fails_instead_of_spinning_without_wait,
        ),
        (
            "launcher_spawns_a_program_from_the_boot_image",
            launcher_spawns_a_program_from_the_boot_image,
        ),
        (
            "the_process_handle_the_launcher_returns_cannot_manage",
            the_process_handle_the_launcher_returns_cannot_manage,
        ),
        (
            "the_launcher_refuses_bad_images_and_requests_without_leaks",
            the_launcher_refuses_bad_images_and_requests_without_leaks,
        ),
        (
            "many_launcher_spawns_do_not_leak",
            many_launcher_spawns_do_not_leak,
        ),
        ("memfsd_serves_the_boot_image", memfsd_serves_the_boot_image),
        (
            "memfsd_tmp_is_the_only_writable_place",
            memfsd_tmp_is_the_only_writable_place,
        ),
        (
            "memfsd_paths_cannot_escape_a_directory_handle",
            memfsd_paths_cannot_escape_a_directory_handle,
        ),
        ("memfsd_rights_only_shrink", memfsd_rights_only_shrink),
        ("memfsd_protocol_violations", memfsd_protocol_violations),
        (
            "memfsd_quotas_and_connection_limit",
            memfsd_quotas_and_connection_limit,
        ),
        (
            "memfsd_refuses_endpoints_it_cannot_serve_and_orphans",
            memfsd_refuses_endpoints_it_cannot_serve_and_orphans,
        ),
        (
            "memfsd_refuses_both_ends_of_one_channel",
            memfsd_refuses_both_ends_of_one_channel,
        ),
        (
            "memfsd_survives_memory_pressure_from_quota_legal_clients",
            memfsd_survives_memory_pressure_from_quota_legal_clients,
        ),
        (
            "memfsd_rights_are_checked_op_by_op",
            memfsd_rights_are_checked_op_by_op,
        ),
        (
            "memfsd_unlinked_open_files_live_until_closed",
            memfsd_unlinked_open_files_live_until_closed,
        ),
        (
            "memfsd_hands_out_programs_the_launcher_can_run",
            memfsd_hands_out_programs_the_launcher_can_run,
        ),
        (
            "memfsd_listing_pages_and_a_slow_client_stalls_only_itself",
            memfsd_listing_pages_and_a_slow_client_stalls_only_itself,
        ),
        (
            "memfsd_exits_when_every_client_is_gone",
            memfsd_exits_when_every_client_is_gone,
        ),
        (
            "syscalls_stay_fast_with_many_live_processes",
            syscalls_stay_fast_with_many_live_processes,
        ),
        (
            "object_info_names_objects_and_peers",
            object_info_names_objects_and_peers,
        ),
        (
            "wait_is_woken_by_the_peer_closing",
            wait_is_woken_by_the_peer_closing,
        ),
        (
            "user_cannot_forge_kernel_lines",
            user_cannot_forge_kernel_lines,
        ),
        ("rights_cannot_be_widened", rights_cannot_be_widened),
        ("stack_grows_lazily", stack_grows_lazily),
        ("syscall_layer_bounds", syscall_layer_bounds),
        ("cycles_do_not_grow_memory", cycles_do_not_grow_memory),
        (
            "job_syscalls_check_handles_rights_and_arguments",
            job_syscalls_check_handles_rights_and_arguments,
        ),
        (
            "job_limits_stop_a_process_bomb_and_usage_comes_back",
            job_limits_stop_a_process_bomb_and_usage_comes_back,
        ),
        (
            "job_limits_stop_a_thread_bomb",
            job_limits_stop_a_thread_bomb,
        ),
        (
            "job_memory_limits_stop_a_hog_and_are_credited_back",
            job_memory_limits_stop_a_hog_and_are_credited_back,
        ),
        (
            "a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling",
            a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling,
        ),
        (
            "job_kill_ends_every_process_below_and_forbids_more",
            job_kill_ends_every_process_below_and_forbids_more,
        ),
        ("jobs_do_not_leak", jobs_do_not_leak),
        (
            "a_refused_process_start_changes_nothing_and_can_be_retried",
            a_refused_process_start_changes_nothing_and_can_be_retried,
        ),
        (
            "page_tables_are_charged_to_the_job",
            page_tables_are_charged_to_the_job,
        ),
        (
            "a_job_limits_how_many_jobs_may_exist_below_it",
            a_job_limits_how_many_jobs_may_exist_below_it,
        ),
        (
            "job_quotas_stop_an_object_and_an_ipc_hog_and_everything_comes_back",
            job_quotas_stop_an_object_and_an_ipc_hog_and_everything_comes_back,
        ),
        (
            "threads_left_behind_when_init_exits",
            threads_left_behind_when_init_exits,
        ),
    ];
    for (name, test) in tests {
        if !report(name, test()) {
            failed += 1;
        }
    }
    println!("init: {} of {} user tests failed", failed, tests.len());
    failed
}
