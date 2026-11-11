//! The first user program: the boot-time user-level test suite (S3b). It exercises every
//! syscall and every error in SYSCALLS.md through the real ABI, in user mode, and reports
//! each test as a `##TEST` line. Expected values are worked out from SYSCALLS.md, not
//! read back from the kernel.
#![no_std]
#![no_main]
// Test failures carry a fixed 200-byte message buffer by design (there is no allocator).
#![allow(clippy::result_large_err)]

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
    for h in [0u32, 1, BAD, u32::MAX, 0x0001_0000] {
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
    // The OD39 loop: make two channels hold each other and drop every handle. 17,000 rounds
    // make 68,000 objects, more than the kernel's cap of 65,536: it must keep sweeping or
    // this ends in NoMemory.
    for i in 0..17_000u32 {
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
        (
            "user_cannot_forge_kernel_lines",
            user_cannot_forge_kernel_lines,
        ),
        ("rights_cannot_be_widened", rights_cannot_be_widened),
        ("stack_grows_lazily", stack_grows_lazily),
        ("syscall_layer_bounds", syscall_layer_bounds),
        ("cycles_do_not_grow_memory", cycles_do_not_grow_memory),
    ];
    for (name, test) in tests {
        if !report(name, test()) {
            failed += 1;
        }
    }
    println!("init: {} of {} user tests failed", failed, tests.len());
    failed
}
