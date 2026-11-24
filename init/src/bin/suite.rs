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

const BAD: u64 = 0x7777_0001; // a handle value nobody was ever given
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

/// The address just above the mapping that holds my stack (the kernel places it, HD2: no fixed address), found with `process_map_info` on a local.
fn my_stack_top() -> usize {
    let local = 0u8;
    let me = sys::process_self().a1 as u64;
    let mut rec = [0u64; 6];
    let _ = sys::process_map_info(me, core::ptr::addr_of!(local) as u64, rec.as_mut_ptr());
    (rec[0] + rec[1]) as usize
}

/// A failure detail with this text.
fn detail(what: &str) -> Detail {
    let mut d = Detail::new();
    let _ = core::fmt::Write::write_str(&mut d, what);
    d
}

fn channel() -> Result<(u64, u64), Detail> {
    let r = ok(sys::channel_create(), "channel_create")?;
    Ok((r.a1 as u64, r.a2 as u64))
}

fn event() -> Result<u64, Detail> {
    Ok(ok(sys::event_create(), "event_create")?.a1 as u64)
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
        st(rt::syscall(rt::abi::syscall::number(5), [0; 6])),
        Some(S::BadSyscall),
        "unassigned number 5"
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
    let dbg = rt::debug::handle() as usize;
    let bad = |ptr: usize, len: usize| st(rt::syscall(abi_debug(), [dbg, ptr, len, 0, 0, 0]));
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
    let top = my_stack_top();
    ensure!(
        (&local as *const u8 as usize) < top && top - (&local as *const u8 as usize) <= 64 * 4096,
        "my stack is the mapping the kernel placed"
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

/// HD4: `debug_write` and `debug_stat` need a debug resource: no handle, a handle of another kind, or one without the right is refused and prints or
/// reads nothing (a program without one can no longer forge a `##TEST` line).
fn debug_calls_need_a_debug_resource() -> Outcome {
    use rt::abi::syscall as nr;
    let dbg = rt::debug::handle();
    ensure!(dbg != 0, "the suite has its debug resource");
    // (the process-wide handle `sys::debug_write` and `sys::debug_stat` use is switched for each probe and put back)
    let via = |h: u64| {
        rt::debug::set(h);
        let r = st(sys::debug_write(b""));
        rt::debug::set(dbg);
        r
    };
    let stat = |h: u64| st(rt::syscall(nr::DEBUG_STAT, [h as usize, 0, 0, 0, 0, 0]));
    ensure_eq!(via(dbg), Some(S::Ok), "the resource itself writes");
    ensure_eq!(stat(dbg), Some(S::Ok), "and reads a counter");
    ensure_eq!(via(0), Some(S::BadHandle), "no handle: no write");
    ensure_eq!(stat(0), Some(S::BadHandle), "no handle: no counter");
    ensure_eq!(
        via(0x7777_7777),
        Some(S::BadHandle),
        "a handle that is not there"
    );
    let e = event()?;
    ensure_eq!(
        via(e),
        Some(S::WrongType),
        "an event is not a debug resource"
    );
    ensure_eq!(
        stat(e),
        Some(S::WrongType),
        "an event is not a debug resource"
    );
    let inspect_only = weaker(dbg, Rights::INSPECT.0)?;
    ensure_eq!(
        via(inspect_only),
        Some(S::AccessDenied),
        "INSPECT alone does not write"
    );
    ensure_eq!(stat(inspect_only), Some(S::Ok), "INSPECT reads");
    let write_only = weaker(dbg, Rights::WRITE.0)?;
    ensure_eq!(
        stat(write_only),
        Some(S::AccessDenied),
        "WRITE alone does not read a counter"
    );
    ensure_eq!(via(write_only), Some(S::Ok), "WRITE writes");
    for h in [inspect_only, write_only, e] {
        sys::handle_close(h);
    }
    // a second `debug_root` from the privileged suite is another handle to the same kind of authority
    let again = sys::debug_root();
    ensure_eq!(again.status, 0, "debug_root again");
    sys::handle_close(again.a1 as u64);
    // a refused `debug_stat` reads as 0, not as the selector it was given
    rt::debug::set(0);
    let refused = sys::debug_stat(105);
    rt::debug::set(dbg);
    ensure_eq!(
        refused,
        0,
        "a process with no debug resource reads no counters"
    );
    // a program the suite started with no startup message (so no debug resource, and not privileged) can neither forge a verdict line nor ask for the resource
    let (p, _mine) = launch(83)?;
    wait_ended(p, 500)?;
    let r = sys::process_info(p);
    ensure_eq!(
        (r.status, r.a1 as i64),
        (0, 0),
        "the confined child's debug_write was BAD_HANDLE and its debug_root ACCESS_DENIED"
    );
    Ok(())
}

fn abi_debug() -> usize {
    rt::abi::syscall::DEBUG_WRITE
}

/// AB1: a handle is 64 bits and the table grows on demand: one process holds twenty thousand handles (duplicates of one event: a handle is not an
/// object), slot numbers pass the old 13-bit limit, every one works, a closed one is refused, and a value with the generation bits changed names nothing.
fn a_process_holds_twenty_thousand_handles() -> Outcome {
    let e = event()?;
    let mut hs: Vec<u64> = Vec::new();
    hs.try_reserve(20_000).map_err(|_| msg_at(line!()))?;
    for _ in 0..20_000 {
        let d = sys::handle_duplicate(e, Rights::SAME.0);
        ensure_eq!(d.status, 0, "a duplicate");
        hs.push(d.a1 as u64);
    }
    let slot = |h: u64| h & 0xff_ffff;
    ensure!(
        hs.iter().any(|&h| slot(h) > 8191),
        "slot numbers above the old 13-bit limit are in use"
    );
    let mut sorted = hs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    ensure_eq!(sorted.len(), 20_000, "every handle value is different");
    for &h in hs.iter().step_by(997) {
        ensure_eq!(st(sys::object_signals(h)), Some(S::Ok), "a handle works");
    }
    // a value that differs from a live one only in its generation names nothing (and is not truncated to a live one)
    let forged = hs[5] ^ (1 << 40);
    ensure_eq!(
        st(sys::object_signals(forged)),
        Some(S::BadHandle),
        "a wrong generation"
    );
    let victim = hs.pop().ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(st(sys::handle_close(victim)), Some(S::Ok), "close one");
    ensure_eq!(
        st(sys::object_signals(victim)),
        Some(S::BadHandle),
        "a closed handle is refused"
    );
    for h in hs {
        sys::handle_close(h);
    }
    sys::handle_close(e);
    Ok(())
}

fn handle_syscalls_reject_bad_values() -> Outcome {
    for h in [
        0u64,
        0x0002_0001,
        BAD,
        u64::MAX,
        0x0001_0000,
        1 << 40 | 1,
        1 << 32,
    ] {
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
    let weak = ok(sys::handle_replace(e, Rights::WAIT.0), "replace")?.a1 as u64;
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
    let mut got = [0u64; 4];
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

fn big_messages_are_copied_outside_the_object_lock() -> Outcome {
    // T215: a message of 1 KiB or more is copied in before the call takes the object lock and out after it lets go; the answers are the ones of a small message
    let (a, b) = channel()?;
    let msg: Vec<u8> = (0..4096usize).map(|i| (i % 251) as u8).collect();
    ensure_eq!(
        st(sys::channel_write(
            a,
            msg.as_ptr(),
            msg.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write 4096 bytes"
    );
    let mut got = alloc::vec![0u8; 4096];
    let r = sys::channel_read(b, got.as_mut_ptr(), 4096, core::ptr::null_mut(), 0);
    ensure_eq!(st(r), Some(S::Ok), "read them");
    ensure_eq!(r.a1, 4096, "the length");
    ensure!(got == msg, "the bytes arrive as they were sent");
    // the order of errors is the small message's: a bad handle before a bad pointer, a bad pointer, then the write itself
    ensure_eq!(
        st(sys::channel_write(
            BAD,
            0x10 as *const u8,
            4096,
            core::ptr::null(),
            0
        )),
        Some(S::BadHandle),
        "bad handle, bad pointer"
    );
    let small = st(sys::channel_write(
        a,
        0x10 as *const u8,
        8,
        core::ptr::null(),
        0,
    ));
    ensure!(small != Some(S::Ok), "a bad pointer is refused: {small:?}");
    ensure_eq!(
        st(sys::channel_write(
            a,
            0x10 as *const u8,
            4096,
            core::ptr::null(),
            0
        )),
        small,
        "a big message says what a small one says"
    );
    // a bad destination loses no message, and the message is still whole after it
    ensure_eq!(
        st(sys::channel_write(
            a,
            msg.as_ptr(),
            msg.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "write again"
    );
    let bad_read = st(sys::channel_read(
        b,
        0x10 as *mut u8,
        4096,
        core::ptr::null_mut(),
        0,
    ));
    ensure!(
        bad_read != Some(S::Ok),
        "a bad destination is refused: {bad_read:?}"
    );
    let mut again = alloc::vec![0u8; 4096];
    let r = sys::channel_read(b, again.as_mut_ptr(), 4096, core::ptr::null_mut(), 0);
    ensure_eq!(st(r), Some(S::Ok), "the message was not lost");
    ensure!(again == msg, "and it is whole");
    // a stream is the same
    let (s, t) = socket()?;
    let w = sys::socket_write(s, msg.as_ptr(), msg.len());
    ensure_eq!(st(w), Some(S::Ok), "socket write");
    let mut sg = alloc::vec![0u8; 8192];
    let r = sys::socket_read(t, sg.as_mut_ptr(), 8192);
    ensure_eq!(st(r), Some(S::Ok), "socket read");
    ensure_eq!(r.a1, 4096, "the stream's length");
    ensure!(sg[..4096] == msg[..], "the stream's bytes");
    Ok(())
}

fn channel_syscall_errors() -> Outcome {
    let (a, b) = channel()?;
    let e = event()?;
    let e_no_transfer = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u64;
    let none8 = core::ptr::null::<u8>();
    let none32 = core::ptr::null::<u64>();
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
            0x10 as *const u64,
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
            ok(sys::handle_duplicate(a, Rights::SAME.0), "dup")?.a1 as u64,
            Rights::READ.0,
        ),
        "replace",
    )?
    .a1 as u64;
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
        st(sys::channel_write(a, none8, 0, 0x10 as *const u64, 1)),
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
            ok(sys::handle_duplicate(b, Rights::SAME.0), "dup")?.a1 as u64,
            Rights::WRITE.0,
        ),
        "replace",
    )?
    .a1 as u64;
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
    let mut hs = [0u64; 2];
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
    // The clock is real time (AB2) and the guest can stall for tens of milliseconds on a loaded host: generous slack above, none below.
    elapsed >= lo && (loose() || elapsed <= hi + 250)
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
    let weak = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u64;
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
        st(sys::object_wait(e, 1 << 8, 0)),
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

/// AB2: `clock_get` is nanoseconds since boot, from the `time` counter, not a tick count: readings are not all whole milliseconds,
/// and a wait of five milliseconds on the ns clock lasts between five and a few more.
fn clock_get_is_nanoseconds() -> Outcome {
    let t0 = sys::clock_ns();
    let mut fractional = false;
    let mut last = t0;
    for _ in 0..200 {
        let t = sys::clock_ns();
        ensure!(t >= last, "the clock went backwards: {last} then {t}");
        fractional |= !t.is_multiple_of(1_000_000);
        last = t;
    }
    ensure!(
        fractional,
        "200 readings, every one a whole millisecond: this is a tick count"
    );
    let d = t0 + 5_000_000;
    let r = sys::object_wait_ns(BOOTSTRAP, sig::READABLE, d);
    let after = sys::clock_ns();
    ensure_eq!(r.status, S::TimedOut.code(), "nothing arrives");
    ensure!(
        after >= d,
        "a wait on a 5 ms deadline ended {} ns early",
        d - after
    );
    ensure!(
        after - t0 < 500_000_000,
        "a wait on a 5 ms deadline took {} ns",
        after - t0
    );
    Ok(())
}

/// AB2 acceptance: no wait ends before its deadline, whatever the length (the kernel rounds a deadline up to the next tick, never
/// down): 300 waits of random length from 0 to 3 ms.
fn sleep_never_wakes_early() -> Outcome {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ sys::clock_ns();
    for i in 0..300 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (seed >> 33) % 3_000_000 + 1;
        let d = sys::clock_ns() + len;
        let r = sys::object_wait_ns(BOOTSTRAP, sig::READABLE, d);
        let after = sys::clock_ns();
        ensure_eq!(r.status, S::TimedOut.code(), "wait {i}");
        ensure!(
            after >= d,
            "wait {i}: a {len} ns wait ended {} ns early",
            d - after
        );
    }
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
            ok(sys::handle_duplicate(BOOTSTRAP, Rights::SAME.0), "dup")?.a1 as u64,
            Rights::READ.0,
        ),
        "replace",
    )?
    .a1 as u64;
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
            ok(sys::handle_duplicate(BOOTSTRAP, Rights::SAME.0), "dup")?.a1 as u64,
            Rights::WRITE.0,
        ),
        "replace",
    )?
    .a1 as u64;
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
    let wide = 1usize << 40; // flips a generation bit: a value that names nothing
    use rt::abi::syscall as nr;
    ensure_eq!(
        st(rt::syscall(
            nr::OBJECT_WAIT,
            [e as usize ^ wide, 1 << 24, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "object_wait with a wide handle"
    );
    // (a valid request record, so the handle is what is refused)
    let msg = [0u8; 4];
    let args = rt::CallArgs {
        req_bytes: msg.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: 4,
        req_nhandles: 0,
        rep_bytes: 0,
        rep_handles: 0,
        rep_max_bytes: 0,
        rep_max_handles: 0,
        deadline: 0,
    };
    ensure_eq!(
        st(sys::channel_call(BOOTSTRAP ^ wide as u64, &args)),
        Some(S::BadHandle),
        "channel_call with a value that names nothing"
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
    let weak = ok(sys::handle_replace(event()?, Rights::SIGNAL.0), "replace")?.a1 as u64;
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
    let mut events = [0u64; 300];
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
    let waited = sys::clock_get() - t0;
    ensure!(
        // (the lower bound is the check, with a tick of slack for the helper's clock starting at its read of the
        // request; the upper one only catches a lost wake-up and is loose because a loaded CI runner can delay
        // the helper by many ticks)
        within(waited, 1, 60),
        "the close came after about 3 ticks (waited {waited})"
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

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;
const TOP: u64 = 0x40_0000_0000;

/// The handle of the program image VMO the kernel-side helper sent us (0 until fetched).
static IMAGE: AtomicU64 = AtomicU64::new(0);
/// The `hello` program image (S4a runtime tests).
static HELLO_IMAGE: AtomicU64 = AtomicU64::new(0);

fn vmo(pages: u64) -> Result<u64, Detail> {
    Ok(ok(sys::vmo_create(pages), "vmo_create")?.a1 as u64)
}

fn proc_create() -> Result<u64, Detail> {
    Ok(ok(sys::process_create(), "process_create")?.a1 as u64)
}

fn weaker(h: u64, rights: u32) -> Result<u64, Detail> {
    let d = ok(sys::handle_duplicate(h, Rights::SAME.0), "duplicate")?.a1 as u64;
    Ok(ok(sys::handle_replace(d, rights), "replace")?.a1 as u64)
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
            [v as usize ^ (1usize << 40), 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "a wide handle"
    );
    // two handles, one VMO: a write through one shows through the other
    let dup = ok(sys::handle_duplicate(v, Rights::SAME.0), "dup")?.a1 as u64;
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
    // (address 0 is not mappable: ANYWHERE is a flag since MM7, and these two calls are refused as before)
    ensure_eq!(
        map(0, 0, 0, PF_R),
        Some(S::InvalidArgs),
        "ANYWHERE with no length"
    );
    ensure_eq!(
        map(0, 0, 100, PF_R),
        Some(S::InvalidArgs),
        "ANYWHERE with an unaligned length"
    );
    ensure_eq!(
        map(0x800, 0, 4096, PF_R),
        Some(S::InvalidArgs),
        "inside page zero"
    );
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
                p as usize ^ (1usize << 40),
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
    let go = |entry: u64, sp: u64, bs: u64| st(sys::process_start(p, entry, sp, bs, 0));
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
    let mut got = [0u64; 2];
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
fn launch(arg: u64) -> Result<(u64, u64), Detail> {
    launch_image(IMAGE.load(Ordering::Relaxed), arg)
}

fn launch_image(image: u64, arg: u64) -> Result<(u64, u64), Detail> {
    launch_image_in(image, arg, 0)
}

/// Build and start a child from `image` charged to `job` (a handle with MANAGE; 0: our own job).
/// With a job, our handles to the child's memory are closed after mapping, so that the job's
/// usage can be seen going back to zero when the child ends.
fn launch_image_in(image: u64, arg: u64, job: u64) -> Result<(u64, u64), Detail> {
    launch_image_with(image, arg, job, &|_| Ok(()))
}

/// `launch_image_in`, calling `before_start(process)` once the child is built and before it runs (to give it an exception channel, say).
fn launch_image_with(
    image: u64,
    arg: u64,
    job: u64,
    before_start: &dyn Fn(u64) -> Result<(), Detail>,
) -> Result<(u64, u64), Detail> {
    ensure!(image != 0, "no image was fetched");
    // (the image is read whole and parsed by the runtime's loader, which places a static-PIE, relocates it and maps its stack: HD2)
    let size = ok(sys::vmo_get_size(image), "the image's size")?.a1 as u64;
    let bytes = rt::spawn::read_image(image, 0, size).ok_or_else(|| detail("read the image"))?;
    let parsed = kcore::elf::parse(&bytes).map_err(|_| detail("parse the image"))?;
    let proc = if job == 0 {
        proc_create()?
    } else {
        ok(sys::process_create_in(job), "process_create_in")?.a1 as u64
    };
    let (entry, stack_top) = rt::spawn::map_image(proc, &parsed, &bytes, job).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("map the image: {e:?}"));
        d
    })?;
    before_start(proc)?;
    let (mine, theirs) = channel()?;
    ok(
        sys::process_start(proc, entry, stack_top, theirs, arg),
        "start",
    )?;
    Ok((proc, mine))
}

fn wait_ended(proc: u64, ticks: u64) -> Result<(), Detail> {
    let r = sys::object_wait(proc, sig::TERMINATED, sys::clock_get() + ticks);
    ensure_eq!(r.status, 0, "the process ended");
    ensure!(r.a1 as u32 & sig::TERMINATED != 0, "TERMINATED is set");
    Ok(())
}

/// Wait until a thread that a child program started has run its start-up and gone to sleep (its CPU time stops moving). The child's threads all
/// store their start argument in one place (`rt::start_arg`), so a thread started with another argument before this one has read its own would change what
/// this one does (T243: on a loaded four-hart run the first thread was seen starting more than 3 ms late).
fn started_and_blocked(t: u64) -> Outcome {
    let mut last = 0u64;
    let mut steady = 0u32;
    for _ in 0..400 {
        nap(5)?;
        let now = sys::thread_cpu_ns(t).a1 as u64;
        // (it has run at all, and has not moved over two naps: a release build's start-up is well under a millisecond)
        if now > 0 && now == last {
            steady += 1;
            if steady >= 2 {
                return Ok(());
            }
        } else {
            steady = 0;
        }
        last = now;
    }
    Err(detail("the thread never settled"))
}

/// `wait_ended` that says what the process and its threads look like when the wait runs out (T243: a failure seen only on the CI runners).
fn wait_ended_or_say(proc: u64, threads: &[u64], ticks: u64) -> Outcome {
    let r = sys::object_wait(proc, sig::TERMINATED, sys::clock_get() + ticks);
    if r.status == 0 {
        return Ok(());
    }
    let mut d = Detail::new();
    let _ = sys::debug_stat(132); // (the kernel prints its scheduler state: T243)
    let pi = sys::process_info(proc);
    let _ = core::fmt::Write::write_fmt(
        &mut d,
        format_args!(
            "the process did not end (status {}): signals {:#x}, info status {} a1 {} a2 {}",
            r.status,
            sys::object_signals(proc).a1,
            pi.status,
            pi.a1 as i64,
            pi.a2
        ),
    );
    for &t in threads {
        let ti = sys::thread_info(t);
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "; thread {t:#x}: signals {:#x}, info status {} cpu_ns {}",
                sys::object_signals(t).a1,
                ti.status,
                sys::thread_cpu_ns(t).a1
            ),
        );
    }
    Err(d)
}

/// HD2: where the kernel puts a child's code, its stack and an ANYWHERE mapping differs from launch to launch (ten launches: at least eight distinct values of each) when
/// the kernel randomises, and is the same every time under `aslr=off`. Each address is read from the child's own address space before it runs.
fn programs_are_placed_at_random() -> Outcome {
    let image = IMAGE.load(Ordering::Relaxed);
    let off = sys::debug_stat(128) == 1;
    let seeded = sys::debug_stat(108) == 1;
    let seen = core::cell::RefCell::new([Vec::<u64>::new(), Vec::new(), Vec::new()]);
    for _ in 0..10 {
        let (p, m) = launch_image_with(image, 8, 0, &|proc| {
            let mut at = 0u64;
            let (mut text, mut stack) = (0u64, 0u64);
            for _ in 0..64 {
                let mut rec = [0u64; 6];
                let next = ok(
                    sys::process_map_info(proc, at, rec.as_mut_ptr()),
                    "map_info",
                )?
                .a1 as u64;
                if rec[2] & 0b100 != 0 {
                    text = rec[0];
                } else if rec[1] == 64 * 4096 && rec[2] & 0b010 != 0 {
                    stack = rec[0];
                }
                if next == 0 {
                    break;
                }
                at = next;
            }
            let v = vmo(4)?;
            let any = ok(
                sys::process_map_anywhere(proc, v, 0, 4 * 4096, PF_R | PF_W),
                "map anywhere",
            )?
            .a1 as u64;
            sys::handle_close(v);
            let mut seen = seen.borrow_mut();
            seen[0].push(text);
            seen[1].push(stack);
            seen[2].push(any);
            Ok(())
        })?;
        wait_ended(p, 500)?;
        sys::handle_close(p);
        sys::handle_close(m);
    }
    for (i, what) in ["code", "stack", "an ANYWHERE mapping"].iter().enumerate() {
        let mut v = seen.borrow()[i].clone();
        ensure_eq!(v.len(), 10, "ten launches were seen");
        ensure!(v.iter().all(|&a| a != 0), "{what} was found in every child");
        v.sort_unstable();
        v.dedup();
        if off {
            ensure_eq!(
                v.len(),
                1,
                "aslr=off: the {what} is where it was the last time"
            );
        } else if seeded {
            ensure!(
                v.len() >= 8,
                "the {what} took only {} distinct places in ten launches",
                v.len()
            );
        }
    }
    Ok(())
}

/// AB7: the system library is mapped into every process at a place of its own, text read-only and executable and a read-only data page after it; the first thread
/// has its base in `a2`; the data page holds what the kernel put there; and `clock_get` is computed in user mode, with no trap.
fn the_system_library_is_in_every_process() -> Outcome {
    // this process: the library was found, its page is filled
    let base = rt::library::base();
    ensure!(base != 0, "the runtime found the library");
    let page = rt::library::page().ok_or_else(|| detail("read the data page"))?;
    ensure_eq!(page.abi_version, libsys::page::ABI, "the ABI version");
    ensure_eq!(page.page_size, 4096, "the page size");
    ensure!(
        page.time_mult != 0 && page.time_shift == 32,
        "the clock's constants"
    );
    ensure!(page.harts_online >= 1, "harts online");
    ensure!(
        page.time_base <= sys::debug_stat(6),
        "the clock's zero is not later than now"
    );
    // its mappings: the text (read, execute) at the base, and the data page (read only) behind it
    let (text_start, text_end) = rt::library::text();
    ensure_eq!(text_start, base, "the text starts at the base");
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let (mut text, mut data) = (None, None);
    let mut at = 0u64;
    for _ in 0..4096 {
        let mut rec = [0u64; 6];
        let next = ok(sys::process_map_info(me, at, rec.as_mut_ptr()), "map_info")?.a1 as u64;
        if rec[0] == base as u64 {
            text = Some(rec);
        } else if rec[0] > base as u64 && rec[0] < base as u64 + 0x10000 && rec[1] == 4096 {
            data = Some(rec);
        }
        if next == 0 {
            break;
        }
        at = next;
    }
    let (text, data) = (
        text.ok_or_else(|| detail("no text mapping at the base"))?,
        data.ok_or_else(|| detail("no data page mapping"))?,
    );
    ensure!(
        text[2] & 0b101 == 0b101 && text[2] & 0b010 == 0,
        "the text is read and execute, not write: {:#b}",
        text[2]
    );
    ensure!(
        text[1] >= (text_end - text_start) as u64,
        "the mapping covers the code"
    );
    ensure_eq!(data[2] & 0b111, 0b001, "the data page is read only");
    // two processes: different bases when the kernel randomises, the same under aslr=off; every child's first thread was given its own base
    let off = sys::debug_stat(128) == 1;
    let mut bases = Vec::new();
    for _ in 0..10 {
        // (a child that blocks, so that it is still there to be asked: one that exits at once may be gone before the question, which says BAD_STATE)
        let (p, m) = launch(16)?;
        let mut at = 0u64;
        let mut small = (0u64, u64::MAX);
        for _ in 0..4096 {
            let mut rec = [0u64; 6];
            let next = ok(
                sys::process_map_info(p, at, rec.as_mut_ptr()),
                "child map_info",
            )?
            .a1 as u64;
            // (the library's text: the smallest executable mapping)
            if rec[2] & 0b100 != 0 && rec[1] < small.1 {
                small = (rec[0], rec[1]);
            }
            if next == 0 {
                break;
            }
            at = next;
        }
        ensure!(small.0 != 0, "the child has the library");
        bases.push(small.0);
        sys::process_kill(p, 0);
        wait_ended(p, 500)?;
        sys::handle_close(p);
        sys::handle_close(m);
    }
    bases.sort_unstable();
    bases.dedup();
    if off {
        ensure_eq!(bases.len(), 1, "aslr=off: one place");
    } else if sys::debug_stat(108) == 1 {
        ensure!(
            bases.len() >= 8,
            "ten children had only {} distinct library places",
            bases.len()
        );
    }
    // `clock_get` through the library never enters the kernel; the same call through the raw entry does (the control)
    let before = sys::debug_stat(131);
    for _ in 0..1000 {
        core::hint::black_box(sys::clock_get());
    }
    ensure_eq!(
        sys::debug_stat(131),
        before,
        "1000 clock_get calls entered the kernel"
    );
    for _ in 0..10 {
        core::hint::black_box(rt::library::raw(rt::abi::syscall::CLOCK_GET, [0; 6]));
    }
    ensure_eq!(
        sys::debug_stat(131),
        before + 10,
        "the control: ten raw calls entered it ten times"
    );
    // and the two clocks agree
    let (a, b) = (
        sys::clock_ns(),
        rt::library::raw(rt::abi::syscall::CLOCK_GET, [0; 6]).a1 as u64,
    );
    ensure!(
        b >= a && b - a < 50_000_000,
        "the user-mode clock ({a}) and the kernel's ({b}) agree"
    );
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
    for (variant, want) in [(1u64, -1001i64), (7, -1001), (5, -1002), (84, -1002)] {
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

/// Floating-point state is private to each thread and process (slice A, OD76): four processes keep
/// different patterns in the FP registers (and different rounding modes) while they are preempted,
/// yield and migrate between harts, one process runs three threads with three patterns, and a process
/// that first touches the FP unit while all that is going on finds zeros.
fn fp_state_is_private_to_each_thread_and_process() -> Outcome {
    let mut procs = Vec::new();
    for seed in 1..=4u64 {
        let mode = seed % 2;
        let (p, _mine) = launch(60 | (mode << 8) | (seed << 16))?;
        procs.push(p);
    }
    let (p, _m) = launch(62)?;
    procs.push(p);
    nap_ticks(5);
    let (fresh, _m) = launch(61)?;
    wait_ended(fresh, 3000)?;
    ensure_eq!(
        sys::process_info(fresh).a1 as i64,
        0,
        "a process's first FP use sees zero registers"
    );
    for (i, p) in procs.into_iter().enumerate() {
        wait_ended(p, 6000)?;
        let code = sys::process_info(p).a1 as i64;
        sys::handle_close(p);
        ensure_eq!(code, 0, "FP process {i} kept its own registers");
    }
    // a process that used FP and then executed a genuinely illegal instruction is still just killed ...
    let (bad, _m) = launch(63)?;
    wait_ended(bad, 3000)?;
    ensure_eq!(
        sys::process_info(bad).a1 as i64,
        -1002,
        "an illegal instruction after FP use is fatal"
    );
    sys::handle_close(bad);
    // ... and what it left in the unit is not seen by the next process
    let (fresh, _m) = launch(61)?;
    wait_ended(fresh, 3000)?;
    ensure_eq!(
        sys::process_info(fresh).a1 as i64,
        0,
        "a killed process's FP registers are not inherited"
    );
    sys::handle_close(fresh);
    Ok(())
}

/// PL2: vector state is per thread (the twin of the FP test above): a first use reads zeros, patterns survive preemption and yields, three threads
/// of one process keep their own, and a process that used the unit and then ran an illegal instruction is still just killed.
fn vector_state_is_private_to_each_thread_and_process() -> Outcome {
    let mut procs = Vec::new();
    for seed in 1..=4u64 {
        let mode = seed % 2;
        let (p, _mine) = launch(92 | (mode << 8) | (seed << 16))?;
        procs.push(p);
    }
    let (p, _m) = launch(94)?;
    procs.push(p);
    nap_ticks(5);
    let (fresh, _m) = launch(93)?;
    wait_ended(fresh, 3000)?;
    ensure_eq!(
        sys::process_info(fresh).a1 as i64,
        0,
        "a process's first vector use sees zero registers"
    );
    sys::handle_close(fresh);
    for (i, p) in procs.into_iter().enumerate() {
        wait_ended(p, 6000)?;
        let code = sys::process_info(p).a1 as i64;
        sys::handle_close(p);
        ensure_eq!(code, 0, "vector process {i} kept its own registers");
    }
    // the edge states: a first instruction that is `vsetvli`, `vill` and `vstart` across a yield, a first instruction that is a vector load
    for (arg, what) in [
        (96u64, "vill and vstart survive a yield"),
        (97, "a vector load enables the unit"),
        (98, "a vector loop and cbo.zero work in the RVA23U64 build"),
    ] {
        let (e, _m) = launch(arg)?;
        wait_ended(e, 3000)?;
        ensure_eq!(sys::process_info(e).a1 as i64, 0, "{}", what);
        sys::handle_close(e);
    }
    let (bad, _m) = launch(95)?;
    wait_ended(bad, 3000)?;
    ensure_eq!(
        sys::process_info(bad).a1 as i64,
        -1002,
        "an illegal instruction after vector use is fatal"
    );
    sys::handle_close(bad);
    // what it left in the unit is not seen by the next process
    let (fresh, _m) = launch(93)?;
    wait_ended(fresh, 3000)?;
    ensure_eq!(
        sys::process_info(fresh).a1 as i64,
        0,
        "a killed process's vector registers are not inherited"
    );
    sys::handle_close(fresh);
    Ok(())
}

// --- The wake path: counters, the bystander baseline, a benchmark (roadmap slice W, step 0) ---------

/// True since slice W step 3: `object_wait` and `channel_call` waiters are woken by id, so bystanders are
/// not woken (the port waits join in step 4). While it was false this test recorded the defect.
const TARGETED_WAKE: bool = true;

/// A bystander: blocked on an event nobody signals until the test ends it.
extern "C" fn bystander(arg: usize) -> ! {
    sys::object_wait(arg as u64, 1 << 24, sys::clock_get() + 60_000);
    sys::thread_exit(0)
}

/// Echoes `ROUND_TRIPS` eight-byte messages on its end of a channel.
extern "C" fn pong(arg: usize) -> ! {
    let mut buf = [0u8; 8];
    for _ in 0..ROUND_TRIPS {
        sys::object_wait(arg as u64, sig::READABLE, sys::clock_get() + 60_000);
        let r = sys::channel_read(arg as u64, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
        if r.status != 0 {
            break;
        }
        sys::channel_write(arg as u64, buf.as_ptr(), 8, core::ptr::null(), 0);
    }
    sys::thread_exit(0)
}

const ROUND_TRIPS: usize = 60;

/// One ping-pong run of `ROUND_TRIPS` round trips between this thread and a partner thread; returns
/// (guest ticks, blocks, spurious wakes, wakes) it took.
fn ping_pong() -> Result<(u64, u64, u64, u64), Detail> {
    let (mine, theirs) = channel()?;
    let t = new_thread(pong, theirs as usize)?;
    nap_ticks(2);
    let before = (
        sys::clock_get(),
        sys::debug_stat(0),
        sys::debug_stat(1),
        sys::debug_stat(2),
    );
    let mut buf = [0u8; 8];
    for i in 0..ROUND_TRIPS {
        let msg = (i as u64).to_le_bytes();
        ensure_eq!(
            st(sys::channel_write(
                mine,
                msg.as_ptr(),
                8,
                core::ptr::null(),
                0
            )),
            Some(S::Ok),
            "ping"
        );
        let w = sys::object_wait(mine, sig::READABLE, sys::clock_get() + 5000);
        ensure_eq!(w.status, 0, "the pong answered");
        ensure_eq!(
            st(sys::channel_read(
                mine,
                buf.as_mut_ptr(),
                8,
                core::ptr::null_mut(),
                0
            )),
            Some(S::Ok),
            "pong"
        );
    }
    let after = (
        sys::clock_get(),
        sys::debug_stat(0),
        sys::debug_stat(1),
        sys::debug_stat(2),
    );
    sys::handle_close(t);
    sys::handle_close(mine);
    Ok((
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2,
        after.3 - before.3,
    ))
}

/// The counters count, and today's design wakes bystanders: the same ping-pong with four threads
/// blocked on unrelated events costs spurious wakes (and time) that the quiet run does not. The numbers
/// go to the log (`s7: wake baseline ...`) as the before-picture for slice W; when `TARGETED_WAKE` is
/// true the same measurement must show no extra spurious wakes at all.
fn wake_baseline_bystanders_and_counters() -> Outcome {
    let quiet = ping_pong()?;
    let mut evs = Vec::new();
    for _ in 0..4 {
        let e = event()?;
        new_thread(bystander, e as usize)?;
        evs.push(e);
    }
    nap_ticks(5); // let them block
    let busy = ping_pong()?;
    for e in &evs {
        sys::event_signal(*e, 0, 1 << 24);
    }
    nap_ticks(3);
    let line = alloc::format!(
        "s7: wake baseline: 0 bystanders {} ticks {} blocks {} spurious {} wakes; 4 bystanders {} ticks {} blocks {} spurious {} wakes",
        quiet.0, quiet.1, quiet.2, quiet.3, busy.0, busy.1, busy.2, busy.3
    );
    sys::debug_write(line.as_bytes());
    ensure!(
        quiet.1 > 0 && quiet.3 > 0,
        "the counters count blocks and wakes"
    );
    if TARGETED_WAKE {
        ensure!(
            busy.2 <= quiet.2 + 30,
            "bystanders are not woken: spurious wakes {} against {} without them",
            busy.2,
            quiet.2
        );
        // and the ping-pong itself no longer wakes anyone that is not waiting for it (it was four
        // spurious wakes per round trip, 241 over the run, before step 3)
        ensure!(
            quiet.2 <= 30,
            "a ping-pong costs almost no spurious wakes: {} over {} round trips",
            quiet.2,
            ROUND_TRIPS
        );
    } else {
        ensure!(
            busy.2 > quiet.2,
            "the baseline shows the defect: bystanders cost spurious wakes ({} against {})",
            busy.2,
            quiet.2
        );
    }
    Ok(())
}

static BYSTANDER_FUTEX: AtomicU32 = AtomicU32::new(0);

/// A futex bystander: asleep on a word nobody wakes until the test ends it.
extern "C" fn futex_bystander(_: usize) -> ! {
    sys::futex_wait(BYSTANDER_FUTEX.as_ptr(), 0, sys::clock_get() + 60_000);
    sys::thread_exit(0)
}

/// Slice W step 2: futex sleepers are woken by id, not by the global queue, so four of them asleep
/// through a ping-pong cost no spurious wakes (the object_wait bystanders of the baseline test still do,
/// until step 3), and waking them by `futex_wake` still works.
fn futex_sleepers_are_not_woken_by_unrelated_syscalls() -> Outcome {
    BYSTANDER_FUTEX.store(0, Ordering::SeqCst);
    let quiet = ping_pong()?;
    for _ in 0..4 {
        new_thread(futex_bystander, 0)?;
    }
    nap_ticks(5); // let them sleep
    let busy = ping_pong()?;
    let line = alloc::format!(
        "s7: futex bystanders: quiet {} spurious, with 4 futex sleepers {} spurious",
        quiet.2,
        busy.2
    );
    sys::debug_write(line.as_bytes());
    // (the quiet run itself varies by a handful of wakes with timing, more at four harts; four
    // sleepers woken at every syscall would add 240, so the margin cannot hide the defect)
    ensure!(
        busy.2 <= quiet.2 + 30,
        "futex sleepers are not woken by unrelated syscalls: {} spurious wakes against {} without them",
        busy.2,
        quiet.2
    );
    // and a real wake still reaches all of them
    BYSTANDER_FUTEX.store(1, Ordering::SeqCst);
    let r = sys::futex_wake(BYSTANDER_FUTEX.as_ptr(), 10);
    ensure_eq!(
        (r.status, r.a1),
        (0, 4),
        "futex_wake woke the four sleepers"
    );
    nap_ticks(3);
    Ok(())
}

/// A port bystander: blocked in `port_wait` on a port nobody binds.
extern "C" fn port_bystander(arg: usize) -> ! {
    sys::port_wait(arg as u64, sys::clock_get() + 60_000);
    sys::thread_exit(0)
}

/// Slice W step 5: port waiters are woken by id too, so four threads blocked in `port_wait` through a
/// ping-pong cost no spurious wakes (the global wait queue, OD43, is gone), and a binding that becomes
/// ready still wakes its waiter.
fn port_waiters_are_not_woken_by_unrelated_syscalls() -> Outcome {
    let quiet = ping_pong()?;
    let mut ports = Vec::new();
    for _ in 0..4 {
        let p = port()?;
        new_thread(port_bystander, p as usize)?;
        ports.push(p);
    }
    nap_ticks(5);
    let busy = ping_pong()?;
    let line = alloc::format!(
        "s7: port bystanders: quiet {} spurious, with 4 port waiters {} spurious",
        quiet.2,
        busy.2
    );
    sys::debug_write(line.as_bytes());
    ensure!(
        busy.2 <= quiet.2 + 30,
        "port waiters are not woken by unrelated syscalls: {} spurious wakes against {} without them",
        busy.2,
        quiet.2
    );
    // a ready binding still wakes its waiter: bind an event to the first port and signal it
    let ev = event()?;
    ensure_eq!(
        st(sys::port_bind(ports[0], ev, 1 << 24, 9)),
        Some(S::Ok),
        "bind"
    );
    sys::event_signal(ev, 0, 1 << 24);
    nap_ticks(5);
    let r = sys::port_wait(ports[0], 0);
    ensure_eq!((r.status, r.a1), (0, 9), "the bound event is reported");
    // release the other bystanders by signalling nothing: they end at their deadline in the log of
    // the process; close the ports to end their waits now
    for p in &ports[1..] {
        sys::handle_close(*p);
    }
    nap_ticks(3);
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

// ---- process_suspend and process_resume (SIG2) ----

/// Read and drop every message waiting on `ch`; how many there were.
fn drain_reports(ch: u64) -> usize {
    let mut n = 0;
    let mut buf = [0u8; 64];
    while sys::channel_read(ch, buf.as_mut_ptr(), 64, core::ptr::null_mut(), 0).status == 0 {
        n += 1;
    }
    n
}

/// Reports from a counting child (`crash` mode 71) arrive within `ticks`.
fn reports_arrive(ch: u64, ticks: u64) -> bool {
    sys::object_wait(ch, sig::READABLE, sys::clock_get() + ticks).status == 0
}

/// The child is silent: nothing arrives for `ticks`, after the reports in flight were dropped.
fn child_is_silent(ch: u64, ticks: u64) -> Outcome {
    nap(8)?; // time to park (a thread on another hart is kicked at once, a tick at the latest)
    drain_reports(ch);
    nap(ticks)?;
    ensure_eq!(drain_reports(ch), 0, "a stopped process runs no user code");
    Ok(())
}

fn process_suspend_stops_a_spinner_and_resume_restarts_it() -> Outcome {
    let (p, mine) = launch(71)?;
    ensure!(reports_arrive(mine, 500), "the child counts");
    ensure_eq!(st(sys::process_suspend(p)), Some(S::Ok), "suspend");
    ensure_eq!(
        sys::object_signals(p).a1 as u32 & sig::STOPPED,
        sig::STOPPED,
        "STOPPED is raised"
    );
    child_is_silent(mine, 40)?;
    ensure_eq!(
        st(sys::process_suspend(p)),
        Some(S::Ok),
        "suspending again is fine"
    );
    child_is_silent(mine, 10)?;
    ensure_eq!(
        st(sys::process_info(p)),
        Some(S::BadState),
        "a stopped process has not ended"
    );
    ensure_eq!(st(sys::process_resume(p)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_signals(p).a1 as u32 & sig::STOPPED,
        0,
        "STOPPED is cleared"
    );
    ensure!(reports_arrive(mine, 500), "the child counts again");
    ensure_eq!(
        st(sys::process_resume(p)),
        Some(S::Ok),
        "resuming a running process is fine"
    );
    ensure_eq!(st(sys::process_kill(p, 5)), Some(S::Ok), "kill");
    wait_ended(p, 500)?;
    ensure_eq!(
        st(sys::process_suspend(p)),
        Some(S::BadState),
        "an ended process cannot be stopped"
    );
    ensure_eq!(st(sys::process_resume(p)), Some(S::BadState), "or resumed");
    Ok(())
}

fn process_suspend_stops_every_thread_on_every_hart() -> Outcome {
    let (p, mine) = launch(71)?;
    // three more counting threads (they share the bootstrap channel), so with several harts some run elsewhere
    let mut ts = [0u64; 3];
    for (i, t) in ts.iter_mut().enumerate() {
        *t = child_thread(p, i as u64, 71)?;
    }
    ensure!(reports_arrive(mine, 500), "the threads count");
    ensure_eq!(st(sys::process_suspend(p)), Some(S::Ok), "suspend");
    child_is_silent(mine, 40)?;
    // a thread made in a stopped process runs nothing until the resume
    let late = child_thread(p, 3, 71)?;
    child_is_silent(mine, 40)?;
    ensure_eq!(st(sys::process_resume(p)), Some(S::Ok), "resume");
    ensure!(reports_arrive(mine, 500), "they count again");
    ensure_eq!(st(sys::process_kill(p, -4)), Some(S::Ok), "kill");
    wait_ended(p, 1000)?;
    for t in ts.into_iter().chain([late]) {
        thread_ended_with(t, -4)?;
    }
    Ok(())
}

fn a_stopped_process_whose_wait_ends_stays_parked_until_resumed() -> Outcome {
    // `crash` mode 14 waits for one message, echoes it reversed and exits 3
    let (p, mine) = launch(14)?;
    nap(5)?;
    ensure_eq!(
        st(sys::process_suspend(p)),
        Some(S::Ok),
        "suspend the blocked child"
    );
    ensure_eq!(
        st(sys::channel_write(
            mine,
            b"abc".as_ptr(),
            3,
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send"
    );
    nap(40)?;
    ensure_eq!(
        sys::object_signals(mine).a1 as u32 & sig::READABLE,
        0,
        "its wait ended but it did not run"
    );
    ensure_eq!(
        st(sys::process_info(p)),
        Some(S::BadState),
        "and it has not exited"
    );
    ensure_eq!(st(sys::process_resume(p)), Some(S::Ok), "resume");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 3, "it finished its work");
    let mut buf = [0u8; 8];
    let r = sys::channel_read(mine, buf.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        (r.status, &buf[..r.a1.min(8)]),
        (0, &b"cba"[..]),
        "the echo"
    );
    Ok(())
}

fn a_stopped_process_can_be_killed_and_suspend_needs_the_kill_right() -> Outcome {
    let (p, _mine) = launch(16)?;
    ensure_eq!(
        st(sys::process_suspend(p)),
        Some(S::Ok),
        "suspend a blocked child"
    );
    nap(5)?;
    ensure_eq!(
        st(sys::process_kill(p, -7)),
        Some(S::Ok),
        "kill it while stopped"
    );
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -7, "the kill's code");
    // rights, handle types
    let (q, _m) = launch(15)?;
    let no_kill = weaker(q, Rights::PROCESS.0 & !Rights::KILL.0)?;
    ensure_eq!(
        st(sys::process_suspend(no_kill)),
        Some(S::AccessDenied),
        "suspend needs KILL"
    );
    ensure_eq!(
        st(sys::process_resume(no_kill)),
        Some(S::AccessDenied),
        "resume needs KILL"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::process_suspend(ev)),
        Some(S::WrongType),
        "a process handle"
    );
    ensure_eq!(
        st(sys::process_resume(9999)),
        Some(S::BadHandle),
        "a handle that is not one"
    );
    let _ = sys::process_kill(q, 1);
    Ok(())
}

fn a_stopped_process_with_parked_threads_is_killed_without_a_resume() -> Outcome {
    let (p, mine) = launch(71)?;
    let t = child_thread(p, 0, 71)?;
    ensure!(reports_arrive(mine, 500), "the threads count");
    ensure_eq!(st(sys::process_suspend(p)), Some(S::Ok), "suspend");
    let late = child_thread(p, 1, 71)?; // parked before it ever ran user code
    child_is_silent(mine, 20)?;
    ensure_eq!(
        st(sys::process_kill(p, -6)),
        Some(S::Ok),
        "kill it while its threads are parked"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -6, "the kill's code");
    thread_ended_with(t, -6)?;
    thread_ended_with(late, -6)
}

fn a_process_can_stop_itself() -> Outcome {
    let (p, mine) = launch(72)?;
    let me_with_kill = weaker(p, Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0)?;
    ensure_eq!(
        st(sys::channel_write(
            mine,
            b"k".as_ptr(),
            1,
            [me_with_kill].as_ptr(),
            1
        )),
        Some(S::Ok),
        "send the child a handle to itself"
    );
    nap(30)?;
    ensure_eq!(
        sys::object_signals(p).a1 as u32 & sig::STOPPED,
        sig::STOPPED,
        "it stopped itself"
    );
    ensure_eq!(
        sys::object_signals(mine).a1 as u32 & sig::READABLE,
        0,
        "and ran nothing more"
    );
    ensure_eq!(
        st(sys::process_resume(p)),
        Some(S::Ok),
        "resume it from outside"
    );
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 4, "it went on to its end");
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
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
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
    let again = ok(sys::process_self(), "a second self handle")?.a1 as u64;
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
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
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

/// MM6: `vmo_decommit` and `vmo_set_size` from user mode. A resizable VMO is mapped and written; a decommitted range reads zero through
/// the mapping (its translations are gone) and through the VMO, the rest keeps its data; the argument and rights errors; a resize changes
/// `vmo_get_size`, a shrink takes the tail's data with it.
fn vmo_decommit_and_resize_from_user_mode() -> Outcome {
    const BASE: u64 = 0x540_0000;
    let me = me()?;
    let v = ok(
        sys::vmo_create_flags(8, 0, rt::abi::vmo_flags::RESIZABLE),
        "a resizable VMO",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, v, BASE, 0, 8 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map it"
    );
    let word = |p: u64| (p as u32 + 1) * 0x1111;
    for p in 0..8u64 {
        // SAFETY: BASE..BASE + 8 pages is mapped read-write in this process.
        unsafe { core::ptr::write_volatile((BASE + p * 4096) as *mut u32, word(p)) };
    }
    ensure_eq!(
        st(sys::vmo_decommit(v, 2 * 4096, 3 * 4096)),
        Some(S::Ok),
        "decommit pages 2 to 4"
    );
    for p in 0..8u64 {
        // SAFETY: as above (a decommitted page faults in again, as zeros).
        let got = unsafe { core::ptr::read_volatile((BASE + p * 4096) as *const u32) };
        let want = if (2..5).contains(&p) { 0 } else { word(p) };
        ensure_eq!(got, want, "page {p} through the mapping");
        let mut b = [0u8; 4];
        ensure_eq!(
            st(sys::vmo_read(v, p * 4096, b.as_mut_ptr(), 4)),
            Some(S::Ok),
            "read through the VMO"
        );
        ensure_eq!(u32::from_le_bytes(b), want, "page {p} through the VMO");
    }
    // the errors
    let ev = event()?;
    for (what, call) in [
        ("decommit", sys::vmo_decommit(BAD, 0, 4096)),
        ("set_size", sys::vmo_set_size(BAD, 4096)),
    ] {
        ensure_eq!(
            st(call),
            Some(S::BadHandle),
            "{what} on a handle that is not one"
        );
    }
    ensure_eq!(
        st(sys::vmo_decommit(ev, 0, 4096)),
        Some(S::WrongType),
        "decommit on an event"
    );
    ensure_eq!(
        st(sys::vmo_set_size(ev, 4096)),
        Some(S::WrongType),
        "set_size on an event"
    );
    sys::handle_close(ev);
    ensure_eq!(
        st(sys::vmo_decommit(v, 100, 4096)),
        Some(S::InvalidArgs),
        "unaligned offset"
    );
    ensure_eq!(
        st(sys::vmo_decommit(v, 0, 100)),
        Some(S::InvalidArgs),
        "unaligned length"
    );
    ensure_eq!(
        st(sys::vmo_decommit(v, 0, 0)),
        Some(S::InvalidArgs),
        "no length"
    );
    ensure_eq!(
        st(sys::vmo_decommit(v, 4 * 4096, 5 * 4096)),
        Some(S::InvalidArgs),
        "past the end"
    );
    let weak = ok(
        sys::handle_duplicate(v, Rights::READ.0 | Rights::MAP.0),
        "a duplicate without WRITE",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::vmo_decommit(weak, 0, 4096)),
        Some(S::AccessDenied),
        "decommit needs WRITE"
    );
    ensure_eq!(
        st(sys::vmo_set_size(weak, 4096)),
        Some(S::AccessDenied),
        "set_size needs WRITE"
    );
    let fixed = vmo(2)?;
    ensure_eq!(
        st(sys::vmo_set_size(fixed, 4 * 4096)),
        Some(S::AccessDenied),
        "a VMO that is not resizable"
    );
    ensure_eq!(
        st(sys::vmo_set_size(v, 100)),
        Some(S::InvalidArgs),
        "a size that is not whole pages"
    );
    ensure_eq!(st(sys::vmo_set_size(v, 0)), Some(S::InvalidArgs), "no size");
    // resizing
    ensure_eq!(st(sys::vmo_set_size(v, 12 * 4096)), Some(S::Ok), "grow");
    ensure_eq!(sys::vmo_get_size(v).a1 as u64, 12 * 4096, "the new size");
    ensure_eq!(st(sys::vmo_set_size(v, 5 * 4096)), Some(S::Ok), "shrink");
    ensure_eq!(sys::vmo_get_size(v).a1 as u64, 5 * 4096, "the shrunk size");
    ensure_eq!(st(sys::vmo_set_size(v, 8 * 4096)), Some(S::Ok), "grow back");
    let mut b = [0xffu8; 4];
    ensure_eq!(
        st(sys::vmo_read(v, 6 * 4096, b.as_mut_ptr(), 4)),
        Some(S::Ok),
        "read a page that was cut off"
    );
    ensure_eq!(u32::from_le_bytes(b), 0, "what the shrink took is gone");
    ensure_eq!(
        st(sys::vmo_read(v, 4 * 4096, b.as_mut_ptr(), 4)),
        Some(S::Ok),
        "read the last page that stayed"
    );
    ensure_eq!(
        u32::from_le_bytes(b),
        0,
        "(page 4 was decommitted before: it reads zero)"
    );
    ensure_eq!(st(sys::process_unmap(me, BASE)), Some(S::Ok), "unmap");
    // V1: a process of another job that maps the range stops a decommit and a shrink (nothing changes), until it unmaps
    {
        let root = root_job()?;
        let j = job(root, u64::MAX, u64::MAX, 256)?;
        let stranger = ok(sys::process_create_in(j), "a process in another job")?.a1 as u64;
        let shared = ok(
            sys::vmo_create_flags(4, 0, rt::abi::vmo_flags::RESIZABLE),
            "a resizable VMO of our own job",
        )?
        .a1 as u64;
        ensure_eq!(
            st(sys::vmo_commit(shared, 0, 4 * 4096)),
            Some(S::Ok),
            "committed, so that a stranger may map it"
        );
        ensure_eq!(
            st(sys::process_map(
                stranger,
                shared,
                0x60_0000,
                0,
                4 * 4096,
                PF_R
            )),
            Some(S::Ok),
            "the stranger maps it"
        );
        ensure_eq!(
            st(sys::vmo_decommit(shared, 0, 4096)),
            Some(S::BadState),
            "a decommit under a stranger's mapping"
        );
        ensure_eq!(
            st(sys::vmo_set_size(shared, 2 * 4096)),
            Some(S::BadState),
            "a shrink under a stranger's mapping"
        );
        ensure_eq!(
            sys::vmo_get_size(shared).a1 as u64,
            4 * 4096,
            "nothing changed"
        );
        ensure_eq!(
            st(sys::vmo_set_size(shared, 8 * 4096)),
            Some(S::Ok),
            "growing is allowed"
        );
        ensure_eq!(
            st(sys::process_unmap(stranger, 0x60_0000)),
            Some(S::Ok),
            "the stranger lets go"
        );
        ensure_eq!(
            st(sys::vmo_decommit(shared, 0, 4096)),
            Some(S::Ok),
            "now the pages can go"
        );
        for h in [shared, stranger, j, root] {
            sys::handle_close(h);
        }
    }
    for h in [weak, fixed, v] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM7: ranges, protect and info from user mode: the "map twice the size and trim" idiom, ANYWHERE, a JIT-style flip from writable to
/// executable that then runs (and write with execute refused), the error rows, and `process_map_info`'s record.
fn process_protect_and_range_unmap_from_user_mode() -> Outcome {
    let me = me()?;
    // two ANYWHERE mappings differ and do not overlap
    let v = vmo(8)?;
    let a = ok(
        sys::process_map_anywhere(me, v, 0, 8 * 4096, PF_R | PF_W),
        "ANYWHERE",
    )?
    .a1 as u64;
    let b = ok(
        sys::process_map_anywhere(me, v, 0, 8 * 4096, PF_R),
        "ANYWHERE again",
    )?
    .a1 as u64;
    ensure!(
        a.is_multiple_of(4096) && b.is_multiple_of(4096) && a != b,
        "distinct page-aligned places: {a:#x} {b:#x}"
    );
    ensure!(a.abs_diff(b) >= 8 * 4096, "no overlap");
    ensure_eq!(
        st(sys::process_unmap(me, b)),
        Some(S::Ok),
        "unmap the second"
    );
    // trim: keep the middle four pages
    ensure_eq!(
        st(sys::process_unmap_range(me, a, 2 * 4096)),
        Some(S::Ok),
        "trim the head"
    );
    ensure_eq!(
        st(sys::process_unmap_range(me, a + 6 * 4096, 2 * 4096)),
        Some(S::Ok),
        "trim the tail"
    );
    // SAFETY: pages 2 to 5 of the mapping stay mapped read-write.
    unsafe {
        core::ptr::write_volatile((a + 2 * 4096) as *mut u32, 0xabcd);
        core::ptr::write_volatile((a + 5 * 4096) as *mut u32, 0xef01);
    }
    let mut rec = [0u64; 6];
    let next = ok(sys::process_map_info(me, a, rec.as_mut_ptr()), "map_info")?.a1 as u64;
    ensure_eq!(
        (rec[0], rec[1], rec[2], rec[5]),
        (a + 2 * 4096, 4 * 4096, 0b011, 2 * 4096),
        "the record of what is left"
    );
    ensure!(next == 0 || next > a, "the next base");
    // holes are fine
    ensure_eq!(
        st(sys::process_unmap_range(me, a, 8 * 4096)),
        Some(S::Ok),
        "unmap over holes"
    );
    ensure_eq!(
        st(sys::process_unmap_range(me, a, 4096)),
        Some(S::Ok),
        "unmap only hole"
    );
    // the JIT flip: write code, make it executable, run it
    const CODE: u64 = 0x5a0_0000;
    let c = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(me, c, CODE, 0, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map for writing"
    );
    // addi a0, zero, 42 ; ret
    let code: [u32; 2] = [0x02a0_0513, 0x0000_8067];
    // SAFETY: the page is mapped read-write.
    unsafe {
        core::ptr::write_volatile(CODE as *mut u32, code[0]);
        core::ptr::write_volatile((CODE + 4) as *mut u32, code[1]);
    }
    ensure_eq!(
        st(sys::process_protect(me, CODE, 4096, PF_W | PF_X)),
        Some(S::InvalidArgs),
        "write with execute"
    );
    ensure_eq!(
        st(sys::process_protect(me, CODE, 4096, PF_R | PF_X)),
        Some(S::Ok),
        "flip to executable"
    );
    // SAFETY: the page holds the two instructions written above and is executable now; `fence.i` synchronises this hart's fetch.
    let got = unsafe {
        core::arch::asm!("fence.i");
        let f: extern "C" fn() -> u64 = core::mem::transmute(CODE as usize);
        f()
    };
    ensure_eq!(got, 42, "the code ran");
    ensure_eq!(
        st(sys::process_protect(me, CODE, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "and back to writable"
    );
    // errors
    ensure_eq!(
        st(sys::process_protect(me, CODE, 2 * 4096, PF_R)),
        Some(S::InvalidArgs),
        "a hole in the range"
    );
    ensure_eq!(
        st(sys::process_protect(me, CODE + 1, 4096, PF_R)),
        Some(S::InvalidArgs),
        "unaligned"
    );
    ensure_eq!(
        st(sys::process_protect(me, CODE, 0, PF_R)),
        Some(S::InvalidArgs),
        "no length"
    );
    ensure_eq!(
        st(sys::process_protect(me, CODE, 4096, 8)),
        Some(S::InvalidArgs),
        "unknown flag"
    );
    ensure_eq!(
        st(sys::process_protect(BAD, CODE, 4096, PF_R)),
        Some(S::BadHandle),
        "bad handle"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::process_protect(ev, CODE, 4096, PF_R)),
        Some(S::WrongType),
        "not a process"
    );
    sys::handle_close(ev);
    // the ceiling: a mapping made through a handle without WRITE can never be made writable
    let ro = ok(
        sys::handle_duplicate(c, Rights::READ.0 | Rights::MAP.0),
        "a read-only handle",
    )?
    .a1 as u64;
    const RO: u64 = 0x5b0_0000;
    ensure_eq!(
        st(sys::process_map(me, ro, RO, 0, 4096, PF_R)),
        Some(S::Ok),
        "map read-only"
    );
    ensure_eq!(
        st(sys::process_protect(me, RO, 4096, PF_R | PF_W)),
        Some(S::AccessDenied),
        "beyond the ceiling"
    );
    ensure_eq!(
        st(sys::process_protect(me, RO, 4096, PF_R | PF_X)),
        Some(S::AccessDenied),
        "execute without the right"
    );
    for at in [CODE, RO] {
        ensure_eq!(st(sys::process_unmap(me, at)), Some(S::Ok), "unmap");
    }
    for h in [v, c, ro] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM7: a page made read-only by `process_protect` faults on a write and ends the process.
fn a_write_to_a_protected_page_ends_the_process() -> Outcome {
    let (p, _m) = launch(101)?;
    wait_ended(p, 1000)?;
    let r = sys::process_info(p);
    ensure_eq!((r.status, r.a1 as i64), (0, -1001), "a page fault ended it");
    Ok(())
}

/// MM8: copy-on-write children from user mode. A SNAPSHOT child keeps the source's contents as they were and each side's later writes are
/// private to it (the source's own writable mapping has to lose its write permission at the call: the negative control that skips that
/// shows the child changing under a write through the source); a PRIVATE child sees what its source changes and its own writes stay
/// its own; the argument errors; a VMO that inherits pages is not decommitted.
fn copy_on_write_children_from_user_mode() -> Outcome {
    const A: u64 = 0x5c0_0000;
    const B: u64 = 0x5d0_0000;
    const C: u64 = 0x5e0_0000;
    let me = me()?;
    let rd = |va: u64| -> u32 {
        // SAFETY: the callers pass addresses inside mappings of this process.
        unsafe { core::ptr::read_volatile(va as *const u32) }
    };
    let wr = |va: u64, v: u32| {
        // SAFETY: as above, for mappings that are writable.
        unsafe { core::ptr::write_volatile(va as *mut u32, v) }
    };
    let snap = rt::abi::child_flags::SNAPSHOT;
    let private = rt::abi::child_flags::PRIVATE;
    let v = vmo(4)?;
    ensure_eq!(
        st(sys::process_map(me, v, A, 0, 4 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the source"
    );
    for p in 0..4u64 {
        wr(A + p * 4096, 100 + p as u32);
    }
    ensure_eq!(
        st(sys::vmo_create_child(v, 0, 4 * 4096, 0, 0)),
        Some(S::InvalidArgs),
        "a flag is needed"
    );
    ensure_eq!(
        st(sys::vmo_create_child(v, 0, 4 * 4096, 0, snap | private)),
        Some(S::InvalidArgs),
        "exactly one flag"
    );
    ensure_eq!(
        st(sys::vmo_create_child(v, 0, 5 * 4096, 0, snap)),
        Some(S::InvalidArgs),
        "past the end"
    );
    ensure_eq!(
        st(sys::vmo_create_child(BAD, 0, 4096, 0, snap)),
        Some(S::BadHandle),
        "no such VMO"
    );
    let c = ok(
        sys::vmo_create_child(v, 0, 4 * 4096, 0, snap),
        "a SNAPSHOT child",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, c, B, 0, 4 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the child"
    );
    for p in 0..4u64 {
        ensure_eq!(
            rd(B + p * 4096),
            100 + p as u32,
            "the child starts with the source's contents, page {p}"
        );
    }
    // a write through the source's old writable mapping must reach neither the child nor the hidden pages
    wr(A, 7_000);
    ensure_eq!(rd(A), 7_000, "the source sees its write");
    ensure_eq!(rd(B), 100, "the child does not");
    wr(B + 4096, 8_000);
    ensure_eq!(rd(B + 4096), 8_000, "the child sees its write");
    ensure_eq!(rd(A + 4096), 101, "the source does not");
    // a loader-style protect of the source's mapping (read-only, then writable again) must not make a page it only inherits writable
    ensure_eq!(
        st(sys::process_protect(me, A, 4 * 4096, PF_R)),
        Some(S::Ok),
        "protect read-only"
    );
    ensure_eq!(
        st(sys::process_protect(me, A, 4 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "and back"
    );
    wr(A + 3 * 4096, 9_000);
    ensure_eq!(rd(A + 3 * 4096), 9_000, "the source sees its write");
    ensure_eq!(
        rd(B + 3 * 4096),
        103,
        "the child does not, after a protect either"
    );
    ensure_eq!(rd(A + 2 * 4096), 102, "an untouched page is still shared");
    ensure_eq!(rd(B + 2 * 4096), 102, "in both");
    // a member gives a page back: it reads zero afterwards, the other side keeps what it saw
    ensure_eq!(
        st(sys::vmo_decommit(v, 0, 4096)),
        Some(S::Ok),
        "decommit on a member of a family"
    );
    ensure_eq!(rd(A), 0, "the source's page reads zero");
    ensure_eq!(rd(B), 100, "the child's does not change");
    ensure_eq!(
        st(sys::vmo_decommit(c, 4096, 4096)),
        Some(S::Ok),
        "and the child's own"
    );
    ensure_eq!(rd(B + 4096), 0, "reads zero");
    ensure_eq!(rd(A + 4096), 101, "the source keeps its page");
    // a PRIVATE child of a fresh VMO sees the source's later changes, and its own writes stay its own
    let u = vmo(2)?;
    let um = 0x5f0_0000;
    ensure_eq!(
        st(sys::process_map(me, u, um, 0, 2 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map u"
    );
    wr(um, 1);
    wr(um + 4096, 2);
    let pc = ok(
        sys::vmo_create_child(u, 0, 2 * 4096, 0, private),
        "a PRIVATE child",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, pc, C, 0, 2 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map it"
    );
    ensure_eq!(rd(C), 1, "it reads the source");
    wr(um, 11);
    ensure_eq!(rd(C), 11, "and sees what the source changes");
    wr(C + 4096, 22);
    ensure_eq!(rd(C + 4096), 22, "its own write");
    ensure_eq!(rd(um + 4096), 2, "does not reach the source");
    // everything goes
    for (h, va) in [(c, B), (pc, C), (v, A), (u, um)] {
        ensure_eq!(st(sys::process_unmap(me, va)), Some(S::Ok), "unmap");
        sys::handle_close(h);
    }
    Ok(())
}

// ---- EX1: thread_suspend, thread_resume, thread_read_state, thread_write_state, thread_spawn SUSPENDED ----

core::arch::global_asm!(
    r#"
    .section .text
    .globl ex1_spin_a1
    .balign 4
ex1_spin_a1:                    # spins until a1 holds 0x5a5a5a, then ends with exit code 77
    li   t1, 0x5a5a5a
1:  beq  a1, t1, 2f
    j    1b
2:  li   a0, 77
    la   t4, EX1_EXIT
    ld   t4, 0(t4)
    jr   t4
    .globl ex1_exit_55
    .balign 4
ex1_exit_55:                    # ends with exit code 55
    li   a0, 55
    la   t4, EX1_EXIT
    ld   t4, 0(t4)
    jr   t4
    .globl ex1_handler
    .balign 4
ex1_handler:                    # a "signal handler": counts a hit, returns to the address the debugger left in t6
    li   t5, 1
    la   t4, EX1_HITS
    amoadd.w zero, t5, (t4)
    jr   t6
    .globl ex1_spin_f1
    .balign 4
ex1_spin_f1:                    # spins until f1 holds 0x1234567, then ends with exit code 66
1:  fmv.x.d t0, f1
    li   t1, 0x1234567
    beq  t0, t1, 2f
    j    1b
2:  li   a0, 66
    la   t4, EX1_EXIT
    ld   t4, 0(t4)
    jr   t4
"#
);

extern "C" {
    fn ex1_spin_a1();
    fn ex1_exit_55();
    fn ex1_handler();
    fn ex1_spin_f1();
}

#[no_mangle]
static EX1_HITS: AtomicU32 = AtomicU32::new(0);
/// The library's `thread_exit` entry, which the routines above end in (AB8: no program makes a call of its own; they have no registers to spare for it).
#[no_mangle]
static EX1_EXIT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

const EX1_MAGIC: u64 = 0x5a5a5a;
const GENERAL: u64 = rt::abi::thread_state::GENERAL;
const FPK: u64 = rt::abi::thread_state::FP;
const VECK: u64 = rt::abi::thread_state::VECTOR;

/// Start a thread at `entry` on a stack of its own; `flags` are extra `thread_spawn` flags. Returns (handle, tid).
fn ex1_spawn(entry: unsafe extern "C" fn(), flags: u32) -> Result<(u64, u64), Detail> {
    EX1_EXIT.store(
        rt::library::entry(rt::abi::syscall::THREAD_EXIT),
        Ordering::Relaxed,
    );
    let top = own_stack()?;
    let rec = rt::SpawnArgs {
        entry: entry as usize as u64,
        flags: rt::abi::spawn::CALLER_STACK | flags,
        sp: top,
        ..rt::SpawnArgs::default()
    };
    let r = ok(sys::thread_spawn(me()?, &rec), "thread_spawn")?;
    Ok((r.a1 as u64, r.a2 as u64))
}

fn ex1_pause(ticks: u64) -> Result<(), Detail> {
    let gate = event()?;
    let _ = sys::object_wait(gate, 1 << 24, sys::clock_get() + ticks);
    sys::handle_close(gate);
    Ok(())
}

/// Poll `cond` (every tick) until it holds or `ticks` have passed.
fn ex1_until(ticks: u64, cond: impl Fn() -> bool) -> bool {
    let end = sys::clock_get() + ticks;
    while sys::clock_get() < end {
        if cond() {
            return true;
        }
        let _ = ex1_pause(1);
    }
    cond()
}

fn ex1_signals(t: u64) -> u32 {
    sys::object_signals(t).a1 as u32
}

fn ex1_read(t: u64) -> Result<[u64; 32], Detail> {
    let mut g = [0u64; 32];
    ok(
        sys::thread_read_state(t, GENERAL, g.as_mut_ptr() as *mut u8, 256),
        "thread_read_state",
    )?;
    Ok(g)
}

fn ex1_write(t: u64, g: &[u64; 32]) -> Option<S> {
    st(sys::thread_write_state(
        t,
        GENERAL,
        g.as_ptr() as *const u8,
        256,
    ))
}

/// A running thread is suspended (it stops, its object says SUSPENDED), its registers are read twice (the same) and one is written: after
/// the resume the thread sees the value. The resume of a thread that is not suspended is BAD_STATE.
fn thread_suspend_stops_a_thread_and_its_registers_can_be_read_and_written() -> Outcome {
    let (t, tid) = ex1_spawn(ex1_spin_a1, 0)?;
    ex1_pause(3)?;
    ensure_eq!(
        ex1_signals(t) & sig::SUSPENDED,
        0,
        "a running thread is not SUSPENDED"
    );
    ensure_eq!(
        st(sys::thread_resume(t)),
        Some(S::BadState),
        "resume of a thread that is not suspended"
    );
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "suspend");
    let r = sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "SUSPENDED rises");
    let (g1, g2) = (ex1_read(t)?, ex1_read(t)?);
    ensure_eq!(g1, g2, "a stopped thread's registers do not change");
    let entry = ex1_spin_a1 as *const () as usize as u64;
    ensure!(
        g1[31] >= entry && g1[31] < entry + 16,
        "the pc {:#x} is in the spin loop at {entry:#x}",
        g1[31]
    );
    ensure_eq!(g1[10], tid, "a1 is the tid a spawned thread starts with");
    let mut w = g1;
    w[10] = EX1_MAGIC;
    ensure_eq!(ex1_write(t, &w), Some(S::Ok), "write a1");
    ensure_eq!(ex1_read(t)?, w, "what was written reads back");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    ensure_eq!(r.status, 0, "the thread saw the new a1 and ended");
    ensure_eq!(
        sys::thread_info(t).a1,
        77,
        "its exit code says it saw the value"
    );
    Ok(())
}

/// The signal-injection sequence: stop a thread, point its pc at a handler (leaving the old pc where the handler returns to), let it run the
/// handler, and see it continue where it was.
fn a_stopped_thread_can_be_sent_to_a_handler_and_back() -> Outcome {
    EX1_HITS.store(0, Ordering::SeqCst);
    let (t, _) = ex1_spawn(ex1_spin_a1, 0)?;
    ex1_pause(2)?;
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "suspend");
    ensure_eq!(
        sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    let mut g = ex1_read(t)?;
    let old_pc = g[31];
    g[30] = old_pc; // t6 (x31) is word 30
    g[31] = ex1_handler as *const () as usize as u64;
    ensure_eq!(ex1_write(t, &g), Some(S::Ok), "point the pc at the handler");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure!(
        ex1_until(100, || EX1_HITS.load(Ordering::SeqCst) == 1),
        "the handler ran"
    );
    ex1_pause(2)?;
    ensure_eq!(EX1_HITS.load(Ordering::SeqCst), 1, "and only once");
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "stop it again");
    ensure_eq!(
        sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped again"
    );
    let mut g = ex1_read(t)?;
    ensure!(
        g[31] >= ex1_spin_a1 as *const () as usize as u64
            && g[31] < ex1_spin_a1 as *const () as usize as u64 + 16,
        "back in the loop at {:#x}",
        g[31]
    );
    g[10] = EX1_MAGIC;
    ensure_eq!(ex1_write(t, &g), Some(S::Ok), "set a1");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it ends"
    );
    ensure_eq!(sys::thread_info(t).a1, 77, "having gone back to its loop");
    // (another thread redirected to a function of its own)
    let (u, _) = ex1_spawn(ex1_spin_a1, 0)?;
    ex1_pause(2)?;
    ensure_eq!(st(sys::thread_suspend(u)), Some(S::Ok), "suspend");
    ensure_eq!(
        sys::object_wait(u, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    let mut g = ex1_read(u)?;
    g[31] = ex1_exit_55 as *const () as usize as u64;
    ensure_eq!(ex1_write(u, &g), Some(S::Ok), "redirect the pc");
    ensure_eq!(st(sys::thread_resume(u)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(u, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it ends"
    );
    ensure_eq!(sys::thread_info(u).a1, 55, "at the new pc");
    Ok(())
}

/// A thread started SUSPENDED runs only after `thread_resume`, with the registers written beforehand (the `fork` child sequence).
fn a_thread_started_suspended_runs_only_after_resume_with_registers_written_first() -> Outcome {
    let top = own_stack()?;
    let rec = |flags: u32| rt::SpawnArgs {
        entry: ex1_spin_a1 as *const () as usize as u64,
        flags: rt::abi::spawn::CALLER_STACK | flags,
        sp: top,
        ..rt::SpawnArgs::default()
    };
    ensure_eq!(
        st(sys::thread_spawn(
            me()?,
            &rec(rt::abi::spawn::SUSPENDED | rt::abi::spawn::DETACHED)
        )),
        Some(S::InvalidArgs),
        "SUSPENDED with DETACHED could never be resumed"
    );
    let r = ok(
        sys::thread_spawn(me()?, &rec(rt::abi::spawn::SUSPENDED)),
        "spawn SUSPENDED",
    )?;
    let (t, tid) = (r.a1 as u64, r.a2 as u64);
    ensure_eq!(
        ex1_signals(t) & sig::SUSPENDED,
        sig::SUSPENDED,
        "SUSPENDED is up at once"
    );
    ex1_pause(4)?;
    ensure_eq!(ex1_signals(t) & sig::TERMINATED, 0, "it has not run");
    let mut g = ex1_read(t)?;
    ensure_eq!(
        g[31],
        ex1_spin_a1 as *const () as usize as u64,
        "it starts at its entry"
    );
    ensure_eq!(g[1], top, "with its stack");
    ensure_eq!(g[10], tid, "and its tid in a1");
    g[10] = EX1_MAGIC; // as a forked child is given its parent's registers with a return value changed
    ensure_eq!(ex1_write(t, &g), Some(S::Ok), "write before it ever ran");
    ex1_pause(3)?;
    ensure_eq!(ex1_signals(t) & sig::TERMINATED, 0, "still not running");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure_eq!(ex1_signals(t) & sig::SUSPENDED, 0, "SUSPENDED falls");
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it runs and ends"
    );
    ensure_eq!(
        sys::thread_info(t).a1,
        77,
        "from the registers that were written"
    );
    Ok(())
}

/// The fork-child sequence with no pause at all: the spawner writes the child's registers and resumes it at once.
fn a_thread_started_suspended_and_resumed_at_once_runs() -> Outcome {
    for _ in 0..8 {
        let (t, _) = ex1_spawn(ex1_spin_a1, rt::abi::spawn::SUSPENDED)?;
        let mut g = ex1_read(t)?;
        g[10] = EX1_MAGIC;
        ensure_eq!(ex1_write(t, &g), Some(S::Ok), "write at once");
        ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume at once");
        ensure_eq!(
            sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
            0,
            "it runs"
        );
        ensure_eq!(
            sys::thread_info(t).a1,
            77,
            "from the registers that were written"
        );
    }
    // a thread that is not suspended can be suspended at once too: it is known from the moment the spawn returns
    let (t, _) = ex1_spawn(ex1_spin_a1, 0)?;
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "suspend at once");
    ensure!(
        sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500).status == 0,
        "it stops"
    );
    let mut g = ex1_read(t)?;
    g[10] = EX1_MAGIC;
    ensure_eq!(ex1_write(t, &g), Some(S::Ok), "write");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it ends"
    );
    Ok(())
}

/// The refusals, nesting, and a thread blocked in a call.
fn suspend_and_state_calls_check_their_arguments_and_rights() -> Outcome {
    let (t, _) = ex1_spawn(ex1_spin_a1, 0)?;
    ex1_pause(2)?;
    let mut g = [0u64; 32];
    ensure_eq!(
        st(sys::thread_read_state(
            t,
            GENERAL,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::BadState),
        "read of a running thread"
    );
    ensure_eq!(
        ex1_write(t, &g),
        Some(S::BadState),
        "write to a running thread"
    );
    ensure_eq!(
        st(sys::thread_read_state(
            0,
            GENERAL,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::BadState),
        "a thread cannot read its own registers"
    );
    let weak = weaker(t, Rights::WAIT.0)?;
    ensure_eq!(
        st(sys::thread_suspend(weak)),
        Some(S::AccessDenied),
        "suspend needs MANAGE"
    );
    ensure_eq!(
        st(sys::thread_resume(weak)),
        Some(S::AccessDenied),
        "resume needs MANAGE"
    );
    ensure_eq!(
        st(sys::thread_suspend(BAD)),
        Some(S::BadHandle),
        "suspend: bad handle"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::thread_suspend(ev)),
        Some(S::WrongType),
        "suspend: an event"
    );
    // nesting: two suspends, one resume leaves it suspended
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "suspend");
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "and again");
    ensure_eq!(
        sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "one resume");
    ensure_eq!(
        ex1_signals(t) & sig::SUSPENDED,
        sig::SUSPENDED,
        "still suspended"
    );
    // the argument checks on a stopped thread
    ensure_eq!(
        st(sys::thread_read_state(
            t,
            GENERAL,
            g.as_mut_ptr() as *mut u8,
            248
        )),
        Some(S::InvalidArgs),
        "a length that is not the layout's"
    );
    ensure_eq!(
        st(sys::thread_read_state(
            t,
            99,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::InvalidArgs),
        "an unknown kind"
    );
    ensure_eq!(
        st(sys::thread_read_state(t, GENERAL, 8 as *mut u8, 256)),
        Some(S::InvalidArgs),
        "a bad buffer"
    );
    let mut w = ex1_read(t)?;
    let pc = w[31];
    w[31] = rt::abi::user::TOP;
    ensure_eq!(
        ex1_write(t, &w),
        Some(S::InvalidArgs),
        "a pc that is not a user address"
    );
    w[31] = pc | 1;
    ensure_eq!(
        ex1_write(t, &w),
        Some(S::InvalidArgs),
        "a pc that is not 2-aligned"
    );
    w[31] = pc;
    ensure_eq!(ex1_read(t)?[31], pc, "nothing changed");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "the second resume");
    ensure_eq!(ex1_signals(t) & sig::SUSPENDED, 0, "SUSPENDED falls at 0");
    ensure_eq!(
        st(sys::thread_resume(t)),
        Some(S::BadState),
        "a third resume"
    );
    w[10] = EX1_MAGIC;
    // (end it: suspend, set a1, resume)
    ensure_eq!(st(sys::thread_suspend(t)), Some(S::Ok), "suspend");
    ensure_eq!(
        sys::object_wait(t, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    ensure_eq!(ex1_write(t, &w), Some(S::Ok), "set a1");
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it ends"
    );
    ensure_eq!(
        st(sys::thread_suspend(t)),
        Some(S::BadState),
        "suspend of an ended thread"
    );
    // a thread blocked in a call has stopped where it is: SUSPENDED rises at once, its registers can be read, not written
    let gate = event()?;
    let b = new_thread(worker_wait_event, gate as usize)?;
    ex1_pause(4)?;
    ensure_eq!(
        st(sys::thread_suspend(b)),
        Some(S::Ok),
        "suspend a blocked thread"
    );
    ensure_eq!(
        sys::object_wait(b, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "SUSPENDED rises while it is blocked"
    );
    ensure_eq!(
        st(sys::thread_read_state(
            b,
            GENERAL,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::Ok),
        "read it"
    );
    ensure_eq!(
        ex1_write(b, &g),
        Some(S::BadState),
        "write is refused inside a blocking call"
    );
    ensure_eq!(st(sys::thread_resume(b)), Some(S::Ok), "resume it");
    ensure_eq!(ex1_signals(b) & sig::SUSPENDED, 0, "SUSPENDED falls");
    ensure_eq!(ex1_signals(b) & sig::TERMINATED, 0, "it goes on waiting");
    // a suspend that arrives while it is blocked and a wake that ends the wait: it parks instead of returning
    ensure_eq!(st(sys::thread_suspend(b)), Some(S::Ok), "suspend again");
    ensure_eq!(
        sys::object_wait(b, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    ensure_eq!(
        st(sys::event_signal(gate, 0, 1 << 24)),
        Some(S::Ok),
        "end its wait"
    );
    ex1_pause(3)?;
    ensure_eq!(
        ex1_signals(b) & sig::TERMINATED,
        0,
        "it has not returned from its call: it is parked"
    );
    ensure_eq!(st(sys::thread_resume(b)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(b, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "now it returns and ends"
    );
    ensure_eq!(
        WOKEN_STATUS.load(Ordering::SeqCst),
        0,
        "its wait had succeeded"
    );
    Ok(())
}

/// FP and vector state round-trip, and a thread started with FP state written for it uses it.
fn fp_and_vector_state_round_trip_through_thread_state() -> Outcome {
    let (t, _) = ex1_spawn(ex1_spin_f1, rt::abi::spawn::SUSPENDED)?;
    let mut f = [0u64; 33];
    ensure_eq!(
        st(sys::thread_read_state(
            t,
            FPK,
            f.as_mut_ptr() as *mut u8,
            264
        )),
        Some(S::Ok),
        "read FP of a thread that never used it"
    );
    ensure_eq!(f, [0u64; 33], "zeros");
    f[1] = 0x123_4567;
    f[10] = 0xdead_beef_0000_0001;
    f[32] = 0x61; // fcsr: frm 3, fflags 1
    ensure_eq!(
        st(sys::thread_write_state(
            t,
            FPK,
            f.as_ptr() as *const u8,
            264
        )),
        Some(S::Ok),
        "write FP"
    );
    let mut back = [0u64; 33];
    ensure_eq!(
        st(sys::thread_read_state(
            t,
            FPK,
            back.as_mut_ptr() as *mut u8,
            264
        )),
        Some(S::Ok),
        "read FP"
    );
    ensure_eq!(back, f, "FP state round-trips");
    // the vector unit, if there is one: find the register length by the size the call accepts
    let mut vlen = 0;
    for vlenb in [16usize, 32, 64, 128, 256] {
        let n = 32 + 32 * vlenb;
        let mut v = vec![0u8; n];
        if st(sys::thread_read_state(t, VECK, v.as_mut_ptr(), n)) == Some(S::Ok) {
            vlen = n;
            ensure!(
                v.iter().all(|&b| b == 0),
                "a thread that never used the vector unit reads zeros"
            );
            for (i, b) in v.iter_mut().enumerate().skip(32) {
                *b = (i % 251) as u8;
            }
            ensure_eq!(
                st(sys::thread_write_state(t, VECK, v.as_ptr(), n)),
                Some(S::Ok),
                "write the vector state"
            );
            let mut w = vec![0u8; n];
            ensure_eq!(
                st(sys::thread_read_state(t, VECK, w.as_mut_ptr(), n)),
                Some(S::Ok),
                "read it back"
            );
            ensure!(v == w, "vector state round-trips");
            break;
        }
    }
    ensure!(
        vlen != 0
            || st(sys::thread_read_state(t, VECK, [0u8; 8].as_mut_ptr(), 8))
                == Some(S::NotSupported),
        "a machine without a vector unit says NOT_SUPPORTED"
    );
    // the thread starts with the FP state written for it: f1 holds the pattern it waits for
    ensure_eq!(st(sys::thread_resume(t)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it ran and ended"
    );
    ensure_eq!(sys::thread_info(t).a1, 66, "having seen f1");
    // a thread that has run (it uses the FP unit: its first FP instruction turned it on) gets FP state written while it is stopped: the unit is
    // loaded from the new state when it runs again
    let (u, _) = ex1_spawn(ex1_spin_f1, 0)?;
    ex1_pause(3)?;
    ensure_eq!(
        st(sys::thread_suspend(u)),
        Some(S::Ok),
        "suspend a running thread that uses FP"
    );
    ensure_eq!(
        sys::object_wait(u, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "stopped"
    );
    let mut z = [0u64; 33];
    ensure_eq!(
        st(sys::thread_read_state(
            u,
            FPK,
            z.as_mut_ptr() as *mut u8,
            264
        )),
        Some(S::Ok),
        "read its FP state"
    );
    ensure_eq!(
        z[1],
        0,
        "f1 is still zero: the thread is waiting for the pattern"
    );
    z[1] = 0x123_4567;
    ensure_eq!(
        st(sys::thread_write_state(
            u,
            FPK,
            z.as_ptr() as *const u8,
            264
        )),
        Some(S::Ok),
        "write f1"
    );
    ensure_eq!(st(sys::thread_resume(u)), Some(S::Ok), "resume");
    ensure_eq!(
        sys::object_wait(u, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "it sees the new f1 and ends"
    );
    ensure_eq!(sys::thread_info(u).a1, 66, "its exit code says it did");
    Ok(())
}

// ---- EX2: exception channels ----

const EX2_PAGE: u64 = 0x5b0_0000;

extern "C" fn ex2_writer(_arg: usize) -> ! {
    // SAFETY: EX2_PAGE is mapped read-only by the test, so this store faults; the handler makes it writable and retries.
    unsafe { core::ptr::write_volatile(EX2_PAGE as *mut u32, 0xe2e2) };
    sys::thread_exit(42)
}

/// Read one exception message from `ch`: (kind, tid, pc, address, cause, thread handle, exception handle).
type Ex2Message = (u64, u64, u64, u64, u64, u64, u64);

fn ex2_message(ch: u64) -> Result<Ex2Message, Detail> {
    let r = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 1000);
    ensure_eq!(r.status, 0, "an exception message arrives");
    let mut b = [0u8; 40];
    let mut hs = [0u64; 4];
    let r = ok(
        sys::channel_read(ch, b.as_mut_ptr(), 40, hs.as_mut_ptr(), 4),
        "read the exception message",
    )?;
    ensure_eq!(r.a1, 40, "40 bytes");
    ensure_eq!(r.a2, 2, "the thread and the exception");
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    Ok((w(0), w(1), w(2), w(3), w(4), hs[0], hs[1]))
}

/// The SIGSEGV-handler pattern: a thread of our own process stores to a read-only page; we are the handler (the process's channel is ours): we make
/// the page writable and RETRY, and the store succeeds.
fn a_handler_thread_can_make_a_faulting_store_succeed_and_retry() -> Outcome {
    let me = me()?;
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map(me, v, EX2_PAGE, 0, 4096, PF_R)),
        Some(S::Ok),
        "map read-only"
    );
    let ch = ok(sys::process_exception_channel(me), "the channel")?.a1 as u64;
    ensure_eq!(
        st(sys::process_exception_channel(me)),
        Some(S::BadState),
        "a second channel while the first is open"
    );
    let top = own_stack()?;
    let r = ok(
        sys::thread_on_stack(me, ex2_writer as *const () as usize as u64, top, 0),
        "the faulting thread",
    )?;
    let (t, tid) = (r.a1 as u64, r.a2 as u64);
    let (kind, mtid, pc, addr, _cause, th, eh) = ex2_message(ch)?;
    ensure_eq!(
        kind,
        u64::from(rt::abi::arch::fault::PAGE_FAULT),
        "a page fault"
    );
    ensure_eq!(mtid, tid, "from that thread");
    ensure_eq!(addr, EX2_PAGE, "at the page");
    ensure!(
        pc >= ex2_writer as *const () as usize as u64
            && pc < ex2_writer as *const () as usize as u64 + 64,
        "in the store at {pc:#x}"
    );
    // the thread is stopped (SUSPENDED) and its state can be read
    ensure_eq!(
        sys::object_wait(th, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "the thread stops"
    );
    let mut g = [0u64; 32];
    ensure_eq!(
        st(sys::thread_read_state(
            th,
            rt::abi::thread_state::GENERAL,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::Ok),
        "read its registers"
    );
    ensure_eq!(g[31], pc, "the pc the message says");
    ensure_eq!(
        st(sys::process_protect(me, EX2_PAGE, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "make the page writable"
    );
    ensure_eq!(
        st(sys::exception_resume(eh, 9)),
        Some(S::InvalidArgs),
        "an unknown decision"
    );
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::RETRY)),
        Some(S::Ok),
        "RETRY"
    );
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::RETRY)),
        Some(S::BadState),
        "resolved once"
    );
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "the thread goes on and ends"
    );
    ensure_eq!(sys::thread_info(t).a1, 42, "its store went through");
    // SAFETY: the page is writable now and was written by the retried store.
    ensure_eq!(
        unsafe { core::ptr::read_volatile(EX2_PAGE as *const u32) },
        0xe2e2,
        "the value is there"
    );
    // (this process's own channel goes: a later accidental fault in the suite ends it with a code instead of waiting for a handler nobody runs)
    for h in [eh, th, ch, t] {
        sys::handle_close(h);
    }
    Ok(())
}

/// A handler in the parent catches a child's illegal instruction, reads where it was, and kills it: the child ends with the usual code.
fn a_parent_handler_can_kill_a_child_that_took_an_illegal_instruction() -> Outcome {
    let ch = core::cell::Cell::new(0u64);
    let (p, _mine) = launch_image_with(IMAGE.load(Ordering::Relaxed), 5, 0, &|p| {
        ch.set(ok(sys::process_exception_channel(p), "the child's channel")?.a1 as u64);
        Ok(())
    })?;
    let (kind, _tid, pc, _addr, _cause, th, eh) = ex2_message(ch.get())?;
    ensure_eq!(
        kind,
        u64::from(rt::abi::arch::fault::ILLEGAL),
        "an illegal instruction"
    );
    ensure!(pc != 0, "with a pc");
    let mut g = [0u64; 32];
    ensure_eq!(
        sys::object_wait(th, sig::SUSPENDED, sys::clock_get() + 500).status,
        0,
        "the child's thread stops"
    );
    ensure_eq!(
        st(sys::thread_read_state(
            th,
            rt::abi::thread_state::GENERAL,
            g.as_mut_ptr() as *mut u8,
            256
        )),
        Some(S::Ok),
        "read its registers"
    );
    ensure_eq!(g[31], pc, "at that pc");
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::KILL)),
        Some(S::Ok),
        "KILL"
    );
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -1002, "the usual exit code");
    Ok(())
}

/// NEXT reaches the job's channel, and closing the exception handle is NEXT; the last level's NEXT ends the process as it always did.
fn next_reaches_the_job_channel_and_closing_the_handle_is_next() -> Outcome {
    let job = self::job(root_job()?, u64::MAX, u64::MAX, u64::MAX)?;
    let jch = ok(sys::job_exception_channel(job), "its channel")?.a1 as u64;
    // 1: NEXT
    let pch = core::cell::Cell::new(0u64);
    let (p, _m) = launch_image_with(IMAGE.load(Ordering::Relaxed), 5, job, &|p| {
        pch.set(ok(sys::process_exception_channel(p), "the process's channel")?.a1 as u64);
        Ok(())
    })?;
    let (_, _, _, _, _, th, eh) = ex2_message(pch.get())?;
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::NEXT)),
        Some(S::Ok),
        "NEXT"
    );
    sys::handle_close(th);
    let (kind, _, _, _, _, th2, eh2) = ex2_message(jch)?;
    ensure_eq!(
        kind,
        u64::from(rt::abi::arch::fault::ILLEGAL),
        "it came to the job's channel"
    );
    ensure_eq!(
        st(sys::exception_resume(eh2, rt::abi::exception::NEXT)),
        Some(S::Ok),
        "NEXT again: no level is left"
    );
    sys::handle_close(th2);
    wait_ended(p, 500)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        -1002,
        "the process ended as it always did"
    );
    // 2: closing the exception handle is NEXT
    let pch2 = core::cell::Cell::new(0u64);
    let (q, _m) = launch_image_with(IMAGE.load(Ordering::Relaxed), 5, job, &|p| {
        pch2.set(ok(sys::process_exception_channel(p), "the process's channel")?.a1 as u64);
        Ok(())
    })?;
    let (_, _, _, _, _, th, eh) = ex2_message(pch2.get())?;
    sys::handle_close(eh);
    sys::handle_close(th);
    let (_, _, _, _, _, th2, eh2) = ex2_message(jch)?;
    ensure_eq!(
        st(sys::exception_resume(eh2, rt::abi::exception::KILL)),
        Some(S::Ok),
        "KILL at the job"
    );
    sys::handle_close(th2);
    wait_ended(q, 500)?;
    ensure_eq!(
        sys::process_info(q).a1 as i64,
        -1002,
        "ended with the usual code"
    );
    Ok(())
}

/// A process with a pending exception that is killed leaks nothing (the end-of-suite checks count objects, frames and charges), and a process
/// whose channel nobody reads any more ends as it always did.
fn a_process_killed_with_a_pending_exception_leaks_nothing_and_a_closed_channel_is_skipped(
) -> Outcome {
    let pch = core::cell::Cell::new(0u64);
    let (p, _m) = launch_image_with(IMAGE.load(Ordering::Relaxed), 5, 0, &|p| {
        pch.set(ok(sys::process_exception_channel(p), "the process's channel")?.a1 as u64);
        Ok(())
    })?;
    let (_, _, _, _, _, th, eh) = ex2_message(pch.get())?;
    ensure_eq!(
        st(sys::process_kill(p, 7)),
        Some(S::Ok),
        "kill it with the exception pending"
    );
    wait_ended(p, 500)?;
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::RETRY)),
        Some(S::BadState),
        "its exceptions are discarded"
    );
    sys::handle_close(eh);
    sys::handle_close(th);
    sys::handle_close(pch.get());
    // a channel nobody reads: the user end is closed before the child runs, so the fault is not delivered and the child ends as it always did
    let (q, _m) = launch_image_with(IMAGE.load(Ordering::Relaxed), 5, 0, &|p| {
        let c = ok(sys::process_exception_channel(p), "a channel")?.a1 as u64;
        sys::handle_close(c);
        Ok(())
    })?;
    wait_ended(q, 500)?;
    ensure_eq!(sys::process_info(q).a1 as i64, -1002, "the usual code");
    Ok(())
}

// ---- MM9: pagers ----

static PG_PAGER: AtomicU64 = AtomicU64::new(0);
static PG_CH: AtomicU64 = AtomicU64::new(0);
static PG_VMO: AtomicU64 = AtomicU64::new(0);
static PG_STOP: AtomicU32 = AtomicU32::new(0);
static PG_SERVED: AtomicU32 = AtomicU32::new(0);
static PG_BAD: AtomicU32 = AtomicU32::new(0);
static PG_SEEN: AtomicU32 = AtomicU32::new(0);
const PG_KEY: u64 = 0x5eed;
const PG_PAGES: u64 = 16;
const PG_MAP: u64 = 0x5c0_0000;

/// A pager thread: serves each request on the channel by filling a page with `page + 1` and moving it in with `pager_supply`.
extern "C" fn pg_server(_arg: usize) -> ! {
    while PG_STOP.load(Ordering::SeqCst) == 0 {
        let ch = PG_CH.load(Ordering::SeqCst);
        if sys::object_wait(ch, sig::READABLE, sys::clock_get() + 20).status != 0 {
            continue;
        }
        let mut b = [0u8; 40];
        let r = sys::channel_read(ch, b.as_mut_ptr(), 40, core::ptr::null_mut(), 0);
        if r.status != 0 || r.a1 != 40 {
            continue;
        }
        let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
        let (key, off, len, kind) = (w(0), w(1), w(2), w(3));
        if key != PG_KEY || kind != rt::abi::pager::READ || len != 4096 || off % 4096 != 0 {
            PG_BAD.fetch_add(1, Ordering::SeqCst);
            continue;
        }
        let src = sys::vmo_create(1).a1 as u64;
        let fill = [(off / 4096 + 1) as u8; 4096];
        sys::vmo_write(src, 0, fill.as_ptr(), 4096);
        let r = sys::pager_supply(
            PG_PAGER.load(Ordering::SeqCst),
            PG_VMO.load(Ordering::SeqCst),
            off,
            4096,
            src,
            0,
        );
        sys::handle_close(src);
        if r.status == 0 {
            PG_SERVED.fetch_add(1, Ordering::SeqCst);
        } else {
            PG_BAD.fetch_add(1, Ordering::SeqCst);
        }
    }
    sys::thread_exit(0)
}

/// A pager, the channel its requests arrive on, and a VMO of `PG_PAGES` pages of it.
fn pager_vmo() -> Result<(u64, u64, u64), Detail> {
    let r = ok(sys::pager_create(), "pager_create")?;
    let (pager, ch) = (r.a1 as u64, r.a2 as u64);
    let v = ok(
        sys::pager_create_vmo(pager, PG_KEY, PG_PAGES * 4096),
        "pager_create_vmo",
    )?
    .a1 as u64;
    Ok((pager, ch, v))
}

/// Read one request from `ch`: (key, offset, length, kind).
fn pager_request(ch: u64) -> Result<(u64, u64, u64, u64), Detail> {
    let r = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 1000);
    ensure_eq!(r.status, 0, "a request arrives");
    let mut b = [0u8; 40];
    let r = ok(
        sys::channel_read(ch, b.as_mut_ptr(), 40, core::ptr::null_mut(), 0),
        "read the request",
    )?;
    ensure_eq!(r.a1, 40, "40 bytes");
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    Ok((w(0), w(1), w(2), w(3)))
}

/// Start a child (`launch(110)`) that reads page 3 of `v` (a copy of the handle is sent to it).
fn pager_reader(v: u64) -> Result<(u64, u64), Detail> {
    let (p, m) = launch(110)?;
    let dup = ok(
        sys::handle_duplicate(v, Rights::SAME.0),
        "duplicate the VMO",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::channel_write(m, b"v".as_ptr(), 1, &dup, 1)),
        Some(S::Ok),
        "send the VMO"
    );
    Ok((p, m))
}

/// MM9: a pager thread serves a 16-page VMO; a reader in another process and a kernel copy in this one both see the pattern.
fn a_pager_serves_a_vmo_to_a_reader_in_another_process() -> Outcome {
    let (pager, ch, v) = pager_vmo()?;
    ensure_eq!(sys::vmo_get_size(v).a1 as u64, PG_PAGES * 4096, "its size");
    ensure_eq!(
        st(sys::vmo_commit(v, 0, 4096)),
        Some(S::AccessDenied),
        "no COMMIT: it would put zeros where the pager's data belongs"
    );
    let mine = ok(sys::handle_duplicate(v, Rights::SAME.0), "duplicate")?.a1 as u64;
    PG_PAGER.store(pager, Ordering::SeqCst);
    PG_CH.store(ch, Ordering::SeqCst);
    PG_VMO.store(mine, Ordering::SeqCst);
    PG_STOP.store(0, Ordering::SeqCst);
    PG_SERVED.store(0, Ordering::SeqCst);
    PG_BAD.store(0, Ordering::SeqCst);
    let server = new_thread(pg_server, 0)?;
    // a kernel copy of an absent page asks the pager and waits; the call then says "try again", and the page is there
    let mut b = [0u8; 8];
    let mut tries = 0;
    loop {
        let r = sys::vmo_read(v, 5 * 4096 + 8, b.as_mut_ptr(), 8);
        if r.status == 0 {
            break;
        }
        ensure_eq!(
            st(r),
            Some(S::ShouldWait),
            "a kernel copy waits for the pager"
        );
        tries += 1;
        ensure!(tries < 10, "the page never arrived");
    }
    ensure_eq!(b, [6u8; 8], "page 5 holds 5 + 1");
    // another process reads page 3 through its own mapping
    let (p, _m) = pager_reader(v)?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        4,
        "it saw page 3 hold 3 + 1"
    );
    PG_STOP.store(1, Ordering::SeqCst);
    ensure_eq!(
        sys::object_wait(server, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "the pager thread ends"
    );
    ensure_eq!(
        PG_BAD.load(Ordering::SeqCst),
        0,
        "every request was well formed"
    );
    ensure!(
        PG_SERVED.load(Ordering::SeqCst) >= 2,
        "it served both pages"
    );
    for h in [v, mine, pager, ch] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM9 (PG1): a reader killed while it waits for a pager leaves nothing behind, and a slow pager stops nobody else.
fn killing_a_reader_waiting_for_a_pager_leaks_nothing() -> Outcome {
    let (pager, ch, v) = pager_vmo()?;
    let (p, _m) = pager_reader(v)?;
    let (key, off, len, kind) = pager_request(ch)?;
    ensure_eq!(
        (key, off, len, kind),
        (PG_KEY, 3 * 4096, 4096, rt::abi::pager::READ),
        "it asked for page 3, with the VMO's key"
    );
    // the pager does nothing: the reader waits, and everyone else goes on
    let (q, _mq) = launch(8)?;
    wait_ended(q, 500)?;
    ensure_eq!(
        st(sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 4)),
        Some(S::TimedOut),
        "the reader is still waiting"
    );
    ensure_eq!(st(sys::process_kill(p, -5)), Some(S::Ok), "kill it");
    wait_ended(p, 500)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -5, "its exit code");
    // (the kernel checks after init that no waiter or pager is left)
    for h in [v, pager, ch] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM9: a pager that goes away fails the reader waiting for it with a PAGER fault.
fn a_pager_that_dies_fails_the_reader_with_a_pager_fault() -> Outcome {
    let (pager, ch, v) = pager_vmo()?;
    let (p, _m) = pager_reader(v)?;
    pager_request(ch)?;
    ensure_eq!(
        st(sys::handle_close(pager)),
        Some(S::Ok),
        "the pager's last handle goes"
    );
    wait_ended(p, 1000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        i64::from(rt::abi::arch::fault::exit_code(rt::abi::arch::fault::PAGER)),
        "the reader ended with a PAGER fault"
    );
    for h in [v, ch] {
        sys::handle_close(h);
    }
    Ok(())
}

extern "C" fn pg_reader(_arg: usize) -> ! {
    // SAFETY: PG_MAP is the test's read-write mapping of the pager's VMO; the read waits for page 2.
    let b = unsafe { core::ptr::read_volatile((PG_MAP + 2 * 4096) as *const u8) };
    PG_SEEN.store(u32::from(b), Ordering::SeqCst);
    sys::thread_exit(0)
}

/// MM9: `pager_fail` makes the waiting thread take a PAGER exception with the status as its cause; the handler supplies the page and RETRYs.
fn a_pager_failure_is_a_pager_exception_and_a_supplied_page_lets_it_retry() -> Outcome {
    let me = me()?;
    let (pager, ch, v) = pager_vmo()?;
    ensure_eq!(
        st(sys::process_map(
            me,
            v,
            PG_MAP,
            0,
            PG_PAGES * 4096,
            PF_R | PF_W
        )),
        Some(S::Ok),
        "map the pager's VMO"
    );
    let ech = ok(sys::process_exception_channel(me), "the channel")?.a1 as u64;
    PG_SEEN.store(0, Ordering::SeqCst);
    let t = new_thread(pg_reader, 0)?;
    let (_, off, _, _) = pager_request(ch)?;
    ensure_eq!(off, 2 * 4096, "it asked for page 2");
    ensure_eq!(
        st(sys::pager_fail(
            pager,
            v,
            2 * 4096,
            4096,
            S::TimedOut.code()
        )),
        Some(S::Ok),
        "fail it"
    );
    let (kind, _tid, _pc, addr, cause, th, eh) = ex2_message(ech)?;
    ensure_eq!(
        kind,
        u64::from(rt::abi::arch::fault::PAGER),
        "a PAGER fault"
    );
    ensure_eq!(addr, PG_MAP + 2 * 4096, "at the page");
    ensure_eq!(
        cause,
        S::TimedOut.code() as i64 as u64,
        "with the pager's status as the cause"
    );
    // the handler supplies the page after all and retries
    let src = vmo(1)?;
    let fill = [0x77u8; 4096];
    ensure_eq!(
        st(sys::vmo_write(src, 0, fill.as_ptr(), 4096)),
        Some(S::Ok),
        "fill a page"
    );
    ensure_eq!(
        st(sys::pager_supply(pager, v, 2 * 4096, 4096, src, 0)),
        Some(S::Ok),
        "supply it"
    );
    ensure_eq!(
        st(sys::exception_resume(eh, rt::abi::exception::RETRY)),
        Some(S::Ok),
        "RETRY"
    );
    ensure_eq!(
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500).status,
        0,
        "the reader goes on"
    );
    ensure_eq!(
        PG_SEEN.load(Ordering::SeqCst),
        0x77,
        "and reads the page the handler supplied"
    );
    for h in [eh, th, ech, src, v, pager, ch] {
        sys::handle_close(h);
    }
    ensure_eq!(st(sys::process_unmap(me, PG_MAP)), Some(S::Ok), "unmap");
    Ok(())
}

/// MM9: the argument rows of the pager calls, the reserved dirty-tracking names, and PRIVATE children of a pager's VMO.
fn pager_calls_check_their_arguments() -> Outcome {
    let (pager, ch, v) = pager_vmo()?;
    ensure_eq!(
        st(sys::pager_create_vmo(BAD, 1, 4096)),
        Some(S::BadHandle),
        "bad pager handle"
    );
    ensure_eq!(
        st(sys::pager_create_vmo(v, 1, 4096)),
        Some(S::WrongType),
        "a VMO is not a pager"
    );
    let weak = weaker(pager, Rights::DUPLICATE.0)?;
    ensure_eq!(
        st(sys::pager_create_vmo(weak, 1, 4096)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    ensure_eq!(
        st(sys::pager_create_vmo(pager, 1, 0)),
        Some(S::InvalidArgs),
        "zero bytes"
    );
    ensure_eq!(
        st(sys::pager_create_vmo(pager, 1, 100)),
        Some(S::InvalidArgs),
        "not a whole page"
    );
    let src = vmo(1)?;
    let plain = vmo(1)?;
    let ro = weaker(src, Rights::READ.0)?;
    ensure_eq!(
        st(sys::pager_supply(pager, plain, 0, 4096, src, 0)),
        Some(S::InvalidArgs),
        "not a pager's VMO"
    );
    ensure_eq!(
        st(sys::pager_supply(pager, v, 1, 4096, src, 0)),
        Some(S::InvalidArgs),
        "offset not aligned"
    );
    ensure_eq!(
        st(sys::pager_supply(pager, v, 0, 4096, ro, 0)),
        Some(S::AccessDenied),
        "the source needs WRITE"
    );
    ensure_eq!(
        st(sys::pager_supply(pager, v, 0, 4096, src, 0)),
        Some(S::BadState),
        "nothing committed in the source"
    );
    ensure_eq!(
        st(sys::pager_fail(pager, v, 0, 4096, 0)),
        Some(S::InvalidArgs),
        "OK is not a failure"
    );
    ensure_eq!(
        st(sys::pager_query_dirty()),
        Some(S::NotSupported),
        "dirty tracking is reserved"
    );
    ensure_eq!(
        st(sys::pager_writeback_done()),
        Some(S::NotSupported),
        "writeback is reserved"
    );
    // a SNAPSHOT of a pager's VMO is refused; a PRIVATE child is made (it asks the parent's pager for what it inherits)
    ensure_eq!(
        st(sys::vmo_create_child(
            v,
            0,
            4096,
            0,
            rt::abi::child_flags::SNAPSHOT
        )),
        Some(S::NotSupported),
        "no SNAPSHOT of a pager's VMO"
    );
    let c = ok(
        sys::vmo_create_child(v, 0, 4096, 0, rt::abi::child_flags::PRIVATE),
        "a PRIVATE child",
    )?
    .a1 as u64;
    for h in [c, ro, plain, src, weak, v, pager, ch] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM6: a mapping that outlives a shrink: touching the part of it past the VMO's new end ends the process with a page fault.
fn touching_past_a_shrunk_vmo_ends_the_process_cleanly() -> Outcome {
    let (p, _m) = launch(100)?;
    wait_ended(p, 1000)?;
    let r = sys::process_info(p);
    ensure_eq!((r.status, r.a1 as i64), (0, -1001), "a page fault ended it");
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

// (below the stack arena, which `thread_spawn` hands out: 0x30_0000_0000 and up)
static NEXT_STACK: AtomicU64 = AtomicU64::new(0x28_0000_0000);
static COUNTER: AtomicU32 = AtomicU32::new(0);
static WOKEN_AT: AtomicU64 = AtomicU64::new(0);
static WOKEN_STATUS: AtomicU32 = AtomicU32::new(99);

fn me() -> Result<u64, Detail> {
    Ok(ok(sys::process_self(), "process_self")?.a1 as u64)
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

fn new_thread(f: extern "C" fn(usize) -> !, arg: usize) -> Result<u64, Detail> {
    let top = own_stack()?;
    Ok(ok(
        sys::thread_on_stack(me()?, f as *const () as usize as u64, top, arg as u64),
        "thread_on_stack",
    )?
    .a1 as u64)
}

extern "C" fn worker_set(arg: usize) -> ! {
    COUNTER.store(arg as u32, Ordering::SeqCst);
    sys::thread_exit(7)
}

/// `arg` is an event handle: wait for user signal 0 and record when and how the wait ended.
/// A deadline for the tests that start thousands of threads (AB2): the clock is real time now, not a tick count that slows down with
/// the machine, and creating two thousand threads takes about twenty seconds of it on QEMU's software CPU.
const SLOW_MS: u64 = 120_000;

extern "C" fn worker_wait_event(arg: usize) -> ! {
    let r = sys::object_wait(arg as u64, 1 << 24, sys::clock_get() + SLOW_MS);
    WOKEN_AT.store(sys::clock_get(), Ordering::SeqCst);
    WOKEN_STATUS.store(r.status as u32, Ordering::SeqCst);
    sys::thread_exit(if r.status == 0 { 0 } else { 1 })
}

fn caller_stack_argument_checks() -> Outcome {
    let top = own_stack()?;
    let me = me()?;
    let entry = worker_set as *const () as usize as u64;
    let go = |p: u64, entry: u64, sp: u64| st(sys::thread_on_stack(p, entry, sp, 0));
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
        go(me ^ (1u64 << 40), entry, top),
        Some(S::BadHandle),
        "a wide handle"
    );
    // syscall 60 was `thread_create`: retired, never reused (AB5)
    ensure_eq!(
        st(rt::syscall(
            60,
            [me as usize, entry as usize, top as usize, 0, 0, 0]
        )),
        Some(S::BadSyscall),
        "the retired number"
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

// ---- thread_interrupt (T56, SCHED-SMP-SPEC task 26) ----
// A worker blocks in one kind of wait; the test interrupts it and reads what the wait returned. The wait that is interrupted
// returns INTERRUPTED (-14); the flag is set whether or not the thread is blocked, and is taken by the blocking call that sees
// it (never by a poll, never by a call whose condition already holds).

static INTR_H: AtomicU64 = AtomicU64::new(0);
static INTR_H2: AtomicU64 = AtomicU64::new(0);
static INTR_WORD: AtomicU32 = AtomicU32::new(0);
static INTR_GO: AtomicU32 = AtomicU32::new(0);
static INTR_STARTED: AtomicU32 = AtomicU32::new(0);
static INTR_R1: AtomicU32 = AtomicU32::new(u32::MAX);
static INTR_R2: AtomicU32 = AtomicU32::new(u32::MAX);
static INTR_R3: AtomicU32 = AtomicU32::new(u32::MAX);
const INTERRUPTED: u32 = -14i32 as u32;

fn intr_reset() {
    for a in [&INTR_GO, &INTR_STARTED, &INTR_WORD] {
        a.store(0, Ordering::SeqCst);
    }
    for a in [&INTR_R1, &INTR_R2, &INTR_R3] {
        a.store(u32::MAX, Ordering::SeqCst);
    }
}

fn intr_wait_go() {
    INTR_STARTED.store(1, Ordering::SeqCst);
    while INTR_GO.load(Ordering::SeqCst) == 0 {
        sys::thread_yield();
    }
}

/// `arg` picks the wait: 0 an object, 1 a port, 2 a futex, 3 a channel call, 4 a poll then a wait, 5 a satisfied wait then a wait,
/// 6 an interrupted wait then a timed one.
extern "C" fn intr_worker(arg: usize) -> ! {
    let (h, h2) = (
        INTR_H.load(Ordering::SeqCst),
        INTR_H2.load(Ordering::SeqCst),
    );
    intr_wait_go();
    let (r1, r2, r3): (u32, u32, u32) = match arg {
        0 => (sys::object_wait(h, 1 << 24, u64::MAX).status as u32, 0, 0),
        1 => (sys::port_wait(h, u64::MAX).status as u32, 0, 0),
        2 => (
            sys::futex_wait(INTR_WORD.as_ptr(), 0, u64::MAX).status as u32,
            0,
            0,
        ),
        3 => {
            let (m, n) = request(33, b'E', b"i");
            let mut reply = [0u8; 32];
            let args = CallArgs {
                req_bytes: m.as_ptr() as u64,
                req_handles: 0,
                req_nbytes: n as u32,
                req_nhandles: 0,
                rep_bytes: reply.as_mut_ptr() as u64,
                rep_handles: 0,
                rep_max_bytes: 32,
                rep_max_handles: 0,
                deadline: u64::MAX,
            };
            (sys::channel_call(h, &args).status as u32, 0, 0)
        }
        4 => (
            // a poll is not a blocking call: it does not take the flag, a wait then does
            sys::object_wait(h, 1 << 24, 0).status as u32,
            sys::object_wait(h, 1 << 24, u64::MAX).status as u32,
            0,
        ),
        5 => (
            // the signal is already there: the wait succeeds and the flag stays for the next blocking call
            sys::object_wait(h2, 1 << 24, u64::MAX).status as u32,
            sys::object_wait(h, 1 << 24, u64::MAX).status as u32,
            0,
        ),
        7 => {
            let (m, n) = request(34, b'E', b"i");
            let mut reply = [0u8; 32];
            let mut mk = |deadline| CallArgs {
                req_bytes: m.as_ptr() as u64,
                req_handles: 0,
                req_nbytes: n as u32,
                req_nhandles: 0,
                rep_bytes: reply.as_mut_ptr() as u64,
                rep_handles: 0,
                rep_max_bytes: 32,
                rep_max_handles: 0,
                deadline,
            };
            // a call with deadline 0 neither sees nor takes the flag; a blocking one then does, before it sends
            let first = sys::channel_call(h, &mk(0)).status as u32;
            let second = sys::channel_call(h, &mk(u64::MAX)).status as u32;
            (first, second, 0)
        }
        8 => (
            sys::port_wait(h, 0).status as u32,
            sys::port_wait(h, u64::MAX).status as u32,
            0,
        ),
        _ => (
            sys::object_wait(h, 1 << 24, u64::MAX).status as u32,
            // the flag was taken once: the next wait blocks, and times out
            sys::object_wait(h, 1 << 24, sys::clock_get() + 30).status as u32,
            0,
        ),
    };
    INTR_R1.store(r1, Ordering::SeqCst);
    INTR_R2.store(r2, Ordering::SeqCst);
    INTR_R3.store(r3, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// Start the worker `kind`, wait until it is (about to be) blocked, interrupt it and wait for its end.
fn intr_run(kind: usize, interrupt_first: bool) -> Result<u32, Detail> {
    intr_reset();
    let t = new_thread(intr_worker, kind)?;
    while INTR_STARTED.load(Ordering::SeqCst) == 0 {
        sys::thread_yield();
    }
    if interrupt_first {
        // (the worker is spinning on GO, not blocked: the flag is set while it runs)
        ensure_eq!(
            st(sys::thread_interrupt(t)),
            Some(S::Ok),
            "interrupt a running thread"
        );
        INTR_GO.store(1, Ordering::SeqCst);
    } else {
        INTR_GO.store(1, Ordering::SeqCst);
        nap_ticks(15); // (let it reach its wait)
        ensure_eq!(
            st(sys::thread_interrupt(t)),
            Some(S::Ok),
            "interrupt a blocked thread"
        );
    }
    let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 1000);
    ensure_eq!(w.status, 0, "the worker ended (the interrupt woke it)");
    sys::handle_close(t);
    Ok(INTR_R1.load(Ordering::SeqCst))
}

fn thread_interrupt_ends_an_object_wait() -> Outcome {
    INTR_H.store(event()?, Ordering::SeqCst);
    ensure_eq!(
        intr_run(0, false)?,
        INTERRUPTED,
        "an object_wait returns INTERRUPTED"
    );
    Ok(())
}

fn thread_interrupt_ends_a_port_wait() -> Outcome {
    let p = ok(sys::port_create(), "port_create")?.a1 as u64;
    let e = event()?;
    ok(sys::port_bind(p, e, 1 << 24, 5), "bind")?;
    INTR_H.store(p, Ordering::SeqCst);
    ensure_eq!(
        intr_run(1, false)?,
        INTERRUPTED,
        "a port_wait returns INTERRUPTED"
    );
    Ok(())
}

fn thread_interrupt_ends_a_futex_wait() -> Outcome {
    ensure_eq!(
        intr_run(2, false)?,
        INTERRUPTED,
        "a futex_wait returns INTERRUPTED"
    );
    Ok(())
}

fn thread_interrupt_ends_a_channel_call() -> Outcome {
    let (a, b) = channel()?;
    INTR_H.store(a, Ordering::SeqCst);
    ensure_eq!(
        intr_run(3, false)?,
        INTERRUPTED,
        "a channel_call returns INTERRUPTED"
    );
    // the request was sent and is not withdrawn (as a timeout leaves it): the server end can still read it
    let mut buf = [0u8; 64];
    let r = sys::channel_read(b, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
    ensure_eq!(r.status, 0, "the request is still queued");
    Ok(())
}

fn thread_interrupt_before_the_wait_is_not_lost() -> Outcome {
    INTR_H.store(event()?, Ordering::SeqCst);
    ensure_eq!(
        intr_run(0, true)?,
        INTERRUPTED,
        "an interrupt that arrived while the thread ran ends its next blocking call at once"
    );
    Ok(())
}

fn thread_interrupt_is_not_taken_by_a_poll() -> Outcome {
    INTR_H.store(event()?, Ordering::SeqCst);
    intr_run(4, true)?;
    ensure_eq!(
        INTR_R1.load(Ordering::SeqCst),
        S::TimedOut as i32 as u32,
        "the poll just timed out"
    );
    ensure_eq!(
        INTR_R2.load(Ordering::SeqCst),
        INTERRUPTED,
        "the wait after it took the flag"
    );
    Ok(())
}

fn thread_interrupt_waits_for_a_wait_that_would_block() -> Outcome {
    let (e, ready) = (event()?, event()?);
    ok(sys::event_signal(ready, 0, 1 << 24), "signal")?;
    INTR_H.store(e, Ordering::SeqCst);
    INTR_H2.store(ready, Ordering::SeqCst);
    intr_run(5, true)?;
    ensure_eq!(
        INTR_R1.load(Ordering::SeqCst),
        0,
        "a wait whose condition holds succeeds, flag or no flag"
    );
    ensure_eq!(
        INTR_R2.load(Ordering::SeqCst),
        INTERRUPTED,
        "and the flag was still there for the next one"
    );
    Ok(())
}

fn thread_interrupt_polls_leave_the_flag_on_every_wait() -> Outcome {
    let p = ok(sys::port_create(), "port_create")?.a1 as u64;
    let e = event()?;
    ok(sys::port_bind(p, e, 1 << 24, 5), "bind")?;
    INTR_H.store(p, Ordering::SeqCst);
    intr_run(8, true)?;
    ensure_eq!(
        INTR_R1.load(Ordering::SeqCst),
        S::TimedOut as i32 as u32,
        "the port poll timed out"
    );
    ensure_eq!(
        INTR_R2.load(Ordering::SeqCst),
        INTERRUPTED,
        "the port wait after it took the flag"
    );
    let (a, b) = channel()?;
    INTR_H.store(a, Ordering::SeqCst);
    intr_run(7, true)?;
    ensure_eq!(
        INTR_R1.load(Ordering::SeqCst),
        S::TimedOut as i32 as u32,
        "a channel_call with deadline 0 timed out"
    );
    ensure_eq!(
        INTR_R2.load(Ordering::SeqCst),
        INTERRUPTED,
        "the blocking call after it took the flag"
    );
    // only the poll's request was sent: the interrupted call sent nothing
    let mut buf = [0u8; 64];
    ensure_eq!(
        sys::channel_read(b, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0).status,
        0,
        "the first request is queued"
    );
    ensure_eq!(
        sys::channel_read(b, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0).status,
        S::ShouldWait as i32,
        "and no second one"
    );
    Ok(())
}

fn thread_interrupt_is_taken_once() -> Outcome {
    INTR_H.store(event()?, Ordering::SeqCst);
    intr_run(6, true)?;
    ensure_eq!(
        INTR_R1.load(Ordering::SeqCst),
        INTERRUPTED,
        "the first blocking call is interrupted"
    );
    ensure_eq!(
        INTR_R2.load(Ordering::SeqCst),
        S::TimedOut as i32 as u32,
        "the next one blocks and times out"
    );
    Ok(())
}

/// The call's own checks: the handle, its type, MANAGE, and a thread that has ended.
fn thread_interrupt_argument_errors() -> Outcome {
    ensure_eq!(
        st(sys::thread_interrupt(0xdead)),
        Some(S::BadHandle),
        "no such handle"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::thread_interrupt(ev)),
        Some(S::WrongType),
        "an event is no thread"
    );
    let t = new_thread(worker_set, 1)?;
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
    let weak = ok(
        sys::handle_duplicate(t, Rights::INSPECT.0),
        "a thread handle without MANAGE",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::thread_interrupt(weak)),
        Some(S::AccessDenied),
        "MANAGE is needed"
    );
    ensure_eq!(
        st(sys::thread_interrupt(t)),
        Some(S::BadState),
        "a thread that has ended cannot be interrupted"
    );
    Ok(())
}

/// Slice SC task 8: priority is bounded by the job (PR1, PR2). Argument errors, own priority up and down, the job's
/// limits (a child may not exceed its parent), a confined program that cannot raise its priority, and lowering a job's
/// maximum clamps a live thread at once.
fn thread_priority_is_bounded_by_the_job() -> Outcome {
    // argument errors and the caller's own priority
    ensure_eq!(
        st(sys::thread_set_sched(0, 0, 0)),
        Some(S::InvalidArgs),
        "priority 0"
    );
    ensure_eq!(
        st(sys::thread_set_sched(0, 64, 0)),
        Some(S::InvalidArgs),
        "priority 64"
    );
    ensure_eq!(
        st(sys::thread_set_sched(0, 10, 2)),
        Some(S::InvalidArgs),
        "a reserved flag"
    );
    ensure_eq!(
        st(sys::thread_set_sched(0xdead, 10, 0)),
        Some(S::BadHandle),
        "no such handle"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::thread_set_sched(ev, 10, 0)),
        Some(S::WrongType),
        "an event is no thread"
    );
    ensure_eq!(
        st(sys::thread_get_sched(ev)),
        Some(S::WrongType),
        "get on an event"
    );
    let before = ok(sys::thread_get_sched(0), "thread_get_sched")?;
    ensure_eq!(
        before.a1 & 0xff,
        10,
        "user threads start at the default priority"
    );
    ensure_eq!(
        st(sys::thread_set_sched(0, 14, 1)),
        Some(S::Ok),
        "set own priority (round robin)"
    );
    let after = ok(sys::thread_get_sched(0), "thread_get_sched")?;
    ensure_eq!(
        after.a1 & 0xffff,
        14 | 1 << 8,
        "priority and flag read back"
    );
    ensure_eq!(
        st(sys::thread_set_sched(0, 10, 0)),
        Some(S::Ok),
        "back to the default"
    );
    // a job's limits: SET_LIMITS needed, a child cannot exceed its parent, a bad value is refused
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    ensure_eq!(
        st(sys::job_set_sched(j, 15, 0, 1024)),
        Some(S::Ok),
        "set a job's maximum priority"
    );
    ensure_eq!(job_use(j, 6)?, (15, 0), "job_info says so");
    let g = job(j, u64::MAX, u64::MAX, u64::MAX)?;
    ensure_eq!(
        job_use(g, 6)?.0,
        15,
        "a child starts at its parent's maximum"
    );
    ensure_eq!(
        st(sys::job_set_sched(g, 16, 0, 1024)),
        Some(S::AccessDenied),
        "above the parent"
    );
    ensure_eq!(
        st(sys::job_set_sched(g, 15, 1, 1024)),
        Some(S::AccessDenied),
        "a budget above the parent's"
    );
    ensure_eq!(
        st(sys::job_set_sched(g, 64, 0, 1024)),
        Some(S::InvalidArgs),
        "priority 64"
    );
    ensure_eq!(
        st(sys::job_set_sched(g, 10, 0, 0)),
        Some(S::InvalidArgs),
        "weight 0"
    );
    let weak = ok(
        sys::handle_duplicate(g, Rights::INSPECT.0),
        "a job handle without SET_LIMITS",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::job_set_sched(weak, 5, 0, 1024)),
        Some(S::AccessDenied),
        "needs SET_LIMITS"
    );
    ensure_eq!(
        st(sys::job_set_sched(ev, 5, 0, 1024)),
        Some(S::WrongType),
        "an event is no job"
    );
    // a confined program cannot raise its priority above its job's maximum (and can within it)
    let (p, _m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 66, j)?;
    wait_ended(p, 3000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        0,
        "the confined program's checks"
    );
    sys::handle_close(p);
    // lowering the maximum clamps a live thread of the job at once
    let (p, m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 67, j)?;
    let w = sys::object_wait(m, sig::READABLE, sys::clock_get() + 3000);
    ensure_eq!(st(w), Some(S::Ok), "the child raised its priority");
    // thread_spawn into the confined process: a priority above its job's maximum is refused
    {
        // (entry 0 is no code in any process: the check after the priority's)
        let entry = 0;
        let rec = |priority: u32| rt::SpawnArgs {
            entry,
            priority,
            ..Default::default()
        };
        ensure_eq!(
            st(sys::thread_spawn(p, &rec(40))),
            Some(S::AccessDenied),
            "thread_spawn above the job's maximum"
        );
        // (a priority within the maximum passes the priority check)
        ensure_eq!(
            st(sys::thread_spawn(p, &rec(12))),
            Some(S::InvalidArgs),
            "a priority within the maximum passes; the entry (0) is refused"
        );
    }
    let mut b = [0u8; 1];
    sys::channel_read(m, b.as_mut_ptr(), 1, core::ptr::null_mut(), 0);
    ensure_eq!(
        st(sys::job_set_sched(j, 8, 0, 1024)),
        Some(S::Ok),
        "lower the job's maximum"
    );
    ensure_eq!(job_use(g, 6)?.0, 8, "and its child's");
    sys::channel_write(m, b"g".as_ptr(), 1, core::ptr::null(), 0);
    wait_ended(p, 3000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        8,
        "the live thread was clamped to the new maximum"
    );
    for h in [p, m, g, weak, j, root, ev] {
        sys::handle_close(h);
    }
    Ok(())
}

/// A job's thread limit is reached with NO_MEMORY (PL8: there is no per-process constant any more: the job's `threads` level bounds the threads of a process,
/// and the system-wide backstop bounds the rest), and the slots come back when the process ends. The threads of the child share one stack mapping (a slice each).
fn thread_limit_and_slots_are_reused() -> Outcome {
    const LIMIT: u64 = 64;
    const STACK_PAGES: u64 = 4;
    let root = root_job()?;
    let j = job(root, u64::MAX, LIMIT, u64::MAX)?;
    let (p, _m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 16, j)?;
    nap(5)?;
    let v = vmo(LIMIT * STACK_PAGES)?;
    let base = ok(
        sys::process_map_anywhere(p, v, 0, LIMIT * STACK_PAGES * 4096, PF_R | PF_W),
        "map the shared stacks into the child",
    )?
    .a1 as u64;
    sys::handle_close(v);
    let entry = child_entry(p)?;
    let mut ts: Vec<u64> = Vec::new();
    loop {
        let top = base + (ts.len() as u64 + 1) * STACK_PAGES * 4096;
        let r = sys::thread_on_stack(p, entry, top, 16);
        if r.status != 0 {
            ensure_eq!(st(r), Some(S::NoMemory), "the limit is NO_MEMORY");
            break;
        }
        ensure!(
            (ts.len() as u64) < LIMIT,
            "more than the limit of threads was allowed"
        );
        ts.push(r.a1 as u64);
    }
    // the child's first thread plus the others: the limit is the job's in all
    ensure_eq!(
        ts.len() as u64,
        LIMIT - 1,
        "threads created on top of the child's first"
    );
    ensure_eq!(job_use(j, 1)?.0, LIMIT, "the job's thread level is full");
    ensure_eq!(st(sys::process_kill(p, 3)), Some(S::Ok), "kill the child");
    wait_ended(p, 1000)?;
    for &t in &ts {
        sys::handle_close(t);
    }
    sys::handle_close(p);
    job_use_settles(j, 1, 0, 500)?;
    // the slots are the job's: another process in it can make threads again
    let (p2, _m2) = launch_image_in(IMAGE.load(Ordering::Relaxed), 16, j)?;
    ensure_eq!(job_use(j, 1)?.0, 1, "the job's slots came back");
    ensure_eq!(st(sys::process_kill(p2, 3)), Some(S::Ok), "kill the second");
    wait_ended(p2, 1000)?;
    sys::handle_close(p2);
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// What the last run of the sweep's map scenario saw (statuses packed by the scenario: printing inside it would add allocations to the count).
static TRACE: AtomicU64 = AtomicU64::new(0);

/// KT9, the guest half: with the kernel built with `fault-alloc` (`make os-allocfail`), fail the Nth kernel heap allocation of each scenario, for N = 1, 2, ...
/// until the scenario completes with no failure injected. Every call must return (an error or success); an infallible allocation on the path is a kernel panic,
/// and the line printed before each run names the scenario and N. Without the feature the test passes at once.
fn kernel_allocation_failure_sweep() -> Outcome {
    use rt::abi::syscall as nr;
    let stat = |which: u64, arg: u64| {
        let r = rt::syscall(
            nr::DEBUG_STAT,
            [
                rt::debug::handle() as usize,
                which as usize,
                arg as usize,
                0,
                0,
                0,
            ],
        );
        // (a refused call must not read as a zero: that would end a scenario early or skip the test)
        assert_eq!(r.status, 0, "debug_stat {which} refused");
        r.a1 as u64
    };
    if stat(203, 0) == 0 {
        return Ok(());
    }
    let scenarios: [(&str, fn()); 9] = [
        ("channel", || {
            let r = sys::channel_create();
            if r.status != 0 {
                return;
            }
            let (a, b) = (r.a1 as u64, r.a2 as u64);
            let msg = [7u8; 64];
            let _ = sys::channel_write(a, msg.as_ptr(), msg.len(), core::ptr::null(), 0);
            let mut buf = [0u8; 64];
            let _ = sys::channel_read(b, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
            sys::handle_close(a);
            sys::handle_close(b);
        }),
        ("channel with a handle", || {
            let c = sys::channel_create();
            let e = sys::event_create();
            if c.status == 0 && e.status == 0 {
                let (a, b, ev) = (c.a1 as u64, c.a2 as u64, e.a1 as u64);
                let hs = [ev];
                let w = sys::channel_write(a, [1u8].as_ptr(), 1, hs.as_ptr(), 1);
                let mut buf = [0u8; 8];
                let mut got = [0u64; 2];
                let r = sys::channel_read(b, buf.as_mut_ptr(), 8, got.as_mut_ptr(), 2);
                if w.status == 0 && r.status == 0 {
                    sys::handle_close(got[0]);
                } else {
                    sys::handle_close(ev);
                }
                sys::handle_close(a);
                sys::handle_close(b);
            } else {
                for r in [c, e] {
                    if r.status == 0 {
                        sys::handle_close(r.a1 as u64);
                        if r.a2 != 0 {
                            sys::handle_close(r.a2 as u64);
                        }
                    }
                }
            }
        }),
        ("vmo write", || {
            let v = sys::vmo_create(4);
            if v.status == 0 {
                let data = [3u8; 100];
                let _ = sys::vmo_write(v.a1 as u64, 0, data.as_ptr(), data.len());
                sys::handle_close(v.a1 as u64);
            }
        }),
        ("port bind", || {
            let p = sys::port_create();
            let e = sys::event_create();
            if p.status == 0 && e.status == 0 {
                let _ = sys::port_bind(p.a1 as u64, e.a1 as u64, 1, 9);
            }
            for r in [p, e] {
                if r.status == 0 {
                    sys::handle_close(r.a1 as u64);
                }
            }
        }),
        ("job and socket", || {
            let root = sys::job_root();
            if root.status == 0 {
                let j = sys::job_create(root.a1 as u64, &[u64::MAX; 6]);
                if j.status == 0 {
                    sys::handle_close(j.a1 as u64);
                }
                sys::handle_close(root.a1 as u64);
            }
            let s = sys::socket_create();
            if s.status == 0 {
                sys::handle_close(s.a1 as u64);
                if s.a2 != 0 {
                    sys::handle_close(s.a2 as u64);
                }
            }
        }),
        ("process create", || {
            let p = sys::process_create();
            if p.status == 0 {
                sys::handle_close(p.a1 as u64);
            }
        }),
        ("thread spawn and join", || {
            // (a failed spawn is NO_MEMORY with nothing left behind; one that works ends and is joined)
            // (a stack the kernel provisions: the program's own would be a VMO and a handle of its own to lose)
            let me = sys::process_self();
            if me.status == 0 {
                let rec = rt::SpawnArgs {
                    entry: worker_set as *const () as usize as u64,
                    arg: 3,
                    ..rt::SpawnArgs::default()
                };
                let t = sys::thread_spawn(me.a1 as u64, &rec);
                if t.status == 0 {
                    sys::object_wait(t.a1 as u64, sig::TERMINATED, sys::clock_get() + 20_000);
                    sys::handle_close(t.a1 as u64);
                }
                sys::handle_close(me.a1 as u64);
            }
        }),
        ("futex wait and wake", || {
            // (the word is 0 and so is `expected`: the wait queues and times out, or says NO_MEMORY with nothing queued; the wake finds nobody or reports it)
            static WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            sys::futex_wait(WORD.as_ptr(), 0, sys::clock_get() + 2);
            sys::futex_wake(WORD.as_ptr(), 1);
        }),
        ("map and touch", || {
            let me = sys::process_self();
            let v = sys::vmo_create(4);
            if me.status == 0 && v.status == 0 {
                let m =
                    sys::process_map_anywhere(me.a1 as u64, v.a1 as u64, 0, 4 * 4096, PF_R | PF_W);
                TRACE.store(m.status as u64 & 0xff, Ordering::Relaxed);
                if m.status == 0 {
                    // SAFETY: the mapping was just made (4 pages, read-write) and `a1` is its base
                    unsafe { core::ptr::write_volatile((m.a1 as usize + 4096) as *mut u8, 1) };
                    // (an unmap that failed for want of memory left the mapping, and with it the VMO: the failure is injected once, so it is retried)
                    let u = sys::process_unmap(me.a1 as u64, m.a1 as u64).status;
                    TRACE.fetch_or((u as u64 & 0xff) << 8, Ordering::Relaxed);
                    if u != 0 {
                        let _ = sys::process_unmap(me.a1 as u64, m.a1 as u64);
                    }
                }
            }
            for r in [me, v] {
                if r.status == 0 {
                    sys::handle_close(r.a1 as u64);
                }
            }
        }),
    ];
    for (name, run) in scenarios {
        let vmos_before = stat(204, 0);
        let objs_before = stat(206, 0);
        let mut n = 1u64;
        loop {
            let mut line = Detail::new();
            let _ =
                core::fmt::Write::write_fmt(&mut line, format_args!("alloc sweep: {name} n={n}\n"));
            let _ = sys::debug_write(line.as_bytes());
            stat(200, n);
            run();
            let fired = stat(201, 0) != 0;
            // (a destruction that found no memory leaks its objects until a sweep reclaims them)
            stat(205, 0);
            ensure_eq!(
                stat(204, 0),
                vmos_before,
                "scenario {name} with the allocation {n} failing left a VMO behind (trace {:#x})",
                TRACE.load(Ordering::Relaxed)
            );
            ensure_eq!(
                stat(206, 0),
                objs_before,
                "scenario {name} with the allocation {n} failing left a kernel object behind"
            );
            if !fired {
                break;
            }
            n += 1;
            ensure!(
                n < 2000,
                "the scenario {name} never completed: allocations without end"
            );
        }
        ensure!(
            n > 1,
            "the scenario {name} made no kernel allocation to fail"
        );
        // the persistent pass: every class-R allocation from the first on fails, as it does with the heap at its reserve; nothing may panic or leak. (A
        // scenario that touches a lazily committed page is left out: the fault that finds no memory ends the process, which is what it must do, and
        // this process is the one running the scenario)
        if name == "map and touch" {
            continue;
        }
        let mut line = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut line,
            format_args!("alloc sweep: {name} persistent\n"),
        );
        let _ = sys::debug_write(line.as_bytes());
        stat(208, 1);
        run();
        stat(201, 0);
        stat(205, 0);
        ensure_eq!(
            stat(204, 0),
            vmos_before,
            "scenario {name} with every allocation failing left a VMO behind"
        );
        ensure_eq!(
            stat(206, 0),
            objs_before,
            "scenario {name} with every allocation failing left a kernel object behind"
        );
    }
    Ok(())
}

/// PL8: a job's region quota bounds the regions of one address space of its processes (the quota replaces the constant): mapping one more VMO than it allows is
/// NO_MEMORY, an unmap makes room, a lowered quota leaves what is mapped and refuses more, and `job_info` 11 reports the effective value.
fn a_jobs_region_quota_bounds_its_processes() -> Outcome {
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let before = ok(sys::job_info(j, 11), "job_info 11")?.a1 as u64;
    ensure_eq!(
        before,
        rt::abi::limits::MAX_REGIONS as u64,
        "no quota: the backstop"
    );
    let (p, _m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 16, j)?;
    nap(5)?;
    // the child already holds some regions (image, stack, library): the quota leaves room for ten more
    let held = {
        let mut n = 0u64;
        let mut at = 0u64;
        for _ in 0..256 {
            let mut rec = [0u64; 6];
            let next = ok(
                sys::process_map_info(p, at, rec.as_mut_ptr()),
                "the child's mappings",
            )?
            .a1 as u64;
            n += 1;
            if next == 0 {
                break;
            }
            at = next;
        }
        n
    };
    let sr = sys::job_set_region_limit(j, held + 10);
    ensure_eq!(
        sr.status,
        0,
        "set the quota (held {held}, a1 {}, a2 {})",
        sr.a1,
        sr.a2
    );
    ensure_eq!(
        ok(sys::job_info(j, 11), "job_info 11")?.a1 as u64,
        held + 10,
        "the quota shows"
    );
    let mut bases = Vec::new();
    let mut vmos = Vec::new();
    loop {
        let v = vmo(1)?;
        let r = sys::process_map_anywhere(p, v, 0, 4096, PF_R | PF_W);
        if r.status != 0 {
            ensure_eq!(st(r), Some(S::NoMemory), "the quota is NO_MEMORY");
            sys::handle_close(v);
            break;
        }
        ensure!(bases.len() < 20, "more regions than the quota allowed");
        bases.push(r.a1 as u64);
        vmos.push(v);
    }
    ensure_eq!(bases.len(), 10, "ten more regions fit");
    // an unmap makes room for one more
    ok(sys::process_unmap(p, bases.pop().unwrap_or(0)), "unmap one")?;
    let v = vmo(1)?;
    let r = sys::process_map_anywhere(p, v, 0, 4096, PF_R | PF_W);
    ensure_eq!(r.status, 0, "room again after an unmap");
    sys::handle_close(v);
    // lowering the quota below what is held leaves it and refuses more
    ensure_eq!(
        st(sys::job_set_region_limit(j, 1)),
        Some(S::Ok),
        "lower the quota"
    );
    let v = vmo(1)?;
    ensure_eq!(
        st(sys::process_map_anywhere(p, v, 0, 4096, PF_R | PF_W)),
        Some(S::NoMemory),
        "nothing more"
    );
    sys::handle_close(v);
    for v in vmos {
        sys::handle_close(v);
    }
    ensure_eq!(st(sys::process_kill(p, 3)), Some(S::Ok), "kill");
    wait_ended(p, 1000)?;
    sys::handle_close(p);
    job_use_settles(j, 2, 0, 500)?;
    sys::handle_close(j);
    sys::handle_close(root);
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
    ensure_eq!(sys::process_info(p).a1 as i64, -1001, "the page fault");
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
    let mut ps = [0u64; 40];
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

/// Map `v` (4 pages) read-write into child `p` where the kernel chooses (the child's own image and stack are placed at random, so no fixed address is free for sure).
fn map_child_stack(p: u64, v: u64) -> Result<u64, Detail> {
    Ok(ok(
        sys::process_map_anywhere(p, v, 0, 4 * 4096, PF_R | PF_W),
        "map a child stack",
    )?
    .a1 as u64)
}

/// The entry point of the program image inside child `p` (HD2: the image is a static-PIE, so the link-time entry is an offset from the base the kernel chose;
/// the image's text is its larger executable mapping).
fn child_entry(p: u64) -> Result<u64, Detail> {
    let mut at = 0u64;
    let mut best = (0u64, 0u64);
    for _ in 0..64 {
        let mut rec = [0u64; 6];
        let next = ok(
            sys::process_map_info(p, at, rec.as_mut_ptr()),
            "the child's mappings",
        )?
        .a1 as u64;
        // (AB7: a started child has the system library's text as well, a few pages: the image's text is the larger executable mapping)
        if rec[2] & 0b100 != 0 && rec[1] > best.1 {
            best = (rec[0], rec[1]);
        }
        if next == 0 {
            break;
        }
        at = next;
    }
    if best.1 == 0 {
        return Err(detail("the child has no executable mapping"));
    }
    Ok(best.0 + link_entry()?)
}

fn link_entry() -> Result<u64, Detail> {
    let image = IMAGE.load(Ordering::Relaxed);
    let mut eh = [0u8; 64];
    ok(
        sys::vmo_read(image, 0, eh.as_mut_ptr(), 64),
        "read the ELF header",
    )?;
    Ok(le(&eh, 24, 8))
}

/// Another thread in child `p` at the program entry with start arg `arg`, on its own stack.
fn child_thread(p: u64, slot: u64, arg: u64) -> Result<u64, Detail> {
    let _ = slot;
    let v = vmo(4)?;
    let base = map_child_stack(p, v)?;
    sys::handle_close(v);
    let entry = child_entry(p)?;
    let r = sys::thread_on_stack(p, entry, base + 4 * 4096, arg);
    if r.status != 0 {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "thread_on_stack in a child: status {} entry {entry:#x} stack {:#x}",
                r.status,
                base + 4 * 4096
            ),
        );
        return Err(d);
    }
    Ok(r.a1 as u64)
}

fn thread_ended_with(t: u64, code: i64) -> Outcome {
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
    sys::object_wait(arg as u64, 1 << 24, u64::MAX);
    sys::thread_exit(1)
}

/// Sending a handle away in a message ends a sibling's wait on it too (a transfer removes the handle from
/// this process: BAD_HANDLE for the waiter, promptly), review finding H1 of slice W step 3.
fn sending_a_waited_handle_ends_a_siblings_wait() -> Outcome {
    let ev = event()?;
    let (a, b) = channel()?;
    WOKEN_STATUS.store(99, Ordering::SeqCst);
    let t = new_thread(worker_wait_event, ev as usize)?;
    nap(4)?;
    for _ in 0..3 {
        sys::thread_yield();
    }
    let hs = [ev];
    ensure_eq!(
        st(sys::channel_write(a, b"x".as_ptr(), 1, hs.as_ptr(), 1)),
        Some(S::Ok),
        "send the event away"
    );
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 100);
    let prompt = r.status == 0;
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 3000);
    ensure!(prompt, "the waiter slept on after its handle was sent away");
    ensure_eq!(
        WOKEN_STATUS.load(Ordering::SeqCst) as i32,
        S::BadHandle.code(),
        "the wait reports BAD_HANDLE"
    );
    sys::handle_close(b);
    Ok(())
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
    let _ev2 = ok(sys::handle_replace(ev, Rights::SAME.0), "replace")?.a1 as u64;
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

fn child_with_a_ready_exit_thread() -> Result<(u64, u64, u64), Detail> {
    let (p, m) = launch(25)?;
    let v = vmo(4)?;
    let base = map_child_stack(p, v)?;
    nap(3)?;
    // a thread that would exit(42) at once
    let t = ok(
        sys::thread_on_stack(p, child_entry(p)?, base + 4 * 4096, 8),
        "thread_on_stack",
    )?
    .a1 as u64;
    Ok((p, m, t))
}

fn a_thread_killed_before_it_ran_runs_no_user_code() -> Outcome {
    // The thread is made on another hart's behalf and can start there as soon as it exists, so whether the kill lands before it runs is a race that only one hart
    // decides (T226: a deterministic version needs the placement masks of SC2). The property is about the runs the kill does win: such a run must end with the
    // kill's code for the process and for the thread, and no run may end any other way than the thread having exited first with its own code (42).
    let mut won = 0;
    for _ in 0..40 {
        let (p, _m, t) = child_with_a_ready_exit_thread()?;
        ensure_eq!(
            st(sys::process_kill(p, 55)),
            Some(S::Ok),
            "kill before it ran"
        );
        wait_ended(p, 1000)?;
        let code = sys::process_info(p).a1 as i64;
        if code == 55 {
            thread_ended_with(t, 55)?;
            won += 1;
            if won >= 3 {
                return Ok(());
            }
        } else {
            ensure_eq!(
                code,
                42,
                "the thread ran first and exited with its own code"
            );
        }
    }
    ensure!(
        won > 0,
        "in forty tries the kill never came before the thread ran"
    );
    Ok(())
}

fn kill_a_child_whose_threads_never_ran() -> Outcome {
    let (p, _m) = launch(15)?;
    let mut ts = [0u64; 7];
    for (i, t) in ts.iter_mut().enumerate() {
        *t = child_thread(p, i as u64, 15)?;
    }
    // (the per-process thread limit is checked by `thread_limit_and_slots_are_reused`; filling it
    // with spinning threads would now take every hart)
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
    let mut ts = [0u64; 6];
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
    let mut ts = [0u64; 7];
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

/// A child that has taken its own process handle is still killed when its parent closes the last
/// handle the parent holds (the self handle must not keep the kill-on-close object alive).
fn kill_on_close_holds_after_the_child_took_a_self_handle() -> Outcome {
    let (p, mine) = launch(64)?;
    nap(5)?; // let the child run process_self and start spinning
    ensure_eq!(st(sys::handle_close(p)), Some(S::Ok), "drop the process");
    let r = sys::object_wait(mine, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(
        r.status,
        0,
        "the child was killed although it held a self handle"
    );
    Ok(())
}

fn a_thread_made_by_the_parent_faults_or_exits() -> Outcome {
    let (p, _m) = launch(16)?;
    nap(3)?;
    let b = child_thread(p, 0, 16)?;
    started_and_blocked(b)?;
    let t = child_thread(p, 1, 7)?; // null load
    wait_ended_or_say(p, &[b, t], 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, -1001, "the fault code");
    thread_ended_with(t, -1001)?;
    thread_ended_with(b, -1001)?;
    let (p, _m) = launch(16)?;
    nap(3)?;
    let b = child_thread(p, 0, 16)?;
    started_and_blocked(b)?;
    let t = child_thread(p, 1, 8)?; // exit(42)
    wait_ended_or_say(p, &[b, t], 1000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 42, "exit(42) from a thread");
    thread_ended_with(t, 42)?;
    thread_ended_with(b, 42)?;
    ensure_eq!(
        st(sys::thread_on_stack(
            p,
            0x10000, // (any entry: the process has ended)
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

fn socket() -> Result<(u64, u64), Detail> {
    let r = ok(sys::socket_create(), "socket_create")?;
    Ok((r.a1 as u64, r.a2 as u64))
}

fn port() -> Result<u64, Detail> {
    Ok(ok(sys::port_create(), "port_create")?.a1 as u64)
}

fn sleep_in_thread(ticks: u64) {
    let gate = sys::event_create().a1 as u64;
    sys::object_wait(gate, 1 << 24, sys::clock_get() + ticks);
    sys::handle_close(gate);
}

extern "C" fn worker_signal_later(arg: usize) -> ! {
    sleep_in_thread(6);
    sys::event_signal(arg as u64, 0, 1 << 24);
    sys::thread_exit(0)
}

extern "C" fn worker_port_wait_then_report(arg: usize) -> ! {
    let r = sys::port_wait(arg as u64, sys::clock_get() + 1000);
    sys::thread_exit(if r.status == 0 { 0 } else { 1 })
}

extern "C" fn worker_write_later(arg: usize) -> ! {
    sleep_in_thread(6);
    // Let the waiter that the set-up calls woke go back to sleep: with one global wait queue a
    // missing wake-up is otherwise masked by an earlier one (found by the wake-up audit).
    for _ in 0..3 {
        sys::thread_yield();
    }
    let r = sys::socket_write(arg as u64, b"ping".as_ptr(), 4);
    // Linger without making any syscall that wakes waiters, so that only the write's own
    // wake-up can end a waiter's sleep (an exit would wake everyone and hide a missing one).
    // (400 ms: the readers' bound is the slack of `within`, 250 ms above the expected time, and a wake by the exit must fall outside it.)
    let until = sys::clock_get() + 400;
    while sys::clock_get() < until {
        core::hint::spin_loop();
    }
    sys::thread_exit(r.status as i32)
}

/// Wait (up to 2000 ticks) until the socket can take a write, then write 3 bytes.
extern "C" fn worker_write_when_writable(arg: usize) -> ! {
    let r = sys::object_wait(arg as u64, sig::WRITABLE, sys::clock_get() + 2000);
    if r.status != 0 {
        sys::thread_exit(1);
    }
    let w = sys::socket_write(arg as u64, b"abc".as_ptr(), 3);
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
                a as usize ^ (1usize << 40),
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
    // the zero-length cases of SYSCALLS.md (T91): a probe is OK on a live socket, BAD_STATE after our own shutdown, PEER_CLOSED
    // when the peer is gone, and the shutdown check comes before the peer check
    let (c, d) = socket()?;
    ensure_eq!(
        st(sys::socket_write(c, big.as_ptr(), 0)),
        Some(S::Ok),
        "a zero-length write on a live socket"
    );
    sys::socket_shutdown(c);
    ensure_eq!(
        st(sys::socket_write(c, big.as_ptr(), 0)),
        Some(S::BadState),
        "a zero-length write after our own shutdown"
    );
    sys::handle_close(d);
    ensure_eq!(
        st(sys::socket_write(c, big.as_ptr(), 0)),
        Some(S::BadState),
        "BAD_STATE comes before PEER_CLOSED"
    );
    let (e2, f2) = socket()?;
    sys::handle_close(f2);
    ensure_eq!(
        st(sys::socket_write(e2, big.as_ptr(), 0)),
        Some(S::PeerClosed),
        "a zero-length write to a dead peer"
    );
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

fn socket_half_close() -> Outcome {
    let (a, b) = socket()?;
    let mut buf = [0u8; 8];
    // argument checks
    ensure_eq!(
        st(sys::socket_shutdown(0xdead)),
        Some(S::BadHandle),
        "a bad handle"
    );
    let (c, _d) = channel()?;
    ensure_eq!(
        st(sys::socket_shutdown(c)),
        Some(S::WrongType),
        "a channel is not a socket"
    );
    let ro = ok(sys::handle_duplicate(b, Rights::READ.0), "dup read-only")?.a1 as u64;
    ensure_eq!(
        st(sys::socket_shutdown(ro)),
        Some(S::AccessDenied),
        "no WRITE right"
    );
    // nothing is closed yet: a wait for WRITE_CLOSED times out
    let t0 = sys::clock_get();
    ensure_eq!(
        st(sys::object_wait(b, sig::WRITE_CLOSED, t0 + 20)),
        Some(S::TimedOut),
        "no half-close yet"
    );
    sys::socket_write(a, b"req".as_ptr(), 3);
    ensure_eq!(st(sys::socket_shutdown(a)), Some(S::Ok), "shutdown");
    ensure_eq!(
        st(sys::socket_shutdown(a)),
        Some(S::Ok),
        "shutdown is idempotent"
    );
    let s = sys::object_signals(b).a1 as u32;
    ensure_eq!(
        s & (sig::READABLE | sig::WRITE_CLOSED | sig::PEER_CLOSED | sig::WRITABLE),
        sig::READABLE | sig::WRITE_CLOSED | sig::WRITABLE,
        "data and WRITE_CLOSED, the peer is not closed and may still write"
    );
    ensure_eq!(
        st(sys::object_wait(b, sig::WRITE_CLOSED, u64::MAX)),
        Some(S::Ok),
        "a wait for WRITE_CLOSED returns"
    );
    let r = sys::socket_read(b, buf.as_mut_ptr(), 8);
    ensure_eq!(
        (r.status, r.a1, &buf[..3]),
        (0, 3, &b"req"[..]),
        "the data first"
    );
    ensure_eq!(
        st(sys::socket_read(b, buf.as_mut_ptr(), 8)),
        Some(S::PeerClosed),
        "then the end of the stream"
    );
    ensure_eq!(
        st(sys::socket_write(a, b"x".as_ptr(), 1)),
        Some(S::BadState),
        "the shut end cannot write"
    );
    // the reply direction still works
    sys::socket_write(b, b"resp".as_ptr(), 4);
    let r = sys::socket_read(a, buf.as_mut_ptr(), 8);
    ensure_eq!(
        (r.status, r.a1, &buf[..4]),
        (0, 4, &b"resp"[..]),
        "the reply arrives"
    );
    ensure_eq!(
        st(sys::socket_read(a, buf.as_mut_ptr(), 8)),
        Some(S::ShouldWait),
        "no end of stream in the other direction"
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
    let mut hb = [0u64; 1];
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
    // Strictly: the writer lingers 400 ms after its write without waking anyone, so a reader woken only by the
    // writer's exit would be about 400 ms late; the bound allows a stalled guest (AB2) and still excludes that.
    let took = sys::clock_get() - t0;
    ensure!(within(took, 5, 25), "after about 6 ms, not {took}");
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
            [p as usize ^ (1usize << 40), e as usize, m as usize, 1, 0, 0]
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
            [p as usize ^ (1usize << 40), 0, 0, 0, 0, 0]
        )),
        Some(S::BadHandle),
        "wide handle"
    );
    ensure_eq!(st(sys::port_unbind(p, 1)), Some(S::Ok), "unbind");
    // (there is no fixed binding limit any more: `port_oneshot_rearm_and_many_bindings`)
    Ok(())
}

fn port_wait_many_takes_a_batch_in_order() -> Outcome {
    use rt::PortPacket;
    let p = port()?;
    let m = 1u32 << 24;
    let mut events = Vec::new();
    for key in 1..=64u64 {
        let e = event()?;
        ensure_eq!(st(sys::port_bind(p, e, m, key)), Some(S::Ok), "bind");
        events.push(e);
    }
    let mut buf = [PortPacket::default(); 64];
    ensure_eq!(
        st(sys::port_wait_many(p, &mut buf, 0)),
        Some(S::TimedOut),
        "nothing ready, no wait"
    );
    for &e in &events {
        sys::event_signal(e, 0, m);
    }
    let r = sys::port_wait_many(p, &mut buf, 0);
    ensure_eq!((r.status, r.a1), (0, 64), "64 ready bindings in one call");
    for (i, rec) in buf.iter().enumerate() {
        ensure_eq!(
            (rec.key, rec.observed, rec.zero),
            (i as u64 + 1, m, 0),
            "record {i}"
        );
    }
    // a smaller batch continues the rotation where port_wait would: 8 now, the next 8 after them
    let r = sys::port_wait_many(p, &mut buf[..8], 0);
    ensure_eq!((r.status, r.a1), (0, 8), "a batch of 8");
    ensure_eq!(buf[0].key, 1, "the cursor wrapped to the first");
    let r = sys::port_wait_many(p, &mut buf[..8], 0);
    ensure_eq!((r.status, r.a1, buf[0].key), (0, 8, 9), "the next 8");
    let single = sys::port_wait(p, 0);
    ensure_eq!(
        (single.status, single.a1),
        (0, 17),
        "port_wait goes on from there"
    );
    // argument checks: capacity, buffer (both before anything is taken), handle
    let call = |h: u64, buf: usize, cap: usize| {
        st(rt::syscall(
            rt::abi::syscall::PORT_WAIT_MANY,
            [h as usize, buf, cap, 0, 0, 0],
        ))
    };
    let at = buf.as_mut_ptr() as usize;
    ensure_eq!(call(p, at, 0), Some(S::InvalidArgs), "capacity 0");
    ensure_eq!(call(p, at, 65), Some(S::InvalidArgs), "capacity 65");
    ensure_eq!(call(p, 0x10, 4), Some(S::InvalidArgs), "an unmapped buffer");
    ensure_eq!(
        call(p, 0x10, 0),
        Some(S::InvalidArgs),
        "capacity 0 and no buffer"
    );
    ensure_eq!(call(BAD, at, 4), Some(S::BadHandle), "bad handle");
    ensure_eq!(call(events[0], at, 4), Some(S::WrongType), "not a port");
    let nowait = weaker(p, Rights::MANAGE.0)?;
    ensure_eq!(call(nowait, at, 4), Some(S::AccessDenied), "needs WAIT");
    ensure_eq!(
        call(p ^ (1u64 << 40), at, 4),
        Some(S::BadHandle),
        "a wide handle"
    );
    Ok(())
}

fn port_wait_many_leaves_everything_on_a_bad_buffer() -> Outcome {
    use rt::PortPacket;
    let p = port()?;
    let ev = event()?;
    let m = 1u32 << 24;
    ensure_eq!(
        st(sys::port_bind_flags(
            p,
            ev,
            m,
            5,
            rt::abi::port_flags::ONESHOT
        )),
        Some(S::Ok),
        "a one-shot binding"
    );
    sys::event_signal(ev, 0, m);
    let r = st(rt::syscall(
        rt::abi::syscall::PORT_WAIT_MANY,
        [p as usize, 0x10, 4, 0, 0, 0],
    ));
    ensure_eq!(r, Some(S::InvalidArgs), "a bad buffer");
    // the one-shot packet was not taken: a good call still gets it, once
    let mut buf = [PortPacket::default(); 4];
    let r = sys::port_wait_many(p, &mut buf, 0);
    ensure_eq!((r.status, r.a1, buf[0].key), (0, 1, 5), "still there");
    ensure_eq!(
        st(sys::port_wait_many(p, &mut buf, 0)),
        Some(S::TimedOut),
        "delivered once"
    );
    // it blocks until something is ready, and a deadline ends the wait
    let t0 = sys::clock_get();
    ensure_eq!(
        st(sys::port_wait_many(p, &mut buf, t0 + 20)),
        Some(S::TimedOut),
        "a deadline with nothing ready"
    );
    ensure!(sys::clock_get() >= t0 + 20, "not early");
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

/// Slice W step 6: a one-shot binding is delivered once and stays quiet (though its level is still
/// high) until `port_rearm`; a blocked waiter is woken by the re-arm; and a port takes far more
/// bindings than the old fixed cap of 256 (the job quota is the limit).
static PW_DONE: AtomicU32 = AtomicU32::new(0);

extern "C" fn pw_waiter(arg: usize) -> ! {
    let r = sys::port_wait(arg as u64, sys::clock_get() + 8_000);
    if r.status == 0 && r.a1 == 7 {
        PW_DONE.fetch_add(1, Ordering::SeqCst);
    }
    sys::thread_exit(0)
}

/// L21b: a port wakes one waiter and the poll of that waiter passes the baton, so every thread waiting on a port whose binding is ready (and stays
/// ready: it is level-triggered) gets its turn and reports the binding.
fn several_waiters_on_one_port_are_woken_in_turn() -> Outcome {
    PW_DONE.store(0, Ordering::SeqCst);
    let p = port()?;
    let ev = event()?;
    ok(sys::port_bind(p, ev, 1 << 24, 7), "bind")?;
    let mut ts = Vec::new();
    for _ in 0..5 {
        ts.push(new_thread(pw_waiter, p as usize)?);
    }
    nap_ticks(30); // all of them are asleep in port_wait
    ensure_eq!(
        PW_DONE.load(Ordering::SeqCst),
        0,
        "a waiter returned with nothing ready"
    );
    ok(sys::event_signal(ev, 0, 1 << 24), "signal")?;
    for t in &ts {
        let w = sys::object_wait(*t, sig::TERMINATED, sys::clock_get() + 8_000);
        ensure_eq!(w.status, 0, "a waiter finished");
    }
    for t in ts {
        sys::handle_close(t);
    }
    ensure_eq!(
        PW_DONE.load(Ordering::SeqCst),
        5,
        "every waiter was woken in turn and saw the binding"
    );
    Ok(())
}

/// L21c (T61): a binding ends with the handle it was made through; a binding made through another handle to the same object stays.
fn a_binding_ends_with_the_handle_it_was_made_through() -> Outcome {
    let p = port()?;
    let ev = event()?;
    let dup = ok(sys::handle_duplicate(ev, Rights::SAME.0), "duplicate")?.a1 as u64;
    ok(
        sys::port_bind(p, ev, 1 << 24, 1),
        "bind through the first handle",
    )?;
    ok(
        sys::port_bind(p, dup, 1 << 25, 2),
        "bind through the duplicate",
    )?;
    ok(
        sys::event_signal(ev, 0, (1 << 24) | (1 << 25)),
        "signal both",
    )?;
    let r = sys::port_wait(p, 0);
    ensure_eq!(r.status, 0, "both bindings are ready");
    // the first handle goes: its binding goes with it, and only the duplicate's key (2) is ever reported again
    ok(sys::handle_close(ev), "close the first handle")?;
    for _ in 0..4 {
        let r = sys::port_wait(p, 0);
        ensure!(
            r.status == 0 && r.a1 == 2,
            "a binding made through a closed handle was still reported (status {}, key {})",
            r.status,
            r.a1
        );
    }
    ok(sys::handle_close(dup), "close the duplicate")?;
    let r = sys::port_wait(p, sys::clock_get() + 20);
    ensure_eq!(st(r), Some(S::TimedOut), "no binding is left");
    Ok(())
}

fn port_oneshot_rearm_and_many_bindings() -> Outcome {
    let p = port()?;
    let ev = event()?;
    let m = 1u32 << 24;
    ensure_eq!(
        st(sys::port_bind_flags(
            p,
            ev,
            m,
            5,
            rt::abi::port_flags::ONESHOT
        )),
        Some(S::Ok),
        "a one-shot binding"
    );
    ensure_eq!(
        st(sys::port_bind_flags(p, ev, m, 6, 2)),
        Some(S::InvalidArgs),
        "an unknown flag"
    );
    ensure_eq!(
        st(sys::port_rearm(p, 99)),
        Some(S::InvalidArgs),
        "unknown key"
    );
    sys::event_signal(ev, 0, m);
    let r = sys::port_wait(p, 0);
    ensure_eq!((r.status, r.a1), (0, 5), "delivered once");
    let r = sys::port_wait(p, sys::clock_get() + 10);
    ensure_eq!(
        st(r),
        Some(S::TimedOut),
        "quiet until re-armed, the level is still high"
    );
    // a waiter blocked in port_wait is woken by the re-arm (the thread waits, we re-arm)
    let t = new_thread(worker_port_wait_then_report, p as usize)?;
    nap_ticks(5);
    ensure_eq!(st(sys::port_rearm(p, 5)), Some(S::Ok), "re-arm");
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 100);
    ensure_eq!(
        st(r),
        Some(S::Ok),
        "the blocked waiter was woken by the re-arm"
    );
    // far more than 256 bindings
    let p2 = port()?;
    for k in 0..600u64 {
        ensure_eq!(st(sys::port_bind(p2, ev, m, k)), Some(S::Ok), "binding {k}");
    }
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
    // (the system-wide budget is derived from RAM, PL8: stat 121; a few sockets more than it holds must be refused)
    let most = sys::debug_stat(121) as usize / SOCKET_BUFFER + 16;
    let mut pairs = vec![(0u64, 0u64); most];
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
    ensure!(hit_the_limit, "{most} sockets of 64 KiB were accepted");
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
    // (the cap is derived from RAM, PL8: on a machine whose cap is beyond what this test can park, the job quotas' tests are what end in NO_MEMORY)
    if sys::debug_stat(120) > 10 * 128 * 64 - 256 {
        return Ok(());
    }
    let mut chans = [0u64; 10];
    for i in 0..5 {
        let (a, b) = channel()?;
        chans[2 * i] = a;
        chans[2 * i + 1] = b;
    }
    let mut made = 0u32;
    let mut refused = None;
    'outer: for &ch in chans.iter() {
        for _ in 0..128 {
            let mut hs = [0u64; 64];
            let mut got = 0;
            for h in hs.iter_mut() {
                let r = sys::event_create();
                if r.status != 0 {
                    refused = st(r);
                    break;
                }
                *h = r.a1 as u64;
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

/// PL8: the backstops are derived from the memory of the machine (KERNEL-V1-SPEC 2.5), read through `debug_stat` 120 to 124. The formulas are
/// restated here, from the frame count the kernel reports (stat 102), so a change to either side fails the test.
fn backstops_follow_the_ram_of_the_machine() -> Outcome {
    let ram = sys::debug_stat(102) * 4096;
    let slots = 32768u64; // the kernel-stack window (1 GiB) over a 32 KiB slot
    ensure!(
        ram >= 64 << 20,
        "the machine has at least 64 MiB, it has {ram}"
    );
    let want = [
        (120, (ram / 4096).max(16384), "objects: RAM / 4096"),
        (121, (ram / 32).max(2 << 20), "queued IPC bytes: RAM / 32"),
        (122, 64, "processes: RAM / 1 MiB, held to 64 until T214"),
        (
            123,
            (ram >> 16).min(slots).max(4096),
            "user threads: RAM / 64 KiB, at most the stack window",
        ),
        (124, (ram / 16 / 4096).max(1024), "DMA pages: RAM / 16"),
    ];
    for (which, expect, what) in want {
        ensure_eq!(sys::debug_stat(which), expect, "{what}");
    }
    Ok(())
}

/// HD5: the reserved places in the ABI are named and refused with the stated status, and a refusal changes nothing (KERNEL-V1-SPEC 7.5).
fn reserved_abi_space_is_refused() -> Outcome {
    use rt::abi::{map_flags, start_features, syscall as nr, thread_state};
    let p = proc_create()?;
    let v = vmo(1)?;
    let at = 0x10_0000_0000u64;
    for flag in [
        map_flags::SHADOW_STACK,
        map_flags::RESERVED_9,
        map_flags::SHADOW_STACK | map_flags::RESERVED_9,
    ] {
        ensure_eq!(
            st(sys::process_map(p, v, at, 0, 4096, PF_R | flag)),
            Some(S::NotSupported),
            "process_map with the reserved flag {flag:#x}"
        );
    }
    ok(
        sys::process_map(p, v, at, 0, 4096, PF_R),
        "the same map without the flag: nothing was mapped by the refusals",
    )?;
    ensure_eq!(
        st(sys::thread_read_state(
            0,
            thread_state::CFI,
            core::ptr::null_mut(),
            0
        )),
        Some(S::NotSupported),
        "the CFI state kind, read"
    );
    ensure_eq!(
        st(sys::thread_write_state(
            0,
            thread_state::CFI,
            core::ptr::null(),
            0
        )),
        Some(S::NotSupported),
        "the CFI state kind, write"
    );
    let (a, _b) = channel()?;
    let q = proc_create()?;
    for (features, want) in [
        (start_features::LANDING_PADS, S::NotSupported),
        (start_features::SHADOW_STACK, S::NotSupported),
        (start_features::POINTER_MASKING, S::NotSupported),
        (start_features::ALL, S::NotSupported),
        (8, S::InvalidArgs),
        (1 << 63, S::InvalidArgs),
        (start_features::LANDING_PADS | 8, S::InvalidArgs),
    ] {
        ensure_eq!(
            st(rt::syscall(
                nr::PROCESS_START,
                [q as usize, 0, 0, a as usize, 0, features as usize]
            )),
            Some(want),
            "process_start with features {features:#x}"
        );
    }
    ensure_eq!(
        st(sys::process_start(q, 0, 0, a, 0)),
        Some(S::InvalidArgs),
        "with features 0 the call is the old one: its entry is checked, and refused"
    );
    ensure_eq!(
        sys::handle_duplicate(a, Rights::SAME.0).status,
        0,
        "the bootstrap handle is still ours: nothing moved"
    );
    Ok(())
}

fn mappings_per_process_are_bounded() -> Outcome {
    // in a process of its own that never starts: closing it frees every region at once (unmapping thousands of
    // regions one by one costs a shootdown each)
    let p = proc_create()?;
    let v = vmo(1)?;
    let base = 0x10_0000_0000u64;
    let mut n = 0u64;
    let mut refused = None;
    while n < 20_000 {
        let r = sys::process_map(p, v, base + n * 8192, 0, 4096, PF_R);
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
    ok(sys::process_unmap(p, base), "unmap one")?;
    ensure_eq!(
        st(sys::process_map(p, v, base, 0, 4096, PF_R)),
        Some(S::Ok),
        "room again"
    );
    sys::handle_close(p);
    sys::handle_close(v);
    Ok(())
}

/// Queued handles are kernel memory: they are charged to the IPC budget.
fn queued_handles_are_charged_to_the_budget() -> Outcome {
    let e = event()?;
    let mut sent = 0u32;
    let mut refused = None;
    // (the budget is derived from RAM, PL8: twenty pairs of channels fill 2 MiB of it, so as many more as the budget is bigger)
    let scale = (sys::debug_stat(121) as usize / (2 << 20)).max(1);
    let mut chans = vec![0u64; 40 * scale];
    let mut nch = 0;
    'outer: for _ in 0..20 * scale {
        let (a, b) = channel()?;
        chans[nch] = a;
        chans[nch + 1] = b;
        nch += 2;
        for side in [a, b] {
            for _ in 0..128 {
                let mut hs = [0u64; 64];
                for h in hs.iter_mut() {
                    let r = sys::handle_duplicate(e, Rights::SAME.0);
                    if r.status != 0 {
                        refused = st(r);
                        break 'outer;
                    }
                    *h = r.a1 as u64;
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
    ensure!(
        (sent as usize) < 4000 * scale,
        "{sent} messages of 64 handles were accepted"
    );
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
        st(sys::thread_on_stack(
            kill_only,
            0x10000,
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
    let mut hb = [0u64; 1];
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
    let mut got = [0u64; 2];
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

static ABI_CONFORMANCE_IMAGE: AtomicU64 = AtomicU64::new(0);

fn fetch_the_conformance_image() -> Outcome {
    let (m, n) = request(24, b'A', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u64; 2];
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
    ABI_CONFORMANCE_IMAGE.store(got[0], Ordering::Relaxed);
    Ok(())
}

static KFUZZ_IMAGE: AtomicU64 = AtomicU64::new(0);

fn fetch_the_fuzzer_image() -> Outcome {
    let (m, n) = request(25, b'F', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u64; 2];
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
    KFUZZ_IMAGE.store(got[0], Ordering::Relaxed);
    Ok(())
}

/// KT8, the guest half: the program `kfuzz` runs the generator of the host fuzzer against this kernel, seed by seed, each in a child of its own inside a small job
/// (a handle to the job goes to it over the bootstrap channel). Whatever the sequence did (the program may even fault: it can close a handle of its own runtime
/// by chance), killing the job must bring the job's counters and the kernel heap back to where they began, and the kernel must still be running (a panic is
/// the boot test's `##PANIC`).
fn the_guest_fuzzer_leaves_nothing_behind() -> Outcome {
    const SEEDS: u64 = 4;
    let root = root_job()?;
    for seed in 1..=SEEDS {
        let j = job(root, 4, 12, 2000)?;
        let heap_before = sys::debug_stat(109);
        let (p, mine) =
            launch_image_in(KFUZZ_IMAGE.load(Ordering::Relaxed), seed as usize as u64, j)?;
        // the job handle the program's job calls act on: a duplicate with every right
        let dup = ok(
            sys::handle_duplicate(j, Rights::JOB.0),
            "duplicate the job handle",
        )?
        .a1 as u64;
        let hs = [dup];
        ensure_eq!(
            st(sys::channel_write(
                mine,
                [0u8; 1].as_ptr(),
                1,
                hs.as_ptr(),
                1
            )),
            Some(S::Ok),
            "send the job handle"
        );
        // it ends by itself (the sequence is finite); if it does not within the budget, that is a hang worth knowing about
        let r = sys::object_wait(p, sig::TERMINATED, sys::clock_get() + 6000);
        ensure!(
            r.status == 0 && r.a1 as u32 & sig::TERMINATED != 0,
            "seed {seed}: the fuzzer ended within its budget"
        );
        sys::job_kill(j, 1);
        sys::handle_close(p);
        sys::handle_close(mine);
        for which in [0, 1, 2, 3, 4, 5, 8] {
            job_use_settles(j, which, 0, 2000)?;
        }
        sys::handle_close(j);
        let held = heap_before.wrapping_sub(sys::debug_stat(109)) as i64;
        ensure!(
            held <= 64 * 1024,
            "seed {seed}: {held} kernel heap bytes stayed behind"
        );
    }
    sys::handle_close(root);
    Ok(())
}

/// AB9: the conformance program (built against `libsys` alone) runs to the end with no check failed; each failure it found comes back as a message.
fn abi_conformance_passes() -> Outcome {
    let (p, mine) = launch_image(ABI_CONFORMANCE_IMAGE.load(Ordering::Relaxed), 0)?;
    wait_ended(p, 3000)?;
    let code = sys::process_info(p).a1 as i64;
    let mut said = Detail::new();
    let mut buf = [0u8; 256];
    loop {
        let r = sys::channel_read(mine, buf.as_mut_ptr(), 256, core::ptr::null_mut(), 0);
        if r.status != 0 {
            break;
        }
        let n = (r.a1 as usize).min(256);
        let _ = core::fmt::Write::write_fmt(
            &mut said,
            format_args!("[{}] ", core::str::from_utf8(&buf[..n]).unwrap_or("?")),
        );
    }
    ensure_eq!(
        code,
        0,
        "checks failed in the conformance program: {}",
        core::str::from_utf8(said.as_bytes()).unwrap_or("?")
    );
    Ok(())
}

fn send_startup(mine: u64, s: &Startup, handles: &[u64]) -> Outcome {
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
fn start_hello(args: &[&[u8]]) -> Result<(u64, u64, u64), Detail> {
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
fn read_to_end(h: u64) -> Result<Vec<u8>, Detail> {
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
    // (the lower bound is what the machine has left for one program: 256 MiB less the kernel, which grows its heap in 2 MiB
    // steps and never gives it back (T81), the boot image and the other programs; it was 112 and a run that had grown the
    // kernel heap once more got 111, and the boot image has grown with every program since (the volume services alone are 2 MiB of debug code), CI saw 99, and 83 once authd, procd and the login programs were in the image (the lower bound is now 64). The test is about the cap stopping the heap, not about the exact figure.)
    ensure!(
        (64..=128).contains(&blocks),
        "the heap gave {blocks} MiB before refusing (cap 128 MiB, some in use: the boot image and the programs take memory from the machine's 256 MiB)"
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
        match rt::io::read(arg as u64, &mut buf) {
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

use proto::blk;
use proto::bootimg;
use proto::launch;

/// A read-only VMO of the boot image, from the kernel-side helper.
/// The boot image VMO, fetched once: the kernel makes a fresh copy of the whole image (about a
/// thousand pages) for every request, so asking in every test left a dozen copies behind and
/// the suite ran the machine out of memory on a runner whose image was a little larger.
fn boot_image_vmo() -> Result<u64, Detail> {
    static CACHED: AtomicU64 = AtomicU64::new(0);
    let have = CACHED.load(Ordering::Relaxed);
    if have != 0 {
        return Ok(have);
    }
    let h = fetch_boot_image_vmo()?;
    CACHED.store(h, Ordering::Relaxed);
    Ok(h)
}

fn fetch_boot_image_vmo() -> Result<u64, Detail> {
    let (m, n) = request(23, b'B', b"");
    let mut reply = [0u8; 16];
    let mut got = [0u64; 2];
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

/// (offset, length) of a boot image entry.
fn image_entry(vmo: u64, name: &[u8]) -> Result<(u64, u64), Detail> {
    // only the table at the front is needed, not the whole image (which is megabytes)
    let size = sys::vmo_get_size(vmo).a1;
    let prefix = rt::spawn::read_image(vmo, 0, (size as u64).min(128 * 1024)).ok_or_else(|| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!("read the boot image table (size {size})"),
        );
        d
    })?;
    let entries = bootimg::parse_prefix(&prefix, size).map_err(|e| {
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
fn start_launcher() -> Result<(u64, u64, u64), Detail> {
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
    ch: u64,
    bi: u64,
    txid: u32,
    off: u64,
    len: u64,
    startup: &[u8],
    forward: &[u64],
) -> Result<(i32, Option<u64>), Detail> {
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
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
    let mut h = [0u64; 1];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), 16, h.as_mut_ptr(), 1);
    ensure_eq!(r.status, 0, "read the reply");
    let (id, status) = launch::decode_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(id, txid, "the reply is for this request");
    Ok((status, if r.a2 == 1 { Some(h[0]) } else { None }))
}

fn hello_startup(args: &[&[u8]]) -> Result<(Vec<u8>, u64, u64, u64, u64), Detail> {
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
        st(sys::thread_on_stack(proc, 0x10000, 0x20_0000_0000, 0)),
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
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup")?.a1 as u64;
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
fn start_memfsd() -> Result<(u64, Fs, u64), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/memfsd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let image_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup")?.a1 as u64;
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
            b"dev".to_vec(),
            b"etc".to_vec(),
            b"home".to_vec(),
            b"include".to_vec(),
            b"lib".to_vec(),
            b"mnt".to_vec(),
            b"share".to_vec(),
            b"svc".to_vec(),
            b"tmp".to_vec(),
            b"usr".to_vec()
        ],
        "the top level: the skeleton"
    );
    ensure!(top.iter().all(|e| e.is_dir), "they are directories");
    // STAT carries the advisory attributes (UV3): the image's programs are executable, its other files are not
    let prog = root.stat(b"bin/hello").map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        (prog.info.kind, prog.info.mode),
        (1, 0o755),
        "a program is 0755"
    );
    let text = root
        .stat(b"home/hello.txt")
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        (text.info.kind, text.info.mode),
        (1, 0o644),
        "a fixture is 0644"
    );
    ensure_eq!(
        text.info.uid,
        proto::file::ABSENT,
        "no owner is recorded: the libc supplies the session's"
    );
    let link = root
        .stat_stop(b"usr/bin", pfl::NOFOLLOW)
        .map_err(|_| msg_at(line!()))?;
    ensure_eq!(
        (link.info.kind, link.size),
        (3, 6),
        "a link is described itself: kind 3, the length of its target"
    );
    let usr_list = {
        let mut u = root
            .open(b"usr", pfl::DIRECTORY, RO_DIR)
            .map_err(|e| msg_e(e, line!()))?;
        let l = u.read_dir().map_err(|e| msg_e(e, line!()))?;
        u.close();
        l
    };
    ensure!(
        usr_list.iter().all(|e| e.is_link && !e.is_dir),
        "a directory listing says which entries are links"
    );
    // `/usr` is a real directory of compatibility links to the real ones, which a client follows (UV1)
    let mut usr = root
        .open(b"usr", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&usr.read_dir().map_err(|e| msg_e(e, line!()))?),
        vec![
            b"bin".to_vec(),
            b"include".to_vec(),
            b"lib".to_vec(),
            b"share".to_vec()
        ],
        "what /usr holds"
    );
    usr.close();
    ensure_eq!(
        root.open_stop(b"usr/bin/hello", 0, prt::READ).err(),
        Some(rt::fs::Stopped::Link {
            consumed: 2,
            target: b"../bin".to_vec()
        }),
        "/usr/bin is a link to ../bin"
    );
    let table = pathres::Table {
        mounts: vec![(vec![], root.ch)],
        cwd: None,
        cwd_handle: None,
    };
    let mut view = Fs::new(root.ch);
    let (mut prog, _) = pathres::open_following(&table, b"/usr/bin/hello", |_, rel| {
        view.open_stop(rel, 0, prt::READ).map_err(|e| match e {
            rt::fs::Stopped::Link { consumed, target } => {
                pathres::Stop::Symlink { consumed, target }
            }
            rt::fs::Stopped::Status(s) => pathres::Stop::Status(s),
        })
    })
    .map_err(|_| msg_at(line!()))?;
    ensure_eq!(
        prog.read(0, 4),
        Ok(b"\x7fELF".to_vec()),
        "a program read through /usr/bin"
    );
    prog.close();
    let mut bin = root
        .open(b"bin", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let progs = names(&bin.read_dir().map_err(|e| msg_e(e, line!()))?);
    ensure_eq!(
        progs,
        vec![
            b"authd".to_vec(),
            b"basic".to_vec(),
            b"blkd".to_vec(),
            b"bomb".to_vec(),
            b"busybox".to_vec(),
            b"cat".to_vec(),
            b"clockd".to_vec(),
            b"consoled".to_vec(),
            b"crash".to_vec(),
            b"ctest".to_vec(),
            b"devd".to_vec(),
            b"devmgr".to_vec(),
            b"dhcpc".to_vec(),
            b"echo".to_vec(),
            b"fetch".to_vec(),
            b"free".to_vec(),
            b"fsd".to_vec(),
            b"gzip".to_vec(),
            b"hello".to_vec(),
            b"hog".to_vec(),
            b"httpd".to_vec(),
            b"init".to_vec(),
            b"ipchog".to_vec(),
            b"kilo".to_vec(),
            b"launcher".to_vec(),
            b"ls".to_vec(),
            b"lua".to_vec(),
            b"memfsd".to_vec(),
            b"netd".to_vec(),
            b"netdrv".to_vec(),
            b"netinfo".to_vec(),
            b"netsup".to_vec(),
            b"nproc".to_vec(),
            b"ping".to_vec(),
            b"probe".to_vec(),
            b"procd".to_vec(),
            b"ps".to_vec(),
            b"registryd".to_vec(),
            b"rngd".to_vec(),
            b"sessiond".to_vec(),
            b"sh".to_vec(),
            b"spin".to_vec(),
            b"sqlite3".to_vec(),
            b"udpecho".to_vec(),
            b"volmgr".to_vec(),
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
    ensure_eq!(
        t.read_all(),
        Ok(include_bytes!("../../../bootfs/etc/trusted").to_vec()),
        "the trust list"
    );
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
        root.stat(b"home/lines.txt").map(|s| (s.is_dir, s.size)),
        Ok((false, 14)),
        "stat by path"
    );
    ensure_eq!(
        root.stat(b"home/sub").map(|s| (s.is_dir, s.size)),
        Ok((true, 1)),
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

/// `rename` through the file server (T46): within a directory, across subdirectories by path, over an
/// existing file, the error cases, rights, and the read-only tree.
fn memfsd_rename() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let mut tmp = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(tmp.mkdir(b"d"), Ok(()), "mkdir d");
    ensure_eq!(tmp.mkdir(b"e"), Ok(()), "mkdir e");
    let mut f = tmp
        .open(b"a", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(f.write(0, b"contents"), Ok(8), "write");
    ensure_eq!(tmp.rename(b"a", b"b"), Ok(()), "rename in place");
    ensure_eq!(
        code(tmp.open(b"a", 0, prt::READ)),
        Some(pst::NOT_FOUND),
        "the old name is gone"
    );
    // the open connection follows the node to its new name
    ensure_eq!(
        f.read_all(),
        Ok(b"contents".to_vec()),
        "an open file survives a rename"
    );
    ensure_eq!(
        tmp.rename(b"b", b"d/c"),
        Ok(()),
        "into a subdirectory by path"
    );
    ensure_eq!(tmp.rename(b"d/c", b"e/z"), Ok(()), "between subdirectories");
    let mut z = tmp
        .open(b"e/z", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        z.read_all(),
        Ok(b"contents".to_vec()),
        "the moved file's data"
    );
    // replacing an existing file
    let mut other = tmp
        .open(b"e/other", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(other.write(0, b"old"), Ok(3), "another file");
    ensure_eq!(tmp.rename(b"e/z", b"e/other"), Ok(()), "replace a file");
    let mut again = tmp
        .open(b"e/other", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        again.read_all(),
        Ok(b"contents".to_vec()),
        "the replacement's data"
    );
    ensure_eq!(
        other.read_all(),
        Ok(b"old".to_vec()),
        "the replaced file lives while held"
    );
    // errors
    ensure_eq!(
        code(tmp.rename(b"nope", b"x")),
        Some(pst::NOT_FOUND),
        "missing source"
    );
    ensure_eq!(
        code(tmp.rename(b"d", b"e/other")),
        Some(pst::NOT_DIRECTORY),
        "a directory over a file"
    );
    ensure_eq!(
        code(tmp.rename(b"e/other", b"d")),
        Some(pst::IS_DIRECTORY),
        "a file over a directory"
    );
    let mut dd = tmp
        .open(b"d", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        dd.mkdir(b"inner"),
        Ok(()),
        "a directory with something in it"
    );
    ensure_eq!(
        code(tmp.rename(b"e", b"d")),
        Some(pst::NOT_EMPTY),
        "over a non-empty directory"
    );
    ensure_eq!(
        code(tmp.rename(b"d", b"d/inner/x")),
        Some(pst::BAD_REQUEST),
        "a directory below itself"
    );
    ensure_eq!(
        code(tmp.rename(b"d", b"../x")),
        Some(pst::BAD_REQUEST),
        "a path with .."
    );
    ensure_eq!(
        code(tmp.rename(b"d", b"")),
        Some(pst::BAD_REQUEST),
        "an empty new path"
    );
    ensure_eq!(
        code(tmp.rename(b"", b"x")),
        Some(pst::BAD_REQUEST),
        "an empty old path"
    );
    ensure_eq!(
        tmp.call(pop::RENAME, 0, 0, 99, 0, b"ab", None)
            .map(|a| a.status),
        Ok(pst::BAD_REQUEST),
        "a split point past the payload"
    );
    // rights: a read-only connection cannot rename
    let mut ro = tmp.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(ro.rename(b"d", b"d2")),
        Some(DENIED),
        "no WRITE, no rename"
    );
    // the boot filesystem is read-only
    let mut etc = root
        .open(b"etc", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()));
    if let Ok(etc) = etc.as_mut() {
        ensure!(
            etc.rename(b"trusted", b"x").is_err(),
            "the boot image cannot be renamed"
        );
    }
    ensure_eq!(
        code(root.rename(b"tmp", b"tmp2")),
        Some(DENIED),
        "nor the root"
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
    ensure_eq!(
        w.size(),
        Ok(3),
        "a write-only connection may ask the size (append, seek)"
    );
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

fn raw_request(ch: u64, bytes: &[u8], handles: &[u64]) -> Outcome {
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

fn reply_status(ch: u64) -> Result<i32, Detail> {
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

/// Slice R: an account confines a client. A connection that is the root of a new account may open only its
/// own limit of connections; sibling accounts and the root are untouched; accounts nest (a child may only be
/// tighter than its parent, and what it uses counts against the parent too); and an account's room comes back
/// when its connections close.
/// Start the service registry (slice SD): the connection to it is the full directory connection.
fn start_registry() -> Result<(u64, Fs), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/registryd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let s = Startup {
        args: vec![b"registryd".to_vec()],
        roles: vec![(Role::ServiceRegistry, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[theirs]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn registryd: {e:?}"));
        d
    })?;
    Ok((p, Fs::new(mine)))
}

/// Take the connection the registry forwarded to a listener (an `Open` frame carrying the client's endpoint)
/// and answer on it, as a service would: `reply` is written to the client.
fn serve_one_forwarded(listener: u64, reply: &[u8]) -> Outcome {
    let w = sys::object_wait(listener, sig::READABLE, sys::clock_get() + 2000);
    ensure_eq!(st(w), Some(S::Ok), "the registry forwarded the connection");
    let mut buf = [0u8; 64];
    let mut hs = [0u64; 2];
    let r = sys::channel_read(listener, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), 2);
    ensure_eq!(st(r), Some(S::Ok), "read the forwarded open");
    ensure_eq!(r.a2, 1, "it carries the client's endpoint");
    let req = proto::file::decode_request(&buf[..r.a1]).map_err(|_| msg_at(line!()))?;
    ensure_eq!(req.op, proto::file::op::OPEN, "an Open");
    ensure!(
        req.payload.is_empty(),
        "with an empty path: it is the service itself"
    );
    let ep = hs[0];
    ensure_eq!(
        st(sys::channel_write(
            ep,
            reply.as_ptr(),
            reply.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "the service answers the client directly"
    );
    sys::handle_close(ep);
    Ok(())
}

/// Slice SD: publish, open, hijack refused, stale then UNAVAILABLE then republished, views, unpublish.
fn registry_publish_open_unavailable_and_views() -> Outcome {
    use proto::file::status as fst;
    let (_p, mut root) = start_registry()?;
    let all = prt::READ | prt::ENUMERATE;
    // a service publishes a name by handing the registry a listener channel
    let (svc_end, listener) = channel()?;
    ensure_eq!(root.publish(b"toy", listener), Ok(()), "publish");
    let (svc2, listener2) = channel()?;
    ensure_eq!(
        code(root.publish(b"toy", listener2)),
        Some(fst::EXISTS),
        "a live name cannot be taken"
    );
    sys::handle_close(listener2);
    sys::handle_close(svc2);
    ensure_eq!(
        code(root.publish(b"bad/name", {
            let (a, b) = channel()?;
            sys::handle_close(a);
            b
        })),
        Some(fst::BAD_REQUEST),
        "names are checked"
    );
    // a client opens /svc/toy: the registry forwards its endpoint to the service, which answers directly
    let client = root
        .open(b"toy", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    serve_one_forwarded(svc_end, b"hello")?;
    let mut got = [0u8; 8];
    let w = sys::object_wait(client.ch, sig::READABLE, sys::clock_get() + 2000);
    ensure_eq!(st(w), Some(S::Ok), "the service's greeting arrives");
    let r = sys::channel_read(client.ch, got.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
    ensure_eq!(
        &got[..r.a1],
        b"hello",
        "the client talks to the service, not the registry"
    );
    ensure_eq!(
        code(root.open(b"nothing", 0, prt::READ)),
        Some(fst::NOT_FOUND),
        "an unpublished name"
    );
    // views: a connection that sees only some names; the rest do not exist for it
    let (other_svc, other_l) = channel()?;
    ensure_eq!(
        root.publish(b"other", other_l),
        Ok(()),
        "publish a second name"
    );
    let mut v = root.view(&[b"toy"], all).map_err(|e| msg_e(e, line!()))?;
    let listing: Vec<Vec<u8>> = names(&v.read_dir().map_err(|e| msg_e(e, line!()))?);
    ensure_eq!(
        listing,
        vec![b"toy".to_vec()],
        "the view lists only its names"
    );
    ensure_eq!(
        names(&root.read_dir().map_err(|e| msg_e(e, line!()))?),
        vec![b"other".to_vec(), b"toy".to_vec()],
        "the full connection lists both"
    );
    ensure_eq!(
        code(v.open(b"other", 0, prt::READ)),
        Some(fst::NOT_FOUND),
        "a name outside the view is not found, whatever it is"
    );
    ensure_eq!(
        code(v.view(&[b"other"], all)),
        Some(fst::BAD_REQUEST),
        "a view cannot grant a name its parent cannot use"
    );
    ensure_eq!(
        code(v.publish(b"x", {
            let (a, b) = channel()?;
            sys::handle_close(a);
            b
        })),
        Some(S::AccessDenied.code()),
        "a view has no PUBLISH right"
    );
    let via_view = v
        .open(b"toy", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    serve_one_forwarded(svc_end, b"again")?;
    sys::handle_close(via_view.ch);
    sys::handle_close(client.ch);
    // the service goes away: its name goes stale and clients are told to retry, not that it is missing
    sys::handle_close(svc_end);
    let mut seen = false;
    for _ in 0..50 {
        if code(root.open(b"toy", 0, prt::READ)) == Some(fst::UNAVAILABLE) {
            seen = true;
            break;
        }
        rt::io::nap(2);
    }
    ensure!(seen, "a dead service's name answers UNAVAILABLE");
    ensure_eq!(
        code(v.open(b"toy", 0, prt::READ)),
        Some(fst::UNAVAILABLE),
        "and through a view that allows it"
    );
    ensure_eq!(
        code(v.open(b"other", 0, prt::READ)),
        Some(fst::NOT_FOUND),
        "while a hidden name still looks missing"
    );
    // it restarts and republishes: the stale entry is replaced, and clients succeed again
    let (svc3, listener3) = channel()?;
    ensure_eq!(
        root.publish(b"toy", listener3),
        Ok(()),
        "republish over the stale entry"
    );
    let again = root
        .open(b"toy", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    serve_one_forwarded(svc3, b"back")?;
    sys::handle_close(again.ch);
    // unpublish
    ensure_eq!(root.unpublish(b"toy"), Ok(()), "unpublish");
    ensure_eq!(code(root.unpublish(b"toy")), Some(fst::NOT_FOUND), "twice");
    ensure_eq!(
        code(root.open(b"toy", 0, prt::READ)),
        Some(fst::NOT_FOUND),
        "gone"
    );
    for h in [svc3, other_svc, v.ch, root.ch] {
        sys::handle_close(h);
    }
    Ok(())
}

fn memfsd_accounts_confine_a_client() -> Outcome {
    let (_p, mut root, _bi) = start_memfsd()?;
    let all = prt::READ | prt::ENUMERATE;
    // an account of 3 connections (itself, and two more)
    let mut a = root
        .account(3, 8, 64 * 1024, all)
        .map_err(|e| msg_e(e, line!()))?;
    let a1 = a.clone_conn(all).map_err(|e| msg_e(e, line!()))?;
    let a2 = a.clone_conn(all).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(a.clone_conn(all)),
        Some(pst::QUOTA),
        "a fourth connection of a three-connection account"
    );
    // opening files counts too (every connection is charged to its account)
    ensure_eq!(
        code(a.open(b"bin", pfl::DIRECTORY, all)),
        Some(pst::QUOTA),
        "an open is a connection"
    );
    // another account, and the root, are not affected
    let mut b = root
        .account(3, 8, 64 * 1024, all)
        .map_err(|e| msg_e(e, line!()))?;
    let _b1 = b.clone_conn(all).map_err(|e| msg_e(e, line!()))?;
    let _r1 = root.clone_conn(all).map_err(|e| msg_e(e, line!()))?;
    // a child account may only be tighter than its parent, and uses the parent's room too
    ensure_eq!(
        code(b.account(5, 8, 64 * 1024, all)),
        Some(pst::BAD_REQUEST),
        "a looser child"
    );
    let mut c = b.account(2, 4, 1024, all).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(c.clone_conn(all)),
        Some(pst::QUOTA),
        "b has 3 connections: itself, one clone, and the child's root; the child's second would be a fourth"
    );
    // closing connections gives the room back
    a1.close();
    a2.close();
    let mut back = false;
    for _ in 0..50 {
        if a.clone_conn(all).is_ok() {
            back = true;
            break;
        }
        rt::io::nap(2);
    }
    ensure!(
        back,
        "the account's room came back after its connections closed"
    );
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
    let mut junk: Vec<(u64, u64)> = Vec::new();
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
    // a write-only file connection may describe itself (POSIX `fstat` needs no access: `tar` does it on its archive), but not ask
    // for a VMO; a connection with no rights at all can do neither
    let mut f = tmp_rw
        .open(b"f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    let mut wo = f.clone_conn(prt::WRITE).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(code(wo.fstat()), None, "a writer may fstat");
    let mut nothing = f.clone_conn(0).map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        code(nothing.fstat()),
        Some(DENIED),
        "fstat without READ or WRITE"
    );
    ensure_eq!(code(wo.get_vmo()), Some(DENIED), "get_vmo without READ");
    ensure_eq!(wo.size(), Ok(0), "a writer may ask the size");
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
    let mut kids = [0u64; 12];
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
    let dup = ok(sys::handle_duplicate(a, Rights::SAME.0), "dup")?.a1 as u64;
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
            [a as usize ^ (1usize << 40), 0, 0, 0, 0, 0]
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
    // SAFETY: a THREAD_YIELD syscall (one the kernel answers: `clock_get` is user-mode code in the library and would use the temporaries); the registers named are inputs and outputs, a0-a2 are
    // clobbered by the results as the ABI says.
    unsafe {
        core::arch::asm!(
            "jalr {lib}",
            lib = in(reg) rt::library::entry(rt::abi::syscall::THREAD_YIELD),
            out("ra") _, out("a7") _,
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
    let wide = |h: u64| h as usize ^ (1usize << 40); // a valid handle with a high bit added
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
    let top = my_stack_top();
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
    .a1 as u64;
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
    .a1 as u64;
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
    // NoMemory. (The cap is derived from RAM now, PL8: the rounds are what it takes to pass it, at most 40,000.)
    let rounds = (sys::debug_stat(120) / 2 + 500).min(40_000) as u32;
    for i in 0..rounds {
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

// --- The block driver (S6b) -----------------------------------------------------------------

/// Everything a supervisor of `blkd` keeps: the root resource, the mapped MMIO window (to reset the
/// device), the DMA buffer (kept across driver lifetimes: killing a driver must not free memory the
/// device may still write to, T42) and the interrupt object when the machine has a PLIC.
struct BlkRig {
    res: u64,
    dev: Blk,
    dma: u64,
    irq: Option<u64>,
    bi: u64,
    job: u64,
}

fn blk_rig() -> Result<Option<BlkRig>, Detail> {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let Some(dev) = find_blk(res)? else {
        return Ok(None);
    };
    let made = sys::interrupt_create(res, dev.irq);
    ensure!(
        matches!(st(made), Some(S::Ok) | Some(S::NotSupported)),
        "interrupt_create: {:?}",
        st(made)
    );
    let irq = (st(made) == Some(S::Ok)).then_some(made.a1 as u64);
    let dma = ok(sys::vmo_create_dma(res, 4), "the DMA buffer")?.a1 as u64;
    let root = root_job()?;
    let job = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    Ok(Some(BlkRig {
        res,
        dev,
        dma,
        irq,
        bi: boot_image_vmo()?,
        job,
    }))
}

impl BlkRig {
    /// Start a driver in the rig's job: (its process, our end of its service channel).
    fn spawn(&self) -> Result<(u64, u64), Detail> {
        let (off, len) = image_entry(self.bi, b"bin/blkd")?;
        let image = rt::spawn::read_image(self.bi, off, len).ok_or_else(|| msg_at(line!()))?;
        let slot = u64::from(self.dev.irq - 1);
        let mmio = ok(
            sys::vmo_create_physical(self.res, 0x1000_1000 + slot * 4096, 4096),
            "the slot's MMIO VMO",
        )?
        .a1 as u64;
        let dma = ok(
            sys::handle_duplicate(self.dma, Rights::SAME.0),
            "dup the DMA buffer",
        )?
        .a1 as u64;
        let (mine, theirs) = channel()?;
        let mut handles = vec![mmio, dma, theirs];
        let mut roles = vec![(Role::BlkMmio, 0), (Role::BlkDma, 1), (Role::ServiceBlk, 2)];
        if let Some(irq) = self.irq {
            let d = ok(
                sys::handle_duplicate(irq, Rights::SAME.0),
                "dup the interrupt",
            )?
            .a1 as u64;
            handles.push(d);
            roles.push((Role::BlkIrq, 3));
        }
        let s = Startup {
            args: vec![b"blkd".to_vec()],
            roles,
            ..Startup::default()
        };
        let bytes = s.encode().map_err(|_| msg_at(line!()))?;
        let p = rt::spawn::spawn_in(&image, &bytes, &handles, self.job).map_err(|e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn blkd: {e:?}"));
            d
        })?;
        Ok((p, mine))
    }

    /// Put the device back to its reset state (a supervisor's job between two drivers).
    fn reset(&self) {
        wr(&self.dev, 0x70, 0);
        wr(&self.dev, 0x64, rd(&self.dev, 0x60));
    }
}

impl Drop for BlkRig {
    fn drop(&mut self) {
        self.reset();
        let me = sys::process_self().a1 as u64;
        sys::process_unmap(me, MMIO_VA);
        sys::handle_close(me);
        for h in [self.dma, self.job, self.res] {
            sys::handle_close(h);
        }
        if let Some(i) = self.irq {
            sys::handle_close(i);
        }
    }
}

/// One raw request/reply exchange on the block service channel.
fn blk_raw(ch: u64, bytes: &[u8]) -> Result<Vec<u8>, Detail> {
    ensure_eq!(
        st(sys::channel_write(
            ch,
            bytes.as_ptr(),
            bytes.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send the request"
    );
    let w = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(st(w), Some(S::Ok), "the driver answers");
    let mut buf = vec![0u8; 16 * 1024];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
    ensure_eq!(st(r), Some(S::Ok), "read the reply");
    buf.truncate(r.a1);
    Ok(buf)
}

fn blk_call(ch: u64, txid: u32, op: blk::Op<'_>) -> Result<(blk::Status, Vec<u8>), Detail> {
    let reply = blk_raw(ch, &blk::Request { txid, op }.encode())?;
    let (t, status, body) = blk::decode_reply(&reply).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(t, txid, "the reply answers this transaction");
    Ok((status, body.to_vec()))
}

fn pattern(sector: u64, i: usize) -> u8 {
    (sector as usize * 31 + i * 7 + 5) as u8
}

fn write_pattern(ch: u64, first: u64, sectors: u64) -> Outcome {
    let mut s = 0;
    while s < sectors {
        let n = (sectors - s).min(16);
        let mut data = Vec::new();
        for k in 0..n {
            data.extend((0..512).map(|i| pattern(first + s + k, i)));
        }
        let (st, _) = blk_call(
            ch,
            s as u32 + 1,
            blk::Op::Write {
                sector: first + s,
                count: n as u32,
                data: &data,
            },
        )?;
        ensure_eq!(st, blk::Status::Ok, "write");
        s += n;
    }
    Ok(())
}

fn check_pattern(ch: u64, first: u64, sectors: u64) -> Outcome {
    let mut s = 0;
    let mut sum = 0u64;
    while s < sectors {
        let n = (sectors - s).min(16);
        let (st, body) = blk_call(
            ch,
            s as u32 + 1000,
            blk::Op::Read {
                sector: first + s,
                count: n as u32,
            },
        )?;
        ensure_eq!(st, blk::Status::Ok, "read");
        ensure_eq!(body.len(), n as usize * 512, "the data length");
        for k in 0..n {
            for i in 0..512 {
                let got = body[k as usize * 512 + i];
                if got != pattern(first + s + k, i) {
                    ensure!(false, "sector {} byte {i}: {got:#x}", first + s + k);
                }
                sum += u64::from(got);
            }
        }
        s += n;
    }
    ensure!(sum > 0, "a checksum of real data");
    Ok(())
}

/// `st`, for use where a local named `st` shadows it.
fn sysst(r: Ret) -> Option<S> {
    st(r)
}

fn blk_round_trip() -> Outcome {
    let Some(rig) = blk_rig()? else {
        return Ok(()); // no disk attached (OS_DISK=0)
    };
    ensure!(
        rig.irq.is_some(),
        "the block device has an interrupt (the AIA is required, PL6)"
    );
    sys::debug_write(b"s6: blkd exercised (interrupts)\n");
    let (p, ch) = rig.spawn()?;
    let (st, body) = blk_call(ch, 1, blk::Op::Info)?;
    ensure_eq!(st, blk::Status::Ok, "info");
    let info = blk::decode_info(&body).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(
        (info.sectors, info.sector_size, info.read_only, info.max_io),
        (16 * 1024 * 2, 512, false, 16),
        "a 16 MiB disk"
    );
    // the harness put a known pattern in sector 0 (a driver that reads zeroes would show)
    let (st, s0) = blk_call(
        ch,
        2,
        blk::Op::Read {
            sector: 0,
            count: 1,
        },
    )?;
    ensure_eq!(st, blk::Status::Ok, "read sector 0");
    ensure_eq!(s0.len(), 512, "a whole sector");
    ensure!(
        (0..512).all(|i| s0[i] == ((i * 7 + 3) & 0xff) as u8),
        "sector 0 holds the harness pattern"
    );
    // a request is answered promptly: a dead interrupt path would fall back to the driver's
    // two-second timeout and pass everything else
    let t = sys::clock_get();
    let (status, _) = blk_call(
        ch,
        4,
        blk::Op::Read {
            sector: 0,
            count: 16,
        },
    )?;
    ensure_eq!(status, blk::Status::Ok, "an 8 KiB read");
    ensure!(
        sys::clock_get() - t < 200,
        "a read took {} ticks",
        sys::clock_get() - t
    );
    // a 64 KiB pattern in 8 KiB requests, read back identical
    write_pattern(ch, 100, 128)?;
    check_pattern(ch, 100, 128)?;
    ensure_eq!(blk_call(ch, 3, blk::Op::Flush)?.0, blk::Status::Ok, "flush");
    // the edges of the disk
    let last = info.sectors - 1;
    write_pattern(ch, last, 1)?;
    check_pattern(ch, last, 1)?;
    // bad requests are refused with the right status and the driver keeps serving
    let bad = |op, want, what: &str| -> Outcome {
        let (st, body) = blk_call(ch, 77, op)?;
        ensure!(st == want && body.is_empty(), "{what}: {st:?}");
        Ok(())
    };
    bad(
        blk::Op::Read {
            sector: 0,
            count: 0,
        },
        blk::Status::BadRequest,
        "count 0",
    )?;
    bad(
        blk::Op::Read {
            sector: 0,
            count: 17,
        },
        blk::Status::TooBig,
        "17 sectors",
    )?;
    bad(
        blk::Op::Read {
            sector: info.sectors,
            count: 1,
        },
        blk::Status::BadRange,
        "past the end",
    )?;
    bad(
        blk::Op::Read {
            sector: last,
            count: 2,
        },
        blk::Status::BadRange,
        "straddling the end",
    )?;
    bad(
        blk::Op::Read {
            sector: u64::MAX,
            count: 2,
        },
        blk::Status::BadRange,
        "wrapping",
    )?;
    bad(
        blk::Op::Write {
            sector: info.sectors,
            count: 1,
            data: &[0u8; 512],
        },
        blk::Status::BadRange,
        "write past the end",
    )?;
    // malformed bytes: too short with a txid, with none, a write whose data is the wrong length
    let r = blk_raw(ch, &[9, 0, 0, 0, 2, 0])?;
    ensure_eq!(
        blk::decode_reply(&r).map(|(t, s, _)| (t, s)),
        Some((9, blk::Status::BadRequest)),
        "a short header is BAD_REQUEST with its txid"
    );
    let mut short_write = blk::Request {
        txid: 10,
        op: blk::Op::Write {
            sector: 0,
            count: 2,
            data: &[0u8; 1024],
        },
    }
    .encode();
    short_write.truncate(short_write.len() - 1);
    let r = blk_raw(ch, &short_write)?;
    ensure_eq!(
        blk::decode_reply(&r).map(|(t, s, _)| (t, s)),
        Some((10, blk::Status::BadRequest)),
        "a write with the wrong data length"
    );
    // a write bigger than any valid request is answered TOO_BIG with its txid (not dropped)
    let big = vec![0u8; 17 * 512];
    let r = blk_raw(
        ch,
        &blk::Request {
            txid: 12,
            op: blk::Op::Write {
                sector: 0,
                count: 17,
                data: &big,
            },
        }
        .encode(),
    )?;
    ensure_eq!(
        blk::decode_reply(&r).map(|(t, s, _)| (t, s)),
        Some((12, blk::Status::TooBig)),
        "an oversized write"
    );
    // a request that carries a handle is refused and its handle closed; the driver is not wedged
    let ev = event()?;
    ensure_eq!(
        sysst(sys::channel_write(
            ch,
            [11u8, 0, 0, 0].as_ptr(),
            4,
            [ev].as_ptr(),
            1
        )),
        Some(S::Ok),
        "send a request with a handle"
    );
    let w = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 2000);
    ensure_eq!(sysst(w), Some(S::Ok), "the driver answers it");
    let mut rb = [0u8; 64];
    let r = sys::channel_read(ch, rb.as_mut_ptr(), 64, core::ptr::null_mut(), 0);
    ensure_eq!(sysst(r), Some(S::Ok), "read the answer");
    ensure_eq!(
        blk::decode_reply(&rb[..r.a1]).map(|(t, s, _)| (t, s)),
        Some((11, blk::Status::BadRequest)),
        "a request with a handle is BAD_REQUEST"
    );
    // still serving, and nothing above disturbed the data
    check_pattern(ch, 100, 128)?;
    // the client going away ends the driver cleanly
    sys::handle_close(ch);
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        0,
        "the driver exits 0 when its client is gone"
    );
    sys::handle_close(p);
    Ok(())
}

fn blk_driver_restart() -> Outcome {
    let Some(rig) = blk_rig()? else {
        return Ok(());
    };
    let (p, ch) = rig.spawn()?;
    write_pattern(ch, 4000, 64)?;
    check_pattern(ch, 4000, 64)?;
    // start a write and kill the driver before it can answer: the device may be mid-request
    let inflight: Vec<u8> = (0..16 * 512).map(|i| (i * 13 + 1) as u8).collect();
    ensure_eq!(
        st(sys::channel_write(
            ch,
            blk::Request {
                txid: 99,
                op: blk::Op::Write {
                    sector: 5000,
                    count: 16,
                    data: &inflight
                }
            }
            .encode()
            .as_ptr(),
            blk::REQ_HEADER + inflight.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "queue a write"
    );
    // kill the driver: its client channel reports the loss, and the data is on the disk, not in it
    ensure_eq!(st(sys::process_kill(p, 55)), Some(S::Ok), "kill the driver");
    wait_ended(p, 2000)?;
    let w = sys::object_wait(ch, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(st(w), Some(S::Ok), "the client sees PEER_CLOSED");
    sys::handle_close(p);
    sys::handle_close(ch);
    // the supervisor resets the device before the buffer goes to the next driver (T42)
    rig.reset();
    let (p2, ch2) = rig.spawn()?;
    check_pattern(ch2, 4000, 64)?;
    // the write that was in flight when the driver died happened completely, in part, or not at
    // all; whichever, every sector holds the old zeros or the new data, and the new driver works
    let (st5, body) = blk_call(
        ch2,
        5,
        blk::Op::Read {
            sector: 5000,
            count: 16,
        },
    )?;
    ensure_eq!(st5, blk::Status::Ok, "read the in-flight sectors");
    for k in 0..16 {
        let sec = &body[k * 512..(k + 1) * 512];
        ensure!(
            sec.iter().all(|&b| b == 0) || sec == &inflight[k * 512..(k + 1) * 512],
            "sector {} is neither old nor new",
            5000 + k
        );
    }
    // it also takes writes: a second generation of data over the first
    write_pattern(ch2, 4000, 16)?;
    check_pattern(ch2, 4000, 16)?;
    ensure_eq!(
        st(sys::process_kill(p2, 56)),
        Some(S::Ok),
        "kill the second driver"
    );
    wait_ended(p2, 2000)?;
    sys::handle_close(p2);
    sys::handle_close(ch2);
    // both drivers' processes, threads and memory are gone: the rig's job is back to nothing
    job_use_settles(rig.job, 0, 0, 2000)?;
    job_use_settles(rig.job, 2, 0, 2000)?;
    Ok(())
}

// --- Futexes and sleeping locks (S7b) -----------------------------------------------------------

use rt::sync::{Condvar, Mutex, Once, Semaphore};

static FW: AtomicU32 = AtomicU32::new(0); // the futex word the tests sleep on
static ARRIVED: AtomicU32 = AtomicU32::new(0);
static WOKEN_N: AtomicU32 = AtomicU32::new(0);
static WOKEN_ORDER: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// Worker `arg`: announce, sleep on FW (value 0) for up to 4 s, record that it woke and in what order.
extern "C" fn futex_sleeper(arg: usize) -> ! {
    ARRIVED.fetch_add(1, Ordering::SeqCst);
    let r = sys::futex_wait(FW.as_ptr(), 0, sys::clock_get() + 4000);
    if r.status == 0 {
        let i = WOKEN_N.fetch_add(1, Ordering::SeqCst) as usize;
        if i < 4 {
            WOKEN_ORDER[i].store(arg as u32, Ordering::SeqCst);
        }
    }
    sys::thread_exit(r.status)
}

fn nap_ticks(ticks: u64) {
    sleep_in_thread(ticks);
}

fn futex_argument_errors() -> Outcome {
    FW.store(0, Ordering::SeqCst);
    let p = FW.as_ptr();
    let wait = |a: u64, e: u64, d: u64| {
        st(rt::syscall(
            rt::abi::syscall::FUTEX_WAIT,
            [a as usize, e as usize, d as usize, 0, 0, 0],
        ))
    };
    ensure_eq!(
        wait(p as u64 + 1, 0, 5),
        Some(S::InvalidArgs),
        "a misaligned address"
    );
    ensure_eq!(
        wait(p as u64 + 2, 0, 5),
        Some(S::InvalidArgs),
        "a half-aligned address"
    );
    ensure_eq!(
        wait(0x10, 0, 5),
        Some(S::InvalidArgs),
        "an address below user memory"
    );
    ensure_eq!(
        wait(0xffff_ffc0_0000_0000, 0, 5),
        Some(S::InvalidArgs),
        "a kernel address"
    );
    ensure_eq!(
        wait(0x7fff_ff00_0000, 0, 5),
        Some(S::InvalidArgs),
        "an unmapped address"
    );
    ensure_eq!(
        wait(p as u64, 1 << 32, 5),
        Some(S::InvalidArgs),
        "an expected value wider than 32 bits"
    );
    ensure_eq!(
        st(sys::futex_wait(p, 1, sys::clock_get() + 50)),
        Some(S::ShouldWait),
        "the word differs: no sleep"
    );
    let t = sys::clock_get();
    ensure_eq!(
        st(sys::futex_wait(p, 0, 0)),
        Some(S::TimedOut),
        "a poll of an equal word"
    );
    // (a generous bound: it only has to tell a poll from a sleep that never ends)
    ensure!(sys::clock_get() - t < 200, "a poll does not sleep");
    ensure_eq!(
        st(sys::futex_wake(p.wrapping_byte_add(1), 1)),
        Some(S::InvalidArgs),
        "wake: misaligned"
    );
    ensure_eq!(
        st(sys::futex_wake(0x10 as *const u32, 1)),
        Some(S::InvalidArgs),
        "wake: below user memory"
    );
    let r = sys::futex_wake(p, 5);
    ensure_eq!((r.status, r.a1), (0, 0), "waking nobody wakes nobody");
    let r = sys::futex_wake(p, 0);
    ensure_eq!((r.status, r.a1), (0, 0), "wake with count 0");
    Ok(())
}

fn futex_times_out_not_early() -> Outcome {
    FW.store(0, Ordering::SeqCst);
    let deadline = sys::clock_get() + 40;
    let r = sys::futex_wait(FW.as_ptr(), 0, deadline);
    ensure_eq!(st(r), Some(S::TimedOut), "nobody wakes us");
    ensure!(sys::clock_get() >= deadline, "not before the deadline");
    ensure!(sys::clock_get() < deadline + 200, "and not long after it");
    Ok(())
}

/// Start `n` sleepers one after the other (so their arrival order is known) and let each reach the
/// kernel's table.
fn start_sleepers(n: u32) -> Result<Vec<u64>, Detail> {
    FW.store(0, Ordering::SeqCst);
    ARRIVED.store(0, Ordering::SeqCst);
    WOKEN_N.store(0, Ordering::SeqCst);
    let mut ts = Vec::new();
    for k in 0..n {
        ts.push(new_thread(futex_sleeper, k as usize + 1)?);
        let until = sys::clock_get() + 2000;
        while ARRIVED.load(Ordering::SeqCst) < k + 1 {
            ensure!(sys::clock_get() < until, "a sleeper never started");
            sys::thread_yield();
        }
        nap_ticks(15); // it is in the kernel's table well within this
    }
    Ok(ts)
}

/// Wait until `n` sleepers have recorded that they woke.
fn woken_reaches(n: u32) -> Outcome {
    let until = sys::clock_get() + 1000;
    while WOKEN_N.load(Ordering::SeqCst) < n {
        ensure!(sys::clock_get() < until, "a woken sleeper never reported");
        nap_ticks(2);
    }
    nap_ticks(10); // and nobody else did
    ensure_eq!(
        WOKEN_N.load(Ordering::SeqCst),
        n,
        "exactly the expected number woke"
    );
    Ok(())
}

fn futex_wake_counts_and_fifo() -> Outcome {
    // counts: three asleep, wake(2) wakes exactly two (the two oldest; which of them records first
    // depends on which hart runs it, so only the set is checked here)
    let ts = start_sleepers(3)?;
    let r = sys::futex_wake(FW.as_ptr(), 2);
    ensure_eq!(
        (r.status, r.a1),
        (0, 2),
        "wake(2) with three asleep wakes exactly two"
    );
    woken_reaches(2)?;
    let mut two = [
        WOKEN_ORDER[0].load(Ordering::SeqCst),
        WOKEN_ORDER[1].load(Ordering::SeqCst),
    ];
    two.sort();
    ensure_eq!(two, [1, 2], "the two oldest");
    // a wake from another process on the same address number reaches nobody
    let (p, _m) = launch(51 | ((FW.as_ptr() as u64) << 8))?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        100,
        "the other process woke nobody"
    );
    sys::handle_close(p);
    nap_ticks(20);
    ensure_eq!(
        WOKEN_N.load(Ordering::SeqCst),
        2,
        "the third is still asleep"
    );
    let r = sys::futex_wake(FW.as_ptr(), 1);
    ensure_eq!((r.status, r.a1), (0, 1), "the last one");
    woken_reaches(3)?;
    ensure_eq!(
        WOKEN_ORDER[2].load(Ordering::SeqCst),
        3,
        "the youngest was woken last"
    );
    let r = sys::futex_wake(FW.as_ptr(), 9);
    ensure_eq!((r.status, r.a1), (0, 0), "nobody left");
    for t in ts {
        sys::handle_close(t);
    }
    // FIFO: one wake at a time, each observed before the next, wakes them in arrival order
    let ts = start_sleepers(3)?;
    for k in 1..=3u32 {
        let r = sys::futex_wake(FW.as_ptr(), 1);
        ensure_eq!((r.status, r.a1), (0, 1), "one wake, one sleeper");
        woken_reaches(k)?;
        ensure_eq!(
            WOKEN_ORDER[k as usize - 1].load(Ordering::SeqCst),
            k,
            "arrival order"
        );
    }
    for t in ts {
        sys::handle_close(t);
    }
    Ok(())
}

fn kill_a_futex_waiter() -> Outcome {
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    let (p, mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 52, j)?;
    // the child says it is about to sleep; a moment later it is in the kernel's table
    let w = sys::object_wait(mine, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(st(w), Some(S::Ok), "the child reaches its futex");
    nap_ticks(30);
    ensure_eq!(st(sys::process_kill(p, 9)), Some(S::Ok), "kill it");
    wait_ended(p, 2000)?;
    ensure_eq!(sys::process_info(p).a1 as i64, 9, "ended by the kill");
    sys::handle_close(p);
    // nothing of it is left (the kernel's own check that no sleeper survives runs at the end)
    job_use_settles(j, 0, 0, 2000)?;
    job_use_settles(j, 1, 0, 2000)?;
    job_use_settles(j, 2, 0, 2000)?;
    sys::handle_close(mine);
    sys::handle_close(j);
    Ok(())
}

static LOCK: Mutex<u64> = Mutex::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
const LOCK_ROUNDS: u64 = 1500;

extern "C" fn mutex_worker(_: usize) -> ! {
    let deadline = sys::clock_get() + 60_000;
    let mut code = 0;
    for _ in 0..LOCK_ROUNDS {
        match LOCK.lock_until(deadline) {
            Some(mut g) => {
                let v = *g;
                if v % 97 == 0 {
                    sys::thread_yield(); // let another thread contend while we hold it
                }
                *g = v + 1;
            }
            None => {
                code = 1;
                break;
            }
        }
    }
    DONE.fetch_add(1, Ordering::SeqCst);
    sys::thread_exit(code)
}

fn mutex_excludes_across_harts() -> Outcome {
    *LOCK.lock() = 0;
    DONE.store(0, Ordering::SeqCst);
    let mut ts = [0u64; 3];
    for t in &mut ts {
        *t = new_thread(mutex_worker, 0)?;
    }
    let until = sys::clock_get() + 70_000;
    while DONE.load(Ordering::SeqCst) < 3 {
        ensure!(
            sys::clock_get() < until,
            "the workers never finished (a lost wakeup?)"
        );
        nap_ticks(5);
    }
    ensure_eq!(*LOCK.lock(), 3 * LOCK_ROUNDS, "no increment was lost");
    for t in ts {
        sys::handle_close(t);
    }
    Ok(())
}

struct Queue {
    items: alloc::collections::VecDeque<u64>,
    closed: bool,
}
static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    items: alloc::collections::VecDeque::new(),
    closed: false,
});
static NOT_EMPTY: Condvar = Condvar::new();
static SUM: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU32 = AtomicU32::new(0);
const ITEMS: u64 = 2500;

extern "C" fn consumer(_: usize) -> ! {
    let deadline = sys::clock_get() + 60_000;
    loop {
        let mut g = match QUEUE.lock_until(deadline) {
            Some(g) => g,
            None => sys::thread_exit(1),
        };
        while g.items.is_empty() && !g.closed {
            let (ng, timed_out) = NOT_EMPTY.wait_until(g, deadline);
            g = ng;
            if timed_out {
                sys::thread_exit(2);
            }
        }
        match g.items.pop_front() {
            Some(v) => {
                SUM.fetch_add(v, Ordering::SeqCst);
                COUNT.fetch_add(1, Ordering::SeqCst);
            }
            None => break, // closed and drained
        }
    }
    sys::thread_exit(0)
}

fn condvar_producer_consumer() -> Outcome {
    SUM.store(0, Ordering::SeqCst);
    COUNT.store(0, Ordering::SeqCst);
    {
        let mut q = QUEUE.lock();
        q.items.clear();
        q.closed = false;
    }
    let c1 = new_thread(consumer, 0)?;
    let c2 = new_thread(consumer, 0)?;
    for i in 1..=ITEMS {
        QUEUE.lock().items.push_back(i);
        NOT_EMPTY.notify_one();
        if i % 50 == 0 {
            sys::thread_yield();
        }
    }
    QUEUE.lock().closed = true;
    NOT_EMPTY.notify_all();
    let until = sys::clock_get() + 70_000;
    while COUNT.load(Ordering::SeqCst) < ITEMS as u32 {
        ensure!(
            sys::clock_get() < until,
            "items were never consumed (a lost wakeup?)"
        );
        nap_ticks(5);
    }
    // both consumers end once the queue is closed and empty
    for c in [c1, c2] {
        let r = sys::object_wait(c, sig::TERMINATED, sys::clock_get() + 5000);
        ensure_eq!(st(r), Some(S::Ok), "a consumer ends");
        ensure_eq!(sys::thread_info(c).a1 as i64, 0, "cleanly");
        sys::handle_close(c);
    }
    ensure_eq!(
        COUNT.load(Ordering::SeqCst),
        ITEMS as u32,
        "every item exactly once"
    );
    ensure_eq!(
        SUM.load(Ordering::SeqCst),
        ITEMS * (ITEMS + 1) / 2,
        "and none twice"
    );
    Ok(())
}

static ONCE: Once = Once::new();
static ONCE_RUNS: AtomicU32 = AtomicU32::new(0);
static SEM: Semaphore = Semaphore::new(0);
static SEM_TAKEN: AtomicU32 = AtomicU32::new(0);

extern "C" fn once_worker(_: usize) -> ! {
    ONCE.call_once(|| {
        sys::thread_yield();
        ONCE_RUNS.fetch_add(1, Ordering::SeqCst);
    });
    let ok_run = ONCE.is_done();
    DONE.fetch_add(1, Ordering::SeqCst);
    sys::thread_exit(i32::from(!ok_run))
}

extern "C" fn sem_worker(_: usize) -> ! {
    if SEM.wait_until(sys::clock_get() + 5000) {
        SEM_TAKEN.fetch_add(1, Ordering::SeqCst);
    }
    sys::thread_exit(0)
}

fn once_and_semaphore() -> Outcome {
    DONE.store(0, Ordering::SeqCst);
    let mut ts = vec![];
    for _ in 0..3 {
        ts.push(new_thread(once_worker, 0)?);
    }
    let until = sys::clock_get() + 5000;
    while DONE.load(Ordering::SeqCst) < 3 {
        ensure!(sys::clock_get() < until, "once workers never finished");
        nap_ticks(5);
    }
    ensure_eq!(
        ONCE_RUNS.load(Ordering::SeqCst),
        1,
        "the initialiser ran once"
    );
    for t in ts.drain(..) {
        ensure_eq!(sys::thread_info(t).a1 as i64, 0, "every caller saw it done");
        sys::handle_close(t);
    }
    // a semaphore: three sleepers, two posts release exactly two
    SEM_TAKEN.store(0, Ordering::SeqCst);
    for _ in 0..3 {
        ts.push(new_thread(sem_worker, 0)?);
    }
    nap_ticks(30);
    SEM.post();
    SEM.post();
    nap_ticks(60);
    ensure_eq!(SEM_TAKEN.load(Ordering::SeqCst), 2, "two posts, two taken");
    SEM.post();
    nap_ticks(60);
    ensure_eq!(
        SEM_TAKEN.load(Ordering::SeqCst),
        3,
        "the third post releases the last"
    );
    ensure!(!SEM.try_wait(), "nothing left over");
    for t in ts {
        sys::handle_close(t);
    }
    Ok(())
}

// --- The C library (S7c) ----------------------------------------------------------------------

/// Run the C test program and hand its transcript to the harness, which compares it with the same
/// program built and run on the host (the host's libc is the oracle): every line goes out as
/// `c|<line>` on the console.
fn c_transcript_matches_the_host() -> Outcome {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/ctest")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (out_mine, out_theirs) = socket()?;
    // a memory filesystem for the program's files: `/` is its root (read-only) and `.` its /tmp (the only
    // writable place), so the program's relative paths are scratch files
    let (_fsd, mut root, _bi) = start_memfsd()?;
    let slash = root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    let dot = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let s = Startup {
        args: vec![b"ctest".to_vec()],
        namespace: vec![(b"/".to_vec(), 1), (b".".to_vec(), 2)],
        roles: vec![(Role::Stdout, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[out_theirs, slash.ch, dot.ch]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn ctest: {e:?}"));
        d
    })?;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rt::io::read(out_mine, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    wait_ended(p, 30_000)?;
    let code = sys::process_info(p).a1 as i64;
    sys::handle_close(p);
    sys::handle_close(out_mine);
    ensure!(
        code != 99,
        "ctest is a stub: the build had no RISC-V C compiler (set CC_RISCV)"
    );
    ensure_eq!(code, 0, "ctest exits 0");
    ensure!(!got.is_empty(), "ctest printed nothing");
    sys::debug_write(b"s7: c transcript begins\n");
    for line in got.split(|&b| b == b'\n') {
        let mut l = b"c|".to_vec();
        l.extend_from_slice(&line[..line.len().min(400)]);
        sys::debug_write(&l);
    }
    sys::debug_write(b"s7: c transcript ends\n");
    Ok(())
}

// --- Lua (S7d) ----------------------------------------------------------------------------------

/// Run `lua /etc/lua/s7.lua` (the upstream interpreter, our C library) and hand its transcript to the
/// harness, which compares it with a Lua built from the same sources and run on the host: every line
/// goes out as `lua|<line>`. `/` is a read-only root, `.` the writable /tmp.
fn lua_script_matches_the_host_build() -> Outcome {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/lua")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (out_mine, out_theirs) = socket()?;
    let err_theirs = ok(sys::handle_duplicate(out_theirs, Rights::SAME.0), "dup")?.a1 as u64;
    let (_fsd, mut root, _bi) = start_memfsd()?;
    let slash = root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    let dot = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let s = Startup {
        args: vec![b"lua".to_vec(), b"/etc/lua/s7.lua".to_vec()],
        namespace: vec![(b"/".to_vec(), 2), (b".".to_vec(), 3)],
        roles: vec![(Role::Stdout, 0), (Role::Stderr, 1)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[out_theirs, err_theirs, slash.ch, dot.ch]).map_err(
        |e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn lua: {e:?}"));
            d
        },
    )?;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rt::io::read(out_mine, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    wait_ended(p, 60_000)?;
    let code = sys::process_info(p).a1 as i64;
    sys::handle_close(p);
    sys::handle_close(out_mine);
    ensure!(
        code != 99,
        "lua is a stub: the build had no RISC-V C compiler (set CC_RISCV)"
    );
    if code != 0 {
        // the interpreter's own diagnostics are the best clue
        let tail = &got[got.len().saturating_sub(120)..];
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "lua exits 0: got {code}, output ends {:?}",
                core::str::from_utf8(tail).unwrap_or("?")
            ),
        );
        return Err(d);
    }
    ensure!(!got.is_empty(), "lua printed nothing");
    sys::debug_write(b"s7: lua transcript begins\n");
    // the console shows only printable ASCII: everything else goes out as \xNN (the harness
    // applies the same rule to the host's output)
    for line in got.split(|&b| b == b'\n') {
        let mut l = b"lua|".to_vec();
        for &b in line {
            if (0x20..0x7f).contains(&b) {
                l.push(b);
            } else {
                l.extend_from_slice(b"\\x");
                l.push(b"0123456789abcdef"[(b >> 4) as usize]);
                l.push(b"0123456789abcdef"[(b & 15) as usize]);
            }
        }
        l.truncate(2000);
        sys::debug_write(&l);
    }
    sys::debug_write(b"s7: lua transcript ends\n");
    Ok(())
}

/// Run one file of the upstream Lua test suite (S7e) under `bin/lua`: `/` is the read-only boot
/// filesystem (the files are in `/etc/lua-tests`), `.` the writable /tmp. The suite's own `assert`s are
/// the judge: the test passes if `lua` exits 0, and a failure shows the end of its output.
fn run_lua_upstream(file: &str) -> Outcome {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/lua")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (out_mine, out_theirs) = socket()?;
    let err_theirs = ok(sys::handle_duplicate(out_theirs, Rights::SAME.0), "dup")?.a1 as u64;
    let (_fsd, mut root, _bi) = start_memfsd()?;
    let slash = root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    let dot = root
        .open(b"tmp", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let setup = b"package.path='/etc/lua-tests/?.lua' _port=true _soft=true _nomsg=true";
    let path = alloc::format!("/etc/lua-tests/{file}.lua");
    let s = Startup {
        args: vec![
            b"lua".to_vec(),
            b"-e".to_vec(),
            setup.to_vec(),
            path.into_bytes(),
        ],
        // (the upstream tests expect a PATH, though this OS has nothing to run through one)
        env: vec![b"PATH=/bin".to_vec()],
        namespace: vec![(b"/".to_vec(), 2), (b".".to_vec(), 3)],
        roles: vec![(Role::Stdout, 0), (Role::Stderr, 1)],
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[out_theirs, err_theirs, slash.ch, dot.ch]).map_err(
        |e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn lua: {e:?}"));
            d
        },
    )?;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rt::io::read(out_mine, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    wait_ended(p, 600_000)?;
    let code = sys::process_info(p).a1 as i64;
    sys::handle_close(p);
    sys::handle_close(out_mine);
    ensure!(
        code != 99,
        "lua is a stub: the build had no RISC-V C compiler (set CC_RISCV)"
    );
    if code != 0 {
        // the whole end of the output, for the log (the verdict line has room for little)
        for line in got[got.len().saturating_sub(1500)..].split(|&b| b == b'\n') {
            let mut l = b"lua-fail|".to_vec();
            l.extend(
                line.iter()
                    .map(|&b| if (0x20..0x7f).contains(&b) { b } else { b'.' }),
            );
            l.truncate(300);
            sys::debug_write(&l);
        }
        let tail: Vec<u8> = got[got.len().saturating_sub(160)..]
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b } else { b'.' })
            .collect();
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "{file}.lua exits 0: got {code}, output ends {:?}",
                core::str::from_utf8(&tail).unwrap_or("?")
            ),
        );
        return Err(d);
    }
    Ok(())
}

macro_rules! lua_upstream_tests {
    ($($name:ident => $file:literal),* $(,)?) => {
        $(fn $name() -> Outcome { run_lua_upstream($file) })*
        const LUA_UPSTREAM: &[(&str, fn() -> Outcome)] = &[$((concat!("lua_upstream_", $file), $name)),*];
    };
}

lua_upstream_tests! {
    lua_up_calls => "calls",
    lua_up_closure => "closure",
    lua_up_constructs => "constructs",
    lua_up_events => "events",
    lua_up_goto => "goto",
    lua_up_literals => "literals",
    lua_up_locals => "locals",
    lua_up_math => "math",
    lua_up_nextvar => "nextvar",
    lua_up_pm => "pm",
    lua_up_sort => "sort",
    lua_up_strings => "strings",
    lua_up_utf8 => "utf8",
    lua_up_vararg => "vararg",
    lua_up_bitwise => "bitwise",
    lua_up_tpack => "tpack",
    lua_up_coroutine => "coroutine",
    lua_up_errors => "errors",
    lua_up_db => "db",
    lua_up_gc => "gc",
    lua_up_gengc => "gengc",
    lua_up_cstack => "cstack",
    lua_up_attrib => "attrib",
    lua_up_files => "files",
}

// --- BASIC on the target (S7a) ----------------------------------------------------------------

/// Run `basic --golden NAME` with `/` bound to a read-only clone of `root`, and return everything
/// it wrote to stdout (the program never judges itself).
fn run_basic_golden(image: &[u8], root: &mut Fs, name: &[u8]) -> Result<Vec<u8>, Detail> {
    let (out_mine, out_theirs) = socket()?;
    let ro = root.clone_conn(RO_DIR).map_err(|e| msg_e(e, line!()))?;
    let s = Startup {
        args: vec![b"basic".to_vec(), b"--golden".to_vec(), name.to_vec()],
        namespace: vec![(b"/".to_vec(), 1)],
        roles: vec![(Role::Stdout, 0)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(image, &bytes, &[out_theirs, ro.ch]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn basic: {e:?}"));
        d
    })?;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rt::io::read(out_mine, &mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    wait_ended(p, 60_000)?;
    let code = sys::process_info(p).a1 as i64;
    sys::handle_close(p);
    sys::handle_close(out_mine);
    ensure_eq!(code, 0, "basic --golden exits 0");
    Ok(got)
}

fn basic_golden_corpus_on_the_target() -> Outcome {
    let (_fsd, mut root, bi) = start_memfsd()?;
    let mut dir = root
        .open(b"etc/basic/golden", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let listing = names(&dir.read_dir().map_err(|e| msg_e(e, line!()))?);
    let scripts: Vec<Vec<u8>> = listing
        .iter()
        .filter_map(|n| n.strip_suffix(b".bas").map(|s| s.to_vec()))
        .collect();
    ensure_eq!(scripts.len(), 68, "the whole corpus is in the boot image");
    let (off, len) = image_entry(bi, b"bin/basic")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let mut failed: Vec<Vec<u8>> = Vec::new();
    let mut first: Option<(Vec<u8>, Vec<u8>)> = None;
    for name in &scripts {
        let got = run_basic_golden(&image, &mut root, name)?;
        let mut path = b"etc/basic/golden/".to_vec();
        path.extend_from_slice(name);
        path.extend_from_slice(b".out");
        let mut f = root
            .open(&path, 0, prt::READ)
            .map_err(|e| msg_e(e, line!()))?;
        let want = f.read_all().map_err(|e| msg_e(e, line!()))?;
        f.close();
        if got != want {
            failed.push(name.clone());
        }
        // negative control: a script's transcript is not another script's expected output
        match &first {
            None => first = Some((got, want)),
            Some((g0, w0)) => {
                ensure!(
                    *g0 != want && got != *w0,
                    "the comparison tells two scripts apart"
                );
            }
        }
    }
    ensure!(
        failed.is_empty(),
        "{} of {} scripts differ: {:?}",
        failed.len(),
        scripts.len(),
        failed
            .iter()
            .take(5)
            .map(|n| core::str::from_utf8(n).unwrap_or("?"))
            .collect::<Vec<_>>()
    );
    Ok(())
}

// --- The device manager (S6c) -----------------------------------------------------------------

/// One OpenBlock exchange: the status and the block channel that came with an Ok.
fn devmgr_open(ch: u64, txid: u32) -> Result<(proto::devmgr::Status, Option<u64>), Detail> {
    devmgr_open_at(ch, txid, 0)
}

/// OpenBlock for the device `index` (in device-tree order).
fn devmgr_open_at(
    ch: u64,
    txid: u32,
    index: u16,
) -> Result<(proto::devmgr::Status, Option<u64>), Detail> {
    let req = proto::devmgr::encode_open_block_at(txid, index);
    ensure_eq!(
        st(sys::channel_write(
            ch,
            req.as_ptr(),
            req.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send OpenBlock"
    );
    let w = sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(st(w), Some(S::Ok), "the manager answers");
    let mut buf = [0u8; 64];
    let mut hs = [0u64; 2];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), hs.len());
    ensure_eq!(st(r), Some(S::Ok), "read the reply");
    let (t, status) = proto::devmgr::decode_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(t, txid, "the reply answers this transaction");
    ensure_eq!(
        r.a2,
        usize::from(status == proto::devmgr::Status::Ok),
        "a handle only with Ok"
    );
    Ok((status, (r.a2 == 1).then_some(hs[0])))
}

fn devmgr_starts_blkd_for_the_disk() -> Outcome {
    use proto::devmgr::Status as D;
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let have_disk = match find_blk(res)? {
        Some(_) => {
            let me = sys::process_self().a1 as u64;
            sys::process_unmap(me, MMIO_VA);
            sys::handle_close(me);
            true
        }
        None => false,
    };
    // start the manager
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/devmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let res_dup = ok(
        sys::handle_duplicate(res, Rights::SAME.0),
        "dup the resource",
    )?
    .a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
    let s = Startup {
        args: vec![b"devmgr".to_vec()],
        roles: vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    // in a job of its own: when it and its driver are gone nothing of theirs is left (no leaked
    // process, thread, page or object)
    let root = root_job()?;
    let mj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    let mgr = rt::spawn::spawn_in(&image, &bytes, &[res_dup, bi_dup, theirs], mj).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn devmgr: {e:?}"));
        d
    })?;
    // a request that is not OpenBlock is refused with its txid
    let bad = [5u8, 0, 0, 0, 9, 0, 0, 0];
    ensure_eq!(
        st(sys::channel_write(
            mine,
            bad.as_ptr(),
            bad.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send a bad request"
    );
    let w = sys::object_wait(mine, sig::READABLE, sys::clock_get() + 5000);
    ensure_eq!(st(w), Some(S::Ok), "the manager answers a bad request");
    let mut rb = [0u8; 16];
    let r = sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    ensure_eq!(
        proto::devmgr::decode_reply(&rb[..r.a1]),
        Some((5, D::BadRequest)),
        "BAD_REQUEST with its txid"
    );
    if !have_disk {
        // no disk in the device tree's block slot: nothing is started
        let (status, h) = devmgr_open(mine, 1)?;
        ensure_eq!((status, h), (D::NoDevice, None), "no block device");
        sys::debug_write(b"s6: devmgr: no block device\n");
    } else {
        sys::debug_write(b"s6: devmgr found the disk\n");
        let (status, h) = devmgr_open(mine, 1)?;
        ensure_eq!(status, D::Ok, "the manager opens the disk");
        let blkch = h.ok_or_else(|| msg_at(line!()))?;
        write_pattern(blkch, 7000, 32)?;
        check_pattern(blkch, 7000, 32)?;
        // one driver at a time
        let (status, h) = devmgr_open(mine, 2)?;
        ensure_eq!(
            (status, h),
            (D::Busy, None),
            "a second open while the first is in use"
        );
        check_pattern(blkch, 7000, 32)?;
        // the client lets go: the driver ends, and a new open starts a fresh one on the same data
        sys::handle_close(blkch);
        let until = sys::clock_get() + 3000;
        let again = loop {
            let (status, h) = devmgr_open(mine, 3)?;
            if status == D::Ok {
                break h.ok_or_else(|| msg_at(line!()))?;
            }
            ensure_eq!(status, D::Busy, "only Busy while the old driver winds down");
            ensure!(sys::clock_get() < until, "the old driver never ended");
            let _ = sys::object_wait(mine, 1 << 24, sys::clock_get() + 5);
        };
        check_pattern(again, 7000, 32)?;
        // slice DM: the manager serves every block device it finds. A second disk (the harness
        // attaches one with a different sector-0 pattern) has its own driver, in use at the same time
        // as the first; a device number past the last is NoDevice.
        let (status, h) = devmgr_open_at(mine, 20, 1)?;
        if status == D::Ok {
            let second = h.ok_or_else(|| msg_at(line!()))?;
            let first = again;
            // both are open now (the first driver is still the one from `again`)
            let (_, sector0_b) = blk_call(
                second,
                1,
                blk::Op::Read {
                    sector: 0,
                    count: 1,
                },
            )?;
            let (_, sector0_a) = blk_call(
                first,
                2,
                blk::Op::Read {
                    sector: 0,
                    count: 1,
                },
            )?;
            let want_a: Vec<u8> = (0..512).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
            let want_b: Vec<u8> = (0..512).map(|i| ((i * 11 + 5) & 0xff) as u8).collect();
            ensure!(
                sector0_a == want_a,
                "device 0 shows the first disk's sector 0"
            );
            ensure!(
                sector0_b == want_b,
                "device 1 shows the second disk's sector 0"
            );
            let (status, h) = devmgr_open_at(mine, 21, 2)?;
            ensure_eq!((status, h), (D::NoDevice, None), "there is no third device");
            let (status, h) = devmgr_open_at(mine, 22, 1)?;
            ensure_eq!(
                (status, h),
                (D::Busy, None),
                "the second device is in use, independently of the first"
            );
            sys::handle_close(second);
            sys::debug_write(b"s6: devmgr served two block devices\n");
        }
        sys::handle_close(again);
    }
    // slice DM: several clients. A second client connects through the first; both are served, and the manager
    // lives until the last one goes.
    {
        use proto::devmgr as dm;
        let (c2, c2_theirs) = channel()?;
        let req = dm::encode_connect(40);
        let hs = [c2_theirs];
        ensure_eq!(
            st(sys::channel_write(
                mine,
                req.as_ptr(),
                req.len(),
                hs.as_ptr(),
                1
            )),
            Some(S::Ok),
            "send Connect"
        );
        let w = sys::object_wait(mine, sig::READABLE, sys::clock_get() + 5000);
        ensure_eq!(st(w), Some(S::Ok), "the manager answers Connect");
        let mut rb = [0u8; 16];
        let r = sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
        ensure_eq!(
            dm::decode_reply(&rb[..r.a1]),
            Some((40, D::Ok)),
            "a second client was accepted"
        );
        let (status, h) = devmgr_open_at(c2, 41, 9)?;
        ensure_eq!(
            (status, h),
            (D::NoDevice, None),
            "the second client is served"
        );
        // Connect without its handle, and OpenBlock with one, are refused
        let bare = dm::encode_connect(42);
        sys::channel_write(c2, bare.as_ptr(), bare.len(), core::ptr::null(), 0);
        let w = sys::object_wait(c2, sig::READABLE, sys::clock_get() + 5000);
        ensure_eq!(st(w), Some(S::Ok), "answered");
        let r = sys::channel_read(c2, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
        ensure_eq!(
            dm::decode_reply(&rb[..r.a1]),
            Some((42, D::BadRequest)),
            "Connect needs its handle"
        );
        // the first client lets go: the manager lives on for the second
        sys::handle_close(mine);
        let (status, h) = devmgr_open_at(c2, 43, 9)?;
        ensure_eq!(
            (status, h),
            (D::NoDevice, None),
            "still served after the first client left"
        );
        sys::handle_close(c2);
    }
    // the last client goes away: the manager ends cleanly
    wait_ended(mgr, 3000)?;
    ensure_eq!(sys::process_info(mgr).a1 as i64, 0, "the manager exits 0");
    sys::handle_close(mgr);
    job_use_settles(mj, 0, 0, 3000)?;
    job_use_settles(mj, 1, 0, 3000)?;
    job_use_settles(mj, 2, 0, 3000)?;
    sys::handle_close(mj);
    sys::handle_close(res);
    Ok(())
}

// --- The disk filesystem server (FS-SPEC step 5c) ---------------------------------------------

/// A volume identity for the test (the harness formats nothing: the test does it, as setup).
const FSD_UUID: [u8; 16] = *b"fsd-suite-volume";

/// `devmgr` in a job of its own: (our channel to it, its process, its job).
fn start_devmgr_in_job(res: u64) -> Result<(u64, u64, u64), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/devmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let res_dup = ok(
        sys::handle_duplicate(res, Rights::SAME.0),
        "dup the resource",
    )?
    .a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
    let s = Startup {
        args: vec![b"devmgr".to_vec()],
        roles: vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let root = root_job()?;
    let mj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    let mgr = rt::spawn::spawn_in(&image, &bytes, &[res_dup, bi_dup, theirs], mj).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn devmgr: {e:?}"));
        d
    })?;
    Ok((mine, mgr, mj))
}

/// The block channel of device `index`, retrying while the previous driver for it winds down.
fn open_disk(mgr_ch: u64, mut txid: u32, index: u16) -> Result<u64, Detail> {
    use proto::devmgr::Status as D;
    let until = sys::clock_get() + 5000;
    loop {
        let (status, h) = devmgr_open_at(mgr_ch, txid, index)?;
        if status == D::Ok {
            return h.ok_or_else(|| msg_at(line!()));
        }
        ensure_eq!(status, D::Busy, "only Busy while the old driver winds down");
        ensure!(sys::clock_get() < until, "the old driver never ended");
        txid += 1;
        let _ = sys::object_wait(mgr_ch, 1 << 24, sys::clock_get() + 5);
    }
}

/// Start `fsd` on the block channel (it takes the channel): (its process, a root connection).
fn spawn_fsd(blk_ch: u64) -> Result<(u64, Fs), Detail> {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/fsd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let s = Startup {
        args: vec![b"fsd".to_vec()],
        roles: vec![(Role::ServiceFs, 0), (Role::BlockDevice, 1)],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    let p = rt::spawn::spawn(&image, &bytes, &[theirs, blk_ch]).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn fsd: {e:?}"));
        d
    })?;
    Ok((p, Fs::new(mine)))
}

fn fsd_pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 13 + i / 251) as u8).collect()
}

fn fsd_keeps_files_across_a_restart() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let have_disk = match find_blk(res)? {
        Some(_) => {
            let me = sys::process_self().a1 as u64;
            sys::process_unmap(me, MMIO_VA);
            sys::handle_close(me);
            true
        }
        None => false,
    };
    if !have_disk {
        return Ok(()); // OS_DISK=0
    }
    let (mgr_ch, mgr, mj) = start_devmgr_in_job(res)?;
    // device 1 is the small second disk (the first belongs to the block-driver tests)
    let blk = open_disk(mgr_ch, 1, 1)?;
    {
        // setup, the way a host tool would do it (the OS has no formatter, FS-SPEC section 6)
        let mut dev = rt::blk::BlkDevice::open(blk).ok_or_else(|| msg_at(line!()))?;
        ensure_eq!(fsfmt::Device::blocks(&dev), 256, "a 1 MiB disk");
        fsfmt::mkfs(&mut dev, FSD_UUID, "fsdtest").map_err(|_| msg_at(line!()))?;
    }
    // the first run: make some files, sync, then more that is never synced, then die
    let (p1, mut root) = spawn_fsd(blk)?;
    ensure_eq!(root.mkdir(b"docs"), Ok(()), "mkdir");
    let mut docs = root
        .open(b"docs", pfl::DIRECTORY, RW_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let mut a = docs
        .open(b"a.txt", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(a.write(0, b"hello, disk"), Ok(11), "write a small file");
    let mut big = docs
        .open(b"big", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        big.write_all(0, &fsd_pattern(20_000)),
        Ok(()),
        "write a file of five blocks"
    );
    ensure_eq!(big.set_size(18_000), Ok(()), "truncate it");
    ensure_eq!(docs.rename(b"a.txt", b"b.txt"), Ok(()), "rename");
    let mut tmp = docs
        .open(b"tmp", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(tmp.write(0, &[7; 5000]), Ok(5000), "a file to remove");
    tmp.close();
    ensure_eq!(docs.unlink(b"tmp"), Ok(()), "unlink");
    ensure_eq!(
        code(root.open(
            b"docs",
            pfl::DIRECTORY | pfl::CREATE | pfl::EXCLUSIVE,
            RW_DIR
        )),
        Some(pst::EXISTS),
        "an error from the disk filesystem"
    );
    // attributes, an index and a query (FS-SPEC 9.1) on the disk
    ensure_eq!(
        a.attr_set(1, b"tag", b"alpha"),
        Ok(()),
        "a text attribute on a file"
    );
    ensure_eq!(
        docs.attr_set(1, b"tag", b"dirtag"),
        Ok(()),
        "and on a directory"
    );
    ensure_eq!(root.make_index(1, b"tag"), Ok(()), "an index on it");
    ensure_eq!(code(root.make_index(1, b"tag")), Some(pst::EXISTS), "twice");
    let all_tags = proto::file::Query {
        index: b"tag",
        term: proto::file::QueryTerm::Range { lo: None, hi: None },
        after: None,
        max: 100,
    };
    let (hits, more) = root.query(&all_tags).map_err(|e| msg_e(e, line!()))?;
    let found: Vec<(Vec<u8>, Vec<u8>)> = hits
        .iter()
        .map(|h| (h.key.clone(), h.path.clone()))
        .collect();
    ensure_eq!(
        found,
        vec![
            (b"alpha".to_vec(), b"docs/b.txt".to_vec()),
            (b"dirtag".to_vec(), b"docs".to_vec())
        ],
        "the query finds both, ordered by key"
    );
    ensure!(!more, "nothing more");
    ensure_eq!(root.sync(), Ok(()), "sync");
    // after the sync: more changes, then the server dies before it commits them
    ensure_eq!(
        a.attr_set(1, b"tag", b"late"),
        Ok(()),
        "an unsynced attribute change"
    );
    let mut late = root
        .open(b"late", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        late.write(0, b"never committed"),
        Ok(15),
        "an unsynced write"
    );
    ensure_eq!(
        st(sys::process_kill(p1, -9)),
        Some(S::Ok),
        "kill the server"
    );
    wait_ended(p1, 3000)?;
    sys::handle_close(p1);
    for f in [late, big, a, docs, root] {
        f.close();
    }
    // the second run: a fresh server on the same disk sees the synced state and none of the rest
    let blk = open_disk(mgr_ch, 10, 1)?;
    let (p2, mut root) = spawn_fsd(blk)?;
    let top = root.read_dir().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&top),
        vec![b"docs".to_vec()],
        "the unsynced file is gone"
    );
    let mut docs = root
        .open(b"docs", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    let inside = docs.read_dir().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&inside),
        vec![b"b.txt".to_vec(), b"big".to_vec()],
        "the synced names"
    );
    let mut b = docs
        .open(b"b.txt", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(b.read_all(), Ok(b"hello, disk".to_vec()), "the small file");
    let mut g = docs
        .open(b"big", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        g.read_all(),
        Ok(fsd_pattern(18_000)),
        "the five-block file, truncated"
    );
    // a change made now is committed when the last client leaves
    {
        let mut w = root
            .open(b"docs", pfl::DIRECTORY, RW_DIR)
            .map_err(|e| msg_e(e, line!()))?;
        ensure_eq!(w.mkdir(b"made-later"), Ok(()), "mkdir in the second run");
        w.close();
    }
    for f in [g, b, docs, root] {
        f.close();
    }
    wait_ended(p2, 5000)?;
    ensure_eq!(
        sys::process_info(p2).a1 as i64,
        0,
        "the server exits 0 once its last client is gone"
    );
    sys::handle_close(p2);
    // the third run: the orderly exit committed it
    let blk = open_disk(mgr_ch, 20, 1)?;
    let (p3, mut root) = spawn_fsd(blk)?;
    let mut docs = root
        .open(b"docs", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&docs.read_dir().map_err(|e| msg_e(e, line!()))?),
        vec![b"b.txt".to_vec(), b"big".to_vec(), b"made-later".to_vec()],
        "the change made just before the last client left"
    );
    docs.close();
    root.close();
    wait_ended(p3, 5000)?;
    sys::handle_close(p3);
    sys::debug_write(b"fs: fsd kept its files across a restart\n");
    sys::handle_close(mgr_ch);
    wait_ended(mgr, 3000)?;
    sys::handle_close(mgr);
    job_use_settles(mj, 0, 0, 3000)?;
    job_use_settles(mj, 1, 0, 3000)?;
    job_use_settles(mj, 2, 0, 3000)?;
    sys::handle_close(mj);
    sys::handle_close(res);
    Ok(())
}

// --- The wall clock (FS-SPEC step C1) ---------------------------------------------------------

/// The harness starts QEMU with `-rtc base=2030-06-15T12:00:00,clock=vm`: the real-time clock starts there and follows the
/// virtual clock, so the clock service must say a time a little after this many nanoseconds since the epoch.
const RTC_BASE_NS: u64 = 1_907_755_200 * 1_000_000_000;

/// A registry, a connection that can publish, and a clock service on `devmgr_ch`: (the view of `clock`, the processes).
fn start_clockd(bi: u64, job_h: u64, devmgr_ch: u64) -> Result<(Fs, Vec<u64>), Detail> {
    let (off, len) = image_entry(bi, b"bin/registryd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (reg_mine, reg_theirs) = channel()?;
    let s = Startup {
        args: vec![b"registryd".to_vec()],
        roles: vec![(Role::ServiceRegistry, 0)],
        ..Startup::default()
    };
    let reg = rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[reg_theirs],
        job_h,
    )
    .map_err(|_| msg_at(line!()))?;
    let mut root = Fs::new(reg_mine);
    let publish = root.clone_conn(prt::ALL).map_err(|e| msg_e(e, line!()))?;
    let (off, len) = image_entry(bi, b"bin/clockd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: vec![b"clockd".to_vec()],
        roles: vec![(Role::ServiceDevmgr, 0), (Role::ServiceRegistry, 1)],
        ..Startup::default()
    };
    let clockd = rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[devmgr_ch, publish.ch],
        job_h,
    )
    .map_err(|_| msg_at(line!()))?;
    let view = root
        .view(&[b"clock"], prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    root.close();
    Ok((view, vec![reg, clockd]))
}

fn clock_service_reports_the_rtc() -> Outcome {
    use proto::clock::quality;
    use rt::clock::Clock;
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let bi = boot_image_vmo()?;
    let root_j = root_job()?;
    let mj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root_j);
    let t = |n: u64| sys::clock_get() + n;
    // ---- with the device manager (whose one client is the clock service): the RTC's time, quality FromRtc
    let (dm_mine, dm_theirs) = channel()?;
    let res_dup = ok(
        sys::handle_duplicate(res, Rights::SAME.0),
        "dup the resource",
    )?
    .a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
    let (off, len) = image_entry(bi, b"bin/devmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: vec![b"devmgr".to_vec()],
        roles: vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        ..Startup::default()
    };
    let mgr = rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[res_dup, bi_dup, dm_theirs],
        mj,
    )
    .map_err(|_| msg_at(line!()))?;
    let (mut view, mut procs) = start_clockd(bi, mj, dm_mine)?;
    procs.push(mgr);
    let mut c = Clock::connect(&mut view, t(8000)).map_err(|_| msg_at(line!()))?;
    let a = c.now(t(3000)).map_err(|_| msg_at(line!()))?;
    ensure_eq!(a.quality, quality::FROM_RTC, "the time comes from the RTC");
    ensure!(
        // (the RTC follows the virtual clock from the base the harness set; the bound only tells a real RTC reading from a made-up
        // one, so it is far above the harness's ceiling on a boot (OS_BOOT_TIMEOUT, 30 minutes) and below a day; a ten-minute window
        // failed on a loaded host, where the guest's tick count also falls behind the virtual clock: T157)
        a.nanos >= RTC_BASE_NS && a.nanos < RTC_BASE_NS + 2 * 3_600 * 1_000_000_000,
        "within two hours after the base the harness set: {} ns",
        a.nanos
    );
    nap_ticks(60);
    let b = c.now(t(3000)).map_err(|_| msg_at(line!()))?;
    ensure!(b.nanos >= a.nanos, "never backwards");
    ensure!(
        b.nanos - a.nanos >= 50_000_000 && b.nanos - a.nanos < 5_000_000_000,
        "it advanced by about the time we slept: {} ns",
        b.nanos - a.nanos
    );
    // a malformed request is answered BAD_REQUEST with its txid, the service lives on
    let bad = [9u8, 0, 0, 0, 7, 0, 0, 0];
    ensure_eq!(
        st(sys::channel_write(
            c.channel(),
            bad.as_ptr(),
            bad.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send a bad request"
    );
    let w = sys::object_wait(c.channel(), sig::READABLE, t(3000));
    ensure_eq!(st(w), Some(S::Ok), "answered");
    let mut rb = [0u8; 32];
    let r = sys::channel_read(c.channel(), rb.as_mut_ptr(), 32, core::ptr::null_mut(), 0);
    ensure_eq!(
        proto::clock::decode_reply(&rb[..r.a1]).map(|(t, s, _, _)| (t, s)),
        Some((9, proto::clock::status::BAD_REQUEST)),
        "BAD_REQUEST with its txid"
    );
    ensure!(c.now(t(3000)).is_ok(), "and it still answers");
    drop(c);
    view.close();
    for p in procs.drain(..) {
        sys::process_kill(p, -1);
        sys::handle_close(p);
    }
    // ---- with no device manager to claim from: quality Unset, counting from the epoch at boot
    let (dead, _gone) = channel()?; // the other end is closed at once: the claim finds nobody
    sys::handle_close(_gone);
    let (mut view, procs) = start_clockd(bi, mj, dead)?;
    let mut c = Clock::connect(&mut view, t(8000)).map_err(|_| msg_at(line!()))?;
    let u = c.now(t(3000)).map_err(|_| msg_at(line!()))?;
    ensure_eq!(u.quality, quality::UNSET, "no RTC: Unset");
    ensure!(
        u.nanos < 3_600 * 1_000_000_000,
        "counted from the epoch at boot: {} ns",
        u.nanos
    );
    drop(c);
    view.close();
    for p in procs {
        sys::process_kill(p, -1);
        sys::handle_close(p);
    }
    sys::debug_write(b"clock: the clock service reported the RTC and the Unset case\n");
    sys::handle_close(mj);
    sys::handle_close(res);
    Ok(())
}

// --- The volume manager (FS-SPEC steps 5d and 5e) ---------------------------------------------

/// The device manager, a registry and the volume manager in one job: (a connection to the registry, the job).
fn start_volmgr_rig(res: u64, bi: u64) -> Result<(Fs, u64), Detail> {
    let root_j = root_job()?;
    let mj = job(root_j, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root_j);
    let (dm_mine, dm_theirs) = channel()?;
    let res_dup = ok(
        sys::handle_duplicate(res, Rights::SAME.0),
        "dup the resource",
    )?
    .a1 as u64;
    let bi_dup = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
    let (off, len) = image_entry(bi, b"bin/devmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: vec![b"devmgr".to_vec()],
        roles: vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::ServiceDevmgr, 2),
        ],
        ..Startup::default()
    };
    rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[res_dup, bi_dup, dm_theirs],
        mj,
    )
    .map_err(|_| msg_at(line!()))?;
    let (off, len) = image_entry(bi, b"bin/registryd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (reg_mine, reg_theirs) = channel()?;
    let s = Startup {
        args: vec![b"registryd".to_vec()],
        roles: vec![(Role::ServiceRegistry, 0)],
        ..Startup::default()
    };
    rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[reg_theirs],
        mj,
    )
    .map_err(|_| msg_at(line!()))?;
    let mut root = Fs::new(reg_mine);
    let publish = root.clone_conn(prt::ALL).map_err(|e| msg_e(e, line!()))?;
    // the clock service, as a second client of the device manager, so that the volume server can stamp files with wall time
    let (cd_dm, cd_theirs) = channel()?;
    let req = proto::devmgr::encode_connect(1);
    ensure_eq!(
        st(sys::channel_write(
            dm_mine,
            req.as_ptr(),
            req.len(),
            &cd_theirs,
            1
        )),
        Some(S::Ok),
        "connect the clock service to the device manager"
    );
    ensure_eq!(
        st(sys::object_wait(
            dm_mine,
            sig::READABLE,
            sys::clock_get() + 5000
        )),
        Some(S::Ok),
        "the manager answers"
    );
    let mut rb = [0u8; 16];
    sys::channel_read(dm_mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    let clock_pub = root.clone_conn(prt::ALL).map_err(|e| msg_e(e, line!()))?;
    let (off, len) = image_entry(bi, b"bin/clockd")?;
    let cimage = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: vec![b"clockd".to_vec()],
        roles: vec![(Role::ServiceDevmgr, 0), (Role::ServiceRegistry, 1)],
        ..Startup::default()
    };
    rt::spawn::spawn_in(
        &cimage,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[cd_dm, clock_pub.ch],
        mj,
    )
    .map_err(|_| msg_at(line!()))?;
    let bi_dup2 = ok(sys::handle_duplicate(bi, Rights::SAME.0), "dup the image")?.a1 as u64;
    let (off, len) = image_entry(bi, b"bin/volmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let s = Startup {
        args: vec![b"volmgr".to_vec()],
        roles: vec![
            (Role::ServiceDevmgr, 0),
            (Role::ServiceRegistry, 1),
            (Role::BootImage, 2),
        ],
        ..Startup::default()
    };
    rt::spawn::spawn_in(
        &image,
        &s.encode().map_err(|_| msg_at(line!()))?,
        &[dm_mine, publish.ch, bi_dup2],
        mj,
    )
    .map_err(|_| msg_at(line!()))?;
    Ok((root, mj))
}

/// Open the published volume `name` (retrying while the volume manager has not published it yet).
fn open_volume(root: &mut Fs, name: &[u8], rights: u32) -> Result<Fs, Detail> {
    let mut view = root
        .view(&[name], prt::ALL)
        .map_err(|e| msg_e(e, line!()))?;
    let until = sys::clock_get() + 15_000;
    loop {
        match view.open_retry(name, pfl::DIRECTORY, rights, until) {
            Ok(f) => return Ok(f),
            Err(e) if sys::clock_get() < until => {
                let _ = e;
                nap_ticks(100);
            }
            Err(e) => return Err(msg_e(e, line!())),
        }
    }
}

/// Kills a job when it goes out of scope, so a test that fails part way does not leave services running on the disk for the
/// next test (a failure then cascades).
struct KillOnDrop(u64);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = sys::job_kill(self.0, -1);
        }
    }
}

fn volmgr_serves_volumes_by_the_table() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let have_disk = match find_blk(res)? {
        Some(_) => {
            let me = sys::process_self().a1 as u64;
            sys::process_unmap(me, MMIO_VA);
            sys::handle_close(me);
            true
        }
        None => false,
    };
    if !have_disk {
        return Ok(());
    }
    // setup: format the second disk (device 1) the way a host tool would
    {
        let (mgr_ch, mgr, mj) = start_devmgr_in_job(res)?;
        let blk = open_disk(mgr_ch, 1, 1)?;
        let mut dev = rt::blk::BlkDevice::open(blk).ok_or_else(|| msg_at(line!()))?;
        fsfmt::mkfs(&mut dev, FSD_UUID, "volmgrtest").map_err(|_| msg_at(line!()))?;
        sys::handle_close(blk);
        sys::handle_close(mgr_ch);
        wait_ended(mgr, 3000)?;
        sys::handle_close(mgr);
        job_use_settles(mj, 0, 0, 3000)?;
        sys::handle_close(mj);
    }
    let bi = boot_image_vmo()?;
    let hex = b"vol-uuid-6673642d73756974652d766f6c756d65";
    // ---- the first run: device 0 is not a volume (ignored), device 1 is, named `data` by the table
    let (mut reg, mj) = start_volmgr_rig(res, bi)?;
    let mut guard1 = KillOnDrop(mj);
    let mut data = open_volume(&mut reg, b"vol-store", RW_DIR)?;
    ensure_eq!(
        data.mkdir(b"kept"),
        Ok(()),
        "make a directory on the volume"
    );
    let mut f = data
        .open(b"kept/note", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        f.write(0, b"through the volume manager"),
        Ok(26),
        "write a file"
    );
    ensure_eq!(data.sync(), Ok(()), "sync");
    f.close();
    // file times are the wall clock's (the rig runs the clock service): everything changed since the RTC's base time
    let base = RTC_BASE_NS.to_le_bytes();
    let since = proto::file::Query {
        index: b"mtime",
        term: proto::file::QueryTerm::Range {
            lo: Some(&base),
            hi: None,
        },
        after: None,
        max: 100,
    };
    let (hits, _) = data.query(&since).map_err(|e| msg_e(e, line!()))?;
    let paths: Vec<Vec<u8>> = hits.iter().map(|h| h.path.clone()).collect();
    ensure!(
        paths.contains(&b"kept/note".to_vec()) && paths.contains(&b"kept".to_vec()),
        "the new file and directory carry wall-clock times: {paths:?}"
    );
    let before = proto::file::Query {
        index: b"mtime",
        term: proto::file::QueryTerm::Range {
            lo: None,
            hi: Some(&base),
        },
        after: None,
        max: 100,
    };
    let (old, _) = data.query(&before).map_err(|e| msg_e(e, line!()))?;
    ensure!(
        old.is_empty(),
        "nothing is older than the clock's base: {} hits",
        old.len()
    );
    data.close();
    // the same volume under its uuid
    let mut again = open_volume(&mut reg, hex, RO_DIR)?;
    let mut kept = again
        .open(b"kept", pfl::DIRECTORY, RO_DIR)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&kept.read_dir().map_err(|e| msg_e(e, line!()))?),
        vec![b"note".to_vec()],
        "by uuid"
    );
    // a read-only grant cannot write
    ensure!(
        again.mkdir(b"x").is_err(),
        "no write through a read-only connection"
    );
    kept.close();
    again.close();
    // a name the table does not map does not exist
    let mut view = reg.view(&[&b"vol-other"[..]], prt::ALL);
    if let Ok(v) = view.as_mut() {
        ensure!(
            v.open(b"vol-other", pfl::DIRECTORY, RO_DIR).is_err(),
            "an unmapped name gives nothing"
        );
    }
    // ---- the machine's services all die and start again: the volume comes back from the disk
    reg.close();
    ok(sys::job_kill(mj, -1), "kill the first rig")?;
    job_use_settles(mj, 0, 0, 5000)?;
    guard1.0 = 0;
    sys::handle_close(mj);
    let (mut reg, mj) = start_volmgr_rig(res, bi)?;
    let mut guard2 = KillOnDrop(mj);
    let mut data = open_volume(&mut reg, b"vol-store", RO_DIR)?;
    let mut f = data
        .open(b"kept/note", 0, prt::READ)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        f.read_all(),
        Ok(b"through the volume manager".to_vec()),
        "the file after the restart"
    );
    f.close();
    data.close();
    reg.close();
    ok(sys::job_kill(mj, -1), "kill the second rig")?;
    job_use_settles(mj, 0, 0, 5000)?;
    guard2.0 = 0;
    sys::handle_close(mj);
    sys::handle_close(res);
    sys::debug_write(b"fs: volmgr served the volume by the table and after a restart\n");
    Ok(())
}

/// Symlinks are followed by the client, never by a server (USER-VIEW-SPEC section 5, step UV1): `fsd` stops a walk at a link and
/// says how much of the path it consumed and what the target is; `pathres` resolves the target lexically and tries again.
fn symlinks_are_followed_by_the_client_not_the_server() -> Outcome {
    use rt::fs::Stopped;
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let have_disk = match find_blk(res)? {
        Some(_) => {
            let me = sys::process_self().a1 as u64;
            sys::process_unmap(me, MMIO_VA);
            sys::handle_close(me);
            true
        }
        None => false,
    };
    if !have_disk {
        return Ok(());
    }
    let (mgr_ch, mgr, mj) = start_devmgr_in_job(res)?;
    let mut guard = KillOnDrop(mj);
    let blk = open_disk(mgr_ch, 1, 1)?;
    {
        let mut dev = rt::blk::BlkDevice::open(blk).ok_or_else(|| msg_at(line!()))?;
        fsfmt::mkfs(&mut dev, FSD_UUID, "links").map_err(|_| msg_at(line!()))?;
    }
    let (p, mut root) = spawn_fsd(blk)?;
    let link = |root: &mut Fs, path: &[u8], target: &[u8]| -> Result<(), Detail> {
        let (a, payload) = proto::file::encode_symlink(path, target);
        let r = root
            .call(pop::SYMLINK, 0, 0, a, 0, &payload, None)
            .map_err(|e| msg_e(e, line!()))?;
        ensure_eq!(r.status, 0, "make a link");
        Ok(())
    };
    ensure_eq!(root.mkdir(b"d"), Ok(()), "mkdir");
    let mut f = root
        .open(b"d/f", pfl::CREATE, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(f.write(0, b"payload"), Ok(7), "write the target");
    f.close();
    link(&mut root, b"d/rel", b"f")?;
    link(&mut root, b"d/up", b"../d/f")?;
    link(&mut root, b"dirl", b"d")?;
    link(&mut root, b"loop1", b"loop2")?;
    link(&mut root, b"loop2", b"loop1")?;
    // the server refuses a target that climbs above the connection root, or is absolute (FS-SPEC 9.1)
    let (a, payload) = proto::file::encode_symlink(b"d/esc", b"../../d/f");
    let r = root
        .call(pop::SYMLINK, 0, 0, a, 0, &payload, None)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        r.status,
        pst::BAD_TARGET,
        "a link that climbs above the root is refused"
    );
    // what a server says when a walk meets a link
    ensure_eq!(
        root.open_stop(b"d/rel", 0, prt::READ).err(),
        Some(Stopped::Link {
            consumed: 2,
            target: b"f".to_vec()
        }),
        "a final link is reported with the components consumed and its target"
    );
    ensure_eq!(
        root.open_stop(b"dirl/f", 0, prt::READ).err(),
        Some(Stopped::Link {
            consumed: 1,
            target: b"d".to_vec()
        }),
        "a link in the middle stops the walk"
    );
    ensure!(
        root.open_stop(b"d/rel", pfl::NOFOLLOW, prt::READ).is_ok(),
        "NOFOLLOW opens the link itself"
    );
    ensure_eq!(
        root.stat_stop(b"d/rel", pfl::NOFOLLOW)
            .map(|s| (s.is_dir, s.size)),
        Ok((false, 1)),
        "and describes it: a link whose size is its target's length"
    );
    ensure_eq!(
        code(root.open(b"d/../d/f", 0, prt::READ)),
        Some(pst::BAD_REQUEST),
        "the server still refuses a raw `..`"
    );
    // the client follows
    let table = pathres::Table {
        mounts: vec![(vec![], root.ch)],
        cwd: Some(vec![b"d".to_vec()]),
        cwd_handle: None,
    };
    let read_via = |path: &[u8]| -> Result<Vec<u8>, Detail> {
        let mut view = Fs::new(root.ch);
        let (mut file, _) = pathres::open_following(&table, path, |_, rel| {
            view.open_stop(rel, 0, prt::READ).map_err(|e| match e {
                Stopped::Link { consumed, target } => pathres::Stop::Symlink { consumed, target },
                Stopped::Status(s) => pathres::Stop::Status(s),
            })
        })
        .map_err(|e| {
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("follow {path:?}: {e:?}"));
            d
        })?;
        let data = file.read_all().map_err(|e| msg_e(e, line!()))?;
        file.close();
        Ok(data)
    };
    ensure_eq!(
        read_via(b"/d/rel")?,
        b"payload".to_vec(),
        "a link in the same directory"
    );
    ensure_eq!(
        read_via(b"/d/up")?,
        b"payload".to_vec(),
        "a link with `..` resolves from the link's own directory"
    );
    ensure_eq!(
        read_via(b"/dirl/f")?,
        b"payload".to_vec(),
        "a link to a directory in the middle of a path"
    );
    ensure_eq!(
        read_via(b"rel")?,
        b"payload".to_vec(),
        "relative to the cwd"
    );
    ensure_eq!(
        read_via(b"../dirl/rel")?,
        b"payload".to_vec(),
        "`..` from the cwd, then two links"
    );
    ensure_eq!(
        read_via(b"/../../d/f")?,
        b"payload".to_vec(),
        "`..` above the root stays at the root"
    );
    ensure!(read_via(b"/loop1").is_err(), "a loop of links ends");
    root.close();
    wait_ended(p, 5000).ok();
    sys::handle_close(p);
    sys::debug_write(b"fs: symlinks are followed by the client\n");
    sys::handle_close(mgr_ch);
    wait_ended(mgr, 3000)?;
    sys::handle_close(mgr);
    job_use_settles(mj, 0, 0, 3000)?;
    guard.0 = 0;
    sys::handle_close(mj);
    sys::handle_close(res);
    Ok(())
}

/// `/dev` (USER-VIEW-SPEC section 8, UV2): `null` and `zero` are files that behave like their Unix namesakes, and nothing can
/// be created in the directory.
fn devd_serves_null_and_zero() -> Outcome {
    let bi = boot_image_vmo()?;
    let (off, len) = image_entry(bi, b"bin/devd")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let (mine, theirs) = channel()?;
    let s = Startup {
        args: vec![b"devd".to_vec()],
        roles: vec![(Role::ServiceFs, 0)],
        ..Startup::default()
    };
    let p = rt::spawn::spawn(&image, &s.encode().map_err(|_| msg_at(line!()))?, &[theirs])
        .map_err(|_| msg_at(line!()))?;
    let mut root = Fs::new(mine);
    let list = root.read_dir().map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        names(&list),
        vec![b"null".to_vec(), b"zero".to_vec()],
        "the directory (no random service: no urandom)"
    );
    let mut null = root
        .open(b"null", 0, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(null.write(0, b"gone"), Ok(4), "a write to null is accepted");
    ensure_eq!(
        null.read(0, 100),
        Ok(vec![]),
        "a read of null is the end at once"
    );
    ensure_eq!(null.size(), Ok(0), "its size is zero");
    null.close();
    let mut zero = root
        .open(b"zero", 0, prt::READ | prt::WRITE)
        .map_err(|e| msg_e(e, line!()))?;
    ensure_eq!(
        zero.read(0, 5000),
        Ok(vec![0u8; 5000]),
        "a read of zero is zeros"
    );
    ensure_eq!(zero.write(0, b"x"), Ok(1), "and a write is accepted");
    zero.close();
    ensure_eq!(
        code(root.open(b"new", pfl::CREATE, prt::READ | prt::WRITE)),
        Some(DENIED),
        "nothing can be created in /dev"
    );
    ensure_eq!(
        code(root.open(b"nope", 0, prt::READ)),
        Some(pst::NOT_FOUND),
        "an unknown device"
    );
    ensure_eq!(
        code(root.mkdir(b"d")),
        Some(pst::BAD_REQUEST),
        "no directories"
    );
    root.close();
    wait_ended(p, 3000)?;
    sys::handle_close(p);
    Ok(())
}

// --- Device capabilities (S6a) -------------------------------------------------------------

fn system_suspend_is_gated_and_not_supported_without_the_firmware() -> Outcome {
    // PM3: gated like the other root-resource calls; the firmware of the reference machine has no suspend extension, and the call says so and changes nothing
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let ev = event()?;
    ensure_eq!(
        st(sys::system_suspend(0xdead)),
        Some(S::BadHandle),
        "bad handle"
    );
    ensure_eq!(
        st(sys::system_suspend(ev)),
        Some(S::WrongType),
        "an event is no resource"
    );
    let weak = ok(sys::handle_duplicate(res, Rights::TRANSFER.0), "dup")?.a1 as u64;
    ensure_eq!(
        st(sys::system_suspend(weak)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    sys::handle_close(weak);
    ensure_eq!(
        sys::debug_stat(126),
        0,
        "the reference firmware has no SUSP"
    );
    ensure_eq!(
        st(sys::system_suspend(res)),
        Some(S::NotSupported),
        "not supported, nothing done"
    );
    ensure_eq!(
        ok(sys::hart_info(res, 0), "hart_info")?.a1,
        2,
        "hart 0 is still online"
    );
    Ok(())
}

fn hart_calls_need_the_resource_and_take_harts_offline_and_back() -> Outcome {
    // PM1: gated like the other root-resource calls; every hart but one goes offline and the last refuses; all come back
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let ev = event()?;
    for f in [
        |r: u64| sys::hart_info(r, 0),
        |r: u64| sys::hart_set_online(r, 0, 1),
    ] {
        ensure_eq!(st(f(0xdead)), Some(S::BadHandle), "bad handle");
        ensure_eq!(st(f(ev)), Some(S::WrongType), "an event is no resource");
        let weak = ok(sys::handle_duplicate(res, Rights::TRANSFER.0), "dup")?.a1 as u64;
        ensure_eq!(st(f(weak)), Some(S::AccessDenied), "no MANAGE");
        sys::handle_close(weak);
    }
    ensure_eq!(
        st(sys::hart_info(res, 4096)),
        Some(S::InvalidArgs),
        "no such hart"
    );
    ensure_eq!(
        st(sys::hart_set_online(res, 4096, 0)),
        Some(S::InvalidArgs),
        "no such hart"
    );
    ensure_eq!(
        st(sys::hart_set_online(res, 0, 2)),
        Some(S::InvalidArgs),
        "online is 0 or 1"
    );
    let mut n = 0;
    while st(sys::hart_info(res, n)) == Some(S::Ok) {
        ensure_eq!(
            ok(sys::hart_info(res, n), "hart_info")?.a1,
            2,
            "online at the start"
        );
        n += 1;
    }
    ensure_eq!(
        st(sys::hart_set_online(res, 0, 1)),
        Some(S::Ok),
        "already online: nothing to do"
    );
    for i in 1..n {
        ensure_eq!(st(sys::hart_set_online(res, i, 0)), Some(S::Ok), "offline");
        ensure_eq!(
            ok(sys::hart_info(res, i), "hart_info")?.a1,
            0,
            "offline reads back"
        );
    }
    // (with the others offline, hart 0 is the last)
    ensure_eq!(
        st(sys::hart_set_online(res, 0, 0)),
        Some(S::BadState),
        "the last hart refuses"
    );
    // a thread still runs and ends with one hart
    let t = new_thread(placement_probe, 0)?;
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
    ensure_eq!(r.status, 0, "a thread ran to its end on the one hart");
    for i in 1..n {
        ensure_eq!(
            st(sys::hart_set_online(res, i, 1)),
            Some(S::Ok),
            "online again"
        );
        ensure_eq!(
            ok(sys::hart_info(res, i), "hart_info")?.a1,
            2,
            "online reads back"
        );
    }
    Ok(())
}

fn system_set_placement_needs_the_resource_and_changes_the_policy() -> Outcome {
    // PM4: the call is gated like the other root-resource calls; the policy in force reads back through debug_stat 125; the default is spread; the scheduler works under both
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let ev = event()?;
    ensure_eq!(
        st(sys::system_set_placement(0xdead, 1)),
        Some(S::BadHandle),
        "bad handle"
    );
    ensure_eq!(
        st(sys::system_set_placement(ev, 1)),
        Some(S::WrongType),
        "an event is no resource"
    );
    let weak = ok(sys::handle_duplicate(res, Rights::TRANSFER.0), "dup")?.a1 as u64;
    ensure_eq!(
        st(sys::system_set_placement(weak, 1)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    ensure_eq!(
        st(sys::system_set_placement(res, 2)),
        Some(S::InvalidArgs),
        "no such policy"
    );
    ensure_eq!(sys::debug_stat(125), 0, "spread is the default");
    ensure_eq!(st(sys::system_set_placement(res, 1)), Some(S::Ok), "pack");
    ensure_eq!(sys::debug_stat(125), 1, "pack is in force");
    // threads still run and finish under pack
    let t = new_thread(placement_probe, 0)?;
    let r = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
    ensure_eq!(r.status, 0, "a thread started under pack ran to its end");
    ensure_eq!(
        st(sys::system_set_placement(res, 0)),
        Some(S::Ok),
        "spread again"
    );
    ensure_eq!(sys::debug_stat(125), 0, "spread is back");
    sys::handle_close(weak);
    Ok(())
}

extern "C" fn placement_probe(_: usize) -> ! {
    for _ in 0..4 {
        sys::thread_yield();
    }
    sys::thread_exit(0)
}

fn device_caps_need_the_resource() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let ev = event()?;
    let plain = vmo(1)?;
    // no resource, no device objects
    ensure_eq!(
        st(sys::vmo_create_physical(0xdead, 0x1000_1000, 4096)),
        Some(S::BadHandle),
        "bad handle"
    );
    ensure_eq!(
        st(sys::vmo_create_physical(ev, 0x1000_1000, 4096)),
        Some(S::WrongType),
        "an event is no resource"
    );
    ensure_eq!(
        st(sys::vmo_create_dma(ev, 1)),
        Some(S::WrongType),
        "dma: an event is no resource"
    );
    let weak = ok(sys::handle_duplicate(res, Rights::TRANSFER.0), "dup")?.a1 as u64;
    ensure_eq!(
        st(sys::vmo_create_physical(weak, 0x1000_1000, 4096)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    ensure_eq!(
        st(sys::vmo_create_dma(weak, 1)),
        Some(S::AccessDenied),
        "dma: no MANAGE"
    );
    // the device tree copy: same gate, and what comes back is a read-only VMO of the right shape
    ensure_eq!(
        st(sys::resource_dtb(0xdead)),
        Some(S::BadHandle),
        "dtb: bad handle"
    );
    ensure_eq!(
        st(sys::resource_dtb(ev)),
        Some(S::WrongType),
        "dtb: an event is no resource"
    );
    ensure_eq!(
        st(sys::resource_dtb(weak)),
        Some(S::AccessDenied),
        "dtb: no MANAGE"
    );
    let dtb = ok(sys::resource_dtb(res), "resource_dtb")?;
    let (dvmo, dlen) = (dtb.a1 as u64, dtb.a2);
    ensure!(
        (40..=256 * 1024).contains(&dlen),
        "a plausible length: {dlen}"
    );
    ensure_eq!(
        sys::vmo_get_size(dvmo).a1,
        dlen.div_ceil(4096) * 4096,
        "sized in whole pages"
    );
    let mut head = [0u8; 8];
    ensure_eq!(
        st(sys::vmo_read(dvmo, 0, head.as_mut_ptr(), 8)),
        Some(S::Ok),
        "read the header"
    );
    ensure_eq!(
        head[..4],
        [0xd0, 0x0d, 0xfe, 0xed],
        "the flattened-tree magic"
    );
    ensure_eq!(
        u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize,
        dlen,
        "the header's total size is the length returned"
    );
    ensure_eq!(
        st(sys::vmo_write(dvmo, 0, head.as_ptr(), 8)),
        Some(S::AccessDenied),
        "read-only"
    );
    sys::handle_close(dvmo);
    // device windows only: never RAM, never off-window, never misaligned or empty
    for (pa, len, what) in [
        (sys::debug_stat(100), 4096u64, "RAM"),
        (sys::debug_stat(104), 4096, "the kernel image"),
        (0x1000_0000, 4096, "just below the window"),
        (0x1000_9000, 4096, "just above the window"),
        (0x1000_8000, 8192, "straddling the end"),
        (0x1000_1001, 4096, "misaligned"),
        (0x1000_1000, 0, "empty"),
        (0x1000_1000, 100, "not a whole page"),
        (u64::MAX - 4095, 4096, "wrapping"),
        // PL5a: the windows come from the device tree and fail closed. What the kernel or the firmware drives, and what nobody decided
        // a process may touch, on the live tree, are refused: the test finisher, the CLINT, the M-level APLIC, the S-level APLIC and its
        // upper half, both IMSIC files, fw-cfg (its DMA interface writes guest RAM), both flash banks, the PCI ECAM window
        (0x10_0000, 4096, "the test finisher"),
        (0x200_0000, 4096, "the CLINT"),
        (0xc00_0000, 4096, "the M-level APLIC"),
        (0xd00_0000, 4096, "the S-level APLIC"),
        (0xd00_4000, 4096, "the S-level APLIC's upper half"),
        (0x2400_0000, 4096, "the M-level IMSIC file"),
        (0x2800_0000, 4096, "the S-level IMSIC file"),
        (0x1010_0000, 4096, "fw-cfg"),
        (0x2000_0000, 4096, "flash bank 0"),
        (0x2200_0000, 4096, "flash bank 1"),
        (0x3000_0000, 4096, "the PCI ECAM window"),
    ] {
        let r = sys::vmo_create_physical(res, pa, len);
        ensure!(st(r) == Some(S::InvalidArgs), "{what}: {:?}", st(r));
    }
    // the RTC page is a window too (the first entry of the table the kernel built from the tree)
    let rtc = ok(
        sys::vmo_create_physical(res, 0x10_1000, 4096),
        "the RTC page",
    )?
    .a1 as u64;
    sys::handle_close(rtc);
    // a good one; the kernel will not copy through it
    let dev = ok(
        sys::vmo_create_physical(res, 0x1000_1000, 4096),
        "an MMIO VMO",
    )?
    .a1 as u64;
    let mut b = [0u8; 4];
    ensure_eq!(
        st(sys::vmo_read(dev, 0, b.as_mut_ptr(), 4)),
        Some(S::NotSupported),
        "no vmo_read of device memory"
    );
    ensure_eq!(
        st(sys::vmo_write(dev, 0, b.as_ptr(), 4)),
        Some(S::NotSupported),
        "no vmo_write of device memory"
    );
    ensure_eq!(
        st(sys::vmo_phys_addr(dev)),
        Some(S::AccessDenied),
        "an MMIO VMO has no DMA right"
    );
    ensure_eq!(
        st(sys::vmo_phys_addr(plain)),
        Some(S::AccessDenied),
        "a plain VMO has no DMA right"
    );
    // DMA: limits, charging, the address only through the DMA right
    ensure_eq!(
        st(sys::vmo_create_dma(res, 0)),
        Some(S::InvalidArgs),
        "zero pages"
    );
    ensure_eq!(
        st(sys::vmo_create_dma(res, 65)),
        Some(S::InvalidArgs),
        "over the per-object limit"
    );
    let root = root_job()?;
    let before = job_use(root, 2)?.0;
    let dma = ok(sys::vmo_create_dma(res, 4), "a DMA VMO")?.a1 as u64;
    let after = job_use(root, 2)?.0;
    ensure_eq!(
        after - before,
        4,
        "the pages are charged to the creator's job at creation"
    );
    let pa = ok(sys::vmo_phys_addr(dma), "phys addr")?.a1 as u64;
    ensure!(
        pa.is_multiple_of(4096)
            && (sys::debug_stat(100)..sys::debug_stat(101)).contains(&(pa as u64)),
        "a RAM address: {pa:#x}"
    );
    ensure_eq!(job_use(root, 2)?.0, after, "asking costs nothing");
    let lesser = ok(
        sys::handle_duplicate(dma, Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0),
        "dup without DMA",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::vmo_phys_addr(lesser)),
        Some(S::AccessDenied),
        "a duplicate without DMA cannot learn the address"
    );
    // zeroed, readable and writable through the normal calls
    let mut buf = [0xffu8; 16];
    ensure_eq!(
        st(sys::vmo_read(dma, 3 * 4096, buf.as_mut_ptr(), 16)),
        Some(S::Ok),
        "read the last page"
    );
    ensure!(buf == [0u8; 16], "a DMA buffer starts zeroed");
    // closing every handle gives the pages back
    sys::handle_close(dma);
    sys::handle_close(lesser);
    job_use_settles(root, 2, before, 1000)?;
    sys::handle_close(dev);
    for h in [res, weak, ev, plain, root] {
        sys::handle_close(h);
    }
    Ok(())
}

/// D3: a process the kernel did not start cannot obtain the resource or mint device objects.
fn a_child_has_no_device_authority() -> Outcome {
    let (p, _mine) = launch(50)?;
    wait_ended(p, 2000)?;
    ensure_eq!(
        sys::process_info(p).a1 as i64,
        7,
        "the child was refused every device call"
    );
    sys::handle_close(p);
    Ok(())
}

/// A virtio-blk device found by scanning the eight slots: its mapped page and PLIC source.
struct Blk {
    regs: *mut u32,
    irq: u32,
}

const MMIO_VA: u64 = 0x600_0000;
const DMA_VA: u64 = 0x500_0000;

fn rd(b: &Blk, off: usize) -> u32 {
    // SAFETY: `b.regs` is a mapped virtio-mmio slot (4 KiB of device memory) and `off` is a
    // register offset inside it.
    unsafe { core::ptr::read_volatile(b.regs.byte_add(off)) }
}

fn wr(b: &Blk, off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { core::ptr::write_volatile(b.regs.byte_add(off), v) }
}

/// Map the virtio-mmio window and find a block device (None when the machine has no disk).
fn find_blk(res: u64) -> Result<Option<Blk>, Detail> {
    let dev = ok(
        sys::vmo_create_physical(res, 0x1000_1000, 8 * 4096),
        "the MMIO window",
    )?
    .a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, dev, MMIO_VA, 0, 8 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the window"
    );
    for slot in 0..8usize {
        let b = Blk {
            regs: (MMIO_VA as usize + slot * 4096) as *mut u32,
            irq: slot as u32 + 1,
        };
        if rd(&b, 0) == 0x7472_6976 && rd(&b, 8) == 2 {
            ensure_eq!(
                rd(&b, 4),
                2,
                "the modern transport (the harness passes force-legacy=false)"
            );
            sys::handle_close(dev);
            sys::handle_close(me);
            return Ok(Some(b));
        }
    }
    // no block device: leave nothing mapped (a later caller maps the window again)
    sys::process_unmap(me, MMIO_VA);
    sys::handle_close(dev);
    sys::handle_close(me);
    Ok(None)
}

/// Bring the device up with one queue of 8 descriptors in the DMA buffer at `pa`; the buffer
/// layout is: descriptors at 0, available ring at 128, used ring at 256, request header at 512,
/// data at 1024, status byte at 2048.
fn blk_up(b: &Blk, pa: u64) -> Outcome {
    wr(b, 0x70, 0); // reset
    wr(b, 0x70, 1); // ACKNOWLEDGE
    wr(b, 0x70, 1 | 2); // DRIVER
    wr(b, 0x14, 1); // device features, high word
    ensure!(rd(b, 0x10) & 1 != 0, "the device offers VIRTIO_F_VERSION_1");
    wr(b, 0x24, 1);
    wr(b, 0x20, 1); // we accept VERSION_1 and nothing else
    wr(b, 0x24, 0);
    wr(b, 0x20, 0);
    wr(b, 0x70, 1 | 2 | 8); // FEATURES_OK
    ensure!(rd(b, 0x70) & 8 != 0, "the device accepted our features");
    wr(b, 0x30, 0); // queue 0
    ensure!(rd(b, 0x34) >= 8, "queue size");
    wr(b, 0x38, 8);
    for (lo, at) in [(0x80, 0u64), (0x90, 128), (0xa0, 256)] {
        wr(b, lo, (pa + at) as u32);
        wr(b, lo + 4, ((pa + at) >> 32) as u32);
    }
    wr(b, 0x44, 1); // QueueReady
    wr(b, 0x70, 1 | 2 | 8 | 4); // DRIVER_OK
    Ok(())
}

/// Submit one read of sector 0 (3 descriptors) and ring the doorbell. `n` is how many requests
/// were submitted before (the available index).
fn blk_read_sector0(b: &Blk, pa: u64, n: u16) {
    let base = DMA_VA as usize;
    let put = |off: usize, v: u64, len: usize| {
        for i in 0..len {
            // SAFETY: the DMA buffer is mapped read-write at DMA_VA for 4 pages.
            unsafe { core::ptr::write_volatile((base + off + i) as *mut u8, (v >> (8 * i)) as u8) };
        }
    };
    // header: type 0 (IN), reserved 0, sector 0
    put(512, 0, 8);
    put(520, 0, 8);
    // descriptors: addr (8), len (4), flags (2), next (2)
    for (d, (addr, len, flags, next)) in [
        (pa + 512, 16u64, 1u64, 1u64),
        (pa + 1024, 512, 1 | 2, 2),
        (pa + 2048, 1, 2, 0),
    ]
    .into_iter()
    .enumerate()
    {
        put(d * 16, addr, 8);
        put(d * 16 + 8, len | (flags << 32) | (next << 48), 8);
    }
    put(2048, 0xff, 1); // status the device must overwrite
                        // available ring: flags 0, idx n+1, ring[n % 8] = 0
    put(128 + 4 + 2 * (n as usize % 8), 0, 2);
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    put(128 + 2, u64::from(n) + 1, 2);
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    wr(b, 0x50, 0); // QueueNotify
}

fn interrupt_object_fires_and_masks() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    // argument checks that hold on every machine
    ensure_eq!(
        st(sys::interrupt_create(res, 0)),
        Some(S::InvalidArgs),
        "source 0"
    );
    ensure_eq!(
        st(sys::interrupt_create(res, 9)),
        Some(S::InvalidArgs),
        "above the allowed range"
    );
    ensure_eq!(
        st(sys::interrupt_create(0xdead, 1)),
        Some(S::BadHandle),
        "no resource"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::interrupt_create(ev, 1)),
        Some(S::WrongType),
        "an event is no resource"
    );
    let Some(b) = find_blk(res)? else {
        return Ok(()); // no disk attached (OS_DISK=0): nothing more to provoke
    };
    let probe = sys::interrupt_create(res, b.irq);
    if st(probe) == Some(S::NotSupported) {
        // no interrupt controller (plain `virt`): drivers poll; the harness checks which machine this is
        // (the window stays mapped otherwise, and the next test that looks for the disk cannot map it)
        let me = ok(sys::process_self(), "process_self")?.a1 as u64;
        sys::process_unmap(me, MMIO_VA);
        return Ok(());
    }
    let irq = ok(probe, "interrupt_create")?.a1 as u64;
    ensure_eq!(
        st(sys::interrupt_create(res, b.irq)),
        Some(S::BadState),
        "one object per source"
    );
    // a DMA buffer for the queue and one request
    let dma = ok(sys::vmo_create_dma(res, 4), "DMA")?.a1 as u64;
    let pa = ok(sys::vmo_phys_addr(dma), "phys")?.a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, dma, DMA_VA, 0, 4 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the DMA buffer"
    );
    blk_up(&b, pa)?; // resets the device, so its line is low from here on
                     // A source can be asserted from before the object existed (an earlier driver's last request, a
                     // level the AIA re-pends when the source is enabled): one spurious interrupt after create is allowed
                     // (SYSCALLS.md). Take it if it came, then the object must be idle.
    nap_ticks(3);
    let mut base = 0; // claims taken before the real request (the spurious one counts)
    if sys::object_signals(irq).a1 as u32 & sig::INTERRUPT != 0 {
        let spurious = sys::interrupt_ack(irq);
        ensure_eq!(
            st(spurious),
            Some(S::Ok),
            "acknowledge the spurious interrupt"
        );
        base = spurious.a1;
        nap_ticks(3);
    }
    ensure_eq!(
        st(sys::interrupt_ack(irq)),
        Some(S::BadState),
        "nothing to acknowledge yet"
    );
    ensure_eq!(
        sys::object_signals(irq).a1 as u32 & sig::INTERRUPT,
        0,
        "quiet before anything happens"
    );
    sys::debug_write(b"s6: interrupt path exercised\n");
    let t0 = sys::clock_get();
    blk_read_sector0(&b, pa, 0);
    let r = sys::object_wait(irq, sig::INTERRUPT, t0 + 1000);
    ensure_eq!(
        st(r),
        Some(S::Ok),
        "the device's interrupt reaches the object"
    );
    // woken by the interrupt, not by the deadline running out (the wait re-reads the signals at
    // its deadline, so a missing wake-up would still return Ok, a second late)
    ensure!(sys::clock_get() - t0 < 200, "the waiter was woken late");
    ensure!(
        rd(&b, 0x60) & 1 != 0,
        "the device says a used buffer is pending"
    );
    // SAFETY: the status byte and first data bytes of the buffer mapped above.
    let (status, d0, d1) = unsafe {
        (
            core::ptr::read_volatile((DMA_VA + 2048) as *const u8),
            core::ptr::read_volatile((DMA_VA + 1024) as *const u8),
            core::ptr::read_volatile((DMA_VA + 1025) as *const u8),
        )
    };
    ensure_eq!(
        (status, d0, d1),
        (0, 3, 10),
        "the sector 0 pattern came through DMA"
    );
    // the device line stays asserted until we clear InterruptStatus: the source is masked, so the
    // kernel is not taking it again and again. Give it time, then count the claims.
    let t0 = sys::clock_get();
    let _ = sys::object_wait(ev, 1 << 24, t0 + 30);
    ensure_eq!(
        sys::object_signals(irq).a1 as u32 & sig::INTERRUPT,
        sig::INTERRUPT,
        "still pending until acknowledged"
    );
    // acknowledge with the line still high: the source is unmasked and the level fires again at
    // once (an AIA source in MSI mode pends only on a rising edge, so the kernel must re-pend it)
    let ack = ok(
        sys::interrupt_ack(irq),
        "acknowledge with the line still high",
    )?;
    ensure_eq!(ack.a1, base + 1, "exactly one claim for one request");
    let again = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 200);
    ensure_eq!(
        st(again),
        Some(S::Ok),
        "an acknowledged source whose line is still high fires again"
    );
    // quiet the device, then acknowledge: the source is unmasked and nothing more arrives
    wr(&b, 0x64, 1);
    let ack = ok(sys::interrupt_ack(irq), "acknowledge")?;
    ensure_eq!(ack.a1, base + 2, "the re-fire was the second claim");
    ensure_eq!(
        sys::object_signals(irq).a1 as u32 & sig::INTERRUPT,
        0,
        "the acknowledgement clears the signal"
    );
    let r = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 20);
    ensure_eq!(
        st(r),
        Some(S::TimedOut),
        "nothing more once the device is quiet"
    );
    let t1 = sys::clock_get();
    blk_read_sector0(&b, pa, 1);
    let r = sys::object_wait(irq, sig::INTERRUPT, t1 + 1000);
    ensure_eq!(st(r), Some(S::Ok), "a later request interrupts again");
    ensure!(
        sys::clock_get() - t1 < 200,
        "the second waiter was woken late"
    );
    wr(&b, 0x64, 1);
    sys::interrupt_ack(irq);
    // leave the device quiet and everything released
    wr(&b, 0x70, 0);
    ensure_eq!(st(sys::process_unmap(me, DMA_VA)), Some(S::Ok), "unmap DMA");
    ensure_eq!(
        st(sys::process_unmap(me, MMIO_VA)),
        Some(S::Ok),
        "unmap MMIO"
    );
    // closing the object releases the source: a new object can own it, with a pending interrupt
    // or not, and nothing is left enabled when the last one goes
    sys::handle_close(irq);
    let again = ok(
        sys::interrupt_create(res, b.irq),
        "the source is free again after close",
    )?
    .a1 as u64;
    sys::handle_close(again);
    for h in [dma, me, res, ev] {
        sys::handle_close(h);
    }
    Ok(())
}

/// Slice I: on the AIA a device interrupt goes to the hart that asked for it (the last one to create
/// or acknowledge the object), never to another one; the block-driver tests above took many. On
/// more than one hart some must have been taken off the boot hart. (The PLIC path counts nothing.)
fn device_interrupts_arrive_where_asked() -> Outcome {
    let (taken, off_boot, misrouted) = (sys::debug_stat(3), sys::debug_stat(4), sys::debug_stat(5));
    sys::debug_write(
        alloc::format!(
            "s6: device interrupts taken {taken}, off the boot hart {off_boot}, misrouted {misrouted}\n"
        )
        .as_bytes(),
    );
    ensure_eq!(
        misrouted,
        0,
        "interrupts taken on a hart they were not aimed at"
    );
    Ok(())
}

fn mmio_reads_the_virtio_magic() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let dev = ok(
        sys::vmo_create_physical(res, 0x1000_1000, 4096),
        "an MMIO VMO",
    )?
    .a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, dev, 0x400_0000, 0, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the first virtio-mmio slot"
    );
    // SAFETY: 0x400_0000 was just mapped (device memory, 4 KiB) in this process; the registers at
    // offsets 0, 4 and 8 are read-only magic, version and device id.
    let (magic, version, device) = unsafe {
        (
            core::ptr::read_volatile(0x400_0000 as *const u32),
            core::ptr::read_volatile(0x400_0004 as *const u32),
            core::ptr::read_volatile(0x400_0008 as *const u32),
        )
    };
    ensure_eq!(magic, 0x7472_6976, "the virtio magic ('virt')");
    ensure!(
        version == 1 || version == 2,
        "virtio-mmio version {version}"
    );
    ensure!(device < 64, "a plausible device id {device}");
    // the kernel never copies through device memory: a syscall pointing at it is refused
    let plain = vmo(1)?;
    ensure_eq!(
        st(sys::vmo_read(plain, 0, 0x400_0010 as *mut u8, 4)),
        Some(S::InvalidArgs),
        "kernel copy into an MMIO mapping"
    );
    ensure_eq!(
        st(sys::debug_write(
            // SAFETY: the range is mapped (device memory); the slice is only handed to the kernel,
            // which must refuse it, and never read here.
            unsafe { core::slice::from_raw_parts(0x400_0000 as *const u8, 4) }
        )),
        Some(S::InvalidArgs),
        "kernel copy out of an MMIO mapping"
    );
    ensure_eq!(st(sys::process_unmap(me, 0x400_0000)), Some(S::Ok), "unmap");
    // the VMO is closable while mapped and after
    sys::handle_close(dev);
    sys::handle_close(res);
    sys::handle_close(me);
    sys::handle_close(plain);
    Ok(())
}

fn root_job() -> Result<u64, Detail> {
    Ok(ok(sys::job_root(), "job_root")?.a1 as u64)
}

fn job(parent: u64, procs: u64, threads: u64, pages: u64) -> Result<u64, Detail> {
    Ok(ok(
        sys::job_create(
            parent,
            &[procs, threads, pages, u64::MAX, u64::MAX, u64::MAX],
        ),
        "job_create",
    )?
    .a1 as u64)
}

/// (used, limit) of a job's resource: 0 processes, 1 threads, 2 pages.
fn job_use(j: u64, which: u64) -> Result<(u64, u64), Detail> {
    let r = ok(sys::job_info(j, which), "job_info")?;
    Ok((r.a1 as u64, r.a2 as u64))
}

/// Wait (up to `ticks`) for a job's usage of `which` to reach `want`.
fn job_use_settles(j: u64, which: u64, want: u64, ticks: u64) -> Outcome {
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

// --- The real-time demo (SCHED-SMP-SPEC 6.1, OS-ARCHITECTURE section 8; task 8) ---------------------------------------------------
//
// "A high-priority thread stays responsive while low-priority CPU hogs run": one hog per online hart at the default priority keeps every
// hart busy, and a probe thread sleeps to a deadline 3 ms away 30 times and records how late each wake was. At a real-time priority the
// lateness is small; at the hogs' own priority (the negative control) the probe waits for a slice end, so a bound that only passes when
// the priority matters is the evidence that the bound means something.

static RT_STOP: AtomicU32 = AtomicU32::new(0);
static RT_PROBE_PRIO: AtomicU32 = AtomicU32::new(0);
static RT_WORST_LATE_US: AtomicU64 = AtomicU64::new(0);
/// The second-worst of the probe's samples (T219: one pause of the host delays one wake, which must not fail the bound).
static RT_SECOND_LATE_US: AtomicU64 = AtomicU64::new(0);

extern "C" fn rt_hog(_: usize) -> ! {
    while RT_STOP.load(Ordering::Relaxed) == 0 {
        core::hint::spin_loop();
    }
    sys::thread_exit(0)
}

extern "C" fn rt_probe(_: usize) -> ! {
    sys::thread_set_sched(0, RT_PROBE_PRIO.load(Ordering::SeqCst) as u8, 0);
    let gate = sys::event_create().a1 as u64;
    let (mut worst, mut second) = (0u64, 0u64);
    for _ in 0..30 {
        let target = sys::clock_get() + 3; // milliseconds: the wait's own unit
        sys::object_wait(gate, 1 << 24, target);
        // (the same clock before and after, in nanoseconds: the time counter has another origin, the firmware's start)
        let late = sys::clock_ns().saturating_sub(target * 1_000_000);
        let late = late / 1000; // microseconds
        if late > worst {
            second = worst;
            worst = late;
        } else {
            second = second.max(late);
        }
    }
    RT_SECOND_LATE_US.store(second, Ordering::SeqCst);
    RT_WORST_LATE_US.store(worst, Ordering::SeqCst);
    sys::handle_close(gate);
    sys::thread_exit(0)
}

/// The probe's worst and second-worst lateness (microseconds) at `prio` with a hog on every hart.
fn worst_lateness_us(prio: u32) -> Result<(u64, u64), Detail> {
    let harts = job_use(root_job()?, 7)?.0;
    RT_STOP.store(0, Ordering::SeqCst);
    RT_PROBE_PRIO.store(prio, Ordering::SeqCst);
    RT_WORST_LATE_US.store(u64::MAX, Ordering::SeqCst);
    let mut hogs = Vec::new();
    for _ in 0..harts {
        hogs.push(new_thread(rt_hog, 0)?);
    }
    nap_ticks(5); // the hogs are running, one per hart
    let probe = new_thread(rt_probe, 0)?;
    let w = sys::object_wait(probe, sig::TERMINATED, sys::clock_get() + 20_000);
    RT_STOP.store(1, Ordering::SeqCst);
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    sys::handle_close(probe);
    ensure_eq!(w.status, 0, "the probe finished");
    Ok((
        RT_WORST_LATE_US.load(Ordering::SeqCst),
        RT_SECOND_LATE_US.load(Ordering::SeqCst),
    ))
}

fn a_real_time_thread_stays_responsive_while_hogs_run_and_a_normal_one_does_not() -> Outcome {
    let (rt, second) = worst_lateness_us(40)?;
    println!("##BENCH rt_user_latency_max_us {rt} us");
    // (QEMU on a loaded host delivers a timer interrupt up to a few ms late, and a pause of the host delays one wake: the bound is on the second-worst of
    // the 30 samples, which a scheduler that delays every wake still fails, and the worst only has to stay within a hundredth of a second: T219)
    ensure!(
        second <= 4_000 && rt <= 30_000,
        "a real-time thread woke {rt} us late (second worst {second} us) with a hog on every hart (bound 4 ms, worst 30 ms)"
    );
    // the negative control: at the hogs' priority the wake waits for a slice to end
    let (normal, _) = worst_lateness_us(10)?;
    ensure!(
        normal > 6_000,
        "an equal-priority probe was only {normal} us late: the hogs are not loading the harts"
    );
    Ok(())
}

// --- The priority-inheritance futex (L23, SCHED-SMP-SPEC 4.8 and 6.2) ---------------------------------------------------------------
//
// The lock word is 0 (free) or the owner's tid; user code takes it with a compare-and-swap and calls the kernel only when that fails.

static PI_WORD: AtomicU32 = AtomicU32::new(0);
static PI_WORD2: AtomicU32 = AtomicU32::new(0);
static PI_STATE: AtomicU32 = AtomicU32::new(0);
static PI_GO: AtomicU32 = AtomicU32::new(0);
static PI_RELEASE: AtomicU32 = AtomicU32::new(0);
static PI_PROGRESS: AtomicU32 = AtomicU32::new(0);
static PI_RESULT_A: AtomicU64 = AtomicU64::new(u64::MAX);
static PI_RESULT_B: AtomicU64 = AtomicU64::new(u64::MAX);

fn pi_my_tid() -> u32 {
    sys::thread_get_sched(0).a2 as u32
}

/// Lock as a program does: the fast path, then the kernel. The status (0: owned).
fn pi_lock(w: &AtomicU32, deadline_ms: u64) -> i64 {
    let me = pi_my_tid();
    if w.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return 0;
    }
    i64::from(sys::futex_lock_pi(w.as_ptr(), deadline_ms).status)
}

fn pi_unlock(w: &AtomicU32) -> i64 {
    let me = pi_my_tid();
    if w.compare_exchange(me, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return 0;
    }
    i64::from(sys::futex_unlock_pi(w.as_ptr()).status)
}

/// The boots that start the thread ids near 2^32 (`ff=tid:...`, the counters' fast-forward) give every thread an id that does not fit in the lock
/// word's 30 bits: a PI futex is NOT_SUPPORTED there, and that is what is tested instead (SYSCALLS.md, T216).
fn pi_ids_do_not_fit() -> Option<Outcome> {
    if sys::thread_get_sched(0).a2 as u64 <= 0x3fff_ffff {
        return None;
    }
    let p = PI_WORD.as_ptr();
    Some((|| -> Outcome {
        ensure_eq!(
            st(sys::futex_lock_pi(p, sys::clock_get() + 50)),
            Some(S::NotSupported),
            "a thread id that does not fit cannot lock a PI futex"
        );
        ensure_eq!(
            st(sys::futex_unlock_pi(p)),
            Some(S::NotSupported),
            "nor unlock one"
        );
        Ok(())
    })())
}

fn pi_reset() {
    for a in [
        &PI_WORD,
        &PI_WORD2,
        &PI_STATE,
        &PI_GO,
        &PI_RELEASE,
        &PI_PROGRESS,
    ] {
        a.store(0, Ordering::SeqCst);
    }
    PI_RESULT_A.store(u64::MAX, Ordering::SeqCst);
    PI_RESULT_B.store(u64::MAX, Ordering::SeqCst);
}

fn pi_wait_for(a: &AtomicU32, want: u32, ms: u64) -> bool {
    let until = sys::clock_get() + ms;
    while a.load(Ordering::SeqCst) != want {
        if sys::clock_get() > until {
            return false;
        }
        nap_ticks(1);
    }
    true
}

/// The low-priority owner of the classic inversion: it takes the lock, says so, and when it is told to go it does work that needs the CPU.
extern "C" fn pi_low_owner(_: usize) -> ! {
    sys::thread_set_sched(0, 5, 0);
    pi_lock(&PI_WORD, u64::MAX);
    PI_STATE.store(1, Ordering::SeqCst);
    while PI_GO.load(Ordering::SeqCst) == 0 {
        core::hint::spin_loop();
    }
    // (the critical section: it runs, and counts, until main says it may let go, so that the boost can be looked at while it lasts)
    while PI_RELEASE.load(Ordering::SeqCst) == 0 {
        PI_PROGRESS.fetch_add(1, Ordering::Relaxed);
    }
    pi_unlock(&PI_WORD);
    PI_STATE.store(2, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// The high-priority waiter: asks for the lock with a deadline and records how it went and how long it took.
extern "C" fn pi_high_waiter(_: usize) -> ! {
    sys::thread_set_sched(0, 25, 0);
    let t0 = sys::clock_get();
    let r = pi_lock(&PI_WORD, t0 + 4_000);
    PI_RESULT_A.store(r as u64, Ordering::SeqCst);
    PI_RESULT_B.store(sys::clock_get() - t0, Ordering::SeqCst);
    if r == 0 {
        pi_unlock(&PI_WORD);
    }
    sys::thread_exit(0)
}

// --- Donation through channel_call (L22, SCHED-SMP-SPEC 4.8) ---------------------------------------------------------------------------------
//
// A server of low priority reads a call's request and then needs the CPU to answer it while mid-priority hogs run on every hart. Without donation it
// starves and the call times out; with the caller's high priority lent to it, it runs and answers.

static CD_A: AtomicU64 = AtomicU64::new(0);
static CD_B: AtomicU64 = AtomicU64::new(0);
static CD_PRIO: AtomicU32 = AtomicU32::new(0);
static CD_READ: AtomicU32 = AtomicU32::new(0);
static CD_GO: AtomicU32 = AtomicU32::new(0);
static CD_ABORT: AtomicU32 = AtomicU32::new(0);
static CD_WORK: AtomicU32 = AtomicU32::new(0);
static CD_STATUS: AtomicU64 = AtomicU64::new(u64::MAX);
static CD_MS: AtomicU64 = AtomicU64::new(0);

extern "C" fn cd_server(_: usize) -> ! {
    sys::thread_set_sched(0, 5, 0);
    let b = CD_B.load(Ordering::SeqCst);
    sys::object_wait(b, sig::READABLE, sys::clock_get() + 8_000);
    let mut req = [0u8; 16];
    let r = sys::channel_read(b, req.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    if r.status == 0 {
        CD_READ.store(1, Ordering::SeqCst);
        // the answer needs the CPU: it waits to be told the hogs are up, then works (unless the run was given up: a starved server that finally runs
        // after the hogs stop must neither work nor answer, or the control would see a reply)
        while CD_GO.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        if CD_ABORT.load(Ordering::SeqCst) != 0 {
            sys::thread_exit(0);
        }
        for _ in 0..200_000 {
            CD_WORK.fetch_add(1, Ordering::Relaxed);
        }
        sys::channel_write(b, req.as_ptr(), 4, core::ptr::null(), 0);
    }
    sys::thread_exit(0)
}

extern "C" fn cd_client(_: usize) -> ! {
    sys::thread_set_sched(0, CD_PRIO.load(Ordering::SeqCst) as u8, 0);
    let req = 1u32.to_le_bytes();
    let mut reply = [0u8; 16];
    let t0 = sys::clock_get();
    let args = CallArgs {
        req_bytes: req.as_ptr() as u64,
        req_handles: 0,
        req_nbytes: 4,
        req_nhandles: 0,
        rep_bytes: reply.as_mut_ptr() as u64,
        rep_handles: 0,
        rep_max_bytes: 16,
        rep_max_handles: 0,
        deadline: t0 + 1_500,
    };
    let r = sys::channel_call(CD_A.load(Ordering::SeqCst), &args);
    CD_STATUS.store(r.status as i64 as u64, Ordering::SeqCst);
    CD_MS.store(sys::clock_get() - t0, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// One run: a server of priority 5 and a client of `client_prio`; hogs of priority 15 on every hart once the server has read the request.
fn donation_run(client_prio: u32) -> Result<(u64, u64, u32), Detail> {
    let r = ok(sys::channel_create(), "channel_create")?;
    CD_A.store(r.a1 as u64, Ordering::SeqCst);
    CD_B.store(r.a2 as u64, Ordering::SeqCst);
    CD_PRIO.store(client_prio, Ordering::SeqCst);
    for a in [&CD_READ, &CD_GO, &CD_WORK, &CD_ABORT] {
        a.store(0, Ordering::SeqCst);
    }
    CD_STATUS.store(u64::MAX, Ordering::SeqCst);
    let harts = job_use(root_job()?, 7)?.0;
    ok(
        sys::thread_set_sched(0, 31, 0),
        "main to the top of the normal band",
    )?;
    let server = new_thread(cd_server, 0)?;
    nap_ticks(10);
    let client = new_thread(cd_client, 0)?;
    // the server reads the request (nothing else is running: it gets the CPU), then the hogs take every hart
    let until = sys::clock_get() + 5_000;
    while CD_READ.load(Ordering::SeqCst) == 0 && sys::clock_get() < until {
        nap_ticks(1);
    }
    ensure_eq!(
        CD_READ.load(Ordering::SeqCst),
        1,
        "the server never read the request"
    );
    RT_STOP.store(0, Ordering::SeqCst);
    let mut hogs = Vec::new();
    for _ in 0..harts {
        let h = new_thread(rt_hog, 0)?;
        ok(sys::thread_set_sched(h, 15, 0), "a hog above the server")?;
        hogs.push(h);
    }
    nap_ticks(10);
    CD_GO.store(1, Ordering::SeqCst);
    // (the call's own deadline is 1.5 s: a client of low priority cannot even return from its timeout while the hogs hold every hart, so they
    // are stopped after 2.5 s whatever happened, and the client is then waited for)
    let _ = sys::object_wait(client, sig::TERMINATED, sys::clock_get() + 2_500);
    // (what the server did while the hogs held the harts: read now, before they stop and let a starved server run)
    let work_under_hogs = CD_WORK.load(Ordering::SeqCst);
    CD_ABORT.store(1, Ordering::SeqCst);
    RT_STOP.store(1, Ordering::SeqCst);
    let w = sys::object_wait(client, sig::TERMINATED, sys::clock_get() + 20_000);
    ok(sys::thread_set_sched(0, 10, 0), "main back to normal")?;
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    sys::object_wait(server, sig::TERMINATED, sys::clock_get() + 20_000);
    sys::handle_close(server);
    sys::handle_close(client);
    sys::handle_close(CD_A.load(Ordering::SeqCst));
    sys::handle_close(CD_B.load(Ordering::SeqCst));
    ensure_eq!(w.status, 0, "the client finished");
    Ok((
        CD_STATUS.load(Ordering::SeqCst),
        CD_MS.load(Ordering::SeqCst),
        work_under_hogs,
    ))
}

/// A call lends its priority to the thread that reads its request: a server of priority 5 answers a priority-25 caller under hogs of priority 15; a
/// caller of the server's own priority gets nothing lent and its call times out (the control: the hogs do starve the server).
fn a_call_lends_its_priority_to_the_server() -> Outcome {
    if sys::debug_stat(127) == 1 {
        return Ok(()); // (round-robin starves nobody: the control cannot hold)
    }
    let (status, _, work) = donation_run(5)?;
    ensure_eq!(
        status as i64,
        S::TimedOut.code() as i64,
        "without a higher-priority caller the starved server answered: the hogs do not load the harts, so the test shows nothing"
    );
    ensure_eq!(work, 0, "the starved server made progress under the hogs");
    let (status, ms, work) = donation_run(25)?;
    ensure_eq!(
        status,
        0,
        "the call with a priority-25 caller did not get its reply"
    );
    ensure!(ms < 1_400, "the reply took {ms} ms");
    ensure_eq!(work, 200_000, "the server did not finish its work");
    Ok(())
}

/// The classic three-thread inversion: a low-priority thread owns the lock and a hog of middle priority runs on every hart, so the owner gets no CPU
/// (checked first: the control); a high-priority thread then waits for the lock, and through the donation the owner runs, finishes and unlocks.
fn pi_futex_bounds_inversion() -> Outcome {
    if sys::debug_stat(127) == 1 {
        return Ok(()); // (round-robin starves nobody: the control cannot hold)
    }
    if let Some(r) = pi_ids_do_not_fit() {
        return r;
    }
    pi_reset();
    let harts = job_use(root_job()?, 7)?.0;
    ok(
        sys::thread_set_sched(0, 31, 0),
        "main to the top of the normal band",
    )?;
    let low = new_thread(pi_low_owner, 0)?;
    ensure!(
        pi_wait_for(&PI_STATE, 1, 5_000),
        "the low-priority thread never took the lock"
    );
    RT_STOP.store(0, Ordering::SeqCst);
    let mut hogs = Vec::new();
    for _ in 0..harts {
        let h = new_thread(rt_hog, 0)?;
        ok(sys::thread_set_sched(h, 15, 0), "a hog above the owner")?;
        hogs.push(h);
    }
    nap_ticks(10);
    PI_GO.store(1, Ordering::SeqCst);
    nap_ticks(200);
    let starved = PI_PROGRESS.load(Ordering::SeqCst);
    let high = new_thread(pi_high_waiter, 0)?;
    // the owner is boosted by the waiter; a change of its own base priority meanwhile (to the same value, even) must not drop the boost
    nap_ticks(20);
    let eff_before = (sys::thread_get_sched(low).a1 >> 16) & 0xff;
    ok(
        sys::thread_set_sched(low, 6, 0),
        "the owner's base priority changes while it is boosted",
    )?;
    let eff_after = (sys::thread_get_sched(low).a1 >> 16) & 0xff;
    // (the owner has been running on the waiter's priority under the hogs: it made progress, and the waiter is still waiting)
    nap_ticks(50);
    let progress_while_boosted = PI_PROGRESS.load(Ordering::SeqCst);
    PI_RELEASE.store(1, Ordering::SeqCst);
    let w = sys::object_wait(high, sig::TERMINATED, sys::clock_get() + 20_000);
    RT_STOP.store(1, Ordering::SeqCst);
    ok(sys::thread_set_sched(0, 10, 0), "main back to normal")?;
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    sys::object_wait(low, sig::TERMINATED, sys::clock_get() + 20_000);
    sys::handle_close(low);
    sys::handle_close(high);
    ensure_eq!(w.status, 0, "the high-priority thread finished");
    ensure_eq!(
        eff_before,
        25,
        "the owner runs at the waiter's priority while it waits"
    );
    ensure_eq!(
        eff_after,
        25,
        "a change of the owner's base priority dropped the boost"
    );
    ensure_eq!(starved, 0, "the owner made progress under a hog on every hart: the hogs do not load the harts, so the test shows nothing");
    ensure_eq!(
        PI_RESULT_A.load(Ordering::SeqCst),
        0,
        "the high-priority thread got the lock"
    );
    let took = PI_RESULT_B.load(Ordering::SeqCst);
    ensure!(took < 3_000, "the lock took {took} ms to come");
    ensure!(
        progress_while_boosted > 0,
        "the owner made no progress while the waiter lent it its priority"
    );
    ensure_eq!(
        PI_WORD.load(Ordering::SeqCst),
        0,
        "the word is free at the end"
    );
    Ok(())
}

extern "C" fn pi_dying_owner(_: usize) -> ! {
    pi_lock(&PI_WORD, u64::MAX);
    PI_STATE.store(1, Ordering::SeqCst);
    while PI_GO.load(Ordering::SeqCst) == 0 {
        sys::thread_yield();
    }
    sys::thread_exit(0) // (still holding the lock)
}

extern "C" fn pi_waiter_a(_: usize) -> ! {
    let t0 = sys::clock_get();
    let r = pi_lock(&PI_WORD, t0 + 5_000);
    PI_RESULT_A.store(r as u64, Ordering::SeqCst);
    PI_RESULT_B.store(sys::clock_get() - t0, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// An owner that exits holding the lock poisons it: the waiter is woken with BAD_STATE, and so is the next thread that asks.
fn pi_futex_owner_death_poisons() -> Outcome {
    if let Some(r) = pi_ids_do_not_fit() {
        return r;
    }
    pi_reset();
    let owner = new_thread(pi_dying_owner, 0)?;
    ensure!(
        pi_wait_for(&PI_STATE, 1, 5_000),
        "the owner never took the lock"
    );
    let waiter = new_thread(pi_waiter_a, 0)?;
    nap_ticks(50); // the waiter is queued
    PI_GO.store(1, Ordering::SeqCst); // the owner exits
    let w = sys::object_wait(waiter, sig::TERMINATED, sys::clock_get() + 10_000);
    sys::object_wait(owner, sig::TERMINATED, sys::clock_get() + 10_000);
    sys::handle_close(owner);
    sys::handle_close(waiter);
    ensure_eq!(
        w.status,
        0,
        "the waiter finished (it was woken, not left to its deadline)"
    );
    ensure_eq!(
        PI_RESULT_A.load(Ordering::SeqCst) as i64,
        S::BadState.code() as i64,
        "the waiter was told the lock is poisoned"
    );
    let waited = PI_RESULT_B.load(Ordering::SeqCst);
    ensure!(
        waited < 3_000,
        "the waiter was left to its deadline ({waited} ms of 5000), not woken by the owner's death"
    );
    ensure!(
        PI_WORD.load(Ordering::SeqCst) & (1 << 30) != 0,
        "the word does not say the owner died"
    );
    ensure_eq!(
        st(sys::futex_lock_pi(PI_WORD.as_ptr(), sys::clock_get() + 100)),
        Some(S::BadState),
        "a later thread finds it poisoned"
    );
    Ok(())
}

extern "C" fn pi_deadlock_a(_: usize) -> ! {
    pi_lock(&PI_WORD, u64::MAX);
    PI_STATE.store(1, Ordering::SeqCst);
    while PI_STATE.load(Ordering::SeqCst) < 2 {
        sys::thread_yield();
    }
    // (B holds the second lock: wait for it, which B will end by refusing the cycle and then handing the lock over)
    let r = pi_lock(&PI_WORD2, sys::clock_get() + 8_000);
    PI_RESULT_A.store(r as u64, Ordering::SeqCst);
    if r == 0 {
        pi_unlock(&PI_WORD2);
    }
    pi_unlock(&PI_WORD);
    sys::thread_exit(0)
}

extern "C" fn pi_deadlock_b(_: usize) -> ! {
    pi_lock(&PI_WORD2, u64::MAX);
    PI_STATE.store(2, Ordering::SeqCst);
    nap_ticks(100); // A is waiting for the second lock
                    // A holds the first and waits for ours: waiting for it would close the cycle
    let r = pi_lock(&PI_WORD, sys::clock_get() + 8_000);
    PI_RESULT_B.store(r as u64, Ordering::SeqCst);
    pi_unlock(&PI_WORD2); // hands the lock to A
    sys::thread_exit(0)
}

/// Two threads each hold a lock and ask for the other's: the second ask is refused with BAD_STATE at once (no sleep), and A gets its lock afterwards.
fn pi_futex_deadlock_is_refused() -> Outcome {
    if let Some(r) = pi_ids_do_not_fit() {
        return r;
    }
    pi_reset();
    let a = new_thread(pi_deadlock_a, 0)?;
    let b = new_thread(pi_deadlock_b, 0)?;
    let wa = sys::object_wait(a, sig::TERMINATED, sys::clock_get() + 20_000);
    let wb = sys::object_wait(b, sig::TERMINATED, sys::clock_get() + 20_000);
    sys::handle_close(a);
    sys::handle_close(b);
    ensure_eq!(wa.status, 0, "A finished");
    ensure_eq!(wb.status, 0, "B finished");
    ensure_eq!(
        PI_RESULT_B.load(Ordering::SeqCst) as i64,
        S::BadState.code() as i64,
        "the ask that closes the cycle is refused"
    );
    ensure_eq!(
        PI_RESULT_A.load(Ordering::SeqCst),
        0,
        "A got the second lock once B let go of it"
    );
    ensure_eq!(PI_WORD.load(Ordering::SeqCst), 0, "the first word is free");
    ensure_eq!(
        PI_WORD2.load(Ordering::SeqCst),
        0,
        "the second word is free"
    );
    Ok(())
}

/// The error cases: a bad address, a relock, a poll, and an unlock by a thread that does not own it.
fn pi_futex_argument_errors() -> Outcome {
    if let Some(r) = pi_ids_do_not_fit() {
        return r;
    }
    pi_reset();
    let p = PI_WORD.as_ptr();
    ensure_eq!(
        st(sys::futex_lock_pi(
            (p as usize + 1) as *const u32,
            sys::clock_get() + 50
        )),
        Some(S::InvalidArgs),
        "a misaligned address"
    );
    ensure_eq!(
        st(sys::futex_lock_pi(
            0x10 as *const u32,
            sys::clock_get() + 50
        )),
        Some(S::InvalidArgs),
        "an address below user memory"
    );
    ensure_eq!(
        st(sys::futex_unlock_pi(0x7fff_ff00_0000 as *const u32)),
        Some(S::InvalidArgs),
        "an unmapped address"
    );
    ensure_eq!(
        pi_lock(&PI_WORD, u64::MAX),
        0,
        "a free lock is taken in user mode"
    );
    ensure_eq!(
        st(sys::futex_lock_pi(p, sys::clock_get() + 50)),
        Some(S::BadState),
        "a relock would wait for itself"
    );
    ensure_eq!(pi_unlock(&PI_WORD), 0, "released by the owner");
    ensure_eq!(
        st(sys::futex_unlock_pi(p)),
        Some(S::AccessDenied),
        "an unlock of a free lock"
    );
    // a lock held by another thread: a poll that would have to wait times out, a wait times out at its deadline, an unlock is refused
    PI_STATE.store(0, Ordering::SeqCst);
    PI_GO.store(0, Ordering::SeqCst);
    let owner = new_thread(pi_dying_owner, 0)?;
    ensure!(
        pi_wait_for(&PI_STATE, 1, 5_000),
        "the owner never took the lock"
    );
    ensure_eq!(
        st(sys::futex_lock_pi(p, 0)),
        Some(S::TimedOut),
        "a poll of a lock that is held"
    );
    let t0 = sys::clock_get();
    ensure_eq!(
        st(sys::futex_lock_pi(p, t0 + 60)),
        Some(S::TimedOut),
        "a wait that reaches its deadline"
    );
    ensure!(
        sys::clock_get() >= t0 + 60,
        "it returned before its deadline"
    );
    ensure_eq!(
        st(sys::futex_unlock_pi(p)),
        Some(S::AccessDenied),
        "an unlock by a thread that is not the owner"
    );
    PI_GO.store(1, Ordering::SeqCst);
    sys::object_wait(owner, sig::TERMINATED, sys::clock_get() + 10_000);
    sys::handle_close(owner);
    Ok(())
}

/// SCHED-SMP-SPEC 4.5 (task 25): a real-time spinner on every hart does not hang the system. The job's budget (the root's: one whole window
/// of CPU per 100 ms) is spent by the hogs within a fraction of each window and they are demoted to the top of the normal band until it ends, so
/// a thread there (this one, at 31) keeps getting the CPU, and the real-time time the job is billed per window stays at its budget.
fn a_runaway_rt_thread_does_not_hang_the_system() -> Outcome {
    let root = root_job()?;
    let harts = job_use(root, 7)?.0;
    RT_STOP.store(0, Ordering::SeqCst);
    let mut hogs = Vec::new();
    for _ in 0..harts {
        hogs.push(new_thread(rt_hog, 0)?);
    }
    ok(
        sys::thread_set_sched(0, 31, 0),
        "main to the top of the normal band",
    )?;
    for &h in &hogs {
        ok(sys::thread_set_sched(h, 40, 0), "a hog turns real-time")?;
    }
    let mut worst_rt = 0u64;
    let mut trace: Vec<(u64, u64)> = Vec::new(); // (ms into the 100 ms window, RT ms billed so far)
    let mut naps_done = 0;
    let until = sys::clock_get() + 20_000;
    while naps_done < 30 && sys::clock_get() < until {
        nap_ticks(10); // the hogs hold every hart: this sleep needs a hart to wake on
        let used = job_use(root, 9)?.0;
        worst_rt = worst_rt.max(used);
        trace.push((sys::clock_get() % 100, used / 1_000_000));
        naps_done += 1;
    }
    RT_STOP.store(1, Ordering::SeqCst);
    ok(sys::thread_set_sched(0, 10, 0), "main back to normal")?;
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    ensure_eq!(naps_done, 30, "thirty naps finished while the hogs ran");
    // (without the budget the job's real-time time in a window would be about `harts` windows long; allow a tick or two of overshoot)
    ensure!(
        worst_rt <= 100_000_000 + harts * 20_000_000,
        "the job was billed {worst_rt} ns of real-time time in one 100 ms window (budget 100 ms, {harts} harts); (ms into window, RT ms): {trace:?}"
    );
    Ok(())
}

static CPU_SPIN_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static CPU_SLEEP_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Spins for 100 ms of the clock and reports the CPU time its own thread was charged (handle 0: the caller).
extern "C" fn cpu_spinner(_: usize) -> ! {
    let start = sys::clock_ns();
    while sys::clock_ns() < start + 100_000_000 {
        core::hint::spin_loop();
    }
    CPU_SPIN_NS.store(sys::thread_cpu_ns(0).a1 as u64, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// Sleeps 50 ms and reports the same.
extern "C" fn cpu_sleeper(_: usize) -> ! {
    let gate = sys::event_create().a1 as u64;
    sys::object_wait(gate, 1 << 24, sys::clock_get() + 50);
    CPU_SLEEP_NS.store(sys::thread_cpu_ns(0).a1 as u64, Ordering::SeqCst);
    sys::thread_exit(0)
}

/// AB6 (SCHED-SMP-SPEC 4.10, AC1): a thread that spins for 100 ms is charged about 100 ms, one that sleeps under 1 ms, an ended thread's handle keeps
/// its figure, and the process's figure is at least the sum of the threads we made and read.
fn cpu_time_is_counted_per_thread_and_process() -> Outcome {
    CPU_SPIN_NS.store(u64::MAX, Ordering::SeqCst);
    CPU_SLEEP_NS.store(u64::MAX, Ordering::SeqCst);
    let me_p = ok(sys::process_self(), "process_self")?.a1 as u64;
    let before = ok(sys::process_cpu_ns(me_p), "process cpu")?.a1 as u64;
    let spin = new_thread(cpu_spinner, 0)?;
    let sleep = new_thread(cpu_sleeper, 0)?;
    for t in [spin, sleep] {
        ensure_eq!(
            sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 20_000).status,
            0,
            "a thread ended"
        );
    }
    let spun = CPU_SPIN_NS.load(Ordering::SeqCst);
    let slept = CPU_SLEEP_NS.load(Ordering::SeqCst);
    // (the thread's own figure at its end: 5 percent either way; the spec's 2 percent needs a quieter machine than a shared CI runner)
    ensure!(
        (95_000_000..=105_000_000).contains(&spun),
        "a thread that spun for 100 ms was charged {spun} ns"
    );
    // (the thread's start-up, its event and its two calls cost about a millisecond on a loaded CI machine: the point is that 50 ms of sleep cost nothing)
    ensure!(
        slept < 5_000_000,
        "a thread that slept 50 ms was charged {slept} ns"
    );
    // an ended thread's handle keeps the total
    let spun_handle = ok(sys::thread_cpu_ns(spin), "thread cpu of an ended thread")?.a1 as u64;
    ensure!(
        spun_handle >= spun && spun_handle <= spun + 2_000_000,
        "the ended thread's handle says {spun_handle} ns, the thread itself said {spun}"
    );
    // the process has at least what these two ran
    let after = ok(sys::process_cpu_ns(me_p), "process cpu")?.a1 as u64;
    ensure!(
        after >= before + spun_handle,
        "the process's figure rose by {} ns, the spinner alone ran {spun_handle}",
        after - before
    );
    // errors: an unknown selector, a handle that is no thread
    ensure_eq!(
        st(sys::thread_info_which(spin, 2)),
        Some(S::InvalidArgs),
        "an unknown selector"
    );
    ensure_eq!(
        st(sys::thread_cpu_ns(me_p)),
        Some(S::WrongType),
        "a process handle is not a thread"
    );
    for h in [spin, sleep] {
        sys::handle_close(h);
    }
    Ok(())
}

/// Spin for `ms` milliseconds of the clock.
fn spin_ms(ms: u64) {
    let until = sys::clock_get() + ms;
    while sys::clock_get() < until {
        core::hint::spin_loop();
    }
}

/// SCHED-SMP-SPEC 4.5 and 4.10 (task 8): the CPU time a job's threads run is counted (`job_info` 10, the job and the jobs below it), and
/// the part of it run at a real-time **base** priority is counted again in the window's figure (`job_info` 9), against the job's budget.
fn job_info_counts_the_cpu_time_its_threads_run() -> Outcome {
    let root = root_job()?;
    let (_, budget) = job_use(root, 9)?;
    ensure_eq!(
        budget,
        100_000_000,
        "the root job's budget is the whole window"
    );
    let before = job_use(root, 10)?.0;
    spin_ms(30);
    let after = job_use(root, 10)?.0;
    ensure!(
        after >= before + 25_000_000,
        "30 ms of spinning added only {} ns to the job's CPU time",
        after - before
    );
    // at a real-time priority the same spin shows in the window's real-time figure (the window may end during it: 100 ms windows, so spin
    // until a spin of 30 ms has been counted twice at most; the figure is what ran since the window began)
    ok(sys::thread_set_sched(0, 40, 0), "become real-time")?;
    let mut best = 0;
    for _ in 0..4 {
        spin_ms(30);
        best = best.max(job_use(root, 9)?.0);
    }
    ok(sys::thread_set_sched(0, 10, 0), "back to the normal band")?;
    ensure!(
        best >= 25_000_000,
        "the real-time figure never passed 25 ms with 120 ms of real-time spinning in 100 ms windows (best {best} ns)"
    );
    // a normal thread adds no real-time time: the figure stops growing when the window is the same and we run at priority 10
    let (a, _) = job_use(root, 9)?;
    spin_ms(10);
    let (b, _) = job_use(root, 9)?;
    ensure!(
        b <= a + 5_000_000 || b < a,
        "10 ms of normal-band spinning added {} ns of real-time time",
        b.saturating_sub(a)
    );
    Ok(())
}

fn job_syscalls_check_handles_rights_and_arguments() -> Outcome {
    let root = root_job()?;
    // a job is a job: the other kinds of handle are the wrong type
    let ev = ok(sys::event_create(), "event")?.a1 as u64;
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
        st(sys::job_set_region_limit(ev, 5)),
        Some(S::WrongType),
        "job_set_region_limit on an event"
    );
    ensure_eq!(
        st(sys::job_set_region_limit(0, 5)),
        Some(S::BadHandle),
        "job_set_region_limit on handle 0"
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
        st(sys::job_info(root, 12)),
        Some(S::InvalidArgs),
        "an unknown resource"
    );
    // 7 is the processors (UV6): at least one, the same number twice, and the same for every job
    let (cpus, cpus_limit) = job_use(root, 7)?;
    ensure!(
        cpus >= 1 && cpus == cpus_limit,
        "job_info 7 gives the online harts twice"
    );
    ensure!(cpus <= 8, "a plausible number of harts");
    let child = job(root, 1, 1, 1)?;
    ensure_eq!(
        job_use(child, 7)?,
        (cpus, cpus),
        "every job sees the same processors"
    );
    sys::handle_close(child);
    ensure_eq!(
        st(sys::job_info(root, 12)),
        Some(S::InvalidArgs),
        "the resource after the last"
    );
    // each operation needs its own right
    let only = |r: Rights| -> Result<u64, Detail> {
        Ok(ok(sys::handle_duplicate(root, r.0), "duplicate")?.a1 as u64)
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
        st(sys::job_set_region_limit(manage, 5)),
        Some(S::AccessDenied),
        "manage may not set the region limit"
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
    let a = ok(sys::process_create_in(j), "first")?.a1 as u64;
    let b = ok(sys::process_create_in(j), "second")?.a1 as u64;
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

fn proc_create_in(job: u64) -> Result<u64, Detail> {
    Ok(ok(sys::process_create_in(job), "process_create_in")?.a1 as u64)
}

fn job_memory_limits_stop_a_hog_and_are_credited_back() -> Outcome {
    let root = root_job()?;
    // the child's own image, its stack and the page tables of the system library (AB7) count against its job too: 80 pages leave it room for
    // some of its own, but not for a hog
    let j = job(root, u64::MAX, u64::MAX, 80)?;
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 30, j)?;
    wait_ended(p, 3000)?;
    let got = sys::process_info(p).a1 as i64;
    ensure!(
        (1..80).contains(&got),
        "the hog got {got} pages of an 80-page job (and was told no)"
    );
    // all of it is given back when the child is gone
    job_use_settles(j, 2, 0, 1000)?;
    // (the root job's own page count is not stable: this program's heap and stack grow too)
    sys::handle_close(p);
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// MM5: a confined program that makes objects and handles until the job refuses cannot hold much kernel memory the job was not charged
/// for. The child (crash mode 99) makes events and duplicates a handle until a call says no, then waits; the kernel heap bytes it holds
/// (the heap leak oracle, `debug_stat` 109, before against after) may exceed what its job was charged (`job_info` 8) by at most 64 KiB.
fn a_confined_program_cannot_hold_much_uncharged_kernel_memory() -> Outcome {
    const SLACK: i64 = 64 * 1024;
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, 128)?; // 128 pages: 64 KiB of kernel bytes
    let before = sys::debug_stat(109);
    let (p, _mine) = launch_image_in(IMAGE.load(Ordering::Relaxed), 99, j)?;
    // wait until its kernel-byte use stops growing
    let (mut last, mut steady) = (0u64, 0);
    for _ in 0..300 {
        nap(10)?;
        let used = job_use(j, 8)?.0;
        steady = if used == last && used > 0 {
            steady + 1
        } else {
            0
        };
        last = used;
        if steady >= 10 {
            break;
        }
    }
    let (used, limit) = job_use(j, 8)?;
    ensure_eq!(
        limit,
        128 * 512,
        "the limit is an eighth of the pages, in bytes"
    );
    ensure!(
        steady >= 10,
        "the child stopped asking for kernel memory (used {used})"
    );
    ensure!(
        used > limit / 2,
        "the child got close to its limit ({used} of {limit})"
    );
    ensure!(used <= limit + 4096, "and not past it ({used} of {limit})");
    let held = before.wrapping_sub(sys::debug_stat(109)) as i64;
    ensure!(
        held <= used as i64 + SLACK,
        "the child holds {held} heap bytes for {used} charged: more than {SLACK} uncharged"
    );
    sys::process_kill(p, 1);
    wait_ended(p, 3000)?;
    sys::handle_close(p);
    // everything the child had comes back
    job_use_settles(j, 8, 0, 2000)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

/// Slice V: `vmo_commit` charges the VMO's job now and fails cleanly; a recipient without the COMMIT right
/// cannot spend the creator's quota; a VMO charged to a stranger's job must be committed before it can be
/// mapped, after which touching it is free and safe.
fn vmo_commit_charges_up_front_and_foreign_maps_need_it() -> Outcome {
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, 6)?; // six pages
    let v = ok(
        sys::vmo_create_in(8, j),
        "a VMO of 8 pages in a six-page job",
    )?
    .a1 as u64;
    ensure_eq!(job_use(j, 2)?.0, 0, "creation is lazy");
    ensure_eq!(
        st(sys::vmo_commit(v, 0, 4 * 4096)),
        Some(S::Ok),
        "commit four pages"
    );
    ensure_eq!(job_use(j, 2)?.0, 4, "charged now");
    ensure_eq!(
        st(sys::vmo_commit(v, 4 * 4096, 4 * 4096)),
        Some(S::NoMemory),
        "four more pages would pass the six-page limit (all or nothing)"
    );
    ensure_eq!(job_use(j, 2)?.0, 4, "a refused commit changes nothing");
    ensure_eq!(
        st(sys::vmo_commit(v, 100, 4096)),
        Some(S::InvalidArgs),
        "an unaligned offset"
    );
    ensure_eq!(
        st(sys::vmo_commit(v, 6 * 4096, 4 * 4096)),
        Some(S::InvalidArgs),
        "past the end"
    );
    // a recipient without COMMIT cannot commit, and cannot spend the creator's quota
    let weak = ok(
        sys::handle_duplicate(v, (Rights::VMO.0 & !Rights::COMMIT.0) & !Rights::SAME.0),
        "a duplicate without COMMIT",
    )?
    .a1 as u64;
    ensure_eq!(
        st(sys::vmo_commit(weak, 4 * 4096, 4096)),
        Some(S::AccessDenied),
        "no COMMIT, no commit"
    );
    // mapping a stranger's VMO (it is charged to job `j`, we are neither `j` nor in it) needs it committed
    let me = me()?;
    let base = NEXT_STACK.fetch_add(0x10_0000, Ordering::SeqCst);
    ensure_eq!(
        st(sys::process_map(me, v, base, 0, 8 * 4096, PF_R | PF_W)),
        Some(S::BadState),
        "an uncommitted stranger's VMO cannot be mapped over its whole length"
    );
    ensure_eq!(
        st(sys::process_map(me, v, base, 4 * 4096, 4096, PF_R | PF_W)),
        Some(S::BadState),
        "an offset into the uncommitted part is refused too"
    );
    ensure_eq!(
        st(sys::process_map(
            me,
            v,
            base,
            2 * 4096,
            2 * 4096,
            PF_R | PF_W
        )),
        Some(S::Ok),
        "pages 2 and 3 are committed, so they can be mapped at an offset"
    );
    sys::process_unmap(me, base);
    ensure_eq!(
        st(sys::process_map(me, v, base, 0, 4 * 4096, PF_R | PF_W)),
        Some(S::Ok),
        "the committed four pages can"
    );
    // touching them charges nothing more and cannot fail
    // SAFETY: the pages were just mapped read-write at `base` and are ours alone.
    unsafe { core::ptr::write_volatile(base as *mut u8, 7) };
    unsafe { core::ptr::write_volatile((base + 3 * 4096) as *mut u8, 7) };
    ensure_eq!(
        job_use(j, 2)?.0,
        4,
        "touching committed pages costs nothing"
    );
    sys::process_unmap(me, base);
    sys::handle_close(weak);
    sys::handle_close(v);
    job_use_settles(j, 2, 0, 1000)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

fn a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling() -> Outcome {
    let root = root_job()?;
    let parent = job(root, u64::MAX, u64::MAX, 8)?;
    let a = job(parent, u64::MAX, u64::MAX, u64::MAX)?; // its own limit allows everything
    let b = job(parent, u64::MAX, u64::MAX, u64::MAX)?;
    let big = ok(sys::vmo_create_in(100, a), "a VMO in a")?.a1 as u64;
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
    let other = ok(sys::vmo_create_in(4, b), "a VMO in b")?.a1 as u64;
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
        let v = ok(sys::vmo_create_in(2, j), "vmo")?.a1 as u64;
        ok(sys::vmo_write(v, 0, b"x".as_ptr(), 1), "commit a page")?;
        for h in [v, p, j] {
            sys::handle_close(h);
        }
    }
    job_use_settles(root, 0, procs0, 500)?;
    sys::handle_close(root);
    Ok(())
}

/// MM2: contiguous runs come from the buddy allocator and go back to it. The most one DMA object may hold is 64 pages (`MAX_DMA_PAGES`;
/// PL8 derives the limits from RAM later), and 1024 pages may be held in all: runs of 64 pages are made and closed many times, several
/// live at once (each at its own address, all in RAM, none overlapping), and runs of an odd length round up and give the tail back.
/// The kernel's final audit says nothing leaked and the free lists merged back.
fn dma_runs_are_allocated_and_freed_repeatedly() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    for round in 0..40u32 {
        let v = ok(sys::vmo_create_dma(res, 64), "a 64-page DMA run")?.a1 as u64;
        let pa = ok(sys::vmo_phys_addr(v), "its address")?.a1;
        ensure_eq!(pa % 4096, 0, "page aligned, round {round}");
        ensure!(
            (sys::debug_stat(100)..sys::debug_stat(101)).contains(&(pa as u64)),
            "inside RAM: {pa:#x}"
        );
        sys::handle_close(v);
    }
    // several at once: no two overlap (sixteen runs of 64 pages is the whole 1024-page allowance)
    let mut live = alloc::vec::Vec::new();
    for _ in 0..16 {
        let v = ok(sys::vmo_create_dma(res, 64), "sixteen runs live at once")?.a1 as u64;
        live.push((v, ok(sys::vmo_phys_addr(v), "address")?.a1));
    }
    for (i, (_, a)) in live.iter().enumerate() {
        for (_, b) in live.iter().skip(i + 1) {
            ensure!(
                a.abs_diff(*b) >= 64 * 4096,
                "runs {a:#x} and {b:#x} overlap"
            );
        }
    }
    for (v, _) in live {
        sys::handle_close(v);
    }
    // an odd length, repeatedly: the rounded-up tail must come back each time
    for _ in 0..40 {
        let v = ok(sys::vmo_create_dma(res, 37), "a 37-page DMA run")?.a1 as u64;
        sys::handle_close(v);
    }
    sys::handle_close(res);
    Ok(())
}

fn a_refused_process_start_changes_nothing_and_can_be_retried() -> Outcome {
    // (found by the independent review of S4.5a: the refusal came after the bootstrap handle had
    // moved, and a second start found handle 1 taken and panicked the kernel)
    let root = root_job()?;
    let j = job(root, u64::MAX, 0, u64::MAX)?; // no threads at all
    let p = proc_create_in(j)?;
    let seg = ok(sys::vmo_create_in(1, j), "segment")?.a1 as u64;
    // one instruction, `jal x0, 0`: a jump to itself. (An empty page is an illegal instruction: the
    // child would fault and end at once, and "one thread running" below would race with its exit.)
    ok(
        sys::vmo_write(seg, 0, 0x0000_006fu32.to_le_bytes().as_ptr(), 4),
        "write the code",
    )?;
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

/// Slice T: a thread's kernel stack (four frames) is billed to the job that owns the thread while the thread
/// lives, and credited back when it ends.
fn thread_stacks_are_charged_to_the_job_and_credited_back() -> Outcome {
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let (p, _m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 16, j)?;
    nap(5)?;
    // map a stack for the thread first (its page-table charge is not what is measured)
    let v = vmo(4)?;
    let base = map_child_stack(p, v)?;
    sys::handle_close(v);
    // (the child may still be starting up, and every page it touches is charged: read the use again until it has stopped moving, T238)
    let mut used = job_use(j, 2)?.0;
    for _ in 0..50 {
        nap(5)?;
        let now = job_use(j, 2)?.0;
        if now == used {
            break;
        }
        used = now;
    }
    let used0 = used;
    let limits = |pages: u64| [u64::MAX, u64::MAX, pages, u64::MAX, u64::MAX, u64::MAX];
    // three pages of room: the four-page stack does not fit, and nothing changes
    ensure_eq!(
        st(sys::job_set_limits(j, &limits(used + 3))),
        Some(S::Ok),
        "set the page limit"
    );
    let entry = child_entry(p)?;
    ensure_eq!(
        st(sys::thread_on_stack(p, entry, base + 4 * 4096, 16)),
        Some(S::NoMemory),
        "a thread whose stack does not fit the job"
    );
    // (nothing of the stack is charged; a page-table page that the attempt needed, where the random placement put the stack's end, may stay, so the room below is
    // counted from what the job has now: T238)
    let after = job_use(j, 2)?.0;
    ensure!(
        after >= used && after - used <= 1,
        "a refused thread charged nothing but at most a page-table page: {used} -> {after}"
    );
    used = after;
    ensure_eq!(
        job_use(j, 1)?.0,
        1,
        "and no thread slot either (the main thread only)"
    );
    ensure_eq!(
        st(sys::job_set_limits(j, &limits(used + 4))),
        Some(S::Ok),
        "four pages of room"
    );
    let r = sys::thread_on_stack(p, entry, base + 4 * 4096, 16);
    if r.status != 0 {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "the thread now fits: status {} (use was {used} before the refusal, {after} after, and is {} now; limit {})",
                r.status,
                job_use(j, 2)?.0,
                used + 4
            ),
        );
        return Err(d);
    }
    let t = r.a1 as u64;
    let got = job_use(j, 2)?.0;
    if got != used + 4 {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "the stack's four pages are charged: got {got}, want {} (before the refusal {used0}, after {after}); thread signals {:#x}, info status {}, cpu_ns {}, process signals {:#x}, use now {}",
                used + 4,
                sys::object_signals(t).a1,
                sys::thread_info(t).status,
                sys::thread_cpu_ns(t).a1,
                sys::object_signals(p).a1,
                job_use(j, 2)?.0
            ),
        );
        return Err(d);
    }
    // ending the process credits the stack back (everything settles to zero)
    ensure_eq!(st(sys::process_kill(p, 3)), Some(S::Ok), "kill");
    wait_ended(p, 1000)?;
    sys::handle_close(t);
    sys::handle_close(p);
    job_use_settles(j, 2, 0, 500)?;
    job_use_settles(j, 1, 0, 500)?;
    sys::handle_close(j);
    sys::handle_close(root);
    Ok(())
}

// --- the network (S8) ---

#[path = "../net_tests.rs"]
mod net_tests;
#[path = "../stream_proto.rs"]
mod stream_proto;

// --- thread_spawn (SCHED-SMP-SPEC task 9) ---

/// A process that ends (exits, faults, is killed) with spawned threads alive gives every stack back: its job's page
/// and thread use return to zero (the review of task 9 found the stacks leaked on a whole-process end).
fn a_process_ending_with_spawned_threads_leaks_nothing() -> Outcome {
    let root = root_job()?;
    for mode in 0..3u64 {
        let j = job(root, u64::MAX, u64::MAX, u64::MAX)?;
        let (p, m) = launch_image_in(IMAGE.load(Ordering::Relaxed), 68 | mode << 8, j)?;
        if mode == 2 {
            let w = sys::object_wait(m, sig::READABLE, sys::clock_get() + 3000);
            ensure_eq!(st(w), Some(S::Ok), "the child spawned its threads");
            ok(sys::process_kill(p, 9), "kill it")?;
        }
        wait_ended(p, 3000)?;
        let code = sys::process_info(p).a1 as i64;
        let want = [0, -1001, 9][mode as usize];
        ensure_eq!(code, want, "exit code, mode {mode}");
        job_use_settles(j, 2, 0, 1000)?;
        job_use_settles(j, 1, 0, 1000)?;
        sys::handle_close(p);
        sys::handle_close(m);
        sys::handle_close(j);
    }
    sys::handle_close(root);
    Ok(())
}

/// How deep `spawn_probe` recurses (a kilobyte a level): 0 for a small caller-provided stack.
static SPAWN_DIG: AtomicU32 = AtomicU32::new(40);
static SPAWN_SEEN: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// What a spawned thread starts with: `a0` the argument, `a1` its tid; `tp` and `sp` are whatever it holds.
extern "C" fn spawn_probe(arg: u64, tid: u64) -> ! {
    let (tp, sp): (u64, u64);
    // SAFETY: reads two registers.
    unsafe {
        core::arch::asm!("mv {}, tp", "mv {}, sp", out(reg) tp, out(reg) sp, options(nomem, nostack))
    };
    // use some stack, so the pages under it exist (a recursion that keeps a kilobyte per level)
    fn dig(n: u32) -> u8 {
        let pad = [n as u8; 1024];
        core::hint::black_box(&pad);
        if n == 0 {
            0
        } else {
            dig(n - 1).wrapping_add(pad[0])
        }
    }
    let _ = core::hint::black_box(dig(SPAWN_DIG.load(Ordering::SeqCst)));
    SPAWN_SEEN[0].store(arg, Ordering::SeqCst);
    SPAWN_SEEN[1].store(tid, Ordering::SeqCst);
    SPAWN_SEEN[2].store(tp, Ordering::SeqCst);
    SPAWN_SEEN[3].store(sp, Ordering::SeqCst);
    SPAWN_SEEN[4].store(1, Ordering::SeqCst);
    sys::thread_exit(3)
}

fn spawn_rec(entry: extern "C" fn(u64, u64) -> !) -> rt::SpawnArgs {
    rt::SpawnArgs {
        entry: entry as *const () as usize as u64,
        ..Default::default()
    }
}

fn thread_spawn_starts_with_the_documented_state() -> Outcome {
    let me = me()?;
    for a in &SPAWN_SEEN {
        a.store(0, Ordering::SeqCst);
    }
    let rec = rt::SpawnArgs {
        arg: 0xfeed,
        tp: 0x1234_5678,
        ..spawn_rec(spawn_probe)
    };
    let r = ok(sys::thread_spawn(me, &rec), "thread_spawn")?;
    let (h, tid) = (r.a1 as u64, r.a2 as u64);
    ensure!(h != 0 && tid != 0, "a handle and a tid");
    let w = sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 1000);
    ensure_eq!(w.status, 0, "the thread ended");
    ensure_eq!(sys::thread_info(h).a1 as i64, 3, "its exit code");
    ensure_eq!(
        SPAWN_SEEN[0].load(Ordering::SeqCst),
        0xfeed,
        "a0 is the argument"
    );
    ensure_eq!(
        SPAWN_SEEN[1].load(Ordering::SeqCst),
        tid,
        "a1 is the tid thread_spawn returned"
    );
    ensure_eq!(
        SPAWN_SEEN[2].load(Ordering::SeqCst),
        0x1234_5678,
        "tp as given"
    );
    let sp = SPAWN_SEEN[3].load(Ordering::SeqCst);
    ensure!(
        (rt::abi::user::STACK_ARENA_BASE..rt::abi::user::STACK_ARENA_END).contains(&sp),
        "the stack is in the arena ({sp:#x})"
    );
    ensure!(sp.is_multiple_of(16), "sp is 16-byte aligned");
    sys::handle_close(h);
    // a detached thread returns no handle
    SPAWN_SEEN[4].store(0, Ordering::SeqCst);
    let d = rt::SpawnArgs {
        flags: rt::abi::spawn::DETACHED,
        ..spawn_rec(spawn_probe)
    };
    let r = ok(sys::thread_spawn(me, &d), "a detached thread")?;
    ensure_eq!(r.a1, 0, "no handle for a detached thread");
    ensure!(r.a2 != 0, "but a tid");
    let until = sys::clock_get() + 1000;
    while SPAWN_SEEN[4].load(Ordering::SeqCst) == 0 && sys::clock_get() < until {
        sys::thread_yield();
    }
    ensure_eq!(
        SPAWN_SEEN[4].load(Ordering::SeqCst),
        1,
        "the detached thread ran"
    );
    // a caller-provided stack: nothing is provisioned, sp is the caller's
    let top = own_stack()?;
    SPAWN_SEEN[4].store(0, Ordering::SeqCst);
    SPAWN_DIG.store(0, Ordering::SeqCst);
    let c = rt::SpawnArgs {
        flags: rt::abi::spawn::CALLER_STACK,
        sp: top,
        ..spawn_rec(spawn_probe)
    };
    let r = ok(sys::thread_spawn(me, &c), "a caller stack")?;
    let w = sys::object_wait(r.a1 as u64, sig::TERMINATED, sys::clock_get() + 1000);
    SPAWN_DIG.store(40, Ordering::SeqCst);
    ensure_eq!(w.status, 0, "it ended");
    let sp = SPAWN_SEEN[3].load(Ordering::SeqCst);
    ensure!(
        sp <= top && sp > top - 4 * 4096,
        "it ran on the caller's stack ({sp:#x})"
    );
    sys::handle_close(r.a1 as u64);
    Ok(())
}

fn thread_spawn_error_cases() -> Outcome {
    let me = me()?;
    let go = |p: u64, rec: &rt::SpawnArgs| st(sys::thread_spawn(p, rec));
    let good = spawn_rec(spawn_probe);
    ensure_eq!(
        st(rt::syscall(abi_nr_spawn(), [me as usize, 8, 0, 0, 0, 0])),
        Some(S::InvalidArgs),
        "a bad args pointer"
    );
    ensure_eq!(go(BAD, &good), Some(S::BadHandle), "a bad handle");
    let ev = event()?;
    ensure_eq!(go(ev, &good), Some(S::WrongType), "an event as the process");
    let weak = weaker(me, Rights::WAIT.0)?;
    ensure_eq!(go(weak, &good), Some(S::AccessDenied), "no MANAGE");
    sys::handle_close(weak);
    let fresh = proc_create()?;
    ensure_eq!(
        go(fresh, &good),
        Some(S::BadState),
        "a process that is not running"
    );
    sys::handle_close(fresh);
    static DATA: u64 = 5;
    let bad_entry = rt::SpawnArgs {
        entry: &DATA as *const u64 as u64,
        ..good
    };
    ensure_eq!(
        go(me, &bad_entry),
        Some(S::InvalidArgs),
        "an entry that is not executable"
    );
    let nothing = rt::SpawnArgs { entry: 0, ..good };
    ensure_eq!(go(me, &nothing), Some(S::InvalidArgs), "entry 0");
    let cases: [(&str, rt::SpawnArgs); 8] = [
        (
            "stack bytes over 64 MiB",
            rt::SpawnArgs {
                stack_bytes: 64 * 1024 * 1024 + 4096,
                ..good
            },
        ),
        (
            "a huge stack",
            rt::SpawnArgs {
                stack_bytes: u64::MAX,
                ..good
            },
        ),
        (
            "a huge guard",
            rt::SpawnArgs {
                guard_bytes: u64::MAX,
                ..good
            },
        ),
        ("a reserved flag", rt::SpawnArgs { flags: 16, ..good }),
        (
            "the reserved word",
            rt::SpawnArgs {
                reserved: 1,
                ..good
            },
        ),
        (
            "priority 64",
            rt::SpawnArgs {
                priority: 64,
                ..good
            },
        ),
        (
            "a stack pointer without CALLER_STACK",
            rt::SpawnArgs { sp: 0x1000, ..good },
        ),
        (
            "a misaligned caller stack",
            rt::SpawnArgs {
                flags: rt::abi::spawn::CALLER_STACK,
                sp: 0x28_0000_0008,
                ..good
            },
        ),
    ];
    for (what, rec) in &cases {
        ensure_eq!(go(me, rec), Some(S::InvalidArgs), "{what}");
    }
    let outside = rt::SpawnArgs {
        flags: rt::abi::spawn::CALLER_STACK,
        sp: rt::abi::user::TOP + 16,
        ..good
    };
    ensure_eq!(
        go(me, &outside),
        Some(S::InvalidArgs),
        "a caller stack outside user memory"
    );
    // nothing changed: the thread count is as it was (a later spawn still works)
    let r = ok(
        sys::thread_spawn(me, &good),
        "a valid spawn after the errors",
    )?;
    sys::object_wait(r.a1 as u64, sig::TERMINATED, sys::clock_get() + 1000);
    sys::handle_close(r.a1 as u64);
    Ok(())
}

fn abi_nr_spawn() -> usize {
    rt::abi::syscall::THREAD_SPAWN
}

/// Stacks come back: after many spawn-and-join rounds (some detached) the root job's page use is where it began.
fn thread_spawn_gives_every_stack_back() -> Outcome {
    let root = root_job()?;
    let me = me()?;
    let base = job_use(root, 2)?.0;
    for round in 0..200u32 {
        let rec = spawn_rec(spawn_probe);
        let r = ok(sys::thread_spawn(me, &rec), "thread_spawn")?;
        let w = sys::object_wait(r.a1 as u64, sig::TERMINATED, sys::clock_get() + 500);
        ensure!(w.status == 0, "a thread ended (round {round})");
        sys::handle_close(r.a1 as u64);
    }
    for _ in 0..50 {
        let rec = rt::SpawnArgs {
            flags: rt::abi::spawn::DETACHED,
            ..spawn_rec(spawn_probe)
        };
        ok(sys::thread_spawn(me, &rec), "a detached thread")?;
    }
    job_use_settles(root, 2, base, 1000)?;
    sys::handle_close(root);
    Ok(())
}

extern "C" fn spawn_wait_event(arg: u64, _tid: u64) -> ! {
    worker_wait_event(arg as usize)
}

/// The arena holds a bounded number of the biggest stacks; when they end, the space is reused.
fn thread_spawn_arena_is_bounded_and_reused() -> Outcome {
    let me = me()?;
    let ev = event()?;
    let big = rt::SpawnArgs {
        arg: ev,
        stack_bytes: 64 * 1024 * 1024,
        ..spawn_rec(spawn_wait_event)
    };
    let mut ts: Vec<u64> = Vec::new();
    loop {
        let r = sys::thread_spawn(me, &big);
        if r.status != 0 {
            ensure_eq!(st(r), Some(S::NoMemory), "a full arena is NO_MEMORY");
            break;
        }
        ts.push(r.a1 as u64);
        ensure!(ts.len() <= 512, "more stacks than the arena can hold");
    }
    ensure!(ts.len() >= 400, "{} 64 MiB stacks fit the arena", ts.len());
    ensure_eq!(
        st(sys::event_signal(ev, 0, 1 << 24)),
        Some(S::Ok),
        "release them"
    );
    for &t in &ts {
        let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + SLOW_MS);
        ensure_eq!(w.status, 0, "a thread ended");
        sys::handle_close(t);
    }
    ensure_eq!(st(sys::event_signal(ev, 1 << 24, 0)), Some(S::Ok), "clear");
    let r = ok(
        sys::thread_spawn(me, &big),
        "after they ended there is room again",
    )?;
    sys::event_signal(ev, 0, 1 << 24);
    sys::object_wait(r.a1 as u64, sig::TERMINATED, sys::clock_get() + 2000);
    sys::handle_close(r.a1 as u64);
    sys::handle_close(ev);
    Ok(())
}

/// Two thousand threads alive at once, each on its own kernel-provisioned stack.
fn two_thousand_threads_live() -> Outcome {
    let me = me()?;
    let ev = event()?;
    let rec = rt::SpawnArgs {
        arg: ev,
        ..spawn_rec(spawn_wait_event)
    };
    let started = sys::clock_get();
    let mut ts: Vec<u64> = Vec::new();
    for _ in 0..2000 {
        let r = sys::thread_spawn(me, &rec);
        ensure!(
            r.status == 0,
            "thread_spawn: status {} after {} threads",
            r.status,
            ts.len()
        );
        ts.push(r.a1 as u64);
    }
    ensure_eq!(
        st(sys::event_signal(ev, 0, 1 << 24)),
        Some(S::Ok),
        "wake them all"
    );
    let spawned = sys::clock_get() - started;
    for (i, &t) in ts.iter().enumerate() {
        let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + SLOW_MS);
        ensure!(
            w.status == 0,
            "a thread ended: status {} for thread {i} (spawned in {spawned} ms, {} ms in)",
            w.status,
            sys::clock_get() - started
        );
        sys::handle_close(t);
    }
    sys::handle_close(ev);
    Ok(())
}

/// Spawning and joining ten thousand times leaks nothing: page use, and (at the end of the suite) kernel threads,
/// objects and frames, are where they began.
fn ten_thousand_threads_leak_nothing() -> Outcome {
    let root = root_job()?;
    let me = me()?;
    let base = job_use(root, 2)?.0;
    let rec = spawn_rec(spawn_probe);
    for round in 0..10_000u32 {
        let r = sys::thread_spawn(me, &rec);
        ensure!(
            r.status == 0,
            "thread_spawn: status {} in round {round}",
            r.status
        );
        let w = sys::object_wait(r.a1 as u64, sig::TERMINATED, sys::clock_get() + 500);
        ensure_eq!(w.status, 0, "a thread ended");
        sys::handle_close(r.a1 as u64);
        if round % 2000 == 1999 {
            // (the test takes minutes on a slow machine and says nothing otherwise: the harness's no-output limit is 180 s)
            println!("ten_thousand_threads_leak_nothing: {} rounds", round + 1);
        }
    }
    job_use_settles(root, 2, base, 1000)?;
    sys::handle_close(root);
    Ok(())
}

/// Slice T: creating and ending threads over and over leaks nothing (the thread slots, the objects and the
/// kernel stacks are all recycled; the end-of-suite checks see that no kernel thread or object is left).
fn thousands_of_threads_come_and_go() -> Outcome {
    // one stack, mapped once and reused: each thread has ended before the next starts
    let top = own_stack()?;
    let me = me()?;
    let entry = worker_set as *const () as usize as u64;
    for round in 0..2000u32 {
        let r = sys::thread_on_stack(me, entry, top, u64::from(round));
        ensure!(
            r.status == 0,
            "thread_create: status {} in round {round}",
            r.status
        );
        let t = r.a1 as u64;
        let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 500);
        ensure_eq!(w.status, 0, "a thread ended");
        sys::handle_close(t);
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
    let v = ok(sys::vmo_create_in(1, j), "a VMO")?.a1 as u64;
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
    .a1 as u64;
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
    .a1 as u64;
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
        ok(sys::socket_create(), "a socket in the root job").map(|r| (r.a1 as u64, r.a2 as u64))?;
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

fn job_create_sees_the_credit_of_a_job_closed_just_before() -> Outcome {
    // (found by the S4.5c review: a dead child kept counting against its parent's limit on jobs
    // until some unrelated charging call settled the credits)
    let root = root_job()?;
    let top = ok(
        sys::job_create(root, &[u64::MAX, u64::MAX, u64::MAX, 1, u64::MAX, u64::MAX]),
        "top",
    )?
    .a1 as u64;
    for round in 0..5 {
        let c = job(top, u64::MAX, u64::MAX, u64::MAX).map_err(|mut d| {
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!(" in round {round}"));
            d
        })?;
        let v = ok(sys::vmo_create_in(1, c), "a VMO in the child")?.a1 as u64;
        sys::handle_close(v);
        sys::handle_close(c);
    }
    job_use_settles(top, 3, 0, 500)?;
    sys::handle_close(top);
    sys::handle_close(root);
    Ok(())
}

// --- Benchmarks (slice B, SCHED-SMP-SPEC 6.1) ------------------------------------------------------------------

/// The `time` counter (its rate is `debug_stat(105)`, from the device tree: 10 MHz on QEMU `virt`).
fn cycles() -> u64 {
    sys::debug_stat(6)
}

/// Report one benchmark as a structured line: ns per operation (smaller is better).
fn bench_line(name: &str, cycles: u64, ops: u64) -> u64 {
    let ns = (u128::from(cycles) * 1_000_000_000
        / u128::from(sys::debug_stat(105).max(1))
        / u128::from(ops.max(1))) as u64;
    println!("##BENCH {name} {ns} ns");
    ns
}

static BENCH_ITERS: AtomicU32 = AtomicU32::new(0);
// (handle values carry a generation in their high bits: too wide to pack into a thread argument)
static BENCH_A: AtomicU64 = AtomicU64::new(0);
static BENCH_B: AtomicU64 = AtomicU64::new(0);

extern "C" fn bench_exit(_: usize) -> ! {
    sys::thread_exit(0)
}

/// Create, run an empty body, exit and join threads, `n` times: ns per thread.
fn bench_thread_create_join(n: u32) -> Result<u64, Detail> {
    let top = own_stack()?;
    let me = me()?;
    let entry = bench_exit as *const () as usize as u64;
    let t0 = cycles();
    for round in 0..n {
        let r = sys::thread_on_stack(me, entry, top, u64::from(round));
        ensure!(
            r.status == 0,
            "thread_create: status {} in round {round}",
            r.status
        );
        let t = r.a1 as u64;
        let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
        ensure_eq!(w.status, 0, "a thread ended");
        sys::handle_close(t);
    }
    Ok(bench_line(
        "thread_create_join",
        cycles() - t0,
        u64::from(n),
    ))
}

/// Drain a port with 64 ready bindings in one `port_wait_many`, `n` times: ns per call (AB4).
fn bench_port_drain_64(n: u32) -> Result<u64, Detail> {
    let p = port()?;
    let m = 1u32 << 24;
    for key in 1..=64u64 {
        let e = event()?;
        ensure_eq!(st(sys::port_bind(p, e, m, key)), Some(S::Ok), "bind");
        sys::event_signal(e, 0, m);
    }
    let mut buf = [rt::PortPacket::default(); 64];
    let t0 = cycles();
    for _ in 0..n {
        let r = sys::port_wait_many(p, &mut buf, 0);
        ensure_eq!((r.status, r.a1), (0, 64), "a full batch");
    }
    Ok(bench_line("port_drain_64", cycles() - t0, u64::from(n)))
}

/// The partner of the event ping-pong: wait for A, clear it, raise B; `BENCH_ITERS` times.
extern "C" fn bench_event_partner(_: usize) -> ! {
    let (a, b) = (
        BENCH_A.load(Ordering::SeqCst),
        BENCH_B.load(Ordering::SeqCst),
    );
    for _ in 0..BENCH_ITERS.load(Ordering::SeqCst) {
        sys::object_wait(a, 1 << 24, u64::MAX);
        sys::event_signal(a, 1 << 24, 0);
        sys::event_signal(b, 0, 1 << 24);
    }
    sys::thread_exit(0)
}

/// Two threads handing a baton through two events: ns per round trip (two wakes).
fn bench_wake_latency_event(n: u32) -> Result<u64, Detail> {
    let (a, b) = (event()?, event()?);
    BENCH_ITERS.store(n, Ordering::SeqCst);
    BENCH_A.store(a, Ordering::SeqCst);
    BENCH_B.store(b, Ordering::SeqCst);
    let t = new_thread(bench_event_partner, 0)?;
    nap_ticks(2); // the partner reaches its wait
    let t0 = cycles();
    for _ in 0..n {
        sys::event_signal(a, 0, 1 << 24);
        sys::object_wait(b, 1 << 24, u64::MAX);
        sys::event_signal(b, 1 << 24, 0);
    }
    let ns = bench_line("wake_latency_event", cycles() - t0, u64::from(n));
    sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
    for h in [t, a, b] {
        sys::handle_close(h);
    }
    Ok(ns)
}

/// The server of the call benchmark: echo every request (the first four bytes are the transaction id).
extern "C" fn bench_call_server(arg: usize) -> ! {
    let ch = arg as u64;
    let mut buf = [0u8; 16];
    loop {
        let w = sys::object_wait(ch, sig::READABLE | sig::PEER_CLOSED, u64::MAX);
        if w.status != 0 {
            break;
        }
        let r = sys::channel_read(ch, buf.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
        if r.status != 0 {
            break;
        }
        sys::channel_write(ch, buf.as_ptr(), r.a1, core::ptr::null(), 0);
    }
    sys::thread_exit(0)
}

extern "C" fn bench_bystander(arg: usize) -> ! {
    sys::object_wait(arg as u64, 1 << 25, u64::MAX); // never raised until the end
    sys::thread_exit(0)
}

/// `channel_call` round trips to a server thread, with `bystanders` threads blocked on an unrelated event.
fn bench_channel_call(name: &str, n: u32, bystanders: u32) -> Result<u64, Detail> {
    let (mine, theirs) = channel()?;
    let idle = event()?;
    let mut by = Vec::new();
    for _ in 0..bystanders {
        by.push(new_thread(bench_bystander, idle as usize)?);
    }
    let server = new_thread(bench_call_server, theirs as usize)?;
    nap_ticks(3);
    let mut reply = [0u8; 16];
    let t0 = cycles();
    for i in 0..n {
        let mut req = [0u8; 8];
        req[..4].copy_from_slice(&(i + 1).to_le_bytes());
        let args = CallArgs {
            req_bytes: req.as_ptr() as u64,
            req_handles: 0,
            req_nbytes: 8,
            req_nhandles: 0,
            rep_bytes: reply.as_mut_ptr() as u64,
            rep_handles: 0,
            rep_max_bytes: 16,
            rep_max_handles: 0,
            deadline: u64::MAX,
        };
        let r = sys::channel_call(mine, &args);
        ensure!(r.status == 0, "channel_call: status {} at {i}", r.status);
    }
    let ns = bench_line(name, cycles() - t0, u64::from(n));
    sys::handle_close(mine); // ends the server
    sys::object_wait(server, sig::TERMINATED, sys::clock_get() + 5000);
    sys::event_signal(idle, 0, 1 << 25); // releases the bystanders
    for t in by {
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
        sys::handle_close(t);
    }
    for h in [server, idle] {
        sys::handle_close(h);
    }
    Ok(ns)
}

// --- pair_scaling (SCHED-SMP-SPEC 6.1) -------------------------------------------------------------------------------
//
// N independent pairs of threads, all released at once, each pair doing a fixed number of round trips: the wall time from the release
// to the last exit, over the operations of all pairs, is the reported cost (`pair_scaling_<kind>_n<N>`, ns per round trip). The scaling
// of 5.2 is the cost at N = 1 over the cost at N: how many times more round trips a second N pairs get through (`bench_compare.py`
// prints it). On a boot with fewer harts than N it shows how little the pairs share, which is the point of running it at both counts.

const PS_MAX: usize = 4;
static PS_GO: AtomicU32 = AtomicU32::new(0);
static PS_ROUNDS: AtomicU32 = AtomicU32::new(0);
static PS_WORD: [AtomicU32; PS_MAX] = [const { AtomicU32::new(0) }; PS_MAX];
static PS_CHAN: [AtomicU64; PS_MAX] = [const { AtomicU64::new(0) }; PS_MAX];

/// Hold a thread until main sets `PS_GO`, so that every pair starts together.
fn ps_gate() {
    while PS_GO.load(Ordering::SeqCst) == 0 {
        sys::futex_wait(PS_GO.as_ptr(), 0, u64::MAX);
    }
}

/// Wait until `word` holds `want` (a futex wait with the value re-checked).
fn ps_await(word: &AtomicU32, want: u32) {
    loop {
        let v = word.load(Ordering::SeqCst);
        if v == want {
            return;
        }
        sys::futex_wait(word.as_ptr(), v, u64::MAX);
    }
}

/// One half of a futex ping-pong: `arg` is `pair * 2 + side`; side 0 raises the word to 1 and waits for 0, side 1 the reverse.
extern "C" fn ps_futex_side(arg: usize) -> ! {
    let (word, side) = (&PS_WORD[arg / 2], (arg % 2) as u32);
    ps_gate();
    for _ in 0..PS_ROUNDS.load(Ordering::SeqCst) {
        ps_await(word, side);
        word.store(1 - side, Ordering::SeqCst);
        sys::futex_wake(word.as_ptr(), 1);
    }
    sys::thread_exit(0)
}

/// A client: `PS_ROUNDS` `channel_call`s on the pair's channel.
extern "C" fn ps_call_client(arg: usize) -> ! {
    let ch = PS_CHAN[arg].load(Ordering::SeqCst);
    let mut reply = [0u8; 16];
    ps_gate();
    for i in 0..PS_ROUNDS.load(Ordering::SeqCst) {
        let mut req = [0u8; 8];
        req[..4].copy_from_slice(&(i + 1).to_le_bytes());
        let args = CallArgs {
            req_bytes: req.as_ptr() as u64,
            req_handles: 0,
            req_nbytes: 8,
            req_nhandles: 0,
            rep_bytes: reply.as_mut_ptr() as u64,
            rep_handles: 0,
            rep_max_bytes: 16,
            rep_max_handles: 0,
            deadline: u64::MAX,
        };
        if sys::channel_call(ch, &args).status != 0 {
            sys::thread_exit(1);
        }
    }
    sys::thread_exit(0)
}

/// Run `pairs` pairs of the given kind for `rounds` round trips each; report and return ns per round trip over all pairs.
fn bench_pairs(kind: &str, pairs: usize, rounds: u32) -> Result<u64, Detail> {
    PS_GO.store(0, Ordering::SeqCst);
    PS_ROUNDS.store(rounds, Ordering::SeqCst);
    let mut threads = Vec::new(); // (handle to wait for, handles to close afterwards)
    let mut closing = Vec::new();
    for p in 0..pairs {
        if kind == "futex" {
            PS_WORD[p].store(0, Ordering::SeqCst);
            threads.push(new_thread(ps_futex_side, p * 2)?);
            threads.push(new_thread(ps_futex_side, p * 2 + 1)?);
        } else {
            let (mine, theirs) = channel()?;
            PS_CHAN[p].store(mine, Ordering::SeqCst);
            closing.push(mine);
            let server = new_thread(bench_call_server, theirs as usize)?;
            threads.push(new_thread(ps_call_client, p)?);
            closing.push(server);
        }
    }
    nap_ticks(3); // everyone is at the gate
    let t0 = cycles();
    PS_GO.store(1, Ordering::SeqCst);
    sys::futex_wake(PS_GO.as_ptr(), u32::MAX);
    for t in &threads {
        let w = sys::object_wait(*t, sig::TERMINATED, sys::clock_get() + 120_000);
        ensure_eq!(w.status, 0, "a pair thread ended");
    }
    let c = cycles() - t0;
    let name = alloc::format!("pair_scaling_{kind}_n{pairs}");
    let ns = bench_line(&name, c, u64::from(rounds) * pairs as u64);
    for t in threads {
        sys::handle_close(t);
    }
    for h in closing {
        sys::handle_close(h); // the channel ends make the servers exit; the server handles are closed with them
    }
    Ok(ns)
}

fn bench_pair_scaling(k: u32) -> Result<u64, Detail> {
    let mut last = 0;
    for pairs in [1, 2, 4] {
        last = bench_pairs("futex", pairs, 500 * k)?;
        last += bench_pairs("channel_call", pairs, 250 * k)?;
    }
    Ok(last)
}

/// The benchmarks (a tenth of the iterations unless the kernel command line says `bench=full`): they run on every
/// boot so they cannot rot, and `make os-bench` runs them in full and compares with `bench/baseline.tsv`.
fn benchmarks() -> Outcome {
    let full = sys::debug_stat(7) != 0;
    let k = if full { 10 } else { 1 };
    let t = bench_thread_create_join(100 * k)?;
    let w = bench_wake_latency_event(200 * k)?;
    let c0 = bench_channel_call("channel_call_rt", 1000 * k, 0)?;
    let c100 = bench_channel_call("channel_call_rt_100_bystanders", 1000 * k, 100)?;
    let d = bench_port_drain_64(1000 * k)?;
    let ps = bench_pair_scaling(k)?;
    // sanity only (the numbers are for the baseline): every operation took some time, and was not absurd
    for (what, ns) in [
        ("thread", t),
        ("wake", w),
        ("call", c0),
        ("call with bystanders", c100),
        ("port drain", d),
        ("pair scaling", ps),
    ] {
        ensure!(
            ns > 0 && ns < 1_000_000_000,
            "{what}: {ns} ns per operation"
        );
    }
    Ok(())
}

// --- Instruction counts (KT5, KERNEL-V1-SPEC 8) ---------------------------------------------------------------------
//
// Wall-time benchmarks move with the host. The guest's own retired-instruction counter does not: under `-icount shift=0` at one hart
// the count of a path is deterministic, so a change that makes a syscall path cost more instructions shows as a number. The kernel opens
// `instret` to user mode (`scounteren`) in test builds only.

/// The retired-instruction counter of this hart (user and kernel instructions both count).
fn instret() -> u64 {
    let v: u64;
    // SAFETY: `rdinstret` reads a CSR the kernel enabled for user mode in test builds; it touches no memory.
    unsafe { core::arch::asm!("rdinstret {}", out(reg) v, options(nomem, nostack)) };
    v
}

/// Report instructions per operation: `##BENCH count_<name> <n> instr` (the harness keeps it like any benchmark line).
fn count_line(name: &str, instructions: u64, ops: u64) -> u64 {
    let per = instructions / ops.max(1);
    println!("##BENCH count_{name} {per} instr");
    per
}

/// Run `f` three times to warm the paths, then `n` times between two `instret` reads.
fn counted(name: &str, n: u64, mut f: impl FnMut() -> Result<(), Detail>) -> Result<u64, Detail> {
    for _ in 0..3 {
        f()?;
    }
    let t0 = instret();
    for _ in 0..n {
        f()?;
    }
    Ok(count_line(name, instret() - t0, n))
}

fn count_channel_call(n: u64) -> Result<u64, Detail> {
    let (mine, theirs) = channel()?;
    let server = new_thread(bench_call_server, theirs as usize)?;
    nap_ticks(3);
    let mut reply = [0u8; 16];
    let mut txid = 0u32;
    let per = counted("channel_call_round_trip", n, || {
        txid += 1;
        let mut req = [0u8; 8];
        req[..4].copy_from_slice(&txid.to_le_bytes());
        let args = CallArgs {
            req_bytes: req.as_ptr() as u64,
            req_handles: 0,
            req_nbytes: 8,
            req_nhandles: 0,
            rep_bytes: reply.as_mut_ptr() as u64,
            rep_handles: 0,
            rep_max_bytes: 16,
            rep_max_handles: 0,
            deadline: u64::MAX,
        };
        let r = sys::channel_call(mine, &args);
        ensure!(r.status == 0, "channel_call: status {}", r.status);
        Ok(())
    })?;
    sys::handle_close(mine); // ends the server
    sys::object_wait(server, sig::TERMINATED, sys::clock_get() + 5000);
    sys::handle_close(server);
    Ok(per)
}

fn instruction_counts_report() -> Outcome {
    let n: u64 = if sys::debug_stat(7) != 0 { 200 } else { 5 };
    // handle_duplicate + handle_close
    let ev = event()?;
    counted("handle_dup_close", n, || {
        let d = ok(sys::handle_duplicate(ev, Rights::SAME.0), "dup")?.a1 as u64;
        ok(sys::handle_close(d), "close")?;
        Ok(())
    })?;
    // channel_write + channel_read of 64 bytes
    let (a, b) = channel()?;
    let msg = [7u8; 64];
    let mut got = [0u8; 64];
    counted("channel_write_read_64", n, || {
        ok(
            sys::channel_write(a, msg.as_ptr(), 64, core::ptr::null(), 0),
            "write",
        )?;
        ok(
            sys::channel_read(b, got.as_mut_ptr(), 64, core::ptr::null_mut(), 0),
            "read",
        )?;
        Ok(())
    })?;
    // event_signal + object_wait (the condition already holds: the wait returns at once) + the clearing signal
    counted("event_signal_wait", n, || {
        ok(sys::event_signal(ev, 0, 1 << 24), "signal")?;
        ok(sys::object_wait(ev, 1 << 24, 0), "wait")?;
        ok(sys::event_signal(ev, 1 << 24, 0), "clear")?;
        Ok(())
    })?;
    // a page fault on a fresh page: touch the next page of a mapped VMO each time
    let pages = n + 8;
    let v = vmo(pages)?;
    let base = NEXT_STACK.fetch_add(0x100_0000, Ordering::SeqCst);
    ok(
        sys::process_map(me()?, v, base, 0, pages * 4096, PF_R | PF_W),
        "map the touched pages",
    )?;
    let mut next = 0u64;
    counted("page_fault_fresh_page", n, || {
        // SAFETY: the page is inside the region just mapped read-write; the write commits it through the fault path.
        unsafe { core::ptr::write_volatile((base + next * 4096) as *mut u8, 1) };
        next += 1;
        Ok(())
    })?;
    // thread_create + exit + join
    let top = own_stack()?;
    let me = me()?;
    let entry = bench_exit as *const () as usize as u64;
    counted("thread_create_join", n, || {
        let t = ok(sys::thread_on_stack(me, entry, top, 0), "thread_on_stack")?.a1 as u64;
        let w = sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 5000);
        ensure_eq!(w.status, 0, "a thread ended");
        sys::handle_close(t);
        Ok(())
    })?;
    // futex_wake with no waiter
    let word = AtomicU32::new(0);
    counted("futex_wake_no_waiter", n, || {
        ok(sys::futex_wake(word.as_ptr(), 1), "futex_wake")?;
        Ok(())
    })?;
    count_channel_call(n)?;
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
        (
            "a_process_holds_twenty_thousand_handles",
            a_process_holds_twenty_thousand_handles,
        ),
        ("event_syscalls", event_syscalls),
        ("channel_round_trip", channel_round_trip),
        ("channel_sends_handles", channel_sends_handles),
        ("channel_syscall_errors", channel_syscall_errors),
        (
            "big_messages_are_copied_outside_the_object_lock",
            big_messages_are_copied_outside_the_object_lock,
        ),
        (
            "system_suspend_is_gated_and_not_supported_without_the_firmware",
            system_suspend_is_gated_and_not_supported_without_the_firmware,
        ),
        (
            "hart_calls_need_the_resource_and_take_harts_offline_and_back",
            hart_calls_need_the_resource_and_take_harts_offline_and_back,
        ),
        (
            "system_set_placement_needs_the_resource_and_changes_the_policy",
            system_set_placement_needs_the_resource_and_changes_the_policy,
        ),
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
        ("clock_get_is_nanoseconds", clock_get_is_nanoseconds),
        ("sleep_never_wakes_early", sleep_never_wakes_early),
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
        (
            "programs_are_placed_at_random",
            programs_are_placed_at_random,
        ),
        (
            "the_system_library_is_in_every_process",
            the_system_library_is_in_every_process,
        ),
        (
            "debug_calls_need_a_debug_resource",
            debug_calls_need_a_debug_resource,
        ),
        ("child_faults_are_contained", child_faults_are_contained),
        ("parent_and_child_talk", parent_and_child_talk),
        ("kill_a_spinning_child", kill_a_spinning_child),
        (
            "process_suspend_stops_a_spinner_and_resume_restarts_it",
            process_suspend_stops_a_spinner_and_resume_restarts_it,
        ),
        (
            "process_suspend_stops_every_thread_on_every_hart",
            process_suspend_stops_every_thread_on_every_hart,
        ),
        (
            "a_stopped_process_with_parked_threads_is_killed_without_a_resume",
            a_stopped_process_with_parked_threads_is_killed_without_a_resume,
        ),
        ("a_process_can_stop_itself", a_process_can_stop_itself),
        (
            "a_stopped_process_whose_wait_ends_stays_parked_until_resumed",
            a_stopped_process_whose_wait_ends_stays_parked_until_resumed,
        ),
        (
            "a_stopped_process_can_be_killed_and_suspend_needs_the_kill_right",
            a_stopped_process_can_be_killed_and_suspend_needs_the_kill_right,
        ),
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
        ("caller_stack_argument_checks", caller_stack_argument_checks),
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
            "kill_on_close_holds_after_the_child_took_a_self_handle",
            kill_on_close_holds_after_the_child_took_a_self_handle,
        ),
        (
            "closing_handles_early_and_kill_on_close",
            closing_handles_early_and_kill_on_close,
        ),
        (
            "a_thread_made_by_the_parent_faults_or_exits",
            a_thread_made_by_the_parent_faults_or_exits,
        ),
        (
            "fp_state_is_private_to_each_thread_and_process",
            fp_state_is_private_to_each_thread_and_process,
        ),
        (
            "vector_state_is_private_to_each_thread_and_process",
            vector_state_is_private_to_each_thread_and_process,
        ),
        (
            "wake_baseline_bystanders_and_counters",
            wake_baseline_bystanders_and_counters,
        ),
        (
            "sending_a_waited_handle_ends_a_siblings_wait",
            sending_a_waited_handle_ends_a_siblings_wait,
        ),
        (
            "port_waiters_are_not_woken_by_unrelated_syscalls",
            port_waiters_are_not_woken_by_unrelated_syscalls,
        ),
        (
            "futex_sleepers_are_not_woken_by_unrelated_syscalls",
            futex_sleepers_are_not_woken_by_unrelated_syscalls,
        ),
        ("socket_syscalls", socket_syscalls),
        ("socket_end_of_stream", socket_end_of_stream),
        ("socket_half_close", socket_half_close),
        (
            "device_caps_need_the_resource",
            device_caps_need_the_resource,
        ),
        (
            "port_oneshot_rearm_and_many_bindings",
            port_oneshot_rearm_and_many_bindings,
        ),
        ("benchmarks_run_and_report", benchmarks),
        ("instruction_counts_report", instruction_counts_report),
        ("blk_round_trip", blk_round_trip),
        ("blk_driver_restart", blk_driver_restart),
        (
            "c_transcript_matches_the_host",
            c_transcript_matches_the_host,
        ),
        (
            "lua_script_matches_the_host_build",
            lua_script_matches_the_host_build,
        ),
        ("futex_argument_errors", futex_argument_errors),
        ("several_waiters_on_one_port_are_woken_in_turn", several_waiters_on_one_port_are_woken_in_turn),
        ("a_binding_ends_with_the_handle_it_was_made_through", a_binding_ends_with_the_handle_it_was_made_through),
        ("a_call_lends_its_priority_to_the_server", a_call_lends_its_priority_to_the_server),
        ("pi_futex_bounds_inversion", pi_futex_bounds_inversion),
        ("pi_futex_owner_death_poisons", pi_futex_owner_death_poisons),
        ("pi_futex_deadlock_is_refused", pi_futex_deadlock_is_refused),
        ("pi_futex_argument_errors", pi_futex_argument_errors),
        ("futex_times_out_not_early", futex_times_out_not_early),
        ("futex_wake_counts_and_fifo", futex_wake_counts_and_fifo),
        ("kill_a_futex_waiter", kill_a_futex_waiter),
        ("mutex_excludes_across_harts", mutex_excludes_across_harts),
        ("condvar_producer_consumer", condvar_producer_consumer),
        ("once_and_semaphore", once_and_semaphore),
        (
            "basic_golden_corpus_on_the_target",
            basic_golden_corpus_on_the_target,
        ),
        (
            "devmgr_starts_blkd_for_the_disk",
            devmgr_starts_blkd_for_the_disk,
        ),
        (
            "a_child_has_no_device_authority",
            a_child_has_no_device_authority,
        ),
        ("mmio_reads_the_virtio_magic", mmio_reads_the_virtio_magic),
        (
            "interrupt_object_fires_and_masks",
            interrupt_object_fires_and_masks,
        ),
        (
            "device_interrupts_arrive_where_asked",
            device_interrupts_arrive_where_asked,
        ),
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
            "port_wait_many_takes_a_batch_in_order",
            port_wait_many_takes_a_batch_in_order,
        ),
        (
            "port_wait_many_leaves_everything_on_a_bad_buffer",
            port_wait_many_leaves_everything_on_a_bad_buffer,
        ),
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
            "backstops_follow_the_ram_of_the_machine",
            backstops_follow_the_ram_of_the_machine,
        ),
        (
            "reserved_abi_space_is_refused",
            reserved_abi_space_is_refused,
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
            "fetch_the_conformance_image",
            fetch_the_conformance_image,
        ),
        ("abi_conformance_passes", abi_conformance_passes),
        ("fetch_the_fuzzer_image", fetch_the_fuzzer_image),
        ("the_guest_fuzzer_leaves_nothing_behind", the_guest_fuzzer_leaves_nothing_behind),
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
        ("memfsd_rename", memfsd_rename),
        (
            "memfsd_paths_cannot_escape_a_directory_handle",
            memfsd_paths_cannot_escape_a_directory_handle,
        ),
        ("memfsd_rights_only_shrink", memfsd_rights_only_shrink),
        ("memfsd_protocol_violations", memfsd_protocol_violations),
        (
            "registry_publish_open_unavailable_and_views",
            registry_publish_open_unavailable_and_views,
        ),
        (
            "memfsd_accounts_confine_a_client",
            memfsd_accounts_confine_a_client,
        ),
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
            "cpu_time_is_counted_per_thread_and_process",
            cpu_time_is_counted_per_thread_and_process,
        ),
        (
            "a_runaway_rt_thread_does_not_hang_the_system",
            a_runaway_rt_thread_does_not_hang_the_system,
        ),
        (
            "job_info_counts_the_cpu_time_its_threads_run",
            job_info_counts_the_cpu_time_its_threads_run,
        ),
        (
            "a_real_time_thread_stays_responsive_while_hogs_run_and_a_normal_one_does_not",
            a_real_time_thread_stays_responsive_while_hogs_run_and_a_normal_one_does_not,
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
            "process_protect_and_range_unmap_from_user_mode",
            process_protect_and_range_unmap_from_user_mode,
        ),
        (
            "a_write_to_a_protected_page_ends_the_process",
            a_write_to_a_protected_page_ends_the_process,
        ),
        (
            "vmo_decommit_and_resize_from_user_mode",
            vmo_decommit_and_resize_from_user_mode,
        ),
        (
            "copy_on_write_children_from_user_mode",
            copy_on_write_children_from_user_mode,
        ),
        (
            "thread_suspend_stops_a_thread_and_its_registers_can_be_read_and_written",
            thread_suspend_stops_a_thread_and_its_registers_can_be_read_and_written,
        ),
        (
            "a_stopped_thread_can_be_sent_to_a_handler_and_back",
            a_stopped_thread_can_be_sent_to_a_handler_and_back,
        ),
        (
            "a_thread_started_suspended_runs_only_after_resume_with_registers_written_first",
            a_thread_started_suspended_runs_only_after_resume_with_registers_written_first,
        ),
        (
            "a_thread_started_suspended_and_resumed_at_once_runs",
            a_thread_started_suspended_and_resumed_at_once_runs,
        ),
        (
            "a_handler_thread_can_make_a_faulting_store_succeed_and_retry",
            a_handler_thread_can_make_a_faulting_store_succeed_and_retry,
        ),
        (
            "a_parent_handler_can_kill_a_child_that_took_an_illegal_instruction",
            a_parent_handler_can_kill_a_child_that_took_an_illegal_instruction,
        ),
        (
            "next_reaches_the_job_channel_and_closing_the_handle_is_next",
            next_reaches_the_job_channel_and_closing_the_handle_is_next,
        ),
        (
            "a_process_killed_with_a_pending_exception_leaks_nothing_and_a_closed_channel_is_skipped",
            a_process_killed_with_a_pending_exception_leaks_nothing_and_a_closed_channel_is_skipped,
        ),
        (
            "a_pager_serves_a_vmo_to_a_reader_in_another_process",
            a_pager_serves_a_vmo_to_a_reader_in_another_process,
        ),
        (
            "killing_a_reader_waiting_for_a_pager_leaks_nothing",
            killing_a_reader_waiting_for_a_pager_leaks_nothing,
        ),
        (
            "a_pager_that_dies_fails_the_reader_with_a_pager_fault",
            a_pager_that_dies_fails_the_reader_with_a_pager_fault,
        ),
        (
            "a_pager_failure_is_a_pager_exception_and_a_supplied_page_lets_it_retry",
            a_pager_failure_is_a_pager_exception_and_a_supplied_page_lets_it_retry,
        ),
        (
            "pager_calls_check_their_arguments",
            pager_calls_check_their_arguments,
        ),
        (
            "suspend_and_state_calls_check_their_arguments_and_rights",
            suspend_and_state_calls_check_their_arguments_and_rights,
        ),
        (
            "fp_and_vector_state_round_trip_through_thread_state",
            fp_and_vector_state_round_trip_through_thread_state,
        ),
        (
            "touching_past_a_shrunk_vmo_ends_the_process_cleanly",
            touching_past_a_shrunk_vmo_ends_the_process_cleanly,
        ),
        (
            "job_memory_limits_stop_a_hog_and_are_credited_back",
            job_memory_limits_stop_a_hog_and_are_credited_back,
        ),
        (
            "a_confined_program_cannot_hold_much_uncharged_kernel_memory",
            a_confined_program_cannot_hold_much_uncharged_kernel_memory,
        ),
        (
            "vmo_commit_charges_up_front_and_foreign_maps_need_it",
            vmo_commit_charges_up_front_and_foreign_maps_need_it,
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
            "dma_runs_are_allocated_and_freed_repeatedly",
            dma_runs_are_allocated_and_freed_repeatedly,
        ),
        (
            "a_refused_process_start_changes_nothing_and_can_be_retried",
            a_refused_process_start_changes_nothing_and_can_be_retried,
        ),
        (
            "thread_interrupt_argument_errors",
            thread_interrupt_argument_errors,
        ),
        (
            "thread_interrupt_ends_an_object_wait",
            thread_interrupt_ends_an_object_wait,
        ),
        (
            "thread_interrupt_ends_a_port_wait",
            thread_interrupt_ends_a_port_wait,
        ),
        (
            "thread_interrupt_ends_a_futex_wait",
            thread_interrupt_ends_a_futex_wait,
        ),
        (
            "thread_interrupt_ends_a_channel_call",
            thread_interrupt_ends_a_channel_call,
        ),
        (
            "thread_interrupt_before_the_wait_is_not_lost",
            thread_interrupt_before_the_wait_is_not_lost,
        ),
        (
            "thread_interrupt_is_not_taken_by_a_poll",
            thread_interrupt_is_not_taken_by_a_poll,
        ),
        (
            "thread_interrupt_waits_for_a_wait_that_would_block",
            thread_interrupt_waits_for_a_wait_that_would_block,
        ),
        (
            "thread_interrupt_polls_leave_the_flag_on_every_wait",
            thread_interrupt_polls_leave_the_flag_on_every_wait,
        ),
        (
            "thread_interrupt_is_taken_once",
            thread_interrupt_is_taken_once,
        ),
        (
            "thread_priority_is_bounded_by_the_job",
            thread_priority_is_bounded_by_the_job,
        ),
        (
            "page_tables_are_charged_to_the_job",
            page_tables_are_charged_to_the_job,
        ),
        (
            "thread_stacks_are_charged_to_the_job_and_credited_back",
            thread_stacks_are_charged_to_the_job_and_credited_back,
        ),
        (
            "stream_buffers_are_charged_to_the_client",
            stream_proto::stream_buffers_are_charged_to_the_client,
        ),
        (
            "a_client_at_quota_stalls_only_itself",
            stream_proto::a_client_at_quota_stalls_only_itself,
        ),
        (
            "one_socket_two_one_shot_bindings",
            stream_proto::one_socket_two_one_shot_bindings,
        ),
        ("stream_pipe_numbers", stream_proto::stream_pipe_numbers),
        (
            "bystander_cost_of_sleeping_bindings",
            stream_proto::bystander_cost_of_sleeping_bindings,
        ),
        (
            "a_dead_client_frees_everything",
            stream_proto::a_dead_client_frees_everything,
        ),
        (
            "a_kill_storm_on_a_stream_loses_no_range",
            stream_proto::a_kill_storm_on_a_stream_loses_no_range,
        ),
        (
            "a_jobs_region_quota_bounds_its_processes",
            a_jobs_region_quota_bounds_its_processes,
        ),
        (
            "kernel_allocation_failure_sweep",
            kernel_allocation_failure_sweep,
        ),
        (
            "a_bad_stream_handle_is_refused",
            stream_proto::a_bad_stream_handle_is_refused,
        ),
        (
            "stream_budgets_with_the_launcher_defaults",
            stream_proto::stream_budgets_with_the_launcher_defaults,
        ),
        (
            "random_service_gives_seeded_distinct_bytes",
            net_tests::random_service_gives_seeded_distinct_bytes,
        ),
        (
            "c_sockets_over_the_stack",
            net_tests::c_sockets_over_the_stack,
        ),
        (
            "net_fetch_from_a_host_server",
            net_tests::net_fetch_from_a_host_server,
        ),
        ("net_httpd_for_the_host", net_tests::net_httpd_for_the_host),
        (
            "net_streams_over_loopback",
            net_tests::net_streams_over_loopback,
        ),
        (
            "net_stream_quotas_and_bad_handles",
            net_tests::net_stream_quotas_and_bad_handles,
        ),
        (
            "net_tcp_echo_for_the_host",
            net_tests::net_tcp_echo_for_the_host,
        ),
        (
            "net_tcp_to_the_host_server",
            net_tests::net_tcp_to_the_host_server,
        ),
        ("net_tcp_churn", net_tests::net_tcp_churn),
        (
            "net_streams_survive_a_stack_restart",
            net_tests::net_streams_survive_a_stack_restart,
        ),
        (
            "net_ping_round_trip_is_measured",
            net_tests::net_ping_round_trip_is_measured,
        ),
        (
            "net_faults_are_survived",
            net_tests::net_faults_are_survived,
        ),
        (
            "net_clients_are_confined",
            net_tests::net_clients_are_confined,
        ),
        ("net_children_restart", net_tests::net_children_restart),
        (
            "net_dhcp_configures_the_interface",
            net_tests::net_dhcp_configures_the_interface,
        ),
        (
            "net_udp_echo_through_hostfwd",
            net_tests::net_udp_echo_through_hostfwd,
        ),
        (
            "net_static_config_and_ping_the_gateway",
            net_tests::net_static_config_and_ping_the_gateway,
        ),
        (
            "net_driver_attaches_and_exchanges_a_frame",
            net_tests::net_driver_attaches_and_exchanges_a_frame,
        ),
        (
            "a_process_ending_with_spawned_threads_leaks_nothing",
            a_process_ending_with_spawned_threads_leaks_nothing,
        ),
        (
            "thread_spawn_starts_with_the_documented_state",
            thread_spawn_starts_with_the_documented_state,
        ),
        ("thread_spawn_error_cases", thread_spawn_error_cases),
        (
            "thread_spawn_gives_every_stack_back",
            thread_spawn_gives_every_stack_back,
        ),
        (
            "thread_spawn_arena_is_bounded_and_reused",
            thread_spawn_arena_is_bounded_and_reused,
        ),
        ("two_thousand_threads_live", two_thousand_threads_live),
        (
            "ten_thousand_threads_leak_nothing",
            ten_thousand_threads_leak_nothing,
        ),
        (
            "thousands_of_threads_come_and_go",
            thousands_of_threads_come_and_go,
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
            "job_create_sees_the_credit_of_a_job_closed_just_before",
            job_create_sees_the_credit_of_a_job_closed_just_before,
        ),
        (
            "fsd_keeps_files_across_a_restart",
            fsd_keeps_files_across_a_restart,
        ),
        (
            "clock_service_reports_the_rtc",
            clock_service_reports_the_rtc,
        ),
        (
            "volmgr_serves_volumes_by_the_table",
            volmgr_serves_volumes_by_the_table,
        ),
        ("devd_serves_null_and_zero", devd_serves_null_and_zero),
        (
            "symlinks_are_followed_by_the_client_not_the_server",
            symlinks_are_followed_by_the_client_not_the_server,
        ),
        (
            "threads_left_behind_when_init_exits",
            threads_left_behind_when_init_exits,
        ),
    ];
    // Each test's guest time (1 ms ticks) goes out as a plain `tt:` line, which the boot script's
    // profiler (`OS_PROFILE`) reads; the verdict harness ignores it (slice TS step 0).
    let timed = |name: &str, test: fn() -> Outcome| -> bool {
        let t0 = sys::clock_get();
        let r = test();
        let ticks = sys::clock_get() - t0;
        let ok = report(name, r);
        println!("tt: {name} {ticks}");
        ok
    };
    // Behind the lossy relay (`net=lossy`, S9-SPEC Q30) only the tests that move bulk TCP data in a way slirp copes with run: the others assume a clean link
    let lossy = sys::debug_stat(9) != 0;
    const LOSSY_TESTS: [&str; 3] = [
        "net_fetch_from_a_host_server",
        "net_httpd_for_the_host",
        "net_tcp_echo_for_the_host",
    ];
    // The smoke tier runs the suite as two parallel boots (`shard=a`, `shard=b`; TESTING.md, OD121): shard b is the network tests
    // (`net_*` and the C sockets test) and the three heaviest stress tests. `scripts/os_boot_test.py` has the same rule and checks each shard ran only its own.
    let shard = sys::debug_stat(13);
    // (the stress test that starts children needs the program image the first of these fetches: shard b runs that one too)
    const PREREQ_OF_B: &str = "fetch_the_program_image";
    /// The names of the Kernel tests, one per line (comments start with `#`); the harness reads the same file.
    const KERNEL_TESTS: &str = include_str!("../../ktests.txt");
    let in_b = |n: &str| {
        n.starts_with("net_")
            || [
                "c_sockets_over_the_stack",
                "ten_thousand_threads_leak_nothing",
                "a_thread_hog_cannot_exhaust_the_kernel",
                "cycles_do_not_grow_memory",
            ]
            .contains(&n)
    };
    for (name, test) in tests {
        if lossy && !LOSSY_TESTS.contains(name) {
            continue;
        }
        if (shard == 1 && in_b(name)) || (shard == 2 && !in_b(name) && *name != PREREQ_OF_B) {
            continue;
        }
        // the kernel-only boot (`shard=k`, KT1): the Kernel tests, which start no server and need no disk or network
        if shard >= 3 {
            let listed = KERNEL_TESTS
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| l.split(' ').collect::<Vec<_>>())
                .find(|w| w[0] == *name);
            match listed {
                None => continue,
                Some(w) if shard == 4 && w.get(1) == Some(&"slow") => continue,
                Some(_) => {}
            }
        }
        if !timed(name, *test) {
            failed += 1;
        }
        // the upstream Lua test files (S7e) run right after our own Lua test
        if *name == "lua_script_matches_the_host_build" {
            for (n, t) in LUA_UPSTREAM {
                if !timed(n, *t) {
                    failed += 1;
                }
            }
        }
    }
    println!(
        "init: {} of {} user tests failed",
        failed,
        tests.len() + LUA_UPSTREAM.len()
    );
    failed
}
