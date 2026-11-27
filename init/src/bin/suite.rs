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

/// Print the mappings of `process` that overlap `[lo, hi)`, one line each (`process_map_info`): what a refused fixed-address map ran into (T272).
fn print_mappings(process: u64, lo: u64, hi: u64) {
    let mut at = lo;
    for _ in 0..4096 {
        let mut rec = [0u64; 6];
        let r = sys::process_map_info(process, at, rec.as_mut_ptr());
        if r.status != 0 || rec[1] == 0 || rec[0] >= hi {
            break;
        }
        if rec[0] + rec[1] > lo {
            println!(
                "  mapping {:#x}..{:#x} ({} pages) perms {:#b} vmo {} offset {:#x}",
                rec[0],
                rec[0] + rec[1],
                rec[1] / 4096,
                rec[2],
                rec[4],
                rec[5]
            );
        }
        if r.a1 == 0 {
            break;
        }
        at = r.a1 as u64;
    }
}

/// Map `length` bytes of `vmo` (from offset 0) into `process` where the kernel chooses (ANYWHERE) and return the address. A test that needs room asks
/// for it this way and does not name an address in the mapping arena, where the program's image, the system library and every ANYWHERE mapping land
/// at random (HD2): a fixed 2 GiB there was refused now and then as an overlap (T272). A refusal prints the process's mappings and names the length.
fn map_anywhere(
    process: u64,
    vmo: u64,
    length: u64,
    flags: u64,
    what: &str,
) -> Result<u64, Detail> {
    let r = sys::process_map_anywhere(process, vmo, 0, length, flags);
    if r.status != 0 {
        println!(
            "{what}: process_map ANYWHERE of {length:#x} bytes refused, status {}; the mappings:",
            r.status
        );
        print_mappings(process, rt::abi::user::BASE, rt::abi::user::TOP);
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!("{what}: ANYWHERE of {length:#x} bytes: status {}", r.status),
        );
        return Err(d);
    }
    Ok(r.a1 as u64)
}

/// Where the suite's fixed-address mappings go: the 32 GiB from the end of the stack arena to the top of the user half. Nothing the kernel places
/// lands there (ANYWHERE mappings, a program's image and the system library go in the mapping arena, stacks in the stack arena), so a fixed address
/// here is free unless a test of this suite mapped it (T272). The thread stacks of `own_stack` grow up from its start (`next_fixed`), the network
/// tests' rings sit at `FIXED_RINGS` (`net_tests.rs`), and the page below the top is never mapped.
const FIXED_WINDOW: u64 = rt::abi::user::STACK_ARENA_END;
/// Where the network tests' rings go: 28 GiB into the fixed window; `next_fixed` hands out the room below.
const FIXED_RINGS: u64 = FIXED_WINDOW + 0x7_0000_0000;

/// The next `bytes` of the fixed window for a test's own mapping (thread stacks and the like): a failure, not an overlap, when the room below
/// `FIXED_RINGS` is used up.
fn next_fixed(bytes: u64) -> Result<u64, Detail> {
    let base = NEXT_STACK.fetch_add(bytes, Ordering::SeqCst);
    ensure!(
        base + bytes <= FIXED_RINGS,
        "the fixed window is used up ({base:#x} + {bytes:#x})"
    );
    Ok(base)
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
        // (the page below the top of the fixed window, which no test maps; in the mapping arena the image or the library may be, T272)
        bad(rt::abi::user::TOP as usize - 4096, 1),
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
    // (the clock is read before the request: the helper's three ticks start when it reads it, so the close can never come
    // earlier than that, however long this thread is held off after the send. Read after the send, a stall of this thread
    // longer than three ticks between the send and the read made the close look instant: T267, seen at two harts on the
    // IOMMU machine, where `channel_write` was seen to return 80 to 120 ms late)
    let watch = PauseWatch::start(5_000); // (every hart keeps a deadline near, so a pause of the host is seen wherever it lands)
    let t0 = sys::clock_get();
    let t0_ns = sys::clock_ns();
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
    let r = sys::object_wait(BOOTSTRAP, sig::PEER_CLOSED, sys::clock_get() + 200);
    ensure_eq!(
        (r.status, r.a1 as u32 & sig::PEER_CLOSED),
        (0, sig::PEER_CLOSED),
        "a blocked wait is woken by PEER_CLOSED"
    );
    let waited = sys::clock_get() - t0;
    // (T284: the window cannot be taken again, since the helper's close is once only; a pause of the host can only lengthen it, so the
    // upper bound allows what the pause detector saw inside it, and the lower bound, the check, is as it was)
    let pause_ms = host_pause_since(t0_ns, 4_000).map_or(0, |(h, us)| {
        pause_line("wait_is_woken_by_the_peer_closing", h, us);
        us.div_ceil(1000)
    });
    drop(watch);
    ensure!(
        // (the lower bound is the check: three ticks from the helper's read of the request, which follows `t0`, less one for
        // the millisecond clock's rounding; the upper one only catches a lost wake-up and is loose because a loaded CI runner
        // can delay the helper, or this thread, by many ticks)
        within(waited, 2, 60 + pause_ms),
        "the close came after about 3 ticks (waited {waited}; a host pause of {pause_ms} ms inside the wait)"
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
        st(sys::vmo_create(abi_top_pages() + 1)),
        Some(S::InvalidArgs),
        "above the user half"
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
    // (T265: a deadline that a pause of the host covered says nothing about the teardown: a stop of the emulator longer than the wait
    // times it out however promptly the kernel works, as the host-stall injector shows. A window the pause detector says a pause covered
    // is waited again, at most three times in a row; a timeout with no pause in its window fails, and says what the detector saw.)
    // (every hart keeps a deadline near while it waits, so that a stop of the emulator makes two harts late wherever it lands, which is
    // the detector's rule for a pause: without it an idle hart has no deadline to be late for. A wait that ends at once costs nothing.)
    let mut covered = 0;
    loop {
        let since = sys::clock_ns();
        // (a timing test that already keeps the heartbeat on keeps it: this wait neither replaces its deadline nor turns it off)
        let _watch = if WATCH_ON.load(Ordering::SeqCst) {
            None
        } else {
            PauseWatch::start(ticks.saturating_add(100))
        };
        let r = sys::object_wait(proc, sig::TERMINATED, sys::clock_get() + ticks);
        if r.status == S::TimedOut.code() {
            if let Some((h, us)) = host_pause_since(since, 2_000) {
                pause_line("wait_ended", h, us);
                covered += 1;
                if covered < 3 {
                    continue;
                }
            }
            let mut d = Detail::new();
            let _ = core::fmt::Write::write_fmt(
                &mut d,
                format_args!(
                    "the process ended: got {} (timed out after {ticks} ms; {covered} windows before it covered by a host pause; the \
                     harts were stopped {} us in this one, summed; signals {:#x})",
                    r.status,
                    host_paused_us_since(since),
                    sys::object_signals(proc).a1
                ),
            );
            return Err(d);
        }
        ensure_eq!(r.status, 0, "the process ended");
        ensure!(r.a1 as u32 & sig::TERMINATED != 0, "TERMINATED is set");
        return Ok(());
    }
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

/// T207 PR 4 (B1): at 4 GiB of RAM, a 2 GiB VMO is committed whole (in slices), mapped and touched, unmapped, decommitted,
/// committed again, and last held by a process that is killed. Each operation is chunked, so no big-lock hold grows past 20 ms (B1's smoke
/// bound, applied to the hold), which a kernel built with `latency-trace` reports (`debug_stat` 14, in 100 ns, and starts its window
/// again; interrupts stay off through a call that nothing else wants the hart for, so the hold is what chunking bounds). Without 3 GiB of RAM it does nothing (the machine cannot hold the VMO).
fn a_two_gib_vmo_committed_whole_stays_within_the_latency_bound() -> Outcome {
    const GIB2: u64 = 2 << 30;
    // (slices of 64 pages, not `MAX_COMMIT_PAGES`: a commit zeroes its pages with the big lock held, and 1024 of them is the milliseconds B1 names as its
    // known worst case; this test is about what T207 chunked)
    const SLICE: u64 = 64 * 4096;
    if sys::debug_stat(102) * 4096 < 3 << 30 {
        return Ok(());
    }
    let bound_ns = 20_000_000u64;
    let at_start = sys::debug_stat(14); // (the boot's own figures are not this test's: the window starts again here)
    let check = |what: &str| -> Outcome {
        let at = sys::debug_stat(14); // (the window ends here: the log says where its longest span began)
        ensure!(
            at == 0 || at * 100 <= bound_ns,
            "the longest big-lock hold after {what} is {} us (the boot's span was {} us)",
            at / 10,
            at_start / 10
        );
        Ok(())
    };
    let me = me()?;
    let root = root_job()?;
    let base = job_use(root, 2)?.0;
    let v = ok(sys::vmo_create_flags(GIB2 / 4096, 0, 0), "a 2 GiB VMO")?.a1 as u64;
    let commit_all = |v: u64| -> Outcome {
        let mut off = 0;
        while off < GIB2 {
            ensure_eq!(
                st(sys::vmo_commit(v, off, SLICE)),
                Some(S::Ok),
                "commit slice at {off}"
            );
            off += SLICE;
        }
        Ok(())
    };
    commit_all(v)?;
    check("the commit")?;
    ensure_eq!(
        job_use(root, 2)?.0 - base,
        GIB2 / 4096,
        "all of it is charged"
    );
    // (where the kernel chooses: a fixed address in the mapping arena may hold the image or the library, T272)
    let at = map_anywhere(me, v, GIB2, PF_R | PF_W, "map all of it")?;
    for i in 0..GIB2 / (1 << 20) {
        // SAFETY: at..at + 2 GiB is mapped read-write in this process.
        unsafe { core::ptr::write_volatile((at + i * (1 << 20)) as *mut u64, i + 1) };
    }
    ensure_eq!(st(sys::process_unmap(me, at)), Some(S::Ok), "unmap all");
    check("the unmap")?;
    ensure_eq!(
        st(sys::vmo_decommit(v, 0, GIB2)),
        Some(S::Ok),
        "decommit all"
    );
    check("the decommit")?;
    ensure_eq!(job_use(root, 2)?.0, base, "the pages are all back");
    // the last holder dies: a process that never started maps it, and is killed
    commit_all(v)?;
    let child = proc_create()?;
    map_anywhere(child, v, GIB2, PF_R | PF_W, "map it into a child")?;
    sys::handle_close(v);
    ensure_eq!(
        st(sys::process_kill(child, 1)),
        Some(S::Ok),
        "kill the holder"
    );
    sys::handle_close(child);
    job_use_settles(root, 2, base, 20_000)?;
    check("the kill")
}

/// T206: closing the head of a chain of channels (each queues both ends of the next) is paid in slices with an interrupts window between them, and a
/// timer tick taken in a window is recorded and handled at the window's own switch point (`irq::window`, `debug_stat(214)`): a few rounds of the close
/// see at least one nested interrupt, every object is freed, and the stack budget check at the end of the boot (KT13) covers the depth.
fn a_close_that_frees_a_chain_takes_interrupts_between_its_slices() -> Outcome {
    const LINKS: usize = 1200;
    static STOP: AtomicU32 = AtomicU32::new(0);
    static STARTED: AtomicU32 = AtomicU32::new(0);
    /// A hart's worth of work to compete with the closing thread: a scheduler tick (a slice end) is armed only on a hart where an equal thread waits, and
    /// the closing thread's own hart must have one (a thread alone on a hart has no timer event at all, L13).
    extern "C" fn spinner(_arg: usize) -> ! {
        STARTED.fetch_add(1, Ordering::SeqCst);
        while STOP.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        sys::thread_exit(0)
    }
    /// Stop the spinners and wait for them to end.
    fn disband(spinners: &mut Vec<u64>) {
        STOP.store(1, Ordering::SeqCst);
        for t in spinners.drain(..) {
            sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 20_000);
            sys::handle_close(t);
        }
    }
    /// Start `harts + 1` spinners, one at a time, each running before the next is made. Placement reads the harts' running-priority hints
    /// (SCHED-SMP-SPEC 4.4), and a hart that has been given a thread but has not switched to it yet still reads as running nothing, so
    /// spinners made back to back can all land on the other harts and leave this thread alone on its own: its hart then arms no tick, and
    /// no interrupt can fall in a window (the 4-hart failure of CI run 46018655715, every round of it). With each one running first, the
    /// first `harts - 1` take the idle harts and the rest go to this thread's hart (rule 5: no hart runs anything below them, so the
    /// creator's), where they compete with it.
    fn muster(harts: usize, spinners: &mut Vec<u64>) -> Outcome {
        STOP.store(0, Ordering::SeqCst);
        STARTED.store(0, Ordering::SeqCst);
        for n in 1..=harts as u32 + 1 {
            match new_thread(spinner, 0) {
                Ok(t) => spinners.push(t),
                Err(e) => {
                    disband(spinners); // (the ones already running stop: they would hold their harts for the rest of the suite)
                    return Err(e);
                }
            }
            let until = sys::clock_get() + 5_000;
            while STARTED.load(Ordering::SeqCst) < n {
                if sys::clock_get() > until {
                    disband(spinners);
                    return Err(detail("a spinner did not start within 5 s"));
                }
                sys::thread_yield();
            }
        }
        Ok(())
    }
    /// One round: a chain of `LINKS` channels, each queuing both ends of the one before, and the close of its head. Returns the interrupts
    /// taken in windows and the windows made while the head was being closed. (The counters are the machine's: the other threads that run
    /// meanwhile are the spinners, which make no system call, and the idle threads, which make no window; what the close hands on to be freed
    /// is part of the close.)
    fn round() -> Result<(u64, u64), Detail> {
        let mut prev: Option<(u64, u64)> = None;
        let mut head = (0, 0);
        for _ in 0..LINKS {
            let (a, b) = channel()?;
            if let Some((pa, pb)) = prev {
                let hs = [pa, pb];
                ok(
                    sys::channel_write(a, core::ptr::null(), 0, hs.as_ptr(), 2),
                    "queue the previous pair",
                )?;
            }
            prev = Some((a, b));
            head = (a, b);
        }
        let (t0, n0) = (sys::debug_stat(214), sys::debug_stat(215));
        sys::handle_close(head.0);
        sys::handle_close(head.1);
        Ok((sys::debug_stat(214) - t0, sys::debug_stat(215) - n0))
    }
    let harts = job_use(root_job()?, 7)?.0 as usize;
    let mut spinners = Vec::new();
    muster(harts, &mut spinners)?;
    // (only the interrupts taken while a chain was being closed count: a tick in a window of the spinners' own teardown between rounds does not)
    let (mut rounds, mut alone, mut traps, mut windows) = (0, 0, 0, 0);
    while traps == 0 && rounds < 40 {
        let (w0, c0) = (sys::clock_ns(), sys::thread_cpu_ns(0).a1 as u64);
        match round() {
            Ok((t, n)) => {
                traps += t;
                windows += n;
            }
            Err(e) => {
                disband(&mut spinners);
                return Err(e);
            }
        }
        rounds += 1;
        // (the premise, checked: with an equal thread waiting on its hart this thread runs a half or a third of the round; nine tenths or
        // more of it means it was alone there after all, moved since by a pull, say, and the spinners are placed around it again)
        let (w1, c1) = (sys::clock_ns(), sys::thread_cpu_ns(0).a1 as u64);
        if c1.saturating_sub(c0) * 10 >= w1.saturating_sub(w0) * 9 {
            alone += 1;
            disband(&mut spinners);
            muster(harts, &mut spinners)?;
        }
    }
    if traps == 0 {
        sys::debug_stat(132); // (debug builds: every hart's scheduler state, where the spinners and this thread are)
    }
    disband(&mut spinners);
    ensure!(
        traps > 0,
        "no interrupt was taken inside a window in {rounds} rounds of closing a chain ({windows} windows were made while closing; in {alone} \
         rounds this thread was alone on its hart)"
    );
    // (everything is gone: a channel can be made again, and nothing of the chain is left to count against the budget)
    let (a, b) = channel()?;
    sys::handle_close(a);
    sys::handle_close(b);
    Ok(())
}

/// T207: while a chunked decommit is under way, a second decommit, a `vmo_set_size` and a `vmo_create_child` on the VMO are refused with SHOULD_WAIT
/// (seen from a thread on another hart; with one hart nothing runs beside the call and the test passes without looking).
fn a_decommit_under_way_refuses_the_calls_that_would_race_it() -> Outcome {
    // T207 (3a, 3d): a chunked decommit holds its VMO to the end, and the calls that would race its cursor are refused with SHOULD_WAIT.
    static RANGE_V: AtomicU64 = AtomicU64::new(0);
    static RANGE_MODE: AtomicU32 = AtomicU32::new(0);
    static RANGE_SEEN: AtomicU32 = AtomicU32::new(0);
    static RANGE_READY: AtomicU32 = AtomicU32::new(0);
    static RANGE_DONE: AtomicU32 = AtomicU32::new(0);

    /// The other hart's side: until the decommit is over, try the call of the round and record that it was refused.
    extern "C" fn range_prober(_arg: usize) -> ! {
        let v = RANGE_V.load(Ordering::SeqCst);
        let mode = RANGE_MODE.load(Ordering::SeqCst);
        RANGE_READY.store(1, Ordering::SeqCst);
        while RANGE_DONE.load(Ordering::SeqCst) == 0 {
            let r = match mode {
                // (the size it already has: nothing changes when the VMO is free)
                0 => sys::vmo_decommit(v, 0, 4096),
                1 => sys::vmo_set_size(v, 16384 * 4096),
                _ => sys::vmo_create_child(v, 0, 4096, 0, rt::abi::child_flags::PRIVATE),
            };
            match st(r) {
                Some(S::ShouldWait) => {
                    RANGE_SEEN.store(1, Ordering::SeqCst);
                }
                Some(S::Ok) if mode == 2 => {
                    sys::handle_close(r.a1 as u64);
                }
                _ => {}
            }
        }
        sys::thread_exit(0)
    }

    let harts = job_use(root_job()?, 7)?.0;
    if harts < 2 {
        return Ok(()); // (nothing runs beside the call on one hart: the refusal cannot be seen)
    }
    let v = ok(
        sys::vmo_create_flags(16384, 0, rt::abi::vmo_flags::RESIZABLE),
        "a 64 MiB resizable VMO",
    )?
    .a1 as u64;
    RANGE_V.store(v, Ordering::SeqCst);
    // mode 0: a second `vmo_decommit`; 1: `vmo_set_size`; 2: `vmo_create_child`
    for mode in 0..3u32 {
        RANGE_MODE.store(mode, Ordering::SeqCst);
        RANGE_SEEN.store(0, Ordering::SeqCst);
        RANGE_READY.store(0, Ordering::SeqCst);
        RANGE_DONE.store(0, Ordering::SeqCst);
        let t = new_thread(range_prober, 0)?;
        while RANGE_READY.load(Ordering::SeqCst) == 0 {
            sys::thread_yield();
        }
        // (the call takes a while: 256 batches of 64 pages; a few tries make the overlap certain)
        for _ in 0..8 {
            if RANGE_SEEN.load(Ordering::SeqCst) != 0 {
                break;
            }
            // (the prober's own calls make the VMO busy for a moment, so the main side is refused now and then too: it asks again)
            let again = |f: &dyn Fn() -> Option<S>| {
                for _ in 0..100_000 {
                    match f() {
                        Some(S::ShouldWait) => sys::thread_yield(),
                        other => return other,
                    };
                }
                Some(S::ShouldWait)
            };
            for k in 0..16u64 {
                ensure_eq!(
                    again(&|| st(sys::vmo_commit(v, k * 1024 * 4096, 1024 * 4096))),
                    Some(S::Ok),
                    "commit the VMO"
                );
            }
            ensure_eq!(
                again(&|| st(sys::vmo_decommit(v, 0, 16384 * 4096))),
                Some(S::Ok),
                "decommit all of it"
            );
        }
        RANGE_DONE.store(1, Ordering::SeqCst);
        sys::object_wait(t, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(t);
        ensure_eq!(
            RANGE_SEEN.load(Ordering::SeqCst),
            1,
            "round {mode}: a call made during the decommit was refused with SHOULD_WAIT"
        );
    }
    ensure_eq!(
        sys::vmo_get_size(v).a1 as u64,
        16384 * 4096,
        "the size is untouched"
    );
    sys::handle_close(v);
    Ok(())
}

/// T207: a decommit inside the window a child inherits makes holes, and a hole's leaf is charged to the child's job: a job that cannot pay stops the
/// call with NO_MEMORY, nothing changed.
fn a_decommit_the_job_cannot_pay_the_holes_for_is_no_memory() -> Outcome {
    let root = root_job()?;
    let parent = ok(sys::vmo_create(64), "a parent of 64 pages")?.a1 as u64;
    ok(
        sys::vmo_write(parent, 0, b"p".as_ptr(), 1),
        "write the parent",
    )?;
    // (a job whose one page allows 512 kernel bytes cannot hold a leaf of 528)
    let tiny = job(root, 1, 1, 1)?;
    let child = ok(
        sys::vmo_create_child(parent, 0, 64 * 4096, tiny, rt::abi::child_flags::PRIVATE),
        "a PRIVATE child in the tiny job",
    )?
    .a1 as u64;
    let before = job_use(tiny, 8)?.0;
    ensure_eq!(
        st(sys::vmo_decommit(child, 0, 4096)),
        Some(S::NoMemory),
        "no room for the hole's leaf"
    );
    ensure_eq!(job_use(tiny, 8)?.0, before, "nothing was charged");
    let mut b = [0u8; 1];
    ok(sys::vmo_read(child, 0, b.as_mut_ptr(), 1), "read the child")?;
    ensure_eq!(b[0], b'p', "the child still shows the parent's page");
    for h in [child, parent, tiny] {
        sys::handle_close(h);
    }
    Ok(())
}

/// T207 PR 4 (MM6): there is no size cap below the user half. A 2 GiB VMO is created, mapped whole and touched one page per 2 MiB (1024 pages
/// committed, and a page table each), read back, then decommitted whole in one call; the job's page usage is back where it started, and a page
/// read afterwards is a fresh zero.
fn a_two_gib_sparse_vmo_is_mapped_touched_and_given_back() -> Outcome {
    const GIB2: u64 = 2 << 30;
    const STEP: u64 = 2 << 20;
    let me = me()?;
    let root = root_job()?;
    let before = job_use(root, 2)?.0;
    let v = ok(
        sys::vmo_create_flags(GIB2 / 4096, 0, rt::abi::vmo_flags::RESIZABLE),
        "a 2 GiB VMO",
    )?
    .a1 as u64;
    ensure_eq!(sys::vmo_get_size(v).a1 as u64, GIB2, "its size");
    // (where the kernel chooses: a fixed address in the mapping arena may hold the image or the library, T272)
    let base = map_anywhere(me, v, GIB2, PF_R | PF_W, "map all of it")?;
    let n = GIB2 / STEP;
    for i in 0..n {
        // SAFETY: base..base + 2 GiB is mapped read-write in this process.
        unsafe { core::ptr::write_volatile((base + i * STEP) as *mut u64, i + 1) };
    }
    // (the pages, and a page table each: they are charged to this process's job, and the frames to the VMO's)
    let touched = job_use(root, 2)?.0 - before;
    ensure!(touched >= n, "a page per touch is charged");
    for i in 0..n {
        // SAFETY: as above.
        let got = unsafe { core::ptr::read_volatile((base + i * STEP) as *const u64) };
        ensure_eq!(got, i + 1, "page {i} reads back");
    }
    ensure_eq!(
        st(sys::vmo_decommit(v, 0, GIB2)),
        Some(S::Ok),
        "decommit all of it"
    );
    ensure_eq!(
        touched - (job_use(root, 2)?.0 - before),
        n,
        "the frames are credited back (the tables stay while the mapping does)"
    );
    // SAFETY: as above (a decommitted page faults in again, as zeros).
    let got = unsafe { core::ptr::read_volatile((base + 5 * STEP) as *const u64) };
    ensure_eq!(got, 0, "a decommitted page reads zero");
    ensure_eq!(
        st(sys::vmo_set_size(v, STEP)),
        Some(S::Ok),
        "shrink to 2 MiB (the tail goes in batches)"
    );
    ensure_eq!(st(sys::process_unmap(me, base)), Some(S::Ok), "unmap");
    job_use_settles(root, 2, before + 1, 20_000)
        .or_else(|_| job_use_settles(root, 2, before, 20_000))?;
    ensure_eq!(
        st(sys::vmo_create_flags(abi_top_pages() + 1, 0, 0)),
        Some(S::InvalidArgs),
        "above the user half"
    );
    sys::handle_close(v);
    Ok(())
}

fn abi_top_pages() -> u64 {
    rt::abi::user::TOP / 4096
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

// (in the fixed window above the stack arena, where nothing the kernel places lands: T272)
static NEXT_STACK: AtomicU64 = AtomicU64::new(FIXED_WINDOW);
static COUNTER: AtomicU32 = AtomicU32::new(0);
static WOKEN_AT: AtomicU64 = AtomicU64::new(0);
static WOKEN_STATUS: AtomicU32 = AtomicU32::new(99);

fn me() -> Result<u64, Detail> {
    Ok(ok(sys::process_self(), "process_self")?.a1 as u64)
}

/// Map a fresh 4-page stack into our own address space; returns its top.
fn own_stack() -> Result<u64, Detail> {
    let base = next_fixed(0x10_0000)?;
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
/// Set by a sweep scenario that lost something the failing allocation must not lose (a wakeup).
static SWEEP_LOST: AtomicU64 = AtomicU64::new(0);

/// `debug_stat` for the allocation-failure sweep and its scenarios (which are plain functions and cannot capture a closure).
fn sweep_stat(which: u64, arg: u64) -> u64 {
    use rt::abi::syscall as nr;
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
}

/// KT9, the guest half: with the kernel built with `fault-alloc` (`make os-allocfail`), fail the Nth kernel heap allocation of each scenario, for N = 1, 2, ...
/// until the scenario completes with no failure injected. Every call must return (an error or success); an infallible allocation on the path is a kernel panic,
/// and the line printed before each run names the scenario and N. Without the feature the test passes at once.
fn kernel_allocation_failure_sweep() -> Outcome {
    let stat = sweep_stat;
    if stat(203, 0) == 0 {
        return Ok(());
    }
    let scenarios: [(&str, fn()); 11] = [
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
        ("snapshot child decommit", || {
            // (a child's decommit inside the window it inherits makes explicit holes, which are allocations of their own)
            sweep_stat(207, 1);
            let v = sys::vmo_create(4);
            if v.status == 0 {
                let src = v.a1 as u64;
                sys::vmo_commit(src, 0, 4 * 4096);
                sweep_stat(207, 0);
                let c = sys::vmo_create_child(src, 0, 4 * 4096, 0, rt::abi::child_flags::SNAPSHOT);
                sweep_stat(207, 1);
                let c = if c.status == 0 {
                    c
                } else {
                    sys::vmo_create_child(src, 0, 4 * 4096, 0, rt::abi::child_flags::SNAPSHOT)
                };
                if c.status == 0 {
                    sweep_stat(207, 0);
                    let d = sys::vmo_decommit(c.a1 as u64, 4096, 2 * 4096);
                    sweep_stat(207, 1);
                    if d.status != 0 {
                        sys::vmo_decommit(c.a1 as u64, 4096, 2 * 4096);
                    }
                    sys::handle_close(c.a1 as u64);
                }
                sys::handle_close(src);
            }
        }),
        ("job set sched", || {
            let root = sys::job_root();
            if root.status == 0 {
                let j = sys::job_create(root.a1 as u64, &[u64::MAX; 6]);
                if j.status == 0 {
                    let _ = sys::job_set_sched(j.a1 as u64, 10, 0, 0);
                    sys::handle_close(j.a1 as u64);
                }
                sys::handle_close(root.a1 as u64);
            }
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
            ensure_eq!(
                SWEEP_LOST.load(Ordering::Relaxed),
                0,
                "scenario {name} with the allocation {n} failing lost a wakeup"
            );
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
        ensure_eq!(
            SWEEP_LOST.load(Ordering::Relaxed),
            0,
            "scenario {name} with every allocation failing lost a wakeup"
        );
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
        // (T266, T197: 5 s, beyond any host pause; a timeout prints the scheduler state. The latency of a kill is T267's)
        wait_ended_or_say(p, &[], 5000)?;
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
    let at = FIXED_WINDOW; // (a fresh child maps only the library, in the mapping arena: this is free)
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
    let base = FIXED_WINDOW; // (free in a fresh child, T272)
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
    'outer: for pair in 0..20 * scale {
        if scale > 1 && pair % 8 == 0 {
            // (with a big RAM the budget is big and this takes minutes: the boot test judges progress by output)
            println!("queued handles: pair {pair} of {}", 20 * scale);
        }
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
///
/// Each seed runs once unmeasured, then is measured. The kernel's object tables keep the room they grew to (the VMO table is a slab of
/// 376-byte slots that doubles when it fills: from 256 slots to 512, with its free list, is 97,280 bytes, kept for the next VMOs and held by
/// nobody), and whether a sequence is the one that pushes a table past its size depends on how many objects the rest of the system has at that
/// moment. The first run brings the tables to what the sequence needs; the same sequence again must give back everything it took. A table
/// grows once and keeps its room, so a measured run that kept bytes is repeated, up to three in all: one that gives everything back passes,
/// and a leak, which takes its bytes on every run, fails all three (T277: the second run alone failed at two harts when the system's VMO
/// count sat just below a doubling).
fn the_guest_fuzzer_leaves_nothing_behind() -> Outcome {
    const SEEDS: u64 = 4;
    let root = root_job()?;
    for seed in 1..=SEEDS {
        guest_fuzzer_run(root, seed)?;
        let mut held = Vec::new();
        for _ in 0..3 {
            let h = guest_fuzzer_run(root, seed)?;
            held.push(h);
            if h <= 64 * 1024 {
                break;
            }
        }
        ensure!(
            held.last().is_some_and(|&h| h <= 64 * 1024),
            "seed {seed}: kernel heap bytes stayed behind in each of three runs: {held:?}"
        );
    }
    sys::handle_close(root);
    Ok(())
}

/// One run of the guest fuzzer with `seed`, in a job of its own below `root`; then the job is killed and its counters come back to zero. Returns
/// the kernel heap bytes the run kept.
fn guest_fuzzer_run(root: u64, seed: u64) -> Result<i64, Detail> {
    let j = job(root, 4, 12, 2000)?;
    let heap_before = sys::debug_stat(109);
    let (p, mine) = launch_image_in(KFUZZ_IMAGE.load(Ordering::Relaxed), seed as usize as u64, j)?;
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
    Ok(heap_before.wrapping_sub(sys::debug_stat(109)) as i64)
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

/// T207 PR 5 (MM6): a program that allocates and frees 64 MiB ends with its job's page usage near where it started (the heap gives an arena
/// back when nothing in it is live), and the memory is there to be made again. Run in this process, whose heap is the same `rt` heap.
fn the_heap_gives_memory_back_when_it_is_freed() -> Outcome {
    let root = root_job()?;
    // (one warm-up round, so that the first arenas and the spare exist before the baseline is read)
    {
        let _warm: Vec<u8> = vec![1u8; 1 << 20];
    }
    let before = job_use(root, 2)?.0;
    let mut peak = 0;
    for round in 0..2 {
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        for i in 0..64 {
            let mut b: Vec<u8> = Vec::new();
            ensure!(
                b.try_reserve_exact(1 << 20).is_ok(),
                "block {i} of round {round}"
            );
            b.resize(1 << 20, i as u8);
            blocks.push(b);
        }
        peak = job_use(root, 2)?.0 - before;
        ensure!(peak >= 64 * 256, "64 MiB held is charged ({peak} pages)");
        for (i, b) in blocks.iter().enumerate() {
            ensure!(
                b[0] == i as u8 && b[(1 << 20) - 1] == i as u8,
                "block {i} intact"
            );
        }
    }
    let after = job_use(root, 2)?.0;
    // (what stays is a spare arena of at most 1 MiB and the tables of what it holds)
    ensure!(
        after <= before + 300,
        "the job holds {} pages more than at the start, after 64 MiB were freed (peak {peak})",
        after as i64 - before as i64
    );
    Ok(())
}

/// T207 PR 5: in a child, memory made and freed three times over (the heap trims between), the cap is still a null allocation and not a fault.
fn runtime_heap_stops_at_its_cap_after_giving_memory_back() -> Outcome {
    let (p, in_w, out_r) = start_hello(&[b"hello", b"trim"])?;
    let _ = (out_r, in_w);
    wait_ended(p, 60_000)?;
    let blocks = sys::process_info(p).a1 as i64;
    ensure!(
        (64..=128).contains(&blocks),
        "the heap gave {blocks} MiB before refusing after three trims (cap 128 MiB; an exit code of 94 is an allocation that failed too soon)"
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

/// The block rig (IOMMU10): the suite as `blkd`'s supervisor, the way `devmgr` starts one. A `devmgr` of its own, and for each driver
/// a connection to it that claims the scratch disk's function (`ClaimPci`) for a new driver job under the rig's; `blkd` started in
/// that job with exactly the claim's handles and the claim as its argument. Closing the connection releases the claim: bus mastering
/// off, the device reset, the domain destroyed (IOMMU-SPEC 10.2). No disk on the bus (the no-disk boot): no rig.
struct BlkRig {
    res: u64,
    /// The boot image (the suite's shared handle: not closed here).
    bi: u64,
    /// The drivers' jobs are made under this one (a child of the root job).
    job: u64,
    /// The rig's own connection to its `devmgr`, its process and its job.
    mgr_ch: u64,
    mgr: u64,
    mj: u64,
}

/// One driver the rig started: its process, our end of its service channel, the connection that holds its claim and its job.
struct BlkDriver {
    p: u64,
    ch: u64,
    conn: u64,
    job: u64,
}

fn blk_rig() -> Result<Option<BlkRig>, Detail> {
    if !pci_disks_present()? {
        return Ok(None);
    }
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let bi = boot_image_vmo()?;
    let job = root_job().and_then(|root| {
        let j = job(root, u64::MAX, u64::MAX, u64::MAX);
        sys::handle_close(root);
        j
    });
    let job = match job {
        Ok(j) => j,
        Err(e) => {
            sys::handle_close(res);
            return Err(e);
        }
    };
    let (mgr_ch, mgr, mj) = match start_devmgr_in_job(res) {
        Ok(m) => m,
        Err(e) => {
            sys::handle_close(job);
            sys::handle_close(res);
            return Err(e);
        }
    };
    Ok(Some(BlkRig {
        res,
        bi,
        job,
        mgr_ch,
        mgr,
        mj,
    }))
}

/// A driver dropped on any path (a test that returned early) is killed and everything of it closed: its connection goes, so the
/// manager releases its claim and later tests find the disk free.
impl Drop for BlkDriver {
    fn drop(&mut self) {
        if self.p != 0 {
            let _ = sys::process_kill(self.p, 57);
        }
        for h in [self.p, self.ch, self.conn, self.job] {
            if h != 0 {
                sys::handle_close(h);
            }
        }
    }
}

/// A new connection to the manager behind `ch` (`Connect`): our end of it.
fn devmgr_connect(ch: u64, txid: u32) -> Result<u64, Detail> {
    use proto::devmgr as dm;
    let (c, theirs) = channel()?;
    let req = dm::encode_connect(txid);
    let w = sys::channel_write(ch, req.as_ptr(), req.len(), [theirs].as_ptr(), 1);
    if st(w) != Some(S::Ok) {
        sys::handle_close(c);
        sys::handle_close(theirs);
        ensure!(false, "send Connect: {:?}", st(w));
    }
    ensure_eq!(
        st(sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000)),
        Some(S::Ok),
        "the manager answers Connect"
    );
    let mut rb = [0u8; 16];
    let r = sys::channel_read(ch, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
    ensure_eq!(
        dm::decode_reply(&rb[..r.a1]),
        Some((txid, dm::Status::Ok)),
        "connected"
    );
    Ok(c)
}

/// Wait until `devmgr` (behind `ch`) lists the scratch disk's function as not claimed: the release is done (the manager is one
/// thread, so a list that says so was answered after it).
fn blk_unclaimed(ch: u64) -> Outcome {
    let t0 = sys::clock_get();
    let mut txid = 500;
    loop {
        txid += 1;
        if pci_list(ch, txid)?
            .iter()
            .any(|e| u64::from(e.rid) == PCI_BLK_DID && e.flags & proto::devmgr::PCI_CLAIMED == 0)
        {
            return Ok(());
        }
        ensure!(
            sys::clock_get() < t0 + 5000,
            "the claim was not released when its connection closed"
        );
        let _ = sys::object_wait(ch, 1 << 24, sys::clock_get() + 5);
    }
}

impl BlkRig {
    /// Claim the scratch disk for a new driver job and start `blkd` in it with the claim's handles.
    fn spawn(&self) -> Result<BlkDriver, Detail> {
        let (off, len) = image_entry(self.bi, b"bin/blkd")?;
        let image = rt::spawn::read_image(self.bi, off, len).ok_or_else(|| msg_at(line!()))?;
        let conn = devmgr_connect(self.mgr_ch, 1)?;
        let dj = match job(self.job, u64::MAX, u64::MAX, u64::MAX) {
            Ok(j) => j,
            Err(e) => {
                sys::handle_close(conn);
                return Err(e);
            }
        };
        let started = (|| {
            let send = ok(
                sys::handle_duplicate(dj, Rights::SAME.0),
                "dup the driver's job",
            )?
            .a1 as u64;
            let (s, claim, mut hs) = pci_claim(conn, 2, PCI_BLK_DID as u16, Some(send))?;
            let claimed = (|| {
                ensure_eq!(s, proto::devmgr::Status::Ok, "the scratch disk is claimed");
                let claim = claim.ok_or_else(|| msg_at(line!()))?;
                // what the driver is given holds no physical address and no control of its domain: the BAR and the two DMA VMOs
                // answer no address, and the domain handle maps and unmaps, nothing more
                for (&h, what) in hs[..4].iter().zip([
                    "the BAR",
                    "the domain",
                    "the queue memory",
                    "the data buffer",
                ]) {
                    ensure_eq!(
                        st(sys::vmo_phys_addr(h)),
                        Some(S::NotSupported),
                        "{what}: no physical address (IOMMU-SPEC 4.8)"
                    );
                }
                for (r, what) in [
                    (sys::iodomain_destroy(hs[1]), "destroy"),
                    (sys::iodomain_fault_read(hs[1]), "read faults"),
                    (sys::iodomain_grant_vector(hs[1], 0), "grant a vector"),
                    (
                        sys::handle_duplicate(hs[1], Rights::IO_MAP.0),
                        "duplicate it",
                    ),
                ] {
                    ensure_eq!(
                        st(r),
                        Some(S::AccessDenied),
                        "the driver's domain handle cannot {what}"
                    );
                }
                let mut roles = vec![
                    (Role::BlkMmio, 0),
                    (Role::DeviceDomain, 1),
                    (Role::BlkQueueDma, 2),
                    (Role::BlkDma, 3),
                ];
                for (k, irq) in claim.irqs.iter().enumerate() {
                    roles.push((
                        Role::device_irq(irq.what).ok_or_else(|| msg_at(line!()))?,
                        4 + k as u32,
                    ));
                }
                roles.push((Role::ServiceBlk, hs.len() as u32));
                let arg = proto::devmgr::encode_claim_arg(&claim).ok_or_else(|| msg_at(line!()))?;
                let s = Startup {
                    args: vec![b"blkd".to_vec(), arg],
                    roles,
                    ..Startup::default()
                };
                s.encode().map_err(|_| msg_at(line!()))
            })();
            let bytes = match claimed {
                Ok(b) => b,
                Err(e) => {
                    for h in hs {
                        sys::handle_close(h);
                    }
                    return Err(e);
                }
            };
            let (mine, theirs) = channel()?;
            hs.push(theirs);
            // (exactly these handles: a driver gets no debug resource; spawn consumes them whatever happens)
            match rt::spawn::spawn_in_exact(&image, &bytes, &hs, dj) {
                Ok(p) => Ok((p, mine)),
                Err(e) => {
                    sys::handle_close(mine);
                    let mut d = Detail::new();
                    let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn blkd: {e:?}"));
                    Err(d)
                }
            }
        })();
        match started {
            Ok((p, ch)) => Ok(BlkDriver {
                p,
                ch,
                conn,
                job: dj,
            }),
            Err(e) => {
                sys::handle_close(conn);
                sys::handle_close(dj);
                let _ = blk_unclaimed(self.mgr_ch);
                Err(e)
            }
        }
    }

    /// Kill driver `d` (if it still runs), close everything of it and release its claim; wait for the release.
    fn release(&self, d: BlkDriver) -> Outcome {
        let _ = sys::process_kill(d.p, 57);
        let ended = wait_ended(d.p, 2000);
        drop(d);
        ended?;
        blk_unclaimed(self.mgr_ch)
    }
}

impl Drop for BlkRig {
    fn drop(&mut self) {
        // the manager ends with its last connection, releasing any claim still held
        sys::handle_close(self.mgr_ch);
        let _ = wait_ended(self.mgr, 5000);
        for h in [self.mgr, self.mj, self.job, self.res] {
            sys::handle_close(h);
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
    sys::debug_write(b"s6: blkd exercised (interrupts)\n");
    let mut d = rig.spawn()?;
    let ch = d.ch;
    let (st, body) = blk_call(ch, 1, blk::Op::Info)?;
    ensure_eq!(st, blk::Status::Ok, "info");
    let info = blk::decode_info(&body).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(
        (info.sectors, info.sector_size, info.read_only, info.max_io),
        (16 * 1024 * 2, 512, false, 16),
        "a 16 MiB disk"
    );
    // the driver mapped its own two buffers into its domain: the queue page and the four data pages, charged to its job
    ensure_eq!(
        sys::job_info(d.job, 12).a1 as u64,
        5,
        "the driver's I/O-mapped pages (its queue page and its data buffer)"
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
    // a request is answered promptly, by interrupt (the driver has no polling mode: a dead interrupt path is its two-second TIMEOUT)
    let fired = sys::debug_stat(163);
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
    ensure!(
        sys::debug_stat(163) > fired,
        "the read completed by a vector's notice (debug_stat 163)"
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
    sys::handle_close(d.ch);
    d.ch = 0;
    wait_ended(d.p, 2000)?;
    let code = sys::process_info(d.p).a1 as i64;
    rig.release(d)?;
    ensure_eq!(code, 0, "the driver exits 0 when its client is gone");
    Ok(())
}

/// Send a 16-sector write of `data` at `sector` on `ch` and do not wait for its answer.
fn blk_send_write(ch: u64, txid: u32, sector: u64, data: &[u8]) -> Outcome {
    let req = blk::Request {
        txid,
        op: blk::Op::Write {
            sector,
            count: (data.len() / 512) as u32,
            data,
        },
    }
    .encode();
    ensure_eq!(
        st(sys::channel_write(
            ch,
            req.as_ptr(),
            req.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "queue a write"
    );
    Ok(())
}

/// Each of the `data.len() / 512` sectors from `sector` on `ch` holds zeros (the old contents) or `data`'s (the new): a write that
/// was in flight when its driver died happened completely, in part, or not at all.
fn blk_old_or_new(ch: u64, sector: u64, data: &[u8]) -> Outcome {
    let n = data.len() / 512;
    let (st, body) = blk_call(
        ch,
        5,
        blk::Op::Read {
            sector,
            count: n as u32,
        },
    )?;
    ensure_eq!(st, blk::Status::Ok, "read the in-flight sectors");
    for k in 0..n {
        let sec = &body[k * 512..(k + 1) * 512];
        ensure!(
            sec.iter().all(|&b| b == 0) || sec == &data[k * 512..(k + 1) * 512],
            "sector {} is neither old nor new",
            sector + k as u64
        );
    }
    Ok(())
}

/// The device-domain counters (`debug_stat` 159 and 160: live domains, I/O-mapped pages) back at `(domains, pages)`, waited for
/// briefly (a release is the manager's work).
fn domains_at_baseline((domains, pages): (u64, u64)) -> Outcome {
    let t0 = sys::clock_get();
    while (sys::debug_stat(159), sys::debug_stat(160)) != (domains, pages)
        && sys::clock_get() < t0 + 2000
    {
        sys::thread_yield();
    }
    ensure_eq!(
        sys::debug_stat(159),
        domains,
        "live domains (debug_stat 159) back at the baseline"
    );
    ensure_eq!(
        sys::debug_stat(160),
        pages,
        "I/O-mapped pages (debug_stat 160) back at the baseline"
    );
    Ok(())
}

/// IOMMU10 (IOMMU-SPEC 10.2): a driver killed with a write in flight. The supervisor releases its claim (bus mastering off, the device
/// reset, the domain destroyed): the domain, its I/O-mapped pages, its vectors and their interrupt identities all come back. A new
/// driver gets a new domain, maps its buffers afresh, and its first request completes by interrupt on its fresh vectors, promptly.
fn blk_driver_restart() -> Outcome {
    let Some(rig) = blk_rig()? else {
        return Ok(());
    };
    let doms = (sys::debug_stat(159), sys::debug_stat(160));
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let d = rig.spawn()?;
    let first = (|| {
        write_pattern(d.ch, 4000, 64)?;
        check_pattern(d.ch, 4000, 64)?;
        ensure_eq!(
            (sys::debug_stat(159), sys::debug_stat(160)),
            (doms.0 + 1, doms.1 + 5),
            "the driver's domain, with its queue page and data buffer mapped"
        );
        ensure_eq!(sys::debug_stat(146), live + 2, "its two vectors are live");
        Ok(())
    })();
    if first.is_err() {
        let _ = rig.release(d);
        return first;
    }
    // start a write and kill the driver before it can answer: the device may be mid-request
    let inflight: Vec<u8> = (0..16 * 512).map(|i| (i * 13 + 1) as u8).collect();
    blk_send_write(d.ch, 99, 5000, &inflight)?;
    ensure_eq!(
        st(sys::process_kill(d.p, 55)),
        Some(S::Ok),
        "kill the driver"
    );
    wait_ended(d.p, 2000)?;
    let w = sys::object_wait(d.ch, sig::PEER_CLOSED, sys::clock_get() + 1000);
    ensure_eq!(st(w), Some(S::Ok), "the client sees PEER_CLOSED");
    // the supervisor releases the claim: nothing of the old driver's device authority is left
    rig.release(d)?;
    ensure!(
        !blk_bus_master(rig.res)?,
        "bus mastering is off once the claim was released"
    );
    domains_at_baseline(doms)?;
    vectors_at_baseline(live, ids)?;
    // a new driver: a new domain, mapped afresh, and its first request completes by interrupt on its new vectors
    let d2 = rig.spawn()?;
    let second = (|| {
        let fired = sys::debug_stat(163);
        let t = sys::clock_get();
        let (st0, s0) = blk_call(
            d2.ch,
            1,
            blk::Op::Read {
                sector: 0,
                count: 1,
            },
        )?;
        ensure_eq!(st0, blk::Status::Ok, "the new driver's first request");
        ensure!(
            sys::clock_get() - t < 200,
            "the first request took {} ticks",
            sys::clock_get() - t
        );
        ensure!(
            sys::debug_stat(163) > fired,
            "the first request completed by a vector's notice (debug_stat 163)"
        );
        ensure!(
            (0..512).all(|i| s0[i] == ((i * 7 + 3) & 0xff) as u8),
            "sector 0 holds the harness pattern"
        );
        ensure_eq!(
            (
                sys::debug_stat(159),
                sys::debug_stat(160),
                sys::debug_stat(146)
            ),
            (doms.0 + 1, doms.1 + 5, live + 2),
            "one domain, its five pages and its two vectors again"
        );
        check_pattern(d2.ch, 4000, 64)?;
        blk_old_or_new(d2.ch, 5000, &inflight)?;
        // it also takes writes: a second generation of data over the first
        write_pattern(d2.ch, 4000, 16)?;
        check_pattern(d2.ch, 4000, 16)
    })();
    let released = rig.release(d2);
    second?;
    released?;
    domains_at_baseline(doms)?;
    vectors_at_baseline(live, ids)?;
    // both drivers' processes, threads and memory are gone: the rig's job is back to nothing
    job_use_settles(rig.job, 0, 0, 2000)?;
    job_use_settles(rig.job, 2, 0, 2000)?;
    Ok(())
}

/// IOMMU10 (IOMMU-SPEC 10.2): `devmgr`'s own driver for disk 0 is killed mid-write (its job, from above: the drivers' job the test
/// gave the manager). The manager sees the driver end and releases the claim at once: bus mastering off, the device reset, the
/// domain destroyed, so the live-domain count drops and the I/O-mapped pages return. The next `OpenBlock` makes a new domain, the
/// new `blkd` maps afresh, and the next write lands. Then the rig variant: a driver (the suite) closes every handle to its data
/// buffer while the buffer is mapped in its domain and a write is in flight, and the write still lands (the domain keeps the frames).
fn blk_driver_killed_mid_write() -> Outcome {
    use proto::devmgr::Status as D;
    if !pci_disks_present()? {
        return Ok(());
    }
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let doms = (sys::debug_stat(159), sys::debug_stat(160));
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let leaked = sys::debug_stat(158);
    let (mine, mgr, mj, dj) = start_devmgr_with_drivers(res)?;
    let r = (|| {
        let ch = open_disk(mine, 1, 0)?;
        write_pattern(ch, 9000, 16)?;
        check_pattern(ch, 9000, 16)?;
        ensure_eq!(
            (sys::debug_stat(159), sys::debug_stat(160)),
            (doms.0 + 1, doms.1 + 5),
            "the driver's domain, with its five pages mapped"
        );
        ensure_eq!(
            sys::job_info(dj, 12).a1 as u64,
            5,
            "the pages are the driver's job's (below the drivers' job)"
        );
        ensure!(
            blk_bus_master(res)?,
            "bus mastering is on while the driver runs"
        );
        // a write in flight, and the driver killed under it
        let inflight: Vec<u8> = (0..16 * 512).map(|i| (i * 29 + 7) as u8).collect();
        blk_send_write(ch, 99, 9100, &inflight)?;
        ensure_eq!(st(sys::job_kill(dj, 77)), Some(S::Ok), "kill the driver");
        let w = sys::object_wait(ch, sig::PEER_CLOSED, sys::clock_get() + 2000);
        sys::handle_close(ch);
        ensure_eq!(st(w), Some(S::Ok), "the client sees PEER_CLOSED");
        // the manager released the claim: the domain and its pages, the vectors, bus mastering
        domains_at_baseline(doms)?;
        vectors_at_baseline(live, ids)?;
        ensure!(
            !blk_bus_master(res)?,
            "bus mastering is off once the driver died"
        );
        ensure_eq!(
            sys::job_info(dj, 12).a1 as u64,
            0,
            "no I/O page is charged below"
        );
        // the kill left the drivers' job closed to new processes: the test, its supervisor, opens it again
        ensure_eq!(
            st(sys::job_set_limits(dj, &sys::NO_LIMITS)),
            Some(S::Ok),
            "lift the drivers' job's limits"
        );
        // a new driver: a new domain, mapped afresh; the in-flight write landed whole, in part or not at all, and the next one lands
        let ch = open_disk(mine, 10, 0)?;
        let r = (|| {
            // (a request answered: the driver has brought the device up)
            check_pattern(ch, 9000, 16)?;
            ensure_eq!(
                (sys::debug_stat(159), sys::debug_stat(160)),
                (doms.0 + 1, doms.1 + 5),
                "a new domain for the new driver, mapped afresh"
            );
            blk_old_or_new(ch, 9100, &inflight)?;
            write_pattern(ch, 9100, 16)?;
            check_pattern(ch, 9100, 16)
        })();
        sys::handle_close(ch);
        r?;
        // (the driver ends with its client, and the manager then releases its claim)
        blk_unclaimed(mine)?;
        // the rig variant: the suite is the driver, and lets go of its data buffer with a write in flight
        let conn = devmgr_connect(mine, 20)?;
        let mut b = match claim_and_bring_up(conn, 21, PCI_BLK_DID as u16, 0, &blk_vectors()) {
            Ok(b) => b,
            Err(e) => {
                sys::handle_close(conn);
                return Err(e);
            }
        };
        let landed = blk_write_after_letting_go(&mut b);
        b.drop_all();
        sys::handle_close(conn);
        let (sector, data) = landed?;
        domains_at_baseline(doms)?;
        let ch = open_disk(mine, 30, 0)?;
        let r = (|| {
            let (s, body) = blk_call(ch, 31, blk::Op::Read { sector, count: 1 })?;
            ensure_eq!(s, blk::Status::Ok, "read the sector back");
            ensure!(
                body == data,
                "the write whose buffer's handles were all closed landed"
            );
            Ok(())
        })();
        sys::handle_close(ch);
        r?;
        let (status, h) = devmgr_open_at(mine, 40, 2)?;
        ensure_eq!((status, h), (D::NoDevice, None), "there is no third disk");
        Ok(())
    })();
    sys::handle_close(mine);
    let end = wait_ended(mgr, 5000);
    sys::handle_close(mgr);
    sys::handle_close(dj);
    sys::handle_close(mj);
    sys::handle_close(res);
    r?;
    end?;
    domains_at_baseline(doms)?;
    vectors_at_baseline(live, ids)?;
    ensure_eq!(
        sys::debug_stat(158),
        leaked,
        "no frame or table lost (debug_stat 158)"
    );
    Ok(())
}

/// The sector the rig variant writes.
const LET_GO_SECTOR: u64 = 9200;

/// On a claimed block function: a one-sector write whose header and data are in the data page and whose status byte is in the queue
/// page (at 2048, past the queue of 8). Before the device is told, the data page is unmapped here and its every handle closed: only
/// the domain's mapping holds the frames. The request must complete OK by interrupt. (The sector and the data written.)
fn blk_write_after_letting_go(b: &mut Claimed) -> Result<(u64, Vec<u8>), Detail> {
    use virtio::Transport;
    const STATUS_AT: u64 = 2048;
    let data: Vec<u8> = (0..512).map(|i| (i * 17 + 9) as u8).collect();
    let h = virtio::blk::header(virtio::blk::T_OUT, LET_GO_SECTOR);
    for (i, w) in h.as_chunks::<8>().0.iter().enumerate() {
        wr64(b.data_va() + BLK_HDR + 8 * i as u64, u64::from_le_bytes(*w));
    }
    for (i, w) in data.as_chunks::<8>().0.iter().enumerate() {
        wr64(
            b.data_va() + BLK_DATA + 8 * i as u64,
            u64::from_le_bytes(*w),
        );
    }
    wr64(b.rings_va() + STATUS_AT, 0xff);
    let rings_io = b.q.io_base;
    ensure!(
        b.q.layout.bytes < STATUS_AT,
        "the queue ends before the status byte"
    );
    let chain = [
        virtio::Buf {
            addr: b.data_io + BLK_HDR,
            len: virtio::blk::HEADER_BYTES,
            device_writes: false,
        },
        virtio::Buf {
            addr: b.data_io + BLK_DATA,
            len: virtio::blk::SECTOR,
            device_writes: false,
        },
        virtio::Buf {
            addr: rings_io + STATUS_AT,
            len: 1,
            device_writes: true,
        },
    ];
    let mut mem = PageMem(b.rings_va());
    let head = vio(b.q.add(&mut mem, &chain, true), "add the request")?;
    // let go of the buffer: unmapped here, and the only handle to it closed (the claim's data VMO)
    let pages = sys::debug_stat(160);
    unmap_here(b.data_va());
    let data_vmo = b.hs[3];
    b.hs[3] = u64::MAX;
    ensure_eq!(
        st(sys::handle_close(data_vmo)),
        Some(S::Ok),
        "close the data VMO"
    );
    ensure_eq!(
        sys::debug_stat(160),
        pages,
        "the domain still maps the buffer"
    );
    let q = b.irq(0).ok_or_else(|| msg_at(line!()))?;
    b.t.notify(0);
    ensure_eq!(
        st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
        Some(S::Ok),
        "the write's interrupt"
    );
    let u = vio(b.q.take_used(&PageMem(b.rings_va())), "the used ring")?;
    ensure_eq!(u.map(|u| u.0), Some(head), "the device returned the write");
    ensure_eq!(
        (rd64(b.rings_va() + STATUS_AT) & 0xff) as u8,
        virtio::blk::S_OK,
        "the write's status"
    );
    let _ = sys::interrupt_ack(q);
    Ok((LET_GO_SECTOR, data))
}

/// One hostile request on a claimed block function: type `t` at `sector`, its header and status in the data page and its data buffer
/// at I/O address `data_io` (512 bytes, the device writing it for a read). Whether the device returned it with status OK within half
/// a second (QEMU does: see `blk_driver_cannot_reach_outside`).
fn blk_hostile_request(b: &mut Claimed, t: u32, sector: u64, data_io: u64) -> Result<bool, Detail> {
    use virtio::Transport;
    let h = virtio::blk::header(t, sector);
    for (i, w) in h.as_chunks::<8>().0.iter().enumerate() {
        wr64(b.data_va() + BLK_HDR + 8 * i as u64, u64::from_le_bytes(*w));
    }
    wr64(b.data_va() + BLK_STATUS, 0xff);
    let chain = [
        virtio::Buf {
            addr: b.data_io + BLK_HDR,
            len: virtio::blk::HEADER_BYTES,
            device_writes: false,
        },
        virtio::Buf {
            addr: data_io,
            len: virtio::blk::SECTOR,
            device_writes: t == virtio::blk::T_IN,
        },
        virtio::Buf {
            addr: b.data_io + BLK_STATUS,
            len: 1,
            device_writes: true,
        },
    ];
    let mut mem = PageMem(b.rings_va());
    let head = vio(b.q.add(&mut mem, &chain, true), "add the request")?;
    b.t.notify(0);
    let q = b.irq(0).ok_or_else(|| msg_at(line!()))?;
    let fired = st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 500)) == Some(S::Ok);
    if fired {
        let _ = sys::interrupt_ack(q);
    }
    let used = vio(b.q.take_used(&PageMem(b.rings_va())), "the used ring")?;
    Ok(used.is_some_and(|u| u.0 == head) && rd8_dma(b.data_va() + BLK_STATUS) == virtio::blk::S_OK)
}

/// Wait for the IOMMU's fault count to pass `before` and settle: the last fault's device id and address.
fn blk_fault_after(before: u64, what: &str) -> Result<(u64, u64), Detail> {
    let t0 = sys::clock_get();
    while iommu_faults() == before && sys::clock_get() < t0 + 2000 {
        sys::thread_yield();
    }
    ensure!(iommu_faults() > before, "{what}: the IOMMU took no fault");
    let mut n = iommu_faults();
    let mut quiet = sys::clock_get();
    while sys::clock_get() < quiet + 50 {
        if iommu_faults() != n {
            n = iommu_faults();
            quiet = sys::clock_get();
        }
        sys::thread_yield();
    }
    let (did, iova, _, _) = iommu_last_fault();
    Ok((did, iova))
}

/// IOMMU10 (IOMMU-SPEC 10, IO1): a hostile driver (the suite, holding a real claim of the scratch disk through `devmgr`) points a
/// descriptor outside its buffer: at another process's frame (the pattern frame, by its physical address), for the device to write it
/// and to read it; at kernel memory, for the device to read it; and at an I/O address past its own mapping. Each time the IOMMU refuses
/// the transfer and records a fault naming the disk (device id 0x28) at the target, and the target is intact: the frame keeps its
/// pattern, and the disk sector the reads were for holds what a real driver wrote there or zeros, never the frame's or the kernel's
/// bytes. (QEMU's virtio does not fail such a request: it moves the data through a bounce buffer, the IOMMU refuses the bounce's
/// transfer, and the request completes with status OK. The refusal shows as the fault and the untouched memory.) The driver's data
/// VMO has no physical address to aim at in the first place.
fn blk_driver_cannot_reach_outside() -> Outcome {
    if !pci_disks_present()? {
        return Ok(());
    }
    with_devmgr(|res, mine| {
        // a sector of the disk with known contents, written by a real driver
        let ch = open_disk(mine, 1, 0)?;
        let w = write_pattern(ch, 9300, 1);
        sys::handle_close(ch);
        w?;
        // (the driver ends with its client, and the manager then releases its claim)
        blk_unclaimed(mine)?;
        let (frame, pa) = pattern_frame(res)?;
        let r = (|| {
            let cases: [(&str, u32, u64); 4] = [
                ("a read into another process's frame", virtio::blk::T_IN, pa),
                (
                    "a write from another process's frame",
                    virtio::blk::T_OUT,
                    pa,
                ),
                (
                    "a write from kernel memory",
                    virtio::blk::T_OUT,
                    0x8020_0000,
                ),
                ("a read past its own mapping", virtio::blk::T_IN, 1),
            ];
            for (k, (what, t, target)) in cases.into_iter().enumerate() {
                let conn = devmgr_connect(mine, 10 + k as u32)?;
                let mut b = match claim_and_bring_up(conn, 2, PCI_BLK_DID as u16, 0, &blk_vectors())
                {
                    Ok(b) => b,
                    Err(e) => {
                        sys::handle_close(conn);
                        return Err(e);
                    }
                };
                let r = (|| {
                    if k == 0 {
                        ensure_eq!(
                            st(sys::vmo_phys_addr(b.hs[3])),
                            Some(S::NotSupported),
                            "the driver's data VMO has no physical address"
                        );
                    }
                    // (the third case aims a megabyte past the data page: nothing of the domain is mapped there)
                    let target = if target == 1 {
                        b.data_io + 0x10_0000
                    } else {
                        target
                    };
                    let before = iommu_faults();
                    let sector = if t == virtio::blk::T_OUT { 9300 } else { 0 };
                    blk_hostile_request(&mut b, t, sector, target)?;
                    let (did, iova) = blk_fault_after(before, what)?;
                    ensure_eq!(did, PCI_BLK_DID, "{what}: the fault names the disk");
                    ensure_eq!(
                        iova & !0xfff,
                        target & !0xfff,
                        "{what}: the fault is at the target"
                    );
                    if let Some(i) = pattern_intact() {
                        ensure!(false, "{what}: the pattern frame changed at word {i}");
                    }
                    Ok(())
                })();
                b.drop_all();
                sys::handle_close(conn);
                r?;
                blk_unclaimed(mine)?;
            }
            // the disk's sector holds what the real driver wrote, or the zeros of a refused transfer: nothing of the frame's or the
            // kernel's memory reached it
            let ch = open_disk(mine, 30, 0)?;
            let c = blk_call(
                ch,
                31,
                blk::Op::Read {
                    sector: 9300,
                    count: 1,
                },
            );
            sys::handle_close(ch);
            let (s, body) = c?;
            ensure_eq!(s, blk::Status::Ok, "read the sector back");
            ensure!(
                (0..512).all(|i| body[i] == pattern(9300, i)) || body.iter().all(|&b| b == 0),
                "the sector the hostile writes were for holds other memory's bytes: {:x?}",
                &body[..16]
            );
            Ok(())
        })();
        pattern_drop(frame);
        r
    })
}

// ---- IOMMU12: the hostile-driver matrix (IOMMU-SPEC 10 and 13; OD148, OD166 point 4) -------------------------------------------------
//
// OD148's sentence, "a device reaches only the frames it has been mapped", attacked from a driver's seat. The driver is a process of
// its own (`crash` mode 121, `Hostile`) started in a driver's job with exactly the handles a real driver is given, and nothing else:
// it makes the calls the suite asks for with them and reports what the kernel said. Two rigs:
//
// * **The device rig** (`with_matrix`): the suite is the supervisor, as `devmgr` is, with the root resource. Two virtio-pci functions
//   are brought up and driven by hand (IOMMU-SPEC amendment 2026-11-27 part 2, OD175: no test machine carries a device that reads or
//   reports memory at a physical address), each with a domain charged to one driver's job that has finite limits (`MATRIX_IO_PAGES`,
//   `MATRIX_VECTORS`, as `devmgr` sets them), its rings and a buffer mapped and vector 0 granted: the entropy function (slot 4), whose
//   one descriptor is the target, and the scratch disk (slot 5, `PCI_BLK_SLOT`), whose read of sector 0 puts its data descriptor at
//   the target (the header and the status byte stay in its own rings page). The hostile driver holds the two drivers' domain handles
//   (IO_MAP and IO_UNMAP), the two buffers (DMA, READ, WRITE), each device's doorbell page and its rings: it writes the descriptors and
//   rings the doorbells itself, through its own mappings; the suite keeps the control handles and does only the bring-up, as `devmgr`
//   resets a function before a claim. Each device row aims one device at a target and checks: the IOMMU took a fault, the device's
//   domain holds a write record at the target (the other device's domain holds none), the last fault names this device, the targets
//   and every bystander's memory are intact, no vector fired and no notice or message on an identity with no route reached a hart. No
//   check reads memory back at a physical address: a refusal is the fault record and the memory left as it was. The message rows (the
//   entropy function only: the disk's vector is the target of one) are the worst case of Q-IOMMU-MSI 6: the rig holds the function's
//   MSI-X table, which a real driver never does. The no-disk boot (`OS_DISK=0`) has no second device, and the rig has nothing to do.
// * **The claim rig** (`with_claimed_hostile`): the scratch disk claimed through `devmgr`'s `ClaimPci` for a driver job, and the
//   claim's handles (blkd's set: the BAR, the domain handle, the queue memory and the data buffer, two interrupts) moved to the
//   hostile driver, plus a duplicate of its data buffer without DMA and one without WRITE. The rows with its own handles and the
//   map-until-refused row run here, against the limits `devmgr` set.
//
// The legitimate use of each rig is checked first (the entropy function writes its buffer; the disk reads sector 0 into its buffer, with
// the status OK and the disk's bytes), so a refusal is not the rig being broken.

/// The driver's job limits of the device rig (`devmgr`'s claims set limits of the same kind: `pcicore::claim`).
const MATRIX_IO_PAGES: u64 = 16;
const MATRIX_VECTORS: u64 = 4;
/// The device rig's second device, the scratch disk's function: where its configuration page, its rings, its buffer and its BARs are
/// mapped here (the entropy function has PCI_CFG_VA and PCI_BAR_VA), and where a request's header and status byte are in its rings
/// page (its own memory: only the data descriptor is aimed).
const MBLK_CFG_VA: u64 = 0x7d0_0000;
const MBLK_RINGS_VA: u64 = 0x7d0_1000;
const MBLK_BUF_VA: u64 = 0x7d1_0000;
const MBLK_BAR_VA: u64 = 0x7d2_0000;
const MBLK_AT: PciAt = PciAt {
    cfg: MBLK_CFG_VA,
    bars: MBLK_BAR_VA,
    room: CLAIM_VA - MBLK_BAR_VA,
};
const MBLK_HDR: u64 = 0x800;
const MBLK_STATUS: u64 = 0xa00;

/// A hostile driver: a `crash` child (mode 121) in a driver's job, holding exactly the handles it was sent.
struct Hostile {
    p: u64,
    ch: u64,
    /// The handles it was sent, as numbered in its own table (the suite holds none of them).
    hs: Vec<u64>,
}

impl Hostile {
    /// Start it in `job` and move `handles` to it.
    fn start(job: u64, handles: &[u64]) -> Result<Hostile, Detail> {
        let (p, ch) = match launch_image_in(IMAGE.load(Ordering::Relaxed), 121, job) {
            Ok(x) => x,
            Err(e) => {
                for &h in handles {
                    sys::handle_close(h);
                }
                return Err(e);
            }
        };
        let x = Hostile {
            p,
            ch,
            hs: Vec::new(),
        };
        let hs = (|| {
            ensure_eq!(
                st(sys::channel_write(
                    ch,
                    [0u8; 4].as_ptr(),
                    4,
                    handles.as_ptr(),
                    handles.len()
                )),
                Some(S::Ok),
                "move the driver's handles to it"
            );
            ensure_eq!(
                st(sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000)),
                Some(S::Ok),
                "the driver says its handles"
            );
            let mut b = [0u8; 128];
            let r = sys::channel_read(ch, b.as_mut_ptr(), b.len(), core::ptr::null_mut(), 0);
            ensure_eq!(st(r), Some(S::Ok), "read them");
            ensure_eq!(r.a1, 8 * handles.len(), "one number per handle");
            Ok(b[..r.a1]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect::<Vec<u64>>())
        })();
        match hs {
            Ok(hs) => Ok(Hostile { hs, ..x }),
            Err(e) => {
                x.end();
                Err(e)
            }
        }
    }

    /// One operation (0 a system call, 1 the library's `sys_vmo_phys_addr`): the status, `a1` and `a2`.
    fn op(&self, op: u64, nr: u64, a: [u64; 6]) -> Result<(Option<S>, u64, u64), Detail> {
        let mut m = [0u8; 64];
        for (i, w) in [op, nr, a[0], a[1], a[2], a[3], a[4], a[5]]
            .iter()
            .enumerate()
        {
            m[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        ensure_eq!(
            st(sys::channel_write(
                self.ch,
                m.as_ptr(),
                64,
                core::ptr::null(),
                0
            )),
            Some(S::Ok),
            "ask the driver"
        );
        ensure_eq!(
            st(sys::object_wait(
                self.ch,
                sig::READABLE,
                sys::clock_get() + 5000
            )),
            Some(S::Ok),
            "the driver answers"
        );
        let mut b = [0u8; 24];
        let r = sys::channel_read(self.ch, b.as_mut_ptr(), 24, core::ptr::null_mut(), 0);
        ensure_eq!((st(r), r.a1), (Some(S::Ok), 24), "read its answer");
        let w = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap_or([0; 8]));
        let ret = Ret {
            status: w(0) as i64 as i32,
            a1: w(1) as usize,
            a2: w(2) as usize,
        };
        Ok((st(ret), w(1), w(2)))
    }

    fn call(&self, nr: usize, a: [u64; 6]) -> Result<(Option<S>, u64, u64), Detail> {
        self.op(0, nr as u64, a)
    }

    /// Map one page of `vmo` into `dom` with `access` (`abi::iodomain` READ and WRITE): the status and the I/O address.
    fn map(&self, dom: u64, vmo: u64, access: u64) -> Result<(Option<S>, u64), Detail> {
        let (s, io, _) = self.call(rt::abi::syscall::IODOMAIN_MAP, [dom, vmo, 0, 1, access, 0])?;
        Ok((s, io))
    }

    fn unmap(&self, dom: u64, io: u64) -> Result<Option<S>, Detail> {
        Ok(self
            .call(rt::abi::syscall::IODOMAIN_UNMAP, [dom, io, 1, 0, 0, 0])?
            .0)
    }

    /// Kill it and wait for it: every handle it held is closed by its teardown.
    fn end(self) {
        if self.p == 0 {
            return;
        }
        let _ = sys::process_kill(self.p, 9);
        let _ = wait_ended(self.p, 2000);
        sys::handle_close(self.p);
        sys::handle_close(self.ch);
    }
}

/// One device of the device rig.
struct MDev {
    did: u64,
    /// The supervisor's control handle (0 once it was given away).
    sup: u64,
    /// The driver's buffer: our handle, where we map it, its I/O address and its physical address.
    buf: u64,
    buf_va: u64,
    buf_io: u64,
    buf_pa: u64,
    /// A vector granted on the domain, and its MSI-X address.
    irq: u64,
    address: u64,
    /// The hostile driver's numbers for its domain handle and its buffer.
    h_dom: u64,
    h_buf: u64,
}

/// What a row needs of a device after it changed the rig (a copy, so the rig can be changed again).
struct MDevAt {
    did: u64,
    buf_io: u64,
    buf_va: u64,
}

/// The device under test of a row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dut {
    Rng,
    Blk,
}

struct Matrix<'a> {
    drv: Hostile,
    /// The entropy function and the disk's function, as the suite mapped them.
    f: &'a PciFn,
    g: &'a PciFn,
    /// Each device's rings, at their I/O addresses in its own domain.
    rings_io: u64,
    blk_rings_io: u64,
    rng: MDev,
    blk: MDev,
    /// The pattern frame (another process's memory, as far as the drivers are concerned: neither domain maps it).
    pa: u64,
    /// Where the hostile driver mapped, in its own space, each device's doorbell (the notify register itself) and each device's rings:
    /// it drives both devices itself through these (`hst`, `hld`).
    h_notify: u64,
    h_blk_notify: u64,
    h_rings: u64,
    h_blk_rings: u64,
}

impl Matrix<'_> {
    /// A store by the hostile driver to its own mapping (`width` 1, 2, 4 or 8 bytes).
    fn hst(&self, va: u64, width: u64, v: u64) -> Outcome {
        let (s, _, _) = self.drv.op(4, 0, [va, width, v, 0, 0, 0])?;
        ensure_eq!(s, Some(S::Ok), "the driver's store at {va:#x}");
        Ok(())
    }
    /// A load by the hostile driver from its own mapping.
    fn hld(&self, va: u64, width: u64) -> Result<u64, Detail> {
        let (s, v, _) = self.drv.op(5, 0, [va, width, 0, 0, 0, 0])?;
        ensure_eq!(s, Some(S::Ok), "the driver's load at {va:#x}");
        Ok(v)
    }
    fn dev(&self, d: Dut) -> &MDev {
        match d {
            Dut::Rng => &self.rng,
            Dut::Blk => &self.blk,
        }
    }
    fn other(&self, d: Dut) -> &MDev {
        match d {
            Dut::Rng => &self.blk,
            Dut::Blk => &self.rng,
        }
    }
    /// The domain of the entropy function as `msi_send` wants it.
    fn rng_domain(&self) -> Domain {
        Domain {
            sup: self.rng.sup,
            drv: 0,
            rings_io: self.rings_io,
        }
    }
}

/// The disk's virtio transport over the suite's mapping of its BARs (the `virtio` crate's, as `pci_blk_read_sector0` drives it).
fn mblk_transport(g: &PciFn) -> virtio::PciTransport<VaRegs> {
    virtio::PciTransport::new(
        VaRegs,
        virtio::PciLocations {
            common: g.common,
            notify: g.notify,
            notify_len: g.notify_len,
            notify_multiplier: g.notify_multiplier,
            isr: g.isr,
            device: g.device,
        },
    )
}

/// The supervisor's part of a disk request (`devmgr` resets a function before a claim): reset the disk and bring it to DRIVER_OK with
/// queue 0 (four entries) at `rings_io` in the entropy function's layout (the descriptors, the available ring at +0x100, the used ring
/// at +0x200), with no vector for the queue and none for configuration changes (MSI-X stays off and INTx is disabled: the driver
/// polls). Queue 0's doorbell, as the suite mapped it.
fn mblk_up(g: &PciFn, rings_io: u64) -> Result<u64, Detail> {
    use virtio::Transport;
    let mut t = mblk_transport(g);
    let t0 = sys::clock_get();
    vio(
        t.reset(&mut || sys::clock_get() < t0 + 1000),
        "reset the disk",
    )?;
    let got = vio(t.negotiate(virtio::feature::REQUIRED), "negotiate")?;
    ensure_eq!(
        got & virtio::feature::REQUIRED,
        virtio::feature::REQUIRED,
        "VERSION_1 and ACCESS_PLATFORM accepted"
    );
    vio(
        t.setup_queue(
            0,
            4,
            (rings_io, rings_io + 0x100, rings_io + 0x200),
            virtio::NO_VECTOR,
        ),
        "queue 0",
    )?;
    t.driver_ok();
    t.notify_address(0).ok_or_else(|| msg_at(line!()))
}

/// The stores that make one disk request, into the rings page whose I/O address is `io`, as (offset, width, value): the page's
/// rings cleared, the header (sector 0, T_IN) at MBLK_HDR, the status byte at MBLK_STATUS set to 0xff, three descriptors (the header,
/// device-readable; 512 bytes of data at `data`, device-writable; the status byte, device-writable), and the chain's head in the
/// available ring, no interrupt asked for. The available ring's index comes last.
fn mblk_request_stores(io: u64, data: u64) -> Vec<(u64, u64, u64)> {
    const NEXT: u64 = 1;
    const WRITE: u64 = 2;
    let mut v: Vec<(u64, u64, u64)> = (0..(0x300 / 8)).map(|i| (8 * i, 8, 0)).collect();
    let h = virtio::blk::header(virtio::blk::T_IN, 0);
    for (i, w) in h.as_chunks::<8>().0.iter().enumerate() {
        v.push((MBLK_HDR + 8 * i as u64, 8, u64::from_le_bytes(*w)));
    }
    v.extend([
        (MBLK_STATUS, 8, 0xff),
        (0, 8, io + MBLK_HDR),
        (
            8,
            8,
            u64::from(virtio::blk::HEADER_BYTES) | (NEXT << 32) | (1 << 48),
        ),
        (16, 8, data),
        (
            24,
            8,
            u64::from(virtio::blk::SECTOR) | ((NEXT | WRITE) << 32) | (2 << 48),
        ),
        (32, 8, io + MBLK_STATUS),
        (40, 8, 1 | (WRITE << 32)),
        (0x100, 2, 1), // VIRTQ_AVAIL_F_NO_INTERRUPT
        (0x104, 2, 0), // ring[0] = descriptor 0
        (0x102, 2, 1), // idx 1
    ]);
    v
}

/// The status byte of the disk's last request (in its rings page, as the suite mapped it).
fn mblk_status() -> u8 {
    rd8_dma(MBLK_RINGS_VA + MBLK_STATUS)
}

/// Whether the disk's buffer, mapped at `va`, holds the scratch disk's sector 0 (byte i is (i * 7 + 3) mod 256).
fn mblk_sector0_at(va: u64) -> bool {
    (0..512u64).all(|i| rd8_dma(va + i) == ((i * 7 + 3) & 0xff) as u8)
}

/// Fill a page we have mapped at `va` with the pattern (`salt` makes each page's its own).
fn fill_page(va: u64, salt: u64) {
    for i in 0..512 {
        wr64(va + 8 * i, pattern_word(i) ^ salt);
    }
}

fn page_intact(va: u64, salt: u64) -> bool {
    (0..512).all(|i| rd64(va + 8 * i) == pattern_word(i) ^ salt)
}

/// The device rig (see the section's comment): `body` gets the rig. Afterwards everything is undone and the leak oracles hold: no
/// domain, mapped page or vector is left, and the driver's job is back to nothing. No disk (the no-disk boot): nothing to do, and the
/// line `matrix: no disk, device rig skipped`, which the harness refuses in a boot that has the disks.
fn with_matrix(body: impl FnOnce(&mut Matrix) -> Outcome) -> Outcome {
    if !pci_disks_present()? {
        println!("matrix: no disk, device rig skipped");
        return Ok(());
    }
    let doms = (sys::debug_stat(159), sys::debug_stat(160));
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let second = Some((PCI_BLK_SLOT, PCI_BLK_ID, MBLK_AT));
    let r = with_functions(IOMMU_TEST_SLOT, PCI_RNG_ID, second, |res, f, g, pa| {
        let g = g.ok_or_else(|| msg_at(line!()))?;
        let root = root_job()?;
        let dj = job(root, u64::MAX, u64::MAX, u64::MAX);
        sys::handle_close(root);
        let dj = dj?;
        // the handles of ours made before the rig owns them (closed here on a failure on the way)
        let mut pending: Vec<u64> = Vec::new();
        let r = (|| {
            ok(
                sys::job_set_io_limits(dj, MATRIX_IO_PAGES, MATRIX_VECTORS),
                "the driver's job's I/O limits",
            )?;
            // each device's rings first (mapped by the supervisor and the driver; the rows do not aim at them), so that the page after
            // its buffer is the first free one of its domain
            let rings = ok(sys::vmo_create_dma_in(res, 1, dj), "the rings")?.a1 as u64;
            pending.push(rings);
            map_here(rings, RINGS_VA, 1, PF_R | PF_W)?;
            let brings = ok(sys::vmo_create_dma_in(res, 1, dj), "the disk's rings")?.a1 as u64;
            pending.push(brings);
            map_here(brings, MBLK_RINGS_VA, 1, PF_R | PF_W)?;
            let mut dev = |did: u64, rings: u64, buf_va: u64| -> Result<(MDev, u64), Detail> {
                let sup = ok(sys::iodomain_create(res, did, dj), "iodomain_create")?.a1 as u64;
                pending.push(sup);
                let drv = ok(
                    sys::handle_duplicate(
                        sup,
                        Rights::IO_MAP.0 | Rights::IO_UNMAP.0 | Rights::TRANSFER.0,
                    ),
                    "the driver's domain handle",
                )?
                .a1 as u64;
                pending.push(drv);
                let rings_io =
                    ok(sys::iodomain_map(drv, rings, 0, 1, 3), "map the rings")?.a1 as u64;
                let buf = ok(sys::vmo_create_dma_in(res, 1, dj), "the driver's buffer")?.a1 as u64;
                pending.push(buf);
                map_here(buf, buf_va, 1, PF_R | PF_W)?;
                let buf_io = ok(sys::iodomain_map(drv, buf, 0, 1, 3), "map the buffer")?.a1 as u64;
                let gv = ok(sys::iodomain_grant_vector(sup, 0), "grant vector 0")?;
                pending.push(gv.a1 as u64);
                let dup = ok(
                    sys::handle_duplicate(
                        buf,
                        Rights::DMA.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0,
                    ),
                    "the driver's buffer handle",
                )?
                .a1 as u64;
                pending.push(dup);
                Ok((
                    MDev {
                        did,
                        sup,
                        buf,
                        buf_va,
                        buf_io,
                        buf_pa: phys_of(buf_va)?,
                        irq: gv.a1 as u64,
                        address: gv.a2 as u64,
                        h_dom: drv,
                        h_buf: dup,
                    },
                    rings_io,
                ))
            };
            let (mut rng, rings_io) = dev(IOMMU_TEST_DID, rings, BUF_VA)?;
            let (mut blk, blk_rings_io) = dev(PCI_BLK_DID, brings, MBLK_BUF_VA)?;
            // and what a driver drives its device with (IOMMU-SPEC 10.1 step 5): each device's doorbell page (a physical VMO, as a
            // claim hands over the BAR; the disk is brought up once to learn where its queue's doorbell is) and its rings (the queue
            // memory)
            let blk_notify = mblk_up(g, blk_rings_io)?;
            let (npa, bnpa) = (f.phys(f.notify), g.phys(blk_notify));
            let mut doorbell = |pa: u64, what: &str| -> Result<u64, Detail> {
                let v = ok(sys::vmo_create_physical(res, pa & !0xfff, 4096), what)?.a1 as u64;
                pending.push(v);
                Ok(v)
            };
            let nvmo = doorbell(npa, "the entropy function's doorbell page")?;
            let bnvmo = doorbell(bnpa, "the disk's doorbell page")?;
            let mut rings_for_driver = |r: u64| -> Result<u64, Detail> {
                let v = ok(
                    sys::handle_duplicate(
                        r,
                        Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0,
                    ),
                    "the driver's rings",
                )?
                .a1 as u64;
                pending.push(v);
                Ok(v)
            };
            let rdup = rings_for_driver(rings)?;
            let brdup = rings_for_driver(brings)?;
            // the drivers' handles move to the hostile driver (whatever happens: `Hostile::start` consumes them)
            let moved = [
                rng.h_dom, rng.h_buf, blk.h_dom, blk.h_buf, nvmo, bnvmo, rdup, brdup,
            ];
            pending.retain(|h| !moved.contains(h));
            let drv = Hostile::start(dj, &moved)?;
            (rng.h_dom, rng.h_buf, blk.h_dom, blk.h_buf) =
                (drv.hs[0], drv.hs[1], drv.hs[2], drv.hs[3]);
            // from here the rig owns every handle: its fields say which are still ours
            pending.clear();
            let mut m = Matrix {
                drv,
                f,
                g,
                rings_io,
                blk_rings_io,
                rng,
                blk,
                pa,
                h_notify: 0,
                h_blk_notify: 0,
                h_rings: 0,
                h_blk_rings: 0,
            };
            let r = (|| {
                // the driver maps its devices' doorbells and their rings into itself
                let at = |h: u64, what: &str| -> Result<u64, Detail> {
                    let (s, a, _) = m.drv.op(3, 0, [h, 0, 4096, PF_R | PF_W, 0, 0])?;
                    ensure_eq!(s, Some(S::Ok), "the driver maps {what}");
                    Ok(a)
                };
                let h_notify = at(m.drv.hs[4], "its doorbell page")? + (npa & 0xfff);
                let h_blk_notify = at(m.drv.hs[5], "the disk's doorbell page")? + (bnpa & 0xfff);
                let h_rings = at(m.drv.hs[6], "its rings")?;
                let h_blk_rings = at(m.drv.hs[7], "the disk's rings")?;
                (m.h_notify, m.h_blk_notify, m.h_rings, m.h_blk_rings) =
                    (h_notify, h_blk_notify, h_rings, h_blk_rings);
                // the rig works: each device writes its own buffer
                fill_page(m.rng.buf_va, 1);
                ensure!(
                    hostile_rng_request(&m, m.rng.buf_io, 64, 2000)?,
                    "the entropy function serves a request into its buffer"
                );
                ensure!(
                    !page_intact(m.rng.buf_va, 1),
                    "the entropy function's bytes landed in its buffer"
                );
                fill_page(m.blk.buf_va, 2);
                ensure!(
                    hostile_blk_request(&m, m.blk.buf_io, 2000)?,
                    "the disk serves a request into its buffer"
                );
                ensure_eq!(
                    mblk_status(),
                    virtio::blk::S_OK,
                    "the disk's request status"
                );
                ensure!(
                    mblk_sector0_at(m.blk.buf_va),
                    "the disk's sector 0 landed in its buffer"
                );
                fill_page(m.rng.buf_va, 1);
                fill_page(m.blk.buf_va, 2);
                drain_domain_faults(m.rng.sup);
                drain_domain_faults(m.blk.sup);
                body(&mut m)
            })();
            // the functions stop before their memory and their domains go
            mmio_wr8(m.f.common + 0x14, 0);
            mmio_wr8(m.g.common + 0x14, 0);
            let drv = core::mem::replace(
                &mut m.drv,
                Hostile {
                    p: 0,
                    ch: 0,
                    hs: Vec::new(),
                },
            );
            drv.end();
            for h in [
                m.rng.sup, m.rng.buf, m.rng.irq, m.blk.sup, m.blk.buf, m.blk.irq, rings, brings,
            ] {
                if h != 0 {
                    sys::handle_close(h);
                }
            }
            r
        })();
        for h in pending {
            sys::handle_close(h);
        }
        unmap_here(RINGS_VA);
        unmap_here(BUF_VA);
        unmap_here(MBLK_RINGS_VA);
        unmap_here(MBLK_BUF_VA);
        let used = (|| {
            let t0 = sys::clock_get();
            while (sys::job_info(dj, 12).a1, sys::job_info(dj, 13).a1) != (0, 0)
                && sys::clock_get() < t0 + 2000
            {
                sys::thread_yield();
            }
            ensure_eq!(
                (sys::job_info(dj, 12).a1, sys::job_info(dj, 13).a1),
                (0, 0),
                "the driver's job holds no I/O page or vector afterwards"
            );
            Ok(())
        })();
        sys::handle_close(dj);
        r.and(used)
    });
    r.and(domains_at_baseline(doms))
        .and(vectors_at_baseline(live, ids))
}

/// `rng_request`, driven by the hostile driver: it writes the descriptor (`buf`, `len` bytes, device-writable) and the available ring in
/// its own mapping of the rings and rings the doorbell in its own mapping of the BAR. The supervisor's part is only the bring-up
/// (`virtio_up_entry`: reset, the rings' I/O addresses, DRIVER_OK), as `devmgr` resets a function before a claim; it then watches the
/// used ring.
fn hostile_rng_request(m: &Matrix, buf: u64, len: u32, wait_ms: u64) -> Result<bool, Detail> {
    let r = m.h_rings;
    for i in 0..(0x300 / 8) {
        m.hst(r + 8 * i, 8, 0)?;
    }
    m.hst(r, 8, buf)?; // descriptor 0: the address,
    m.hst(r + 8, 8, u64::from(len) | (2 << 32))?; // its length, VIRTQ_DESC_F_WRITE, no next
    m.hst(r + 0x100, 2, 1)?; // VIRTQ_AVAIL_F_NO_INTERRUPT
    m.hst(r + 0x104, 2, 0)?; // ring[0] = descriptor 0
    m.hst(r + 0x102, 2, 1)?; // idx 1
    virtio_up_entry(
        m.f,
        (m.rings_io, m.rings_io + 0x100, m.rings_io + 0x200),
        pcicore::MsixEntry {
            address: 0,
            data: 0,
            masked: true,
        },
    )?;
    m.hst(m.h_notify, 2, 0)?; // the doorbell: queue 0
    let t0 = sys::clock_get();
    while sys::clock_get() < t0 + wait_ms {
        if m.hld(r + 0x202, 2)? == 1 {
            return Ok(true);
        }
        sys::thread_yield();
    }
    Ok(false)
}

/// A disk request (`mblk_request_stores`: sector 0 into `data`), driven by the hostile driver: it writes the request in its own mapping
/// of the disk's rings and rings the doorbell in its own mapping; the supervisor's part is the bring-up (`mblk_up`). Whether the
/// device returned the request within `wait_ms`.
fn hostile_blk_request(m: &Matrix, data: u64, wait_ms: u64) -> Result<bool, Detail> {
    let r = m.h_blk_rings;
    for (off, width, v) in mblk_request_stores(m.blk_rings_io, data) {
        m.hst(r + off, width, v)?;
    }
    mblk_up(m.g, m.blk_rings_io)?;
    m.hst(m.h_blk_notify, 2, 0)?; // the doorbell: queue 0
    let t0 = sys::clock_get();
    while sys::clock_get() < t0 + wait_ms {
        if m.hld(r + 0x202, 2)? == 1 {
            return Ok(true);
        }
        sys::thread_yield();
    }
    Ok(false)
}

/// `hostile_blk_request` with the suite driving the disk itself, through its own mappings of the rings and the doorbell.
fn blk_request_by_the_suite(m: &Matrix, data: u64, wait_ms: u64) -> Result<bool, Detail> {
    for (off, width, v) in mblk_request_stores(m.blk_rings_io, data) {
        match width {
            8 => wr64(MBLK_RINGS_VA + off, v),
            _ => wr16(MBLK_RINGS_VA + off, v as u16),
        }
    }
    let notify = mblk_up(m.g, m.blk_rings_io)?;
    mmio_wr16(notify, 0);
    let t0 = sys::clock_get();
    while sys::clock_get() < t0 + wait_ms {
        if rd16(MBLK_RINGS_VA + 0x202) == 1 {
            return Ok(true);
        }
        sys::thread_yield();
    }
    Ok(false)
}

/// `dut_write` with the suite driving the device itself (through its own mappings of the BAR and the rings): only for a row whose driver
/// is dead.
fn dut_write_by_the_suite(m: &Matrix, d: Dut, target: u64) -> Result<bool, Detail> {
    match d {
        Dut::Rng => rng_request(m.f, m.rings_io, target, 64, 300),
        Dut::Blk => blk_request_by_the_suite(m, target, 300),
    }
}

/// Make device `d` write at I/O address `target` (the entropy function's descriptor, the disk's data descriptor), driven by the hostile
/// driver (`hostile_rng_request`, `hostile_blk_request`): whether the device returned the request. (A device may return a request
/// whose write was refused, as QEMU's virtio moves a buffer through a bounce buffer: the fault records and the memory are the oracle of
/// a refusal, never this answer.)
fn dut_write(m: &Matrix, d: Dut, target: u64) -> Result<bool, Detail> {
    match d {
        Dut::Rng => hostile_rng_request(m, target, 64, 300),
        Dut::Blk => hostile_blk_request(m, target, 300),
    }
}

/// Make device `d` send its configuration-change message: a memory write of the word `data` at `address` (MSI-X entry 0, which the
/// rig writes) that needs no fetch from its rings, so a quarantine row shows a write refused, not only a ring read. The entropy
/// function as `msi_send` does it; the disk brought up (`mblk_up`), its configuration vector on entry 0 and the driver error that
/// makes a modern virtio device send it (`virtio_message`), then its MSI-X off again once the device has settled.
fn dut_message(m: &Matrix, d: Dut, address: u64, data: u32) -> Outcome {
    match d {
        Dut::Rng => msi_send(m.f, &m.rng_domain(), address, data),
        Dut::Blk => {
            let r = (|| {
                mblk_up(m.g, m.blk_rings_io)?;
                m.g.msix_on(pcicore::MsixEntry {
                    address,
                    data,
                    masked: false,
                })?;
                mmio_wr16(m.g.common + 0x10, 0);
                ensure_eq!(
                    mmio_rd16(m.g.common + 0x10),
                    0,
                    "the disk's configuration vector took MSI-X entry 0"
                );
                virtio_message(m.g);
                msi_settle();
                Ok(())
            })();
            m.g.msix_off();
            r
        }
    }
}

/// The bystanders after a row: the pattern frame, both buffers (as last filled: the device under test's own only when `own` is set),
/// no vector fired, no notice and no message on an identity with no route since `(notices, no_route)`.
fn bystanders_intact(
    m: &Matrix,
    own: Option<Dut>,
    (notices, no_route): (u64, u64),
    what: &str,
) -> Outcome {
    if let Some(i) = pattern_intact() {
        ensure!(false, "{what}: the pattern frame changed at word {i}");
    }
    for (d, salt) in [(Dut::Rng, 1), (Dut::Blk, 2)] {
        if own != Some(d) {
            ensure!(
                page_intact(m.dev(d).buf_va, salt),
                "{what}: the {d:?} buffer changed"
            );
        }
        if m.dev(d).irq != 0 {
            ensure_eq!(
                sys::object_signals(m.dev(d).irq).a1 as u32 & sig::INTERRUPT,
                0,
                "{what}: the {d:?} domain's vector fired"
            );
        }
    }
    ensure_eq!(
        sys::debug_stat(162),
        notices,
        "{what}: a notice reached a hart"
    );
    ensure_eq!(
        sys::debug_stat(140),
        no_route,
        "{what}: a message on an identity with no route"
    );
    Ok(())
}

/// A device row: device `d` aimed at `target` is refused. The IOMMU faults, `d`'s domain holds a write record at the target's page
/// and the other domain none, the last fault names `d`, and every bystander is intact (`own` as `bystanders_intact`).
fn dut_refused(m: &Matrix, d: Dut, target: u64, own: Option<Dut>, what: &str) -> Outcome {
    let what = alloc::format!("{d:?}: {what}");
    let base = (sys::debug_stat(162), sys::debug_stat(140));
    let before = iommu_faults();
    // (whether the device returned the request says nothing: the fault records and the memory below are the oracle)
    let _ = dut_write(m, d, target)?;
    // (at least one record: each device writes its bytes as several transactions, each refused and recorded)
    ensure!(
        iommu_wait_faults(before).is_some(),
        "{what}: the IOMMU took no fault"
    );
    let (did, _, _, _) = iommu_last_fault();
    ensure_eq!(did, m.dev(d).did, "{what}: the last fault names the device");
    let recs = drain_domain_faults(m.dev(d).sup);
    ensure!(
        recs.iter()
            .any(|&(_, a, dir, _)| a & !0xfff == target & !0xfff && dir == DIR_WRITE),
        "{what}: no write record at {target:#x} in the device's domain: {recs:x?}"
    );
    let others = drain_domain_faults(m.other(d).sup);
    ensure!(
        others.is_empty(),
        "{what}: the other device's domain has records: {others:x?}"
    );
    bystanders_intact(m, own, base, &what)
}

/// Each device of the rig, aimed at the target `target(m, d)` gives.
fn matrix_row(what: &str, target: impl Fn(&Matrix, Dut) -> Result<u64, Detail>) -> Outcome {
    with_matrix(|m| {
        for d in [Dut::Rng, Dut::Blk] {
            let t = target(m, d)?;
            dut_refused(m, d, t, None, what)?;
        }
        Ok(())
    })
}

/// Row: a descriptor at a kernel address (the first page of the kernel image, `debug_stat` 104).
fn hostile_dma_at_a_kernel_address() -> Outcome {
    matrix_row("the kernel image", |_, _| Ok(sys::debug_stat(104)))
}

/// Row: a descriptor at another process's frame (the pattern frame, by its physical address).
fn hostile_dma_at_another_process_frame() -> Outcome {
    matrix_row("another process's frame", |m, _| Ok(m.pa))
}

/// Row: a descriptor at another device's buffer, by its physical address (by its I/O address it would be this device's own window:
/// each domain has its own).
fn hostile_dma_at_another_device_buffer() -> Outcome {
    matrix_row("another device's buffer", |m, d| Ok(m.other(d).buf_pa))
}

/// Row: a descriptor at another device's BAR (peer to peer: an address the IOMMU does not map is refused like any other).
fn hostile_dma_at_another_device_bar() -> Outcome {
    matrix_row("another device's BAR", |m, d| {
        let bar = match d {
            Dut::Rng => m.g.phys(m.g.common) & !0xfff,
            Dut::Blk => m.f.phys(m.f.common) & !0xfff,
        };
        ensure!(bar != 0, "the other device's BAR");
        Ok(bar)
    })
}

/// Row: an IMSIC page as DMA: a device write into every hart's supervisor interrupt file (its `seteipnum` register takes the low word
/// as an identity). Refused as a translation (the files are not in any domain): the fault and the refusal are the check. Neither
/// row chooses a word that is a live identity (the disk writes its sector 0, which begins 0x18110a03, the entropy function random
/// bytes), so "no hart is interrupted" (the bystander check) is a backstop here, not the oracle.
fn hostile_dma_at_an_interrupt_file() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let harts = hart_count(res);
    sys::handle_close(res);
    with_matrix(|m| {
        for d in [Dut::Rng, Dut::Blk] {
            for h in 0..harts {
                dut_refused(
                    m,
                    d,
                    IMSIC_S + h * 4096,
                    None,
                    &alloc::format!("hart {h}'s file"),
                )?;
            }
        }
        Ok(())
    })
}

/// Row: its own buffer, one page beyond (the domain's first free page: its mappings are made lowest first, the buffer last).
fn hostile_dma_one_page_past_its_buffer() -> Outcome {
    matrix_row("one page past its buffer", |m, d| {
        let rings_io = if d == Dut::Rng {
            m.rings_io
        } else {
            m.blk_rings_io
        };
        ensure!(rings_io < m.dev(d).buf_io, "the rings are below the buffer");
        Ok(m.dev(d).buf_io + 4096)
    })
}

/// Row: a write through a read-only mapping: the driver maps its own buffer again, for the device to read only, and the device writes
/// there. Its buffer (both mappings name it) is intact.
fn hostile_dma_through_a_read_only_mapping() -> Outcome {
    with_matrix(|m| {
        for d in [Dut::Rng, Dut::Blk] {
            let x = m.dev(d);
            let (s, ro) = m.drv.map(x.h_dom, x.h_buf, rt::abi::iodomain::READ)?;
            ensure_eq!(
                s,
                Some(S::Ok),
                "{d:?}: the driver maps its buffer read-only"
            );
            let r = dut_refused(m, d, ro, None, "a read-only mapping");
            ensure_eq!(
                m.drv.unmap(x.h_dom, ro)?,
                Some(S::Ok),
                "{d:?}: and unmaps it"
            );
            r?;
        }
        Ok(())
    })
}

/// Row: use after its own unmap: the driver maps its buffer again, the device writes there (it lands), the driver unmaps it, the
/// device writes there again: refused, and the buffer keeps what it held after the first write.
fn hostile_dma_after_its_own_unmap() -> Outcome {
    with_matrix(|m| {
        for (d, salt) in [(Dut::Rng, 1), (Dut::Blk, 2)] {
            let x = m.dev(d);
            let (s, io) = m.drv.map(x.h_dom, x.h_buf, 3)?;
            ensure_eq!(s, Some(S::Ok), "{d:?}: the driver maps its buffer again");
            let served = dut_write(m, d, io)?;
            ensure!(
                served && !page_intact(x.buf_va, salt),
                "{d:?}: the write through the second mapping lands"
            );
            ensure_eq!(m.drv.unmap(x.h_dom, io)?, Some(S::Ok), "{d:?}: unmap it");
            fill_page(x.buf_va, salt);
            dut_refused(m, d, io, None, "after its own unmap")?;
            ensure!(
                page_intact(x.buf_va, salt),
                "{d:?}: the unmapped write reached the buffer"
            );
        }
        Ok(())
    })
}

/// Row: use after its supervisor destroyed the domain. The device is quarantined (IOMMU-SPEC 6.3, A14): its requests are refused
/// and not reported, and the buffer (still the driver's, mapped here) keeps its contents. A request cannot be fetched (its rings are
/// gone with the domain), so the device's writes are shown by its configuration-change message (`dut_message`), a write that needs no
/// fetch: first while the domain lives, aimed at its own buffer, where it lands (the rig is not broken), then quarantined, at its old
/// buffer address and at another frame.
fn hostile_dma_after_the_domain_is_destroyed() -> Outcome {
    for d in [Dut::Rng, Dut::Blk] {
        with_matrix(|m| {
            let x = m.dev(d);
            let salt = if d == Dut::Rng { 1 } else { 2 };
            dut_message(m, d, x.buf_io, 0x5157_0a1e)?;
            msi_settle();
            ensure_eq!(
                rd64(x.buf_va) as u32,
                0x5157_0a1e,
                "{d:?}: its message lands in its buffer while the domain lives"
            );
            fill_page(x.buf_va, salt);
            drain_domain_faults(x.sup);
            let base = (sys::debug_stat(162), sys::debug_stat(140));
            ok(
                sys::iodomain_destroy(x.sup),
                "the supervisor destroys the domain",
            )?;
            let faults = iommu_faults();
            for (target, what) in [(x.buf_io, "its old buffer"), (m.pa, "another frame")] {
                let served = dut_write(m, d, target)?;
                ensure!(
                    !served,
                    "{d:?}: the quarantined device was served at {what}"
                );
                dut_message(m, d, target, 0x5157_0a1e)?;
            }
            msi_settle();
            ensure_eq!(
                iommu_faults(),
                faults,
                "{d:?}: a quarantined device's faults are not reported (A14)"
            );
            ensure!(
                page_intact(x.buf_va, salt),
                "{d:?}: the destroyed domain's buffer changed"
            );
            bystanders_intact(m, None, base, "after the destroy")
        })?;
    }
    Ok(())
}

/// Row: use after its supervisor died. The control handle goes to a child that is killed; the domain lives on through the driver's
/// handle (IOMMU-SPEC 4.5: the last handle destroys it), so the device still reaches its buffer and nothing else (a fault, nobody to
/// read it); when the driver dies too, the domain goes and the device reaches nothing, unreported.
fn hostile_dma_after_its_supervisor_died() -> Outcome {
    for d in [Dut::Rng, Dut::Blk] {
        let domains = sys::debug_stat(159);
        with_matrix(|m| {
            let sup = match d {
                Dut::Rng => core::mem::take(&mut m.rng.sup),
                Dut::Blk => core::mem::take(&mut m.blk.sup),
            };
            let (supervisor, ch) = launch(15)?;
            let w = sys::channel_write(ch, [0u8; 4].as_ptr(), 4, [sup].as_ptr(), 1);
            let killed = sys::process_kill(supervisor, 9);
            let ended = wait_ended(supervisor, 1000);
            sys::handle_close(supervisor);
            sys::handle_close(ch);
            ensure_eq!(
                st(w),
                Some(S::Ok),
                "the supervisor is given the control handle"
            );
            ok(killed, "kill the supervisor")?;
            ended?;
            ensure_eq!(
                sys::debug_stat(159),
                domains + 2,
                "{d:?}: the domain outlives its supervisor while the driver holds it"
            );
            let x = MDevAt {
                did: m.dev(d).did,
                buf_io: m.dev(d).buf_io,
                buf_va: m.dev(d).buf_va,
            };
            let salt = if d == Dut::Rng { 1 } else { 2 };
            ensure!(
                dut_write(m, d, x.buf_io)?,
                "{d:?}: the device reaches its buffer"
            );
            fill_page(x.buf_va, salt);
            let base = (sys::debug_stat(162), sys::debug_stat(140));
            let before = iommu_faults();
            dut_write(m, d, m.pa)?;
            ensure!(
                iommu_wait_faults(before).is_some(),
                "{d:?}: the write outside its mapping faulted"
            );
            ensure_eq!(
                iommu_last_fault().0,
                x.did,
                "{d:?}: the fault names the device"
            );
            bystanders_intact(m, None, base, "after the supervisor died")?;
            // the driver dies too: the last handle goes and the domain with it
            let drv = core::mem::replace(
                &mut m.drv,
                Hostile {
                    p: 0,
                    ch: 0,
                    hs: Vec::new(),
                },
            );
            drv.end();
            let t0 = sys::clock_get();
            while sys::debug_stat(159) != domains + 1 && sys::clock_get() < t0 + 2000 {
                sys::thread_yield();
            }
            ensure_eq!(
                sys::debug_stat(159),
                domains + 1,
                "{d:?}: the domain went with its last handle"
            );
            // (no driver is left to drive the device: the suite rings it with the dead driver's descriptor, as a device that kept a
            // stale request would)
            let faults = iommu_faults();
            ensure!(
                !dut_write_by_the_suite(m, d, x.buf_io)?,
                "{d:?}: the device reached its old buffer after both died"
            );
            // (and a write that needs no fetch from the rings: its message, at its old buffer)
            dut_message(m, d, x.buf_io, 0x5157_0a1e)?;
            msi_settle();
            ensure_eq!(
                iommu_faults(),
                faults,
                "{d:?}: quarantined: nothing reported"
            );
            ensure!(page_intact(x.buf_va, salt), "{d:?}: the old buffer changed");
            Ok(())
        })?;
    }
    Ok(())
}

/// A message row on the entropy function (its MSI-X entry 0 says `(address, data)`, which the rig writes: the worst case of
/// Q-IOMMU-MSI 6): an IOMMU fault naming the function whose cause is `cause` (any when None), and the bystanders intact.
fn msi_refused(m: &Matrix, address: u64, data: u32, cause: Option<u64>, what: &str) -> Outcome {
    let base = (sys::debug_stat(162), sys::debug_stat(140));
    let before = iommu_faults();
    msi_send(m.f, &m.rng_domain(), address, data)?;
    ensure!(
        iommu_wait_faults(before).is_some(),
        "{what}: the message raised no fault"
    );
    let (did, _, c, ttype) = iommu_last_fault();
    ensure_eq!(
        (did, ttype),
        (IOMMU_TEST_DID, TTYPE_WRITE),
        "{what}: the fault names the function, a write"
    );
    if let Some(cause) = cause {
        ensure_eq!(c, cause, "{what}: the cause");
    }
    msi_settle();
    ensure!(
        drain_domain_faults(m.blk.sup).is_empty(),
        "{what}: the disk's domain has records"
    );
    drain_domain_faults(m.rng.sup);
    bystanders_intact(m, Some(Dut::Rng), base, what)
}

/// Message row: another device's vector page and identity (the disk's granted vector: where the kernel sends its notice, as
/// `debug_stat` 148 says). Refused as a translation (no domain maps an interrupt file) and the vector does not fire.
fn hostile_msi_at_another_device_vector() -> Outcome {
    with_matrix(|m| {
        let route = vector_route(m.blk.irq);
        ensure!(route != u64::MAX, "the disk's vector has a route");
        let (file, id) = (route >> 16, route & 0xffff);
        msi_refused(
            m,
            IMSIC_S + file * 4096,
            id as u32,
            None,
            "another device's vector",
        )
    })
}

/// Message row: the inter-hart identity on every hart's file.
fn hostile_msi_at_the_inter_hart_identity() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let harts = hart_count(res);
    sys::handle_close(res);
    with_matrix(|m| {
        for h in 0..harts {
            msi_refused(
                m,
                IMSIC_S + h * 4096,
                1,
                None,
                &alloc::format!("the IPI identity of hart {h}"),
            )?;
        }
        Ok(())
    })
}

/// Message row: the IOMMU's own interrupt identities (its fault queue's and command queue's wired sources, which the kernel routes:
/// every wired source with a route that the suite did not make, `debug_stat` 145).
fn hostile_msi_at_the_iommu_identity() -> Outcome {
    with_matrix(|m| {
        let mut n = 0;
        for src in 1..128u64 {
            let r = rt::syscall(
                rt::abi::syscall::DEBUG_STAT,
                [rt::debug::handle() as usize, 145, src as usize, 0, 0, 0],
            );
            if r.status != 0 || r.a1 == usize::MAX || src == u64::from(RTC_IRQ) {
                continue;
            }
            let (file, id) = ((r.a1 as u64) >> 16, r.a1 as u64 & 0xffff);
            msi_refused(
                m,
                IMSIC_S + file * 4096,
                id as u32,
                None,
                &alloc::format!("the kernel's source {src} (identity {id} on file {file})"),
            )?;
            n += 1;
        }
        ensure!(n >= 1, "the IOMMU's fault source has a route");
        Ok(())
    })
}

/// Message row: an ungranted vector of its own (page 3 of its window): MSI entry not valid (cause 262).
fn hostile_msi_on_an_ungranted_vector() -> Outcome {
    with_matrix(|m| {
        let a = m.rng.address + 3 * 4096;
        msi_refused(m, a, 1, Some(CAUSE_MSI_INVALID), "an ungranted vector")
    })
}

/// Message row: a granted vector with a wrong data word. No fault by design (IOMMU-SPEC 8.4: the word sets a bit in the vector's own
/// file, which sends nothing); the vector does not fire for it, and the stray bit is found and counted (`debug_stat` 147) at the
/// acknowledgement of a real message.
fn hostile_msi_with_a_wrong_data_word() -> Outcome {
    with_matrix(|m| {
        let base = (sys::debug_stat(162), sys::debug_stat(140));
        let (before, strays) = (iommu_faults(), sys::debug_stat(147));
        msi_send(m.f, &m.rng_domain(), m.rng.address, 6)?;
        msi_settle();
        ensure_eq!(iommu_faults(), before, "a wrong data word is no fault");
        bystanders_intact(m, Some(Dut::Rng), base, "a wrong data word")?;
        msi_send(m.f, &m.rng_domain(), m.rng.address, 1)?;
        ensure_eq!(
            st(sys::object_wait(
                m.rng.irq,
                sig::INTERRUPT,
                sys::clock_get() + 2000
            )),
            Some(S::Ok),
            "the right word fires the vector"
        );
        ok(sys::interrupt_ack(m.rng.irq), "acknowledge")?;
        ensure_eq!(
            sys::debug_stat(147),
            strays + 1,
            "the stray bit is counted once"
        );
        Ok(())
    })
}

/// Message row: after its vectors were revoked (the last handle to the interrupt closed): MSI entry not valid, nothing delivered.
fn hostile_msi_after_the_vector_is_revoked() -> Outcome {
    with_matrix(|m| {
        sys::handle_close(core::mem::take(&mut m.rng.irq));
        msi_refused(
            m,
            m.rng.address,
            1,
            Some(CAUSE_MSI_INVALID),
            "a revoked vector",
        )
    })
}

/// The claim rig (see the section's comment): `body` gets the hostile driver, holding the scratch disk's claim (its handles in
/// `hs[0..]` in the claim's order: the BAR, the domain, the queue memory, the data buffer, the interrupts) and then a duplicate of the
/// data buffer without DMA and one without WRITE; and the driver's job. No disk (the no-disk boot): nothing to do.
fn with_claimed_hostile(body: impl FnOnce(u64, &Hostile, u64, &mut Claimed) -> Outcome) -> Outcome {
    if !pci_disks_present()? {
        return Ok(());
    }
    with_devmgr(|res, mgr| {
        let mut b = claim_and_bring_up(mgr, 1, PCI_BLK_DID as u16, 0, &blk_vectors())?;
        let r = (|| {
            let data = b.hs[3];
            let nodma = ok(
                sys::handle_duplicate(
                    data,
                    Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0,
                ),
                "the buffer without DMA",
            )?
            .a1 as u64;
            let ro = ok(
                sys::handle_duplicate(
                    data,
                    Rights::DMA.0 | Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0,
                ),
                "the buffer without WRITE",
            )?
            .a1 as u64;
            let mut hs = core::mem::take(&mut b.hs);
            hs.extend([nodma, ro]);
            let drv = Hostile::start(b.job, &hs)?;
            let r = body(res, &drv, b.job, &mut b);
            drv.end();
            r
        })();
        b.drop_all();
        r
    })
}

/// The driver's own handles, with no device involved: each refused, and nothing changes (its job's I/O pages, the live mappings).
fn hostile_own_handle_row(
    what: &str,
    want: S,
    call: impl Fn(&Hostile) -> Result<(Option<S>, u64, u64), Detail>,
) -> Outcome {
    with_claimed_hostile(|_, drv, job, _| {
        let before = (
            sys::job_info(job, 12).a1,
            sys::debug_stat(160),
            sys::debug_stat(159),
        );
        let (s, _, _) = call(drv)?;
        ensure_eq!(s, Some(want), "the driver cannot {what}");
        ensure_eq!(
            (
                sys::job_info(job, 12).a1,
                sys::debug_stat(160),
                sys::debug_stat(159)
            ),
            before,
            "{what}: nothing changed"
        );
        // and its domain handle still does what it may
        let (s, io) = drv.map(drv.hs[1], drv.hs[3], 3)?;
        ensure_eq!(s, Some(S::Ok), "{what}: the driver still maps its buffer");
        ensure_eq!(drv.unmap(drv.hs[1], io)?, Some(S::Ok), "and unmaps it");
        Ok(())
    })
}

fn hostile_cannot_map_without_the_dma_right() -> Outcome {
    hostile_own_handle_row("map a VMO it holds without DMA", S::AccessDenied, |d| {
        d.call(
            rt::abi::syscall::IODOMAIN_MAP,
            [d.hs[1], d.hs[d.hs.len() - 2], 0, 1, 3, 0],
        )
    })
}

fn hostile_cannot_map_writable_without_write() -> Outcome {
    hostile_own_handle_row(
        "map read-write a buffer it holds without WRITE",
        S::AccessDenied,
        |d| {
            d.call(
                rt::abi::syscall::IODOMAIN_MAP,
                [d.hs[1], d.hs[d.hs.len() - 1], 0, 1, 3, 0],
            )
        },
    )
}

fn hostile_cannot_destroy_its_domain() -> Outcome {
    hostile_own_handle_row("destroy its domain", S::AccessDenied, |d| {
        d.call(rt::abi::syscall::IODOMAIN_DESTROY, [d.hs[1], 0, 0, 0, 0, 0])
    })
}

fn hostile_cannot_grant_itself_a_vector() -> Outcome {
    hostile_own_handle_row("grant itself a vector", S::AccessDenied, |d| {
        d.call(
            rt::abi::syscall::IODOMAIN_GRANT_VECTOR,
            [d.hs[1], 2, 0, 0, 0, 0],
        )
    })
}

fn hostile_cannot_read_or_wait_for_its_faults() -> Outcome {
    hostile_own_handle_row("read its device's faults", S::AccessDenied, |d| {
        d.call(
            rt::abi::syscall::IODOMAIN_FAULT_READ,
            [d.hs[1], 0, 0, 0, 0, 0],
        )
    })?;
    hostile_own_handle_row("wait for its device's faults", S::AccessDenied, |d| {
        d.call(
            rt::abi::syscall::OBJECT_WAIT,
            [d.hs[1], u64::from(sig::DEVICE_FAULT), 0, 0, 0, 0],
        )
    })
}

/// Own-handle row (IOMMU-SPEC 4.8, INVARIANTS D2): the driver learns no physical address by any way it has. The library's two old
/// calls answer NOT_SUPPORTED without entering the kernel; the retired numbers themselves, issued raw with each of its handles, are
/// BAD_SYSCALL; the test kernel's physical-address selector (`debug_stat` 164) refuses every handle it holds (none is a debug
/// resource) and no handle; and it can obtain neither the debug resource nor the root resource.
fn hostile_learns_no_physical_address() -> Outcome {
    hostile_own_handle_row(
        "learn its buffer's physical address",
        S::NotSupported,
        |d| d.op(1, 0, [d.hs[3], 0, 0, 0, 0, 0]),
    )?;
    hostile_own_handle_row("make a DMA VMO by the old call", S::NotSupported, |d| {
        d.op(2, 0, [d.hs[3], 1, 0, 0, 0, 0])
    })?;
    with_claimed_hostile(|_, drv, _, _| {
        for &(name, nr) in rt::abi::syscall::RETIRED {
            for &h in drv.hs.iter().chain(&[0u64]) {
                let (s, _, _) = drv.call(nr, [h, 1, 0, 0, 0, 0])?;
                ensure_eq!(
                    s,
                    Some(S::BadSyscall),
                    "the retired {name} ({nr}) raw, with handle {h}"
                );
            }
        }
        // (a refused call leaves `a1` as it was, the selector: the status is the check)
        for &h in &drv.hs {
            let (s, _, _) = drv.call(rt::abi::syscall::DEBUG_STAT, [h, 164, 0x1_0000, 0, 0, 0])?;
            ensure_eq!(
                s,
                Some(S::WrongType),
                "debug_stat 164 with its handle {h} (no debug resource)"
            );
        }
        let (s, _, _) = drv.call(rt::abi::syscall::DEBUG_STAT, [0, 164, 0x1_0000, 0, 0, 0])?;
        ensure_eq!(s, Some(S::BadHandle), "debug_stat 164 with no handle");
        for (nr, what) in [
            (rt::abi::syscall::DEBUG_ROOT, "the debug resource"),
            (rt::abi::syscall::RESOURCE_ROOT, "the root resource"),
        ] {
            let (s, a1, _) = drv.call(nr, [0; 6])?;
            ensure_eq!(
                (s, a1),
                (Some(S::AccessDenied), 0),
                "the driver asks for {what}"
            );
        }
        Ok(())
    })
}

/// Limits row (OD166 point 4): the driver maps its buffer until refused. NO_MEMORY exactly at the limit `devmgr` set on its job
/// (`job_info` 12), and then the mappings are given back with nothing leaked; another driver's job (the entropy function claimed
/// beside it) is not disturbed, before or after.
fn hostile_maps_until_its_job_refuses() -> Outcome {
    with_claimed_hostile(|_, drv, job, _| {
        let doms = (sys::debug_stat(159), sys::debug_stat(160));
        let (used, limit) = (
            sys::job_info(job, 12).a1 as u64,
            sys::job_info(job, 12).a2 as u64,
        );
        ensure!(
            limit != u64::MAX && limit >= used,
            "the driver's job has a finite limit on I/O-mapped pages: {used} of {limit}"
        );
        let mut ios = Vec::new();
        let r = (|| {
            loop {
                let (s, io) = drv.map(drv.hs[1], drv.hs[3], 3)?;
                match s {
                    Some(S::Ok) => ios.push(io),
                    Some(S::NoMemory) => break,
                    other => ensure!(false, "map {}: {other:?}", ios.len()),
                }
                ensure!(ios.len() as u64 <= limit, "more mappings than the limit");
            }
            ensure_eq!(
                used + ios.len() as u64,
                limit,
                "refused exactly at the job's limit"
            );
            ensure_eq!(
                sys::job_info(job, 12).a1 as u64,
                limit,
                "the job's usage is at its limit"
            );
            Ok(())
        })();
        for &io in &ios {
            ensure_eq!(drv.unmap(drv.hs[1], io)?, Some(S::Ok), "unmap {io:#x}");
        }
        r?;
        ensure_eq!(
            (
                sys::job_info(job, 12).a1 as u64,
                sys::debug_stat(159),
                sys::debug_stat(160)
            ),
            (used, doms.0, doms.1),
            "every mapping came back"
        );
        Ok(())
    })
}

/// Limits row: a child job with no limits of its own is held to its parent's (the limits are hierarchical, IOMMU-SPEC 5.3). A domain
/// charged to a child of a driver's job maps and holds vectors only up to the parent's limits; then the child's usage is gone.
fn hostile_child_job_is_held_to_the_parents_limits() -> Outcome {
    let doms = (sys::debug_stat(159), sys::debug_stat(160));
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let r = with_test_function(|res, _, _| {
        let root = root_job()?;
        let dj = job(root, u64::MAX, u64::MAX, u64::MAX);
        sys::handle_close(root);
        let dj = dj?;
        let r = (|| {
            ok(sys::job_set_io_limits(dj, 3, 2), "the parent's limits")?;
            let cj = job(dj, u64::MAX, u64::MAX, u64::MAX)?;
            let r = (|| {
                ensure_eq!(
                    (
                        sys::job_info(cj, 12).a2 as u64,
                        sys::job_info(cj, 13).a2 as u64
                    ),
                    (3, 2),
                    "the child's effective limits are the parent's"
                );
                let sup = ok(
                    sys::iodomain_create(res, IOMMU_TEST_DID, cj),
                    "a domain charged to the child",
                )?
                .a1 as u64;
                let buf = ok(sys::vmo_create_dma_in(res, 1, cj), "a buffer")?.a1 as u64;
                let r = (|| {
                    let drv = Hostile::start(
                        cj,
                        &[
                            ok(
                                sys::handle_duplicate(
                                    sup,
                                    Rights::IO_MAP.0 | Rights::IO_UNMAP.0 | Rights::TRANSFER.0,
                                ),
                                "dup",
                            )?
                            .a1 as u64,
                            ok(sys::handle_duplicate(buf, Rights::SAME.0), "dup")?.a1 as u64,
                        ],
                    )?;
                    let r = (|| {
                        let mut n = 0;
                        loop {
                            let (s, _) = drv.map(drv.hs[0], drv.hs[1], 3)?;
                            match s {
                                Some(S::Ok) => n += 1,
                                Some(S::NoMemory) => break,
                                other => ensure!(false, "map {n}: {other:?}"),
                            }
                            ensure!(n <= 3, "past the parent's limit");
                        }
                        ensure_eq!(n, 3, "the child maps up to the parent's limit of 3 pages");
                        ensure_eq!(sys::job_info(dj, 12).a1 as u64, 3, "counted on the parent");
                        let mut irqs = Vec::new();
                        for i in 0..4 {
                            let g = sys::iodomain_grant_vector(sup, i);
                            if st(g) == Some(S::Ok) {
                                irqs.push(g.a1 as u64);
                            } else {
                                ensure_eq!(st(g), Some(S::NoMemory), "vector {i}: the limit");
                                break;
                            }
                        }
                        let held = irqs.len();
                        for h in irqs {
                            sys::handle_close(h);
                        }
                        ensure_eq!(
                            held,
                            2,
                            "the child's domain holds the parent's limit of 2 vectors"
                        );
                        Ok(())
                    })();
                    drv.end();
                    r
                })();
                sys::handle_close(buf);
                sys::handle_close(sup);
                r
            })();
            let t0 = sys::clock_get();
            while (sys::job_info(dj, 12).a1, sys::job_info(dj, 13).a1) != (0, 0)
                && sys::clock_get() < t0 + 2000
            {
                sys::thread_yield();
            }
            let back = (sys::job_info(dj, 12).a1, sys::job_info(dj, 13).a1);
            sys::handle_close(cj);
            r?;
            ensure_eq!(back, (0, 0), "the parent's usage comes back");
            Ok(())
        })();
        sys::handle_close(dj);
        r
    });
    r.and(domains_at_baseline(doms))
        .and(vectors_at_baseline(live, ids))
}

/// Limits row: every driver job `devmgr` claims for has finite limits of both kinds (`job_info` 12 and 13), seen on the jobs the
/// suite names in its claims: the block function (`ClaimPci`) and the entropy source (`ClaimPci` of its requester id). (`OpenBlock`,
/// `ClaimNet` and `ClaimRng` set them through the same `pcicore::claim`, whose host test checks the limits for every class.)
fn every_driver_job_has_finite_io_limits() -> Outcome {
    if !pci_disks_present()? {
        return Ok(());
    }
    with_devmgr(|_, mgr| {
        for (k, rid, vectors) in [
            (0u64, PCI_BLK_DID as u16, blk_vectors()),
            (
                1,
                PCI_RNG_RID,
                virtio::VectorMap {
                    config: virtio::NO_VECTOR,
                    queues: vec![0],
                },
            ),
        ] {
            let b = claim_and_bring_up(mgr, 10 + k as u32, rid, k, &vectors)?;
            let r = (|| {
                for which in [12u64, 13] {
                    let i = sys::job_info(b.job, which);
                    ensure_eq!(st(i), Some(S::Ok), "job_info {which}");
                    ensure!(
                        (i.a2 as u64) < 1 << 20 && i.a2 as u64 >= i.a1 as u64,
                        "{rid:#x}: the driver's job's limit {which} is finite: {} of {:#x}",
                        i.a1,
                        i.a2
                    );
                }
                Ok(())
            })();
            b.drop_all();
            r?;
        }
        Ok(())
    })
}

/// T42 (2026-11-24) section 4, against the kernel's mechanism: a supervisor (`devmgr`) and its `blkd` are killed with a read in flight
/// (the device writing the driver's buffer). The domain goes with its last handle, after its fence (IOMMU-SPEC 6.3), and only then
/// are the buffer's frames freed. The test then fills memory with a pattern (fresh DMA VMOs: the frames the allocator gives back first
/// are the ones just freed), waits well past the request, and checks the pattern; then every baseline. The negative control
/// `fault-iommu-skip-inval` (an unmap that frees without the invalidation and the fence) is run against it by hand (TEST-TOOLS.md):
/// as with `iodomain_unmap_stops_the_device`, QEMU's device finishes a request before the kernel frees anything far more often than
/// not, so the control fails here only when the request is still running at the free.
fn dma_survives_a_dead_supervisor() -> Outcome {
    let Some(rig) = blk_rig()? else {
        return Ok(());
    };
    let doms = (sys::debug_stat(159), sys::debug_stat(160));
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let leaked = sys::debug_stat(158);
    let r = (|| {
        let d = rig.spawn()?;
        // a request answered: the driver has the device up; then a long read in flight
        let (s, _) = blk_call(
            d.ch,
            1,
            blk::Op::Read {
                sector: 0,
                count: 1,
            },
        )?;
        ensure_eq!(s, blk::Status::Ok, "a first read");
        let req = blk::Request {
            txid: 2,
            op: blk::Op::Read {
                sector: 100,
                count: 16,
            },
        }
        .encode();
        ensure_eq!(
            st(sys::channel_write(
                d.ch,
                req.as_ptr(),
                req.len(),
                core::ptr::null(),
                0
            )),
            Some(S::Ok),
            "a read in flight"
        );
        // both die at once: the driver and the supervisor that holds the domain's control handle
        let k1 = sys::process_kill(d.p, 9);
        let k2 = sys::process_kill(rig.mgr, 9);
        ensure_eq!((st(k1), st(k2)), (Some(S::Ok), Some(S::Ok)), "kill both");
        wait_ended(d.p, 2000)?;
        wait_ended(rig.mgr, 2000)?;
        drop(d);
        // reused frames, filled
        let res = rig.res;
        let mut fills = Vec::new();
        let r = (|| {
            for k in 0..8u64 {
                let v = ok(sys::vmo_create_dma_in(res, 4, 0), "fill memory")?.a1 as u64;
                fills.push(v);
                map_here(v, SCRATCH_VA + k * 4 * 4096, 4, PF_R | PF_W)?;
                for p in 0..4 {
                    fill_page(SCRATCH_VA + (k * 4 + p) * 4096, k * 4 + p);
                }
            }
            nap_ticks(300);
            for k in 0..32u64 {
                ensure!(
                    page_intact(SCRATCH_VA + k * 4096, k),
                    "page {k} of the reused memory changed after the supervisor and the driver died"
                );
            }
            Ok(())
        })();
        for (k, v) in fills.into_iter().enumerate() {
            unmap_here(SCRATCH_VA + k as u64 * 4 * 4096);
            sys::handle_close(v);
        }
        r
    })();
    // the supervisor that would have reset the function is dead: it is left running (DRIVER_OK, MSI-X on, bus mastering on, every
    // transaction refused), so the suite stands in for the teardown `devmgr` would have done, for the tests after this one
    let quiet = with_function(PCI_BLK_SLOT, PCI_BLK_ID, |_, _, _| Ok(()));
    drop(rig);
    r?;
    quiet?;
    domains_at_baseline(doms)?;
    vectors_at_baseline(live, ids)?;
    ensure_eq!(sys::debug_stat(158), leaked, "no frame leaked");
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
    let have_disk = pci_disks_present()?;
    // start the manager, in a job of its own: when it and its driver are gone nothing of theirs is left (no leaked process, thread,
    // page or object)
    let (mine, mgr, mj) = start_devmgr_in_job(res)?;
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

/// `ListPci` on `ch`: the manager's entries.
fn pci_list(ch: u64, txid: u32) -> Result<Vec<proto::devmgr::PciEntry>, Detail> {
    let req = proto::devmgr::encode_list_pci(txid);
    ensure_eq!(
        st(sys::channel_write(
            ch,
            req.as_ptr(),
            req.len(),
            core::ptr::null(),
            0
        )),
        Some(S::Ok),
        "send ListPci"
    );
    ensure_eq!(
        st(sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000)),
        Some(S::Ok),
        "the manager answers ListPci"
    );
    let mut buf = vec![0u8; 12 + proto::devmgr::PCI_ENTRY_LEN * proto::devmgr::MAX_PCI_ENTRIES];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
    ensure_eq!(st(r), Some(S::Ok), "read the list");
    let (t, status, entries) =
        proto::devmgr::decode_list_pci_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!((t, status), (txid, proto::devmgr::Status::Ok), "the list");
    Ok(entries)
}

/// `ClaimPci` of `rid` on `ch`, carrying `job` if any: (the status, the claim, the handles).
fn pci_claim(
    ch: u64,
    txid: u32,
    rid: u16,
    job: Option<u64>,
) -> Result<
    (
        proto::devmgr::Status,
        Option<proto::devmgr::PciClaim>,
        Vec<u64>,
    ),
    Detail,
> {
    let req = proto::devmgr::encode_claim_pci(txid, rid);
    let hs: Vec<u64> = job.into_iter().collect();
    ensure_eq!(
        st(sys::channel_write(
            ch,
            req.as_ptr(),
            req.len(),
            hs.as_ptr(),
            hs.len()
        )),
        Some(S::Ok),
        "send ClaimPci"
    );
    ensure_eq!(
        st(sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000)),
        Some(S::Ok),
        "the manager answers ClaimPci"
    );
    let mut buf = [0u8; 128];
    let mut got = [0u64; 16];
    let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), got.as_mut_ptr(), got.len());
    ensure_eq!(st(r), Some(S::Ok), "read the claim's reply");
    let (t, status, claim) =
        proto::devmgr::decode_claim_pci_reply(&buf[..r.a1]).ok_or_else(|| msg_at(line!()))?;
    ensure_eq!(t, txid, "the reply answers this transaction");
    let hs = got[..r.a2].to_vec();
    ensure_eq!(
        hs.len(),
        claim.as_ref().map_or(0, |c| c.handles()),
        "the handles the reply says"
    );
    if status == proto::devmgr::Status::Ok {
        // the second message: bus mastering is on, the device may be started
        ensure_eq!(
            st(sys::object_wait(ch, sig::READABLE, sys::clock_get() + 5000)),
            Some(S::Ok),
            "the manager says the device may start"
        );
        let r = sys::channel_read(ch, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut(), 0);
        ensure_eq!(st(r), Some(S::Ok), "read the second message");
        ensure_eq!(
            proto::devmgr::decode_claim_pci_started(&buf[..r.a1]),
            Some(txid),
            "the claim's second message"
        );
    }
    Ok((status, claim, hs))
}

/// A child job of the root job for a driver, and a duplicate of it to send (the claim takes the handle it is sent).
fn driver_job() -> Result<(u64, u64), Detail> {
    let root = root_job()?;
    let j = job(root, u64::MAX, u64::MAX, u64::MAX);
    sys::handle_close(root);
    let j = j?;
    let d = ok(
        sys::handle_duplicate(j, Rights::SAME.0),
        "dup the driver's job",
    )?
    .a1 as u64;
    Ok((j, d))
}

/// Whether bus mastering is on for the block function: its command register, read through the root resource.
fn blk_bus_master(res: u64) -> Result<bool, Detail> {
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let cfg = ok(
        sys::vmo_create_physical(res, PCI_ECAM + (PCI_BLK_SLOT << 15), 4096),
        "the block function's configuration page",
    )?
    .a1 as u64;
    let m = sys::process_map(me, cfg, PCI_CFG_VA, 0, 4096, PF_R);
    sys::handle_close(cfg);
    ensure_eq!(st(m), Some(S::Ok), "map it");
    let on = cfg_rd16(4) & (1 << 2) != 0;
    sys::process_unmap(me, PCI_CFG_VA);
    sys::handle_close(me);
    Ok(on)
}

/// IOMMU8, IOMMU9 (IOMMU-SPEC 8, 9): `devmgr` lists bus 0 as the command line built it (the four virtio functions, each claimable),
/// and hands a driver a claim: the BAR VMO with the virtio structures, the domain with IO_MAP
/// and IO_UNMAP only, the DMA VMOs, one interrupt handle per vector of the class (wait and acknowledge, no duplicate), the driver's
/// job limited; bus mastering on after the reply. The claim reads the disk through the IOMMU; a second claim is Busy, the host bridge is Refused, a claim without its job is
/// BadRequest; when the claimant's connection closes the claim is released (bus mastering off, the domain destroyed) and can be
/// taken again.
fn pci_devices_are_found_and_claimed() -> Outcome {
    use proto::devmgr as dm;
    use proto::devmgr::Status as D;
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let (mine, mgr, mj) = start_devmgr_in_job(res)?;
    // every handle the test may still hold when a check fails is closed on the way out (a claim left open would keep its domain
    // alive into later tests of the same boot)
    let mut held: Vec<u64> = Vec::new();
    let disks = pci_disks_present()?;
    let r = (|| {
        // the list: the host bridge and the four functions of the harness (two in the no-disk boot), nothing else, in requester-id
        // order
        let list = pci_list(mine, 1)?;
        let rids: Vec<u16> = list.iter().map(|e| e.rid).collect();
        ensure_eq!(
            rids,
            if disks {
                vec![0, 0x20, 0x28, 0x30, 0x38]
            } else {
                vec![0, 0x20, 0x38]
            },
            "bus 0: {list:x?}"
        );
        let want = dm::PCI_CLAIMABLE | dm::PCI_MSIX | dm::PCI_VIRTIO | dm::PCI_ACCESS_PLATFORM;
        for e in &list {
            match e.rid {
                0 => {
                    ensure_eq!((e.vendor, e.device), (0x1b36, 0x0008), "the host bridge");
                    ensure_eq!(
                        e.flags & dm::PCI_CLAIMABLE,
                        0,
                        "the host bridge is not claimable"
                    );
                }
                rid => {
                    let dev = match rid {
                        0x20 => 0x1044,
                        0x38 => 0x1041,
                        _ => 0x1042,
                    };
                    ensure_eq!(
                        (e.vendor, e.device),
                        (0x1af4, dev),
                        "the function at {rid:#x}"
                    );
                    ensure_eq!(
                        e.flags,
                        want,
                        "the function at {rid:#x} is claimable, MSI-X, virtio, ACCESS_PLATFORM"
                    );
                }
            }
        }
        if !disks {
            // the no-disk boot: nothing to open on either bus
            let (status, h) = devmgr_open_at(mine, 30, 0)?;
            ensure_eq!((status, h), (D::NoDevice, None), "no disk to open");
            return Ok(());
        }
        // a second client, whose connection will hold the claim
        let (c2, c2_theirs) = channel()?;
        held.push(c2);
        let req = dm::encode_connect(2);
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
        ensure_eq!(
            st(sys::object_wait(
                mine,
                sig::READABLE,
                sys::clock_get() + 5000
            )),
            Some(S::Ok),
            "the manager answers Connect"
        );
        let mut rb = [0u8; 16];
        let r = sys::channel_read(mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
        ensure_eq!(dm::decode_reply(&rb[..r.a1]), Some((2, D::Ok)), "connected");
        // refusals: no job, a function that cannot be claimed, a function that is not there
        let (s, _, _) = pci_claim(c2, 3, 0x28, None)?;
        ensure_eq!(s, D::BadRequest, "a claim without the driver's job");
        let (dj, send) = driver_job()?;
        held.push(dj);
        let (s, _, _) = pci_claim(c2, 4, 0, Some(send))?;
        ensure_eq!(s, D::Refused, "the host bridge");
        let send = ok(sys::handle_duplicate(dj, Rights::SAME.0), "dup")?.a1 as u64;
        let (s, _, _) = pci_claim(c2, 5, 0x48, Some(send))?;
        ensure_eq!(s, D::NoDevice, "no function at 00:09.0");
        ensure!(
            !blk_bus_master(res)?,
            "bus mastering is off before the claim"
        );
        // the claim
        let send = ok(sys::handle_duplicate(dj, Rights::SAME.0), "dup")?.a1 as u64;
        let (s, claim, hs) = pci_claim(c2, 6, 0x28, Some(send))?;
        held.extend(&hs);
        ensure_eq!(s, D::Ok, "the block function is claimed");
        let c = claim.ok_or_else(|| msg_at(line!()))?;
        ensure_eq!((c.vendor, c.device), (0x1af4, 0x1042), "the claim's ids");
        ensure_eq!(
            c.dma_pages,
            vec![1, 4],
            "the block class's DMA VMOs: the rings and the data pages"
        );
        ensure_eq!(
            c.irqs,
            vec![
                dm::PciIrq {
                    entry: 0,
                    what: dm::IRQ_CONFIG
                },
                dm::PciIrq { entry: 1, what: 0 }
            ],
            "the block class's vectors: the configuration change on entry 0, queue 0 on entry 1"
        );
        ensure_eq!(
            (c.handles(), hs.len()),
            (6, 6),
            "the BAR, the domain, two DMA VMOs, two interrupts"
        );
        let (bar, dom, rings, data) = (hs[0], hs[1], hs[2], hs[3]);
        for &h in &hs[4..] {
            ensure!(vector_route(h) != u64::MAX, "a live vector");
            ensure_eq!(
                st(sys::interrupt_ack(h)),
                Some(S::BadState),
                "an interrupt handle acknowledges (nothing fired yet)"
            );
            ensure_eq!(
                st(sys::handle_duplicate(h, Rights::WAIT.0)),
                Some(S::AccessDenied),
                "the driver's interrupt handle cannot be duplicated"
            );
        }
        ensure_eq!(
            sys::vmo_get_size(bar).a1 as u64,
            c.bar_bytes,
            "the BAR VMO's size"
        );
        ensure_eq!(
            (
                sys::job_info(dj, 12).a2 as u64,
                sys::job_info(dj, 13).a2 as u64
            ),
            (7, 2),
            "the driver's job is limited to its DMA pages and a margin of two, and its MSI-X vectors (one queue: two)"
        );
        // the domain handle maps and unmaps, nothing more
        for (r, what) in [
            (sys::iodomain_destroy(dom), "destroy"),
            (sys::iodomain_fault_read(dom), "read faults"),
            (sys::iodomain_grant_vector(dom, 0), "grant a vector"),
            (sys::handle_duplicate(dom, Rights::IO_MAP.0), "duplicate it"),
        ] {
            ensure_eq!(
                st(r),
                Some(S::AccessDenied),
                "the driver's domain handle cannot {what}"
            );
        }
        // the second message said bus mastering is on
        ensure!(
            blk_bus_master(res)?,
            "bus mastering is on once the claim's second message came"
        );
        ensure_eq!(
            pci_list(c2, 7)?
                .iter()
                .find(|e| e.rid == 0x28)
                .map(|e| e.flags & dm::PCI_CLAIMED),
            Some(dm::PCI_CLAIMED),
            "listed as claimed"
        );
        let send = ok(sys::handle_duplicate(dj, Rights::SAME.0), "dup")?.a1 as u64;
        let (s, _, _) = pci_claim(mine, 8, 0x28, Some(send))?;
        ensure_eq!(s, D::Busy, "a second claim of the function");
        let send = ok(sys::handle_duplicate(dj, Rights::SAME.0), "dup")?.a1 as u64;
        let (s, _, _) = pci_claim(mine, 9, 0x20, Some(send))?;
        ensure_eq!(s, D::Busy, "a second function for the same driver job");
        // the network class, on its own job: the configuration change, receive and transmit, and both limits from its plan
        let (nj, send) = driver_job()?;
        held.push(nj);
        let (s, net, nhs) = pci_claim(c2, 12, 0x38, Some(send))?;
        held.extend(&nhs);
        ensure_eq!(s, D::Ok, "the network function is claimed");
        let net = net.ok_or_else(|| msg_at(line!()))?;
        ensure_eq!(
            (net.dma_pages.clone(), net.irqs.clone()),
            (
                vec![2, 64, 64],
                vec![
                    dm::PciIrq {
                        entry: 0,
                        what: dm::IRQ_CONFIG
                    },
                    dm::PciIrq { entry: 1, what: 0 },
                    dm::PciIrq { entry: 2, what: 1 }
                ]
            ),
            "the network class: the rings and two buffer pools; the configuration change, receive and transmit"
        );
        ensure_eq!(
            (net.handles(), nhs.len()),
            (8, 8),
            "the BAR, the domain, three DMA VMOs, three interrupts"
        );
        ensure!(
            nhs[5..].iter().all(|&h| vector_route(h) != u64::MAX),
            "three live vectors"
        );
        ensure_eq!(
            (
                sys::job_info(nj, 12).a2 as u64,
                sys::job_info(nj, 13).a2 as u64
            ),
            (132, 3),
            "the network driver's job: its 130 DMA pages and the margin of two, and its three vectors"
        );
        // a read through the claim, as a driver does it: the BAR here, the rings and the data mapped by the driver
        let me = ok(sys::process_self(), "process_self")?.a1 as u64;
        let m = sys::process_map(me, bar, PCI_BAR_VA, 0, c.bar_bytes, PF_R | PF_W);
        sys::handle_close(me);
        ensure_eq!(st(m), Some(S::Ok), "map the BAR VMO");
        let r = (|| {
            map_here(rings, RINGS_VA, 1, PF_R | PF_W)?;
            map_here(data, BUF_VA, 1, PF_R | PF_W)?;
            let rings_io = ok(sys::iodomain_map(dom, rings, 0, 1, 3), "map the rings")?.a1 as u64;
            let data_io = ok(sys::iodomain_map(dom, data, 0, 1, 3), "map a data page")?.a1 as u64;
            for i in 0..512 {
                wr64(BUF_VA + 8 * i, 0);
            }
            let f = PciFn {
                common: PCI_BAR_VA + u64::from(c.common.0),
                notify: PCI_BAR_VA + u64::from(c.notify.0),
                notify_len: u64::from(c.notify.1),
                notify_multiplier: c.notify_multiplier,
                isr: PCI_BAR_VA + u64::from(c.isr.0),
                device: (c.device_cfg.1 != 0).then(|| {
                    (
                        PCI_BAR_VA + u64::from(c.device_cfg.0),
                        u64::from(c.device_cfg.1),
                    )
                }),
                msix: None,
            };
            let used = pci_blk_read_sector0(
                &f,
                rings_io,
                (data_io + BLK_HDR, data_io + BLK_DATA, data_io + BLK_STATUS),
                2000,
            )?;
            ensure!(
                used.is_some(),
                "the claimed device did not return the request"
            );
            ensure_eq!(
                rd8_dma(BUF_VA + BLK_STATUS),
                virtio::blk::S_OK,
                "the status"
            );
            ensure_eq!(blk_pattern_wrong(), None, "sector 0 through the claim");
            Ok(())
        })();
        unmap_here(BUF_VA);
        unmap_here(RINGS_VA);
        unmap_here(PCI_BAR_VA);
        r?;
        // the claimant goes: the claim is released. The manager is one thread, so a list that no longer says CLAIMED was answered
        // after the whole release (bus mastering off, the reset, the domain destroyed).
        held.retain(|&h| h != c2);
        sys::handle_close(c2);
        let t0 = sys::clock_get();
        loop {
            let e = pci_list(mine, 10)?;
            if e.iter()
                .any(|e| e.rid == 0x28 && e.flags & dm::PCI_CLAIMED == 0)
            {
                break;
            }
            ensure!(
                sys::clock_get() < t0 + 5000,
                "the claim was not released when its connection closed"
            );
            let _ = sys::object_wait(mine, 1 << 24, sys::clock_get() + 5);
        }
        ensure!(
            !blk_bus_master(res)?,
            "bus mastering is off once the claim was released"
        );
        ensure_eq!(
            st(sys::iodomain_map(dom, rings, 0, 1, 3)),
            Some(S::BadState),
            "the domain was destroyed with the claim"
        );
        for h in hs {
            held.retain(|&x| x != h);
            sys::handle_close(h);
        }
        // and it can be claimed again, by the same job now that it holds nothing
        let send = ok(sys::handle_duplicate(dj, Rights::SAME.0), "dup")?.a1 as u64;
        let (s, _, hs) = pci_claim(mine, 11, 0x28, Some(send))?;
        held.extend(&hs);
        ensure_eq!(s, D::Ok, "claimed again after the release");
        Ok(())
    })();
    for h in held {
        sys::handle_close(h);
    }
    sys::handle_close(mine);
    let end = wait_ended(mgr, 5000);
    sys::handle_close(mgr);
    sys::handle_close(mj);
    sys::handle_close(res);
    r?;
    end
}

// --- The disk filesystem server (FS-SPEC step 5c) ---------------------------------------------

/// A volume identity for the test (the harness formats nothing: the test does it, as setup).
const FSD_UUID: [u8; 16] = *b"fsd-suite-volume";

/// `devmgr` in a job of its own: (our channel to it, its process, its job).
fn start_devmgr_in_job(res: u64) -> Result<(u64, u64, u64), Detail> {
    let (mine, mgr, mj, dj) = start_devmgr_with_drivers(res)?;
    sys::handle_close(dj);
    Ok((mine, mgr, mj))
}

/// `devmgr` in a job of its own whose drivers' jobs are made under a child job of it: (our channel, its process, its job, the
/// drivers' job). Killing the drivers' job kills every driver it started and nothing else.
fn start_devmgr_with_drivers(res: u64) -> Result<(u64, u64, u64, u64), Detail> {
    let bi = boot_image_vmo()?;
    let (mine, theirs) = channel()?;
    let root = root_job()?;
    let mj = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    sys::handle_close(root);
    let dj = job(mj, u64::MAX, u64::MAX, u64::MAX)?;
    let mgr = spawn_devmgr(res, bi, theirs, mj, dj)?;
    Ok((mine, mgr, mj, dj))
}

/// Start `devmgr` in job `mj`, serving `theirs`, with duplicates of the root resource, the boot image and `drivers` (the job it makes
/// each driver's job under: IOMMU10).
fn spawn_devmgr(res: u64, bi: u64, theirs: u64, mj: u64, drivers: u64) -> Result<u64, Detail> {
    let (off, len) = image_entry(bi, b"bin/devmgr")?;
    let image = rt::spawn::read_image(bi, off, len).ok_or_else(|| msg_at(line!()))?;
    let dup = |h: u64, what: &str| {
        ok(sys::handle_duplicate(h, Rights::SAME.0), what).map(|r| r.a1 as u64)
    };
    let hs = [
        dup(res, "dup the resource")?,
        dup(bi, "dup the image")?,
        dup(drivers, "dup the drivers' job")?,
        theirs,
    ];
    let s = Startup {
        args: vec![b"devmgr".to_vec()],
        roles: vec![
            (Role::Resource, 0),
            (Role::BootImage, 1),
            (Role::Job, 2),
            (Role::ServiceDevmgr, 3),
        ],
        ..Startup::default()
    };
    let bytes = s.encode().map_err(|_| msg_at(line!()))?;
    rt::spawn::spawn_in(&image, &bytes, &hs, mj).map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("spawn devmgr: {e:?}"));
        d
    })
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
    let have_disk = pci_disks_present()?;
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
    let mgr = spawn_devmgr(res, bi, dm_theirs, mj, mj)?;
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
    // (the process handle is kept: closing the last handle to a process kills it; the rig ends with its job)
    let _mgr = spawn_devmgr(res, bi, dm_theirs, mj, mj)?;
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
    let have_disk = pci_disks_present()?;
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
    let have_disk = pci_disks_present()?;
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

/// IOMMU12 (IOMMU-SPEC 2.6, 4.8; D1, D2): no device that can do DMA is reachable outside the IOMMU, and no physical address is
/// learnable. With the root resource, the eight virtio-mmio slots of QEMU `virt` at their fixed addresses (whose DMA the IOMMU does not
/// see; `kcore/tests/devwin.rs` checks the real tree as a whole), fw-cfg (whose DMA interface
/// writes guest RAM) and the IOMMU's own registers are refused as device memory, and the slots' interrupt sources are not admitted;
/// `sys_vmo_phys_addr` and the two-argument `sys_vmo_create_dma` are the library's stubs, which answer NOT_SUPPORTED whatever the
/// arguments; and their old numbers are not calls of the kernel (BAD_SYSCALL), through the library's raw entry or a bare `ecall`.
fn no_dma_device_outside_the_iommu() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let r = (|| {
        for slot in 0..8u64 {
            let pa = 0x1000_1000 + slot * 0x1000;
            ensure_eq!(
                st(sys::vmo_create_physical(res, pa, 4096)),
                Some(S::InvalidArgs),
                "virtio-mmio slot {slot} at {pa:#x} is no device window"
            );
            // its wired source (1 + slot on QEMU `virt`) is not admitted either
            ensure_eq!(
                st(sys::interrupt_create(res, 1 + slot as u32)),
                Some(S::InvalidArgs),
                "virtio-mmio slot {slot}'s interrupt source"
            );
        }
        for (pa, len, what) in [
            (
                0x1000_1000u64,
                0x8000u64,
                "the eight virtio-mmio slots at once",
            ),
            (0x1010_0000, 4096, "fw-cfg"),
            (0x1010_0000, 0x1000, "fw-cfg's DMA interface page"),
            (0x301_0000, 4096, "the IOMMU's register page"),
        ] {
            ensure_eq!(
                st(sys::vmo_create_physical(res, pa, len)),
                Some(S::InvalidArgs),
                "{what}"
            );
        }
        // the two calls the kernel no longer provides: the library keeps them as stubs (KERNEL-V1-SPEC 3.1) that answer NOT_SUPPORTED
        // without entering the kernel, whatever the arguments, a real DMA VMO, a plain one and the resource included
        for name in ["VMO_PHYS_ADDR", "VMO_CREATE_DMA"] {
            ensure!(
                rt::library::stub(name) != 0,
                "the system library has the stub of {name}"
            );
        }
        let dma = ok(sys::vmo_create_dma_in(res, 1, 0), "a DMA VMO")?.a1 as u64;
        let plain = vmo(1)?;
        let args = [0, 1, 4, 64, 65, 1024, u64::MAX, res, dma, plain];
        for &a in &args {
            let r = sys::vmo_phys_addr(a);
            ensure_eq!(
                (st(r), r.a1, r.a2),
                (Some(S::NotSupported), 0, 0),
                "sys_vmo_phys_addr({a:#x})"
            );
            for &b in &args {
                let r = sys::vmo_create_dma(a, b);
                ensure_eq!(
                    (st(r), r.a1, r.a2),
                    (Some(S::NotSupported), 0, 0),
                    "sys_vmo_create_dma({a:#x}, {b:#x})"
                );
            }
        }
        // and their numbers are not calls: through the library's raw entry and a bare `ecall` alike
        for &(name, n) in rt::abi::syscall::RETIRED {
            for a in [
                [0usize; 6],
                [res as usize, 1, 0, 0, 0, 0],
                [dma as usize, 0, 0, 0, 0, 0],
            ] {
                ensure_eq!(
                    st(rt::library::raw(n, a)),
                    Some(S::BadSyscall),
                    "{name} ({n}) through sys_raw"
                );
                ensure_eq!(
                    st(rt::syscall(n, a)),
                    Some(S::BadSyscall),
                    "{name} ({n}) by ecall"
                );
            }
        }
        sys::handle_close(dma);
        sys::handle_close(plain);
        Ok(())
    })();
    sys::handle_close(res);
    r
}

fn device_caps_need_the_resource() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let ev = event()?;
    let plain = vmo(1)?;
    // no resource, no device objects (the RTC's page is a window of every machine here)
    ensure_eq!(
        st(sys::vmo_create_physical(0xdead, 0x10_1000, 4096)),
        Some(S::BadHandle),
        "bad handle"
    );
    ensure_eq!(
        st(sys::vmo_create_physical(ev, 0x10_1000, 4096)),
        Some(S::WrongType),
        "an event is no resource"
    );
    ensure_eq!(
        st(sys::vmo_create_dma_in(ev, 1, 0)),
        Some(S::WrongType),
        "dma: an event is no resource"
    );
    let weak = ok(sys::handle_duplicate(res, Rights::TRANSFER.0), "dup")?.a1 as u64;
    ensure_eq!(
        st(sys::vmo_create_physical(weak, 0x10_1000, 4096)),
        Some(S::AccessDenied),
        "no MANAGE"
    );
    // IOMMU8: the PCI host's configuration space, a BAR in its memory window and a function's domain likewise
    for (h, want, what) in [
        (0xdead, S::BadHandle, "bad handle"),
        (ev, S::WrongType, "an event"),
        (weak, S::AccessDenied, "no MANAGE"),
    ] {
        ensure_eq!(
            st(sys::vmo_create_physical(h, 0x3000_0000, 4096)),
            Some(want),
            "the ECAM window: {what}"
        );
        ensure_eq!(
            st(sys::vmo_create_physical(h, 0x4000_0000, 4096)),
            Some(want),
            "a BAR in the 32-bit window: {what}"
        );
        ensure_eq!(
            st(sys::iodomain_create(h, 0x28, 0)),
            Some(want),
            "a domain for 00:05.0: {what}"
        );
    }
    ensure_eq!(
        st(sys::vmo_create_dma_in(weak, 1, 0)),
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
        (
            0x10_0000,
            4096,
            "just below the RTC's page (the test finisher)",
        ),
        (0x10_2000, 4096, "just above the RTC's page"),
        (0x10_1000, 8192, "straddling the RTC's page's end"),
        (0x10_1001, 4096, "misaligned"),
        (0x10_1000, 0, "empty"),
        (0x10_1000, 100, "not a whole page"),
        (u64::MAX - 4095, 4096, "wrapping"),
        // PL5a: the windows come from the device tree and fail closed. What the kernel or the firmware drives, and what nobody decided
        // a process may touch, on the live tree, are refused: the CLINT, the M-level APLIC, the S-level APLIC and its upper half, both
        // IMSIC files, fw-cfg (its DMA interface writes guest RAM), both flash banks, the IOMMU's registers (IOMMU5: the kernel drives
        // them), the PCI host's I/O-port window (IOMMU-SPEC 2.6: only its ECAM and memory windows are windows), and since IOMMU12 every
        // virtio-mmio slot (their DMA is outside the IOMMU: `no_dma_device_outside_the_iommu`)
        (0x200_0000, 4096, "the CLINT"),
        (0xc00_0000, 4096, "the M-level APLIC"),
        (0xd00_0000, 4096, "the S-level APLIC"),
        (0xd00_4000, 4096, "the S-level APLIC's upper half"),
        (0x2400_0000, 4096, "the M-level IMSIC file"),
        (0x2800_0000, 4096, "the S-level IMSIC file"),
        (0x1010_0000, 4096, "fw-cfg"),
        (0x2000_0000, 4096, "flash bank 0"),
        (0x2200_0000, 4096, "flash bank 1"),
        (0x301_0000, 4096, "the IOMMU's registers"),
        (0x300_0000, 4096, "the PCI I/O-port window"),
        (0x1000_1000, 4096, "the first virtio-mmio slot"),
        (0x8000_0000, 4096, "just above the PCI 32-bit window (RAM)"),
    ] {
        let r = sys::vmo_create_physical(res, pa, len);
        ensure!(st(r) == Some(S::InvalidArgs), "{what}: {:?}", st(r));
    }
    // IOMMU5 (IOMMU-SPEC 2.6): the PCI host is behind the IOMMU, so its configuration window (ECAM) and its memory windows are the root
    // resource's; the IOMMU denies every function until a domain exists (`iommu_is_on_and_denies_by_default`)
    for (pa, what) in [
        (0x3000_0000u64, "the PCI ECAM window"),
        (0x3fff_f000, "the end of the ECAM window"),
        (0x4000_0000, "the PCI 32-bit memory window"),
        (0x7fff_f000, "the end of the 32-bit window"),
        (0x4_0000_0000, "the PCI 64-bit memory window"),
    ] {
        let r = ok(sys::vmo_create_physical(res, pa, 4096), what)?;
        sys::handle_close(r.a1 as u64);
    }
    // a good one, the RTC page (the first entry of the table the kernel built from the tree); the kernel will not copy through it
    let dev = ok(
        sys::vmo_create_physical(res, 0x10_1000, 4096),
        "the RTC page",
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
    // nor through a mapping of it: a syscall pointing into device memory is refused (D1)
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    ensure_eq!(
        st(sys::process_map(me, dev, 0x400_0000, 0, 4096, PF_R | PF_W)),
        Some(S::Ok),
        "map the RTC page"
    );
    ensure_eq!(
        st(sys::vmo_read(plain, 0, 0x400_0010 as *mut u8, 4)),
        Some(S::InvalidArgs),
        "kernel copy into a device mapping"
    );
    ensure_eq!(
        st(sys::debug_write(
            // SAFETY: the range is mapped (device memory); the slice is only handed to the kernel, which must refuse it, and never
            // read here.
            unsafe { core::slice::from_raw_parts(0x400_0000 as *const u8, 4) }
        )),
        Some(S::InvalidArgs),
        "kernel copy out of a device mapping"
    );
    ensure_eq!(st(sys::process_unmap(me, 0x400_0000)), Some(S::Ok), "unmap");
    sys::handle_close(me);
    // D2 (IOMMU-SPEC 4.8): no physical address, for device memory, a plain VMO or a DMA VMO, with or without the DMA right
    ensure_eq!(
        st(sys::vmo_phys_addr(dev)),
        Some(S::NotSupported),
        "device memory has no address to learn"
    );
    ensure_eq!(
        st(sys::vmo_phys_addr(plain)),
        Some(S::NotSupported),
        "a plain VMO has no address to learn"
    );
    // DMA (`vmo_create_dma_in`): limits, charging, zeroed
    ensure_eq!(
        st(sys::vmo_create_dma_in(res, 0, 0)),
        Some(S::InvalidArgs),
        "zero pages"
    );
    let root = root_job()?;
    let before = job_use(root, 2)?.0;
    let dma = ok(sys::vmo_create_dma_in(res, 4, 0), "a DMA VMO")?.a1 as u64;
    let after = job_use(root, 2)?.0;
    ensure_eq!(
        after - before,
        4,
        "the pages are charged to the creator's job at creation"
    );
    ensure_eq!(
        st(sys::vmo_phys_addr(dma)),
        Some(S::NotSupported),
        "a DMA VMO with the DMA right has no address to learn"
    );
    ensure_eq!(job_use(root, 2)?.0, after, "asking costs nothing");
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
        15,
        "the child was refused every device call (bit 3: the PCI host's windows and a domain, IOMMU8)"
    );
    sys::handle_close(p);
    Ok(())
}

/// D5 for a wired source (IOMMU4's route code; since IOMMU12 on the RTC, the one wired device left: its alarm raises a level that stays
/// high until cleared): the object fires once for a raised line, the source stays masked until the acknowledgement, an acknowledgement
/// with the line still high fires again at once (the kernel re-pends it: `fault-aia-no-repend` must fail here), the acknowledgement
/// unmasks it (`fault-aia-no-unmask` must fail here), and closing the object frees the source.
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
        "a source of no admitted device"
    );
    ensure_eq!(
        st(sys::interrupt_create(0xdead, RTC_IRQ)),
        Some(S::BadHandle),
        "no resource"
    );
    let ev = event()?;
    ensure_eq!(
        st(sys::interrupt_create(ev, RTC_IRQ)),
        Some(S::WrongType),
        "an event is no resource"
    );
    sys::handle_close(ev);
    sys::handle_close(res);
    with_rtc(|res| {
        let Some(irq) = rtc_object(res)? else {
            return Ok(()); // no interrupt controller or no admitted RTC source: nothing to provoke
        };
        let r = (|| {
            ensure_eq!(
                st(sys::interrupt_create(res, RTC_IRQ)),
                Some(S::BadState),
                "one object per source"
            );
            rtc_quiet();
            // A source can be asserted from before the object existed (an earlier test's alarm, a level the AIA re-pends when the
            // source is enabled): one spurious interrupt after create is allowed (SYSCALLS.md). Take it if it came, then the object
            // must be idle.
            nap_ticks(3);
            let mut base = 0; // claims taken before the real one (the spurious one counts)
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
            rtc_raise();
            ensure_eq!(
                st(sys::object_wait(irq, sig::INTERRUPT, t0 + 1000)),
                Some(S::Ok),
                "the RTC's interrupt reaches the object"
            );
            // woken by the interrupt, not by the deadline running out (the wait re-reads the signals at its deadline, so a missing
            // wake-up would still return Ok, a second late)
            ensure!(sys::clock_get() - t0 < 200, "the waiter was woken late");
            // the line stays high until the alarm is cleared: the source is masked, so the kernel is not taking it again and again.
            // Give it time, then count the claims.
            nap_ticks(30);
            ensure_eq!(
                sys::object_signals(irq).a1 as u32 & sig::INTERRUPT,
                sig::INTERRUPT,
                "still pending until acknowledged"
            );
            // acknowledge with the line still high: the source is unmasked and the level fires again at once (an AIA source in MSI
            // mode pends only on a rising edge, so the kernel must re-pend it)
            let ack = ok(
                sys::interrupt_ack(irq),
                "acknowledge with the line still high",
            )?;
            ensure_eq!(ack.a1, base + 1, "exactly one claim for one raised line");
            ensure_eq!(
                st(sys::object_wait(
                    irq,
                    sig::INTERRUPT,
                    sys::clock_get() + 200
                )),
                Some(S::Ok),
                "an acknowledged source whose line is still high fires again"
            );
            // quiet the device, then acknowledge: the source is unmasked and nothing more arrives
            rtc_quiet();
            let ack = ok(sys::interrupt_ack(irq), "acknowledge")?;
            ensure_eq!(ack.a1, base + 2, "the re-fire was the second claim");
            ensure_eq!(
                sys::object_signals(irq).a1 as u32 & sig::INTERRUPT,
                0,
                "the acknowledgement clears the signal"
            );
            ensure_eq!(
                st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 20)),
                Some(S::TimedOut),
                "nothing more once the device is quiet"
            );
            // the unmask took: a later alarm interrupts again
            let t1 = sys::clock_get();
            rtc_raise();
            ensure_eq!(
                st(sys::object_wait(irq, sig::INTERRUPT, t1 + 1000)),
                Some(S::Ok),
                "a later alarm interrupts again"
            );
            ensure!(
                sys::clock_get() - t1 < 200,
                "the second waiter was woken late"
            );
            rtc_quiet();
            ok(sys::interrupt_ack(irq), "acknowledge the second")?;
            Ok(())
        })();
        rtc_quiet();
        sys::handle_close(irq);
        r?;
        // closing the object releases the source: a new object can own it, and nothing is left enabled when the last one goes
        let again = ok(
            sys::interrupt_create(res, RTC_IRQ),
            "the source is free again after close",
        )?
        .a1 as u64;
        sys::handle_close(again);
        Ok(())
    })
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
    // IOMMU4: no message was taken on an identity with no route (IO12)
    ensure_eq!(
        sys::debug_stat(140),
        0,
        "messages on interrupt identities with no route"
    );
    Ok(())
}

// --- Interrupt identities (IOMMU4, IOMMU-SPEC 8.1, IO12) ---------------------------------------------------------------------------
//
// The goldfish RTC of QEMU `virt` (source 11, page 0x10_1000) is a wired source a kernel-only boot has with no disk: an alarm set at the
// current time raises its line at once, and the line stays high (level) until `CLEAR_INTERRUPT`, while `IRQ_ENABLED` gates it. So it
// fires as often as the tests like, and fires again after every acknowledgement while it stays high.

const RTC_PA: u64 = 0x10_1000;
const RTC_IRQ: u32 = 11;
const RTC_VA: u64 = 0x700_0000;

/// Map the RTC page at `RTC_VA`; false when the machine has no such window.
fn rtc_map(res: u64) -> Result<bool, Detail> {
    let r = sys::vmo_create_physical(res, RTC_PA, 4096);
    if st(r) != Some(S::Ok) {
        return Ok(false);
    }
    let v = r.a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let m = sys::process_map(me, v, RTC_VA, 0, 4096, PF_R | PF_W);
    sys::handle_close(v);
    sys::handle_close(me);
    ensure_eq!(st(m), Some(S::Ok), "map the RTC page");
    Ok(true)
}

/// Lower the RTC's line and unmap its page: every way out of a test that mapped it comes through here, so the next test can map it again.
fn rtc_unmap() {
    rtc_quiet();
    let me = sys::process_self();
    if st(me) == Some(S::Ok) {
        sys::process_unmap(me.a1 as u64, RTC_VA);
        sys::handle_close(me.a1 as u64);
    }
}

fn rtc_wr(off: u64, v: u32) {
    // SAFETY: RTC_VA maps the RTC's page (`rtc_map`); `off` is one of its 32-bit registers.
    unsafe { core::ptr::write_volatile((RTC_VA + off) as *mut u32, v) }
}

fn rtc_rd(off: u64) -> u32 {
    // SAFETY: as `rtc_wr`.
    unsafe { core::ptr::read_volatile((RTC_VA + off) as *const u32) }
}

/// Raise the RTC's line and keep it high: interrupts enabled, an alarm at the current time (it fires at once and stays pending).
fn rtc_raise() {
    let lo = rtc_rd(0x00); // TIME_LOW (latches TIME_HIGH)
    let hi = rtc_rd(0x04);
    rtc_wr(0x10, 1); // IRQ_ENABLED
    rtc_wr(0x0c, hi); // ALARM_HIGH
    rtc_wr(0x08, lo); // ALARM_LOW: sets the alarm, already due, so it fires now
}

/// Lower the RTC's line for good: interrupts off, alarm cleared, interrupt cleared.
fn rtc_quiet() {
    rtc_wr(0x10, 0);
    rtc_wr(0x14, 1); // CLEAR_ALARM
    rtc_wr(0x1c, 1); // CLEAR_INTERRUPT
}

/// An interrupt object for the RTC, or `None` when this machine has no interrupt controller or does not admit the RTC's source.
fn rtc_object(res: u64) -> Result<Option<u64>, Detail> {
    let r = sys::interrupt_create(res, RTC_IRQ);
    match st(r) {
        Some(S::Ok) => Ok(Some(r.a1 as u64)),
        Some(S::NotSupported) | Some(S::InvalidArgs) => Ok(None),
        other => {
            ensure!(false, "interrupt_create on the RTC: {other:?}");
            Ok(None)
        }
    }
}

/// Run `body` with the RTC mapped and a root resource handle, and undo both on every way out. Ok without running it when the machine
/// has no RTC window.
fn with_rtc(body: impl FnOnce(u64) -> Outcome) -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let r = match rtc_map(res) {
        Ok(true) => {
            let r = body(res);
            rtc_unmap();
            r
        }
        Ok(false) => Ok(()), // (no RTC window: nothing to fire)
        Err(e) => Err(e),
    };
    sys::handle_close(res);
    r
}

fn hart_count(res: u64) -> u64 {
    let mut n = 0;
    while st(sys::hart_info(res, n)) == Some(S::Ok) {
        n += 1;
    }
    n
}

// ---- IOMMU5: the IOMMU is on and every device is denied by default (IOMMU-SPEC 2.4, 3.1, 7; IO1) ----------------------------
//
// The independent device is a virtio-pci function the harness puts on the command line (`scripts/os_boot_test.py`: `virtio-rng-pci`
// at slot `IOMMU_TEST_SLOT`, with `iommu_platform=on`), driven by hand: no test machine carries a device that reads or reports memory at
// a physical address (IOMMU-SPEC amendment 2026-11-27 part 2, OD175). It does both directions here, through its registers: a **read**
// is the device fetching its available ring from an address the test chose (a queue notification), and a **write** is its MSI-X
// message, whose address and
// data word the test writes into the MSI-X table and which QEMU's virtio transport sends when the driver writes a value other than 1
// to `queue_enable` (its configuration-change interrupt, `virtio_error`). No domain exists, so the directory holds no entry for the
// function: every transaction must be refused and reported (cause 258), and nothing must reach memory or an interrupt file.

/// The slot of the harness's test function on bus 0 (`addr=0x4`): device id `0x20`, `00:04.0`.
const IOMMU_TEST_SLOT: u64 = 4;
const IOMMU_TEST_DID: u64 = IOMMU_TEST_SLOT << 3;
/// The PCI host of QEMU `virt`: its configuration window (ECAM) and its 32-bit memory window (from the device tree; the suite names
/// the same machine's addresses elsewhere, as `device_caps_need_the_resource` does).
const PCI_ECAM: u64 = 0x3000_0000;
const PCI_MEM32: u64 = 0x4000_0000;
/// The supervisor IMSIC's first interrupt file on QEMU `virt` (one page per hart file).
const IMSIC_S: u64 = 0x2800_0000;
/// Where this test maps the function's configuration page, its BARs and the pattern frame.
const PCI_CFG_VA: u64 = 0x780_0000;
const PCI_BAR_VA: u64 = 0x781_0000;
const PATTERN_VA: u64 = 0x790_0000;
/// Cause 258, "device-directory entry not valid" (the IOMMU specification's fault causes; `iommucore::faultq`).
const CAUSE_DDT_INVALID: u64 = 258;
/// Transaction types of a fault record: an untranslated read and an untranslated write.
const TTYPE_READ: u64 = 2;
const TTYPE_WRITE: u64 = 3;

/// The IOMMU's counters (`debug_stat`, SYSCALLS.md): faults taken, then the last fault's device id, address and cause.
fn iommu_faults() -> u64 {
    sys::debug_stat(150)
}
/// The last fault record: (device id, address, cause, transaction type).
fn iommu_last_fault() -> (u64, u64, u64, u64) {
    let c = sys::debug_stat(155);
    (
        sys::debug_stat(153),
        sys::debug_stat(154),
        c & 0xfff,
        (c >> 32) & 0x3f,
    )
}

/// Wait until the fault count has moved past `before` and then stayed still for a while (a device may fault more than once for one
/// request); the count reached, or None if it never moved.
fn iommu_wait_faults(before: u64) -> Option<u64> {
    let t0 = sys::clock_get();
    while iommu_faults() == before {
        if sys::clock_get() > t0 + 2000 {
            return None;
        }
        sys::thread_yield();
    }
    let mut last = iommu_faults();
    let mut still = sys::clock_get();
    while sys::clock_get() < still + 50 {
        let now = iommu_faults();
        if now != last {
            last = now;
            still = sys::clock_get();
        }
        sys::thread_yield();
    }
    Some(last)
}

/// One virtio-pci function, mapped: its configuration page and the virtio structures and MSI-X table inside its BARs.
struct PciFn {
    common: u64,
    notify: u64,
    notify_len: u64,
    notify_multiplier: u32,
    isr: u64,
    /// The device configuration (address, length), if the function has one.
    device: Option<(u64, u64)>,
    /// Its MSI-X capability and table, when the suite drives the function by hand (`pci_fn_map`); a function claimed through
    /// `devmgr` has none here (the manager keeps the table).
    msix: Option<PciMsix>,
}

/// A hand-driven function's MSI-X capability, its BARs (as `pcicore` sizes them) and where each BAR is mapped here.
struct PciMsix {
    m: pcicore::Msix,
    bars: Vec<pcicore::Bar>,
    maps: Vec<(u8, u64)>,
    /// Where its configuration page is mapped here, and the physical address of its lowest BAR, mapped here at `bars_va`.
    cfg: u64,
    lo: u64,
    bars_va: u64,
}

/// `pcicore`'s MSI-X sequence over the hand-driven function: its configuration page and its BARs where `pci_fn_map` mapped them
/// (`pcicore` checks every offset against the BAR's size before it asks).
struct PciMsixRegs<'a>(&'a PciMsix);

impl pcicore::MsixAccess for PciMsixRegs<'_> {
    fn cfg_read16(&mut self, off: u16) -> u16 {
        cfgx_rd16(self.0.cfg, u64::from(off))
    }
    fn cfg_write16(&mut self, off: u16, v: u16) {
        cfgx_wr16(self.0.cfg, u64::from(off), v)
    }
    fn bar_read32(&mut self, bar: u8, off: u64) -> u32 {
        self.0
            .maps
            .iter()
            .find(|m| m.0 == bar)
            .map_or(u32::MAX, |m| mmio_rd32(m.1 + off))
    }
    fn bar_write32(&mut self, bar: u8, off: u64, v: u32) {
        if let Some(m) = self.0.maps.iter().find(|m| m.0 == bar) {
            mmio_wr32(m.1 + off, v)
        }
    }
}

impl PciFn {
    /// Program MSI-X entry 0 with `msg` and turn MSI-X on (`pcicore::msix_enable`: the function masked while the table is written,
    /// every other entry masked and cleared, INTx disabled).
    fn msix_on(&self, msg: pcicore::MsixEntry) -> Outcome {
        let x = self.msix.as_ref().ok_or_else(|| msg_at(line!()))?;
        let r = pcicore::msix_enable(&x.m, &x.bars, &[(0, msg)], &mut PciMsixRegs(x));
        ensure_eq!(r, Ok(()), "MSI-X programmed through pcicore");
        Ok(())
    }

    /// MSI-X off and the table cleared (`pcicore::msix_disable`).
    fn msix_off(&self) {
        if let Some(x) = &self.msix {
            let _ = pcicore::msix_disable(&x.m, &x.bars, &mut PciMsixRegs(x));
        }
    }

    /// Whether MSI-X is enabled (message control, read through the configuration page).
    fn msix_enabled(&self) -> bool {
        self.msix.as_ref().is_some_and(|x| {
            cfgx_rd16(x.cfg, u64::from(x.m.control_offset())) & pcicore::msix::ENABLE != 0
        })
    }

    /// The physical address of the register at `va` of its BAR window (`pci_fn_map` mapped the window from its lowest BAR).
    fn phys(&self, va: u64) -> u64 {
        self.msix.as_ref().map_or(0, |x| x.lo + (va - x.bars_va))
    }
}

fn cfg_rd32(off: u64) -> u32 {
    cfgx_rd32(PCI_CFG_VA, off)
}
fn cfg_rd16(off: u64) -> u16 {
    cfgx_rd16(PCI_CFG_VA, off)
}
/// The configuration register at `off` of the function whose page is mapped at `cfg` (PCI_CFG_VA, or the matrix's MBLK_CFG_VA).
fn cfgx_rd32(cfg: u64, off: u64) -> u32 {
    // SAFETY: `cfg` maps a function's 4 KiB configuration page, readable (`pci_fn_map`, `pci_disks_present`, `blk_bus_master`);
    // `off` is an aligned register inside it.
    unsafe { core::ptr::read_volatile((cfg + off) as *const u32) }
}
fn cfgx_rd16(cfg: u64, off: u64) -> u16 {
    // SAFETY: as `cfgx_rd32`, a 16-bit register.
    unsafe { core::ptr::read_volatile((cfg + off) as *const u16) }
}
fn cfgx_wr16(cfg: u64, off: u64, v: u16) {
    // SAFETY: as `cfgx_rd16`; every caller that writes mapped the page writable (`pci_fn_map`).
    unsafe { core::ptr::write_volatile((cfg + off) as *mut u16, v) }
}
fn mmio_wr16(va: u64, v: u16) {
    // SAFETY: `va` is inside a BAR window this process mapped (`pci_fn_map` at PCI_BAR_VA or MBLK_BAR_VA, or a claim's BAR VMO), at a
    // register the virtio structure there defines.
    unsafe { core::ptr::write_volatile(va as *mut u16, v) }
}
fn mmio_rd16(va: u64) -> u16 {
    // SAFETY: as `mmio_wr16`.
    unsafe { core::ptr::read_volatile(va as *const u16) }
}
fn mmio_wr32(va: u64, v: u32) {
    // SAFETY: as `mmio_wr16`.
    unsafe { core::ptr::write_volatile(va as *mut u32, v) }
}
fn mmio_rd32(va: u64) -> u32 {
    // SAFETY: as `mmio_wr16`.
    unsafe { core::ptr::read_volatile(va as *const u32) }
}
fn mmio_wr8(va: u64, v: u8) {
    // SAFETY: as `mmio_wr16`.
    unsafe { core::ptr::write_volatile(va as *mut u8, v) }
}
fn mmio_rd8(va: u64) -> u8 {
    // SAFETY: as `mmio_wr16`.
    unsafe { core::ptr::read_volatile(va as *const u8) }
}

/// Where `pci_bus_program` maps bus 0's configuration space (32 devices of 8 functions of 4 KiB) while it enumerates.
const PCI_BUS_VA: u64 = 0x7c0_0000;
const PCI_BUS_BYTES: u64 = 1 << 20;
/// The length of the host's 32-bit memory window (from the device tree, as `devmgr` reads it).
const PCI_MEM32_LEN: u64 = 0x4000_0000;

/// Place every memory BAR of bus 0 the way `devmgr` does (IOMMU8: `pcicore::assign` over the whole bus, which is deterministic), so
/// that a hand-driven function and `devmgr`'s functions never decode overlapping windows: enumerate, size, assign, program every
/// function's BARs (each function is left with decoding off) and return the assignment and what `pcicore` made of each function.
#[allow(clippy::type_complexity)]
fn pci_bus_program(res: u64) -> Result<(Vec<pcicore::Assignment>, Vec<pcicore::Function>), Detail> {
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let ecam = sys::vmo_create_physical(res, PCI_ECAM, PCI_BUS_BYTES);
    if st(ecam) != Some(S::Ok) {
        sys::handle_close(me);
        ensure!(
            false,
            "bus 0's configuration space is a device window: {:?}",
            st(ecam)
        );
    }
    let m = sys::process_map(
        me,
        ecam.a1 as u64,
        PCI_BUS_VA,
        0,
        PCI_BUS_BYTES,
        PF_R | PF_W,
    );
    sys::handle_close(ecam.a1 as u64);
    if st(m) != Some(S::Ok) {
        sys::handle_close(me);
        ensure!(false, "map bus 0's configuration space: {:?}", st(m));
    }
    // SAFETY (the three accessors): PCI_BUS_VA maps bus 0's whole configuration space read-write (just above), and every offset is
    // an aligned register of one function's 4 KiB page inside it.
    let rd32 = |va: u64| unsafe { core::ptr::read_volatile(va as *const u32) };
    let wr32 = |va: u64, v: u32| unsafe { core::ptr::write_volatile(va as *mut u32, v) };
    let wr16 = |va: u64, v: u16| unsafe { core::ptr::write_volatile(va as *mut u16, v) };
    let mut bars = Vec::new();
    let mut funcs = Vec::new();
    for dev in 0..pcicore::DEVICES {
        for func in 0..pcicore::FUNCTIONS {
            let Some(bdf) = pcicore::Bdf::new(0, dev, func) else {
                continue;
            };
            let va = PCI_BUS_VA + bdf.ecam_offset();
            let mut cfg = [0u8; pcicore::CFG_BYTES];
            for (i, c) in cfg.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *c = rd32(va + 4 * i as u64).to_le_bytes();
            }
            let Ok(Some(h)) = pcicore::header(&cfg) else {
                if func == 0 {
                    break;
                }
                continue;
            };
            wr16(
                va + u64::from(pcicore::reg::COMMAND),
                pcicore::command_for_sizing(h.command),
            );
            let mut masks = [0u32; 6];
            if h.header_type == 0 {
                for (i, m) in masks.iter_mut().enumerate() {
                    let off = va + u64::from(pcicore::reg::BAR0) + 4 * i as u64;
                    let orig = rd32(off);
                    wr32(off, pcicore::SIZING_VALUE);
                    *m = rd32(off);
                    wr32(off, orig);
                }
            }
            // (a function `describe` refuses is left out, as `devmgr` leaves it out)
            if let Ok(Some(f)) = pcicore::describe(bdf, &cfg, &masks) {
                bars.extend(f.bars.iter().map(|b| (bdf, *b)));
                funcs.push(f);
            }
            if func == 0 && !h.multifunction {
                break;
            }
        }
    }
    let r = pcicore::assign((PCI_MEM32, PCI_MEM32_LEN), &bars);
    if let Ok(assigned) = &r {
        for a in assigned {
            for (off, v) in pcicore::bar_writes(a) {
                wr32(PCI_BUS_VA + a.bdf.ecam_offset() + u64::from(off), v);
            }
        }
    }
    sys::process_unmap(me, PCI_BUS_VA);
    sys::handle_close(me);
    match r {
        Ok(a) => Ok((a, funcs)),
        Err(e) => {
            ensure!(false, "bus 0's BARs do not fit the 32-bit window: {e:?}");
            Ok((Vec::new(), Vec::new()))
        }
    }
}

/// Where a hand-driven function's configuration page and BAR window are mapped here, and how much room the window has.
#[derive(Clone, Copy)]
struct PciAt {
    cfg: u64,
    bars: u64,
    room: u64,
}

/// The test function's place (`with_function`; `cfg_rd32` and the other accessors read this configuration page).
const PCI_AT: PciAt = PciAt {
    cfg: PCI_CFG_VA,
    bars: PCI_BAR_VA,
    room: PATTERN_VA - PCI_BAR_VA,
};

/// Map the configuration page of the function at `slot` (whose vendor and device ids are `id`) and its BARs at `at`, on a bus
/// `pci_bus_program` has just programmed as `devmgr` would (`assigned`, `funcs`), and find its virtio structures and MSI-X table. The
/// function is left with memory decoding and bus mastering on and INTx off. A second function is mapped from the same programming
/// (programming the bus again would turn every function's decoding off).
fn pci_fn_map(
    res: u64,
    slot: u64,
    id: u32,
    assigned: &[pcicore::Assignment],
    funcs: &[pcicore::Function],
    at: PciAt,
) -> Result<PciFn, Detail> {
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let cfg = sys::vmo_create_physical(res, PCI_ECAM + (slot << 15), 4096);
    ensure_eq!(
        st(cfg),
        Some(S::Ok),
        "the test function's configuration page is a device window (the ECAM window is the root resource's)"
    );
    let cfg = cfg.a1 as u64;
    let m = sys::process_map(me, cfg, at.cfg, 0, 4096, PF_R | PF_W);
    sys::handle_close(cfg);
    ensure_eq!(st(m), Some(S::Ok), "map the configuration page");
    ensure_eq!(
        cfgx_rd32(at.cfg, 0),
        id,
        "the harness's function at slot {slot} (scripts/os_boot_test.py, PCI_DEVICES)"
    );
    cfgx_wr16(at.cfg, 4, 0); // decoding off while the BARs are mapped
    let bdf = pcicore::Bdf::new(0, slot as u8, 0).ok_or_else(|| msg_at(line!()))?;
    let mine: Vec<pcicore::Assignment> =
        assigned.iter().filter(|a| a.bdf == bdf).copied().collect();
    let lo = mine.iter().map(|a| a.addr).min().unwrap_or(PCI_MEM32);
    let hi = mine
        .iter()
        .map(|a| a.addr + a.span)
        .max()
        .unwrap_or(PCI_MEM32);
    ensure!(
        !mine.is_empty() && hi - lo <= at.room,
        "the function's BARs {mine:x?}"
    );
    let w = sys::vmo_create_physical(res, lo, hi - lo);
    ensure_eq!(
        st(w),
        Some(S::Ok),
        "the host's 32-bit memory window is a device window"
    );
    let w = w.a1 as u64;
    let m = sys::process_map(me, w, at.bars, 0, hi - lo, PF_R | PF_W);
    sys::handle_close(w);
    sys::handle_close(me);
    ensure_eq!(st(m), Some(S::Ok), "map the BARs");
    let maps: Vec<(u8, u64)> = mine
        .iter()
        .map(|a| (a.index, at.bars + (a.addr - lo)))
        .collect();
    let va =
        |bar: u8, off: u64| -> u64 { maps.iter().find(|m| m.0 == bar).map_or(0, |m| m.1 + off) };
    // the virtio structures and the MSI-X capability, as `pcicore` read them (the same code as `devmgr`'s)
    let f = funcs
        .iter()
        .find(|f| f.bdf == bdf)
        .cloned()
        .ok_or_else(|| msg_at(line!()))?;
    let (Some((_, l)), Some(msix)) = (f.virtio, f.msix) else {
        ensure!(
            false,
            "pcicore found no virtio structures or no MSI-X at {bdf}: {f:x?}"
        );
        return Err(msg_at(line!()));
    };
    ensure_eq!(l.notify_multiplier, 4, "the notify multiplier QEMU uses");
    cfgx_wr16(at.cfg, 4, (1 << 1) | (1 << 2) | (1 << 10)); // memory decoding, bus mastering, INTx disabled
    Ok(PciFn {
        common: va(l.bar, l.common.offset),
        notify: va(l.bar, l.notify.offset),
        notify_len: l.notify.len,
        notify_multiplier: l.notify_multiplier,
        isr: va(l.bar, l.isr.offset),
        device: l.device.map(|d| (va(l.bar, d.offset), d.len)),
        msix: Some(PciMsix {
            m: msix,
            bars: f.bars,
            maps,
            cfg: at.cfg,
            lo,
            bars_va: at.bars,
        }),
    })
}

fn pci_fn_down(f: &PciFn) {
    pci_fn_down_at(f, PCI_AT)
}

/// `pci_fn_down` for a function `pci_fn_map` mapped at `at`.
fn pci_fn_down_at(f: &PciFn, at: PciAt) {
    mmio_wr8(f.common + 0x14, 0); // virtio reset
    f.msix_off();
    cfgx_wr16(at.cfg, 4, 1 << 10); // no decoding, no bus mastering
    pci_unmap_at(at);
}

/// Unmap the configuration page and the BARs, whichever are mapped (the way out of a `pci_fn_map` that failed part way: its device
/// registers are not touched, since they may not be mapped; the command register is still 0, as `pci_fn_map` wrote it first).
fn pci_unmap() {
    pci_unmap_at(PCI_AT)
}

fn pci_unmap_at(at: PciAt) {
    let me = sys::process_self();
    if st(me) == Some(S::Ok) {
        let me = me.a1 as u64;
        sys::process_unmap(me, at.bars);
        sys::process_unmap(me, at.cfg);
        sys::handle_close(me);
    }
}

/// Reset the function and bring it to DRIVER_OK with queue 0 at `rings` (descriptors, available ring, used ring) and the
/// configuration-change interrupt on MSI-X vector 0, whose table entry says `(address, data)` (masked: `virtio_up_masked`).
fn virtio_up(f: &PciFn, rings: (u64, u64, u64), msg: (u64, u32)) -> Outcome {
    virtio_up_entry(
        f,
        rings,
        pcicore::MsixEntry {
            address: msg.0,
            data: msg.1,
            masked: false,
        },
    )
}

fn virtio_up_entry(f: &PciFn, rings: (u64, u64, u64), msg: pcicore::MsixEntry) -> Outcome {
    let c = f.common;
    mmio_wr8(c + 0x14, 0);
    let t0 = sys::clock_get();
    while mmio_rd8(c + 0x14) != 0 {
        ensure!(sys::clock_get() < t0 + 1000, "the reset did not complete");
    }
    mmio_wr8(c + 0x14, 1); // ACKNOWLEDGE
    mmio_wr8(c + 0x14, 1 | 2); // DRIVER
    mmio_wr32(c, 1); // device features, high word
    let offered = mmio_rd32(c + 4);
    ensure!(
        offered & 3 == 3,
        "VERSION_1 and ACCESS_PLATFORM offered (high word {offered:#x}): the harness's iommu_platform=on"
    );
    mmio_wr32(c + 8, 1);
    mmio_wr32(c + 12, 3); // we take VERSION_1 and ACCESS_PLATFORM
    mmio_wr32(c + 8, 0);
    mmio_wr32(c + 12, 0);
    mmio_wr8(c + 0x14, 1 | 2 | 8); // FEATURES_OK
    ensure!(mmio_rd8(c + 0x14) & 8 != 0, "FEATURES_OK accepted");
    // the MSI-X table entry and MSI-X on (pcicore's sequence), then the vector for configuration changes
    f.msix_on(msg)?;
    mmio_wr16(c + 0x10, 0);
    ensure_eq!(
        mmio_rd16(c + 0x10),
        0,
        "the configuration vector took MSI-X entry 0"
    );
    mmio_wr16(c + 0x16, 0); // queue 0
    let max = mmio_rd16(c + 0x18);
    ensure!(max >= 4, "queue size {max}");
    mmio_wr16(c + 0x18, 4);
    for (at, v) in [(0x20u64, rings.0), (0x28, rings.1), (0x30, rings.2)] {
        mmio_wr32(c + at, v as u32);
        mmio_wr32(c + at + 4, (v >> 32) as u32);
    }
    mmio_wr16(c + 0x1a, 0xffff); // no vector for the queue
    mmio_wr16(c + 0x1c, 1);
    mmio_wr8(c + 0x14, 1 | 2 | 8 | 4); // DRIVER_OK
    Ok(())
}

/// Make the function send its configuration-change message: a `queue_enable` value other than 1 is a driver error, which a modern
/// virtio device reports by setting NEEDS_RESET and interrupting.
fn virtio_message(f: &PciFn) {
    mmio_wr16(f.common + 0x16, 0);
    mmio_wr16(f.common + 0x1c, 2);
}

/// A frame the test owns, filled with a pattern, mapped at PATTERN_VA: (its VMO, its physical address). The address comes from the
/// test kernel's debug selector (`phys_of`): no call tells a program a physical address (IOMMU-SPEC 4.8), and a hostile device is
/// aimed at the frame by it.
fn pattern_frame(res: u64) -> Result<(u64, u64), Detail> {
    let dma = ok(sys::vmo_create_dma_in(res, 1, 0), "a DMA frame")?.a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let m = sys::process_map(me, dma, PATTERN_VA, 0, 4096, PF_R | PF_W);
    sys::handle_close(me);
    ensure_eq!(st(m), Some(S::Ok), "map the pattern frame");
    let pa = phys_of(PATTERN_VA)?;
    for i in 0..512u64 {
        // SAFETY: PATTERN_VA maps the frame read-write for 4096 bytes.
        unsafe { core::ptr::write_volatile((PATTERN_VA + 8 * i) as *mut u64, pattern_word(i)) };
    }
    Ok((dma, pa))
}

/// The physical address of the page this process has mapped at `va`: the test kernel's `debug_stat` 164 (IOMMU12), for a test that
/// aims a device at a frame. Refused (all ones) in a release kernel and for an address with nothing mapped.
fn phys_of(va: u64) -> Result<u64, Detail> {
    // (touch the page, so that it is mapped and not only reserved)
    // SAFETY: the caller mapped `va` readable.
    let _ = unsafe { core::ptr::read_volatile(va as *const u8) };
    let r = rt::syscall(
        rt::abi::syscall::DEBUG_STAT,
        [rt::debug::handle() as usize, 164, va as usize, 0, 0, 0],
    );
    ensure!(
        r.status == 0 && r.a1 != usize::MAX,
        "the physical address of {va:#x} (debug_stat 164): {} {:#x}",
        r.status,
        r.a1
    );
    Ok(r.a1 as u64)
}

fn pattern_word(i: u64) -> u64 {
    0x5a5a_0000_0000_0000 | (i * 0x9e37_79b9)
}

fn pattern_intact() -> Option<u64> {
    (0..512u64).find(|&i| {
        // SAFETY: as in `pattern_frame`.
        unsafe { core::ptr::read_volatile((PATTERN_VA + 8 * i) as *const u64) != pattern_word(i) }
    })
}

fn pattern_drop(dma: u64) {
    let me = sys::process_self();
    if st(me) == Some(S::Ok) {
        sys::process_unmap(me.a1 as u64, PATTERN_VA);
        sys::handle_close(me.a1 as u64);
    }
    sys::handle_close(dma);
}

/// The vendor and device ids (configuration dword 0) of the harness's virtio-rng-pci and virtio-blk-pci (modern).
const PCI_RNG_ID: u32 = 0x1044_1af4;
const PCI_BLK_ID: u32 = 0x1042_1af4;

/// The root resource, the test function brought up, and a pattern frame; everything undone after `body`, however it ends.
fn with_test_function(body: impl FnOnce(u64, &PciFn, u64) -> Outcome) -> Outcome {
    with_function(IOMMU_TEST_SLOT, PCI_RNG_ID, body)
}

/// `with_test_function` for the function at `slot`, whose ids are `id`.
fn with_function(slot: u64, id: u32, body: impl FnOnce(u64, &PciFn, u64) -> Outcome) -> Outcome {
    with_functions(slot, id, None, |res, f, _, pa| body(res, f, pa))
}

/// `with_function`, and when `second` names one (its slot, its ids and where it is mapped here), a second function brought up beside
/// the first from the same programming of the bus, and taken down after `body` like the first.
fn with_functions(
    slot: u64,
    id: u32,
    second: Option<(u64, u32, PciAt)>,
    body: impl FnOnce(u64, &PciFn, Option<&PciFn>, u64) -> Outcome,
) -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let r = (|| {
        let (dma, pa) = pattern_frame(res)?;
        let r = (|| {
            let (assigned, funcs) = pci_bus_program(res)?;
            let f = match pci_fn_map(res, slot, id, &assigned, &funcs, PCI_AT) {
                Ok(f) => f,
                Err(e) => {
                    pci_unmap();
                    return Err(e);
                }
            };
            let r = match second {
                None => body(res, &f, None, pa),
                Some((slot2, id2, at2)) => {
                    match pci_fn_map(res, slot2, id2, &assigned, &funcs, at2) {
                        Ok(g) => {
                            let r = body(res, &f, Some(&g), pa);
                            pci_fn_down_at(&g, at2);
                            r
                        }
                        Err(e) => {
                            pci_unmap_at(at2);
                            Err(e)
                        }
                    }
                }
            };
            pci_fn_down(&f);
            r
        })();
        pattern_drop(dma);
        r
    })();
    sys::handle_close(res);
    r
}

/// IOMMU-SPEC 2.4 step 5 and 3.1 (IO1): the directory is empty, so a function with no domain can neither read nor write memory, and
/// each refused transaction is a fault record naming the function, the address and the direction.
fn iommu_is_on_and_denies_by_default() -> Outcome {
    with_test_function(|_, f, pa| {
        let before = iommu_faults();
        // a read: the device fetches its available ring, which the test put in the pattern frame
        virtio_up(f, (pa, pa + 0x100, pa + 0x200), (pa + 0x800, 0xdead_beef))?;
        mmio_wr16(f.notify, 0); // queue 0's notification (its notify offset is 0: the only queue)
        let Some(after_read) = iommu_wait_faults(before) else {
            ensure!(
                false,
                "the device's read of {pa:#x} raised no fault (faults {before})"
            );
            return Ok(());
        };
        let (did, iova, cause, ttype) = iommu_last_fault();
        ensure_eq!(did, IOMMU_TEST_DID, "the read's fault names the function");
        ensure_eq!(cause, CAUSE_DDT_INVALID, "no directory entry");
        ensure_eq!(ttype, TTYPE_READ, "a read");
        ensure!(
            iova & !0xfff == pa,
            "the read's address {iova:#x} is in the frame at {pa:#x}"
        );
        // a write: the message the test put in the MSI-X table, aimed at the pattern frame
        virtio_message(f);
        let Some(after_write) = iommu_wait_faults(after_read) else {
            ensure!(
                false,
                "the device's write to {:#x} raised no fault",
                pa + 0x800
            );
            return Ok(());
        };
        let (did, iova, cause, ttype) = iommu_last_fault();
        ensure_eq!(did, IOMMU_TEST_DID, "the write's fault names the function");
        ensure_eq!(cause, CAUSE_DDT_INVALID, "no directory entry");
        ensure_eq!(ttype, TTYPE_WRITE, "a write");
        ensure_eq!(iova, pa + 0x800, "the write's address");
        ensure!(after_write >= before + 2, "two faults at least");
        if let Some(i) = pattern_intact() {
            ensure!(
                false,
                "the pattern changed at word {i}: the device reached memory"
            );
        }
        Ok(())
    })
}

/// IOMMU-SPEC 3.1 and 8 (IO1, IO9): a function with no domain cannot interrupt a hart. Its MSI-X entry is programmed with a real
/// interrupt file and identity, first the inter-hart identity of hart file 0, then the identity the kernel gave an RTC interrupt object
/// of this test (`debug_stat` 145 names it): each message is a refused write with a fault record, and the object never fires.
fn iommu_denies_messages_by_default() -> Outcome {
    with_test_function(|res, f, pa| {
        let no_route = sys::debug_stat(140);
        let before = iommu_faults();
        virtio_up(f, (pa, pa + 0x100, pa + 0x200), (IMSIC_S, 1))?;
        virtio_message(f);
        let Some(after) = iommu_wait_faults(before) else {
            ensure!(
                false,
                "the message to the inter-hart identity raised no fault"
            );
            return Ok(());
        };
        let (did, iova, cause, ttype) = iommu_last_fault();
        ensure_eq!(
            did,
            IOMMU_TEST_DID,
            "the message's fault names the function"
        );
        ensure_eq!(cause, CAUSE_DDT_INVALID, "no directory entry");
        ensure_eq!(ttype, TTYPE_WRITE, "a message is a write");
        ensure_eq!(iova, IMSIC_S, "the interrupt file's address");
        // an identity that is open and routed: the RTC object's
        let Some(obj) = rtc_object(res)? else {
            ensure!(false, "no RTC interrupt object on this machine");
            return Ok(());
        };
        let route = rt::syscall(
            rt::abi::syscall::DEBUG_STAT,
            [rt::debug::handle() as usize, 145, RTC_IRQ as usize, 0, 0, 0],
        );
        let r = (|| {
            ensure!(
                route.status == 0 && route.a1 as u64 != u64::MAX,
                "the RTC object's route (debug_stat 145): {:?}",
                st(route)
            );
            let (file, id) = ((route.a1 as u64) >> 16, (route.a1 as u64) & 0xffff);
            virtio_up(
                f,
                (pa, pa + 0x100, pa + 0x200),
                (IMSIC_S + file * 0x1000, id as u32),
            )?;
            virtio_message(f);
            ensure!(
                iommu_wait_faults(after).is_some(),
                "the message to identity {id} of file {file} raised no fault"
            );
            let (did, iova, _, ttype) = iommu_last_fault();
            ensure_eq!(did, IOMMU_TEST_DID, "the second message's fault");
            ensure_eq!(ttype, TTYPE_WRITE, "a write");
            ensure_eq!(iova, IMSIC_S + file * 0x1000, "file {file}'s address");
            ensure_eq!(
                st(sys::object_wait(
                    obj,
                    sig::INTERRUPT,
                    sys::clock_get() + 100
                )),
                Some(S::TimedOut),
                "the RTC object fired: the message reached identity {id} of file {file}"
            );
            ensure_eq!(sys::debug_stat(140), no_route, "messages with no route");
            if let Some(i) = pattern_intact() {
                ensure!(false, "the pattern changed at word {i}");
            }
            Ok(())
        })();
        sys::handle_close(obj);
        r
    })
}

// ---- device domains (IOMMU6; IOMMU-SPEC 4, 6, 7.4, 10) -------------------------------------------------------------------------
//
// The same virtio-rng function, now with a domain: the test is the supervisor (the handle `iodomain_create` returns) and, through a
// duplicate with IO_MAP and IO_UNMAP only, the driver. One request is one descriptor the device fetches (a **read** of memory the
// test wrote: the available ring and the descriptor) and fills with random bytes (a **write** into the buffer), then the used ring
// it writes back. Its configuration message is masked, so the only transactions are the queue's.

/// Where the device-domain tests map their DMA VMOs into this process: the rings, the buffer (two pages) and the read-only page.
const IODMA_VA: u64 = 0x7a0_0000;
const RINGS_VA: u64 = IODMA_VA;
const BUF_VA: u64 = IODMA_VA + 0x1_0000;
const RO_VA: u64 = IODMA_VA + 0x2_0000;
const SCRATCH_VA: u64 = IODMA_VA + 0x10_0000;
/// The direction field of `iodomain_fault_read` (abi::iodomain).
const DIR_READ: u64 = 1;
const DIR_WRITE: u64 = 2;

fn wr16(va: u64, v: u16) {
    // SAFETY: `va` is inside a DMA VMO this process mapped read-write: at IODMA_VA (RINGS_VA, BUF_VA, RO_VA, SCRATCH_VA), or the
    // matrix's MBLK_RINGS_VA and MBLK_BUF_VA.
    unsafe { core::ptr::write_volatile(va as *mut u16, v) }
}
fn rd16(va: u64) -> u16 {
    // SAFETY: as `wr16`.
    unsafe { core::ptr::read_volatile(va as *const u16) }
}
fn wr64(va: u64, v: u64) {
    // SAFETY: as `wr16`.
    unsafe { core::ptr::write_volatile(va as *mut u64, v) }
}
fn rd64(va: u64) -> u64 {
    // SAFETY: as `wr16`.
    unsafe { core::ptr::read_volatile(va as *const u64) }
}

/// Map `vmo` (its first `pages` pages) into this process at `va`.
fn map_here(vmo: u64, va: u64, pages: u64, flags: u64) -> Outcome {
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let m = sys::process_map(me, vmo, va, 0, pages * 4096, flags);
    sys::handle_close(me);
    ensure_eq!(st(m), Some(S::Ok), "map a DMA VMO at {va:#x}");
    Ok(())
}

fn unmap_here(va: u64) {
    if let Ok(me) = ok(sys::process_self(), "process_self") {
        sys::process_unmap(me.a1 as u64, va);
        sys::handle_close(me.a1 as u64);
    }
}

/// One request on queue 0: its rings at I/O address `rings` (descriptors, then the available ring at +0x100, the used ring at
/// +0x200; the test writes them through RINGS_VA, which maps the same VMO when `rings` is its mapping), one descriptor of `len`
/// bytes at I/O address `buf` for the device to write. True if the device returned the descriptor within the wait.
fn rng_request(f: &PciFn, rings: u64, buf: u64, len: u32, wait_ms: u64) -> Result<bool, Detail> {
    for i in 0..(0x300 / 8) {
        wr64(RINGS_VA + 8 * i, 0);
    }
    wr64(RINGS_VA, buf); // descriptor 0: the address,
    wr64(RINGS_VA + 8, u64::from(len) | (2 << 32)); // its length, VIRTQ_DESC_F_WRITE, no next
    wr16(RINGS_VA + 0x100, 1); // VIRTQ_AVAIL_F_NO_INTERRUPT
    wr16(RINGS_VA + 0x104, 0); // ring[0] = descriptor 0
    wr16(RINGS_VA + 0x102, 1); // idx 1
                               // the configuration message masked: the queue's transactions are the only ones
    virtio_up_entry(
        f,
        (rings, rings + 0x100, rings + 0x200),
        pcicore::MsixEntry {
            address: 0,
            data: 0,
            masked: true,
        },
    )?;
    mmio_wr16(f.notify, 0);
    let t0 = sys::clock_get();
    while sys::clock_get() < t0 + wait_ms {
        if rd16(RINGS_VA + 0x202) == 1 {
            return Ok(true);
        }
        sys::thread_yield();
    }
    Ok(false)
}

/// Every fault record waiting in the domain's ring, oldest first: (cause, I/O address, direction, dropped).
fn drain_domain_faults(sup: u64) -> Vec<(u64, u64, u64, u64)> {
    let mut out = Vec::new();
    loop {
        let r = sys::iodomain_fault_read(sup);
        if r.status != 0 {
            break;
        }
        let w = r.a1 as u64;
        out.push((w & 0xffff, r.a2 as u64, (w >> 16) & 0xffff, w >> 32));
    }
    out
}

// ---- the PCI block function (IOMMU8; IOMMU-SPEC 15.1 L1) -----------------------------------------------------------------------
//
// The harness's virtio-blk-pci at slot `PCI_BLK_SLOT` (`iommu_platform=on`): since IOMMU10 the scratch disk (16 MiB, whose sector 0
// holds byte i = (i * 7 + 3) mod 256 and is never written), driven through the `virtio` crate (the driver's code path: negotiation
// with VERSION_1 and ACCESS_PLATFORM, a split queue) by polling, with MSI-X and INTx off. Kernel tests: the suite is the supervisor
// and the driver, as in the IOMMU6 tests. The no-disk boot (`OS_DISK=0`) has no disk on either bus, and these tests have nothing to
// drive there (`pci_disks_present`).

/// The slot of the harness's block function on bus 0 (`addr=0x5`): device id `0x28`, `00:05.0`. `OpenBlock`'s device 0.
const PCI_BLK_SLOT: u64 = 5;
const PCI_BLK_DID: u64 = PCI_BLK_SLOT << 3;

/// Whether the harness put the disks on bus 0 (the no-disk boot, `OS_DISK=0`, leaves them off): the scratch disk's function at
/// `PCI_BLK_SLOT` answers its configuration reads with the virtio vendor.
fn pci_disks_present() -> Result<bool, Detail> {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let me = ok(sys::process_self(), "process_self")?.a1 as u64;
    let cfg = ok(
        sys::vmo_create_physical(res, PCI_ECAM + (PCI_BLK_SLOT << 15), 4096),
        "the block function's configuration page",
    )?
    .a1 as u64;
    sys::handle_close(res);
    let m = sys::process_map(me, cfg, PCI_CFG_VA, 0, 4096, PF_R);
    sys::handle_close(cfg);
    ensure_eq!(st(m), Some(S::Ok), "map it");
    let vendor = cfg_rd16(0);
    sys::process_unmap(me, PCI_CFG_VA);
    sys::handle_close(me);
    Ok(vendor == 0x1af4)
}
/// Where the request's parts are in the data page (mapped at BUF_VA): the header, the sector, the status byte.
const BLK_HDR: u64 = 0;
const BLK_DATA: u64 = 0x200;
const BLK_STATUS: u64 = 0x400;

/// The virtio registers by address: the transport's offsets are the addresses `pci_fn_map` (or a claim's BAR mapping) found, which are
/// inside the mapped BARs.
struct VaRegs;
impl virtio::Regs for VaRegs {
    fn read8(&self, off: u64) -> u8 {
        mmio_rd8(off)
    }
    fn read16(&self, off: u64) -> u16 {
        mmio_rd16(off)
    }
    fn read32(&self, off: u64) -> u32 {
        mmio_rd32(off)
    }
    fn write8(&mut self, off: u64, v: u8) {
        mmio_wr8(off, v)
    }
    fn write16(&mut self, off: u64, v: u16) {
        mmio_wr16(off, v)
    }
    fn write32(&mut self, off: u64, v: u32) {
        mmio_wr32(off, v)
    }
}

/// The queue memory, mapped read-write at RINGS_VA (one page).
struct RingsMem;
impl RingsMem {
    fn at(off: u64) -> u64 {
        RINGS_VA + (off & 0xfff)
    }
}
impl virtio::Mem for RingsMem {
    fn read16(&self, off: u64) -> u16 {
        rd16(Self::at(off))
    }
    fn read32(&self, off: u64) -> u32 {
        // SAFETY: `at` keeps the address inside the page mapped read-write at RINGS_VA, and the queue layout aligns it.
        unsafe { core::ptr::read_volatile(Self::at(off) as *const u32) }
    }
    fn write16(&mut self, off: u64, v: u16) {
        wr16(Self::at(off), v)
    }
    fn write32(&mut self, off: u64, v: u32) {
        // SAFETY: as `read32`.
        unsafe { core::ptr::write_volatile(Self::at(off) as *mut u32, v) }
    }
    fn write64(&mut self, off: u64, v: u64) {
        wr64(Self::at(off), v)
    }
}

fn vio<T>(r: Result<T, virtio::Error>, what: &str) -> Result<T, Detail> {
    r.map_err(|e| {
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(&mut d, format_args!("{what}: {e:?}"));
        d
    })
}

/// Bring the block function up through the `virtio` crate, with one split queue of 8 at I/O address `rings_io` (the page mapped at
/// RINGS_VA), and read sector 0: the header at `hdr_io` (the test writes it through BUF_VA + BLK_HDR), the data at `data_io`, the
/// status byte at `status_io`. Polled, no interrupt asked for. The used element, or None if the device did not return the request
/// within `wait_ms`.
fn pci_blk_read_sector0(
    f: &PciFn,
    rings_io: u64,
    (hdr_io, data_io, status_io): (u64, u64, u64),
    wait_ms: u64,
) -> Result<Option<(u16, u32)>, Detail> {
    use virtio::Transport;
    for i in 0..512 {
        wr64(RINGS_VA + 8 * i, 0);
    }
    let h = virtio::blk::header(virtio::blk::T_IN, 0);
    for (i, w) in h.as_chunks::<8>().0.iter().enumerate() {
        wr64(BUF_VA + BLK_HDR + 8 * i as u64, u64::from_le_bytes(*w));
    }
    wr64(BUF_VA + BLK_STATUS, 0xff);
    let mut t = virtio::PciTransport::new(
        VaRegs,
        virtio::PciLocations {
            common: f.common,
            notify: f.notify,
            notify_len: f.notify_len,
            notify_multiplier: f.notify_multiplier,
            isr: f.isr,
            device: f.device,
        },
    );
    let t0 = sys::clock_get();
    vio(t.reset(&mut || sys::clock_get() < t0 + 1000), "reset")?;
    let got = vio(t.negotiate(virtio::feature::REQUIRED), "negotiate")?;
    ensure_eq!(
        got & virtio::feature::REQUIRED,
        virtio::feature::REQUIRED,
        "VERSION_1 and ACCESS_PLATFORM accepted"
    );
    let mut q = vio(virtio::SplitQueue::new(8, rings_io, 4096), "the queue")?;
    vio(
        t.setup_queue(0, 8, q.addresses(), virtio::NO_VECTOR),
        "queue 0",
    )?;
    t.driver_ok();
    let chain = [
        virtio::Buf {
            addr: hdr_io,
            len: virtio::blk::HEADER_BYTES,
            device_writes: false,
        },
        virtio::Buf {
            addr: data_io,
            len: virtio::blk::SECTOR,
            device_writes: true,
        },
        virtio::Buf {
            addr: status_io,
            len: 1,
            device_writes: true,
        },
    ];
    let head = vio(q.add(&mut RingsMem, &chain, false), "add the request")?;
    t.notify(0);
    let t0 = sys::clock_get();
    while sys::clock_get() < t0 + wait_ms {
        if let Some(u) = vio(q.take_used(&RingsMem), "the used ring")? {
            ensure_eq!(u.0, head, "the used element names the request");
            return Ok(Some(u));
        }
        sys::thread_yield();
    }
    Ok(None)
}

/// The byte at `va` of a page this test mapped (BUF_VA's).
fn rd8_dma(va: u64) -> u8 {
    (rd64(va & !7) >> (8 * (va & 7))) as u8
}

/// The first byte of the sector read at BUF_VA + BLK_DATA that is not the disk's pattern.
fn blk_pattern_wrong() -> Option<u64> {
    (0..512u64).find(|&i| rd8_dma(BUF_VA + BLK_DATA + i) != ((i * 7 + 3) & 0xff) as u8)
}

/// The block function with a domain (the suite is the supervisor), the driver's duplicate, and a rings page and a data page mapped
/// here and in the domain: `body` gets the function, the supervisor's handle, the rings' and the data page's I/O addresses and the
/// pattern frame's physical address. The device is reset before the domain goes.
fn with_blk_domain(body: impl FnOnce(&PciFn, u64, u64, u64, u64) -> Outcome) -> Outcome {
    if !pci_disks_present()? {
        return Ok(()); // the no-disk boot
    }
    with_function(PCI_BLK_SLOT, PCI_BLK_ID, |res, f, pa| {
        let sup = ok(
            sys::iodomain_create(res, PCI_BLK_DID, 0),
            "iodomain_create for the block function",
        )?
        .a1 as u64;
        let r = (|| {
            let drv = ok(
                sys::handle_duplicate(sup, Rights::IO_MAP.0 | Rights::IO_UNMAP.0),
                "the driver's duplicate",
            )?
            .a1 as u64;
            let rings = ok(sys::vmo_create_dma_in(res, 1, 0), "the rings' DMA VMO")?.a1 as u64;
            let data = ok(sys::vmo_create_dma_in(res, 1, 0), "the data page")?.a1 as u64;
            let r = (|| {
                map_here(rings, RINGS_VA, 1, PF_R | PF_W)?;
                map_here(data, BUF_VA, 1, PF_R | PF_W)?;
                let rings_io =
                    ok(sys::iodomain_map(drv, rings, 0, 1, 3), "map the rings")?.a1 as u64;
                let data_io =
                    ok(sys::iodomain_map(drv, data, 0, 1, 3), "map the data page")?.a1 as u64;
                for i in 0..512 {
                    wr64(BUF_VA + 8 * i, 0);
                }
                body(f, sup, rings_io, data_io, pa)
            })();
            mmio_wr8(f.common + 0x14, 0); // the device stops before its memory and its domain go
            unmap_here(BUF_VA);
            unmap_here(RINGS_VA);
            sys::handle_close(data);
            sys::handle_close(rings);
            sys::handle_close(drv);
            r
        })();
        sys::handle_close(sup);
        r
    })
}

/// IOMMU8 (IOMMU-SPEC 15.1 L1, 9): a virtio-blk-pci function behind the IOMMU reads sector 0 through a domain, by the `virtio` crate's
/// negotiation and split queue, polled: every byte is the disk's, the status is OK, the IOMMU took no fault, and MSI-X and INTx stayed
/// off.
fn pci_virtio_blk_reads_through_the_iommu() -> Outcome {
    with_blk_domain(|f, _, rings_io, data_io, _| {
        let before = iommu_faults();
        let used = pci_blk_read_sector0(
            f,
            rings_io,
            (data_io + BLK_HDR, data_io + BLK_DATA, data_io + BLK_STATUS),
            2000,
        )?;
        ensure!(
            used.is_some(),
            "the device did not return the request (faults {} -> {}, status {:#x})",
            before,
            iommu_faults(),
            rd8_dma(BUF_VA + BLK_STATUS)
        );
        ensure_eq!(
            rd8_dma(BUF_VA + BLK_STATUS),
            virtio::blk::S_OK,
            "the request's status"
        );
        if let Some(i) = blk_pattern_wrong() {
            ensure!(
                false,
                "sector 0 byte {i} is {:#x}, the disk holds {:#x}",
                rd8_dma(BUF_VA + BLK_DATA + i),
                (i * 7 + 3) & 0xff
            );
        }
        ensure_eq!(iommu_faults(), before, "the read took no IOMMU fault");
        ensure!(!f.msix_enabled(), "MSI-X stayed off (this test polls)");
        ensure!(cfg_rd16(4) & (1 << 10) != 0, "INTx stayed disabled");
        Ok(())
    })
}

/// IOMMU8 (IOMMU-SPEC 15.1 L1, IO1): the same function, told to put the sector in a frame its domain does not map (the pattern
/// frame, by its physical address): the IOMMU refuses the write and records a fault naming the function, and the frame is unchanged.
/// (QEMU's virtio moves a buffer it may not write through a bounce buffer, so the request itself may still be returned, as in the
/// IOMMU6 tests: the refusal shows as memory unchanged and a fault record.)
fn pci_virtio_outside_its_mapping_faults() -> Outcome {
    with_blk_domain(|f, sup, rings_io, data_io, pa| {
        let before = iommu_faults();
        let _ = pci_blk_read_sector0(
            f,
            rings_io,
            (data_io + BLK_HDR, pa, data_io + BLK_STATUS),
            500,
        )?;
        expect_fault(
            sup,
            pa,
            DIR_WRITE,
            "a sector read into a frame outside the mapping",
        )?;
        ensure!(iommu_faults() > before, "the IOMMU counted the fault");
        let (did, _, _, _) = iommu_last_fault();
        ensure_eq!(did, PCI_BLK_DID, "the last fault names the block function");
        if let Some(i) = pattern_intact() {
            ensure!(false, "the pattern frame changed at word {i}");
        }
        Ok(())
    })
}

/// Wait for DEVICE_FAULT on the control handle, then take every record: one at `iova` in direction `dir` must be among them.
fn expect_fault(sup: u64, iova: u64, dir: u64, what: &str) -> Outcome {
    let w = sys::object_wait(sup, sig::DEVICE_FAULT, sys::clock_get() + 2000);
    ensure_eq!(
        st(w),
        Some(S::Ok),
        "{what}: DEVICE_FAULT rises on the supervisor's handle"
    );
    // (a device may fault more than once for one request: let it finish)
    let mut quiet = sys::clock_get();
    let mut n = iommu_faults();
    while sys::clock_get() < quiet + 50 {
        if iommu_faults() != n {
            n = iommu_faults();
            quiet = sys::clock_get();
        }
        sys::thread_yield();
    }
    let recs = drain_domain_faults(sup);
    ensure!(
        recs.iter()
            .any(|&(_, a, d, _)| a & !0xfff == iova & !0xfff && d == dir),
        "{what}: no record at {iova:#x} direction {dir} among {recs:x?}"
    );
    ensure_eq!(
        sys::object_signals(sup).a1 as u32 & sig::DEVICE_FAULT,
        0,
        "{what}: the read that empties the ring clears DEVICE_FAULT"
    );
    Ok(())
}

/// The test function brought up with a domain: the control handle, the driver's duplicate, and the rings mapped both ways.
struct Domain {
    sup: u64,
    drv: u64,
    rings_io: u64,
}

/// `with_test_function` with a domain: `body` gets the root resource, the function, the domain and the pattern frame's physical address.
fn with_domain(body: impl FnOnce(u64, &PciFn, &Domain, u64) -> Outcome) -> Outcome {
    with_test_function(|res, f, pa| {
        let sup = ok(
            sys::iodomain_create(res, IOMMU_TEST_DID, 0),
            "iodomain_create for the test function",
        )?
        .a1 as u64;
        let r = (|| {
            let drv = ok(
                sys::handle_duplicate(sup, Rights::IO_MAP.0 | Rights::IO_UNMAP.0),
                "the driver's duplicate",
            )?
            .a1 as u64;
            let rings = ok(sys::vmo_create_dma_in(res, 1, 0), "the rings' DMA VMO")?.a1 as u64;
            let r = (|| {
                map_here(rings, RINGS_VA, 1, PF_R | PF_W)?;
                let rings_io = ok(
                    sys::iodomain_map(drv, rings, 0, 1, 3),
                    "the driver maps the rings",
                )?
                .a1 as u64;
                let d = Domain { sup, drv, rings_io };
                let r = body(res, f, &d, pa);
                unmap_here(RINGS_VA);
                r
            })();
            sys::handle_close(rings);
            sys::handle_close(drv);
            r
        })();
        sys::handle_close(sup);
        r
    })
}

/// IOMMU-SPEC 4.1 to 4.3, 3.3 and IO3/IO4: through the driver's duplicate (IO_MAP and IO_UNMAP only), the device reaches what was
/// mapped, at the addresses the kernel chose, with the access granted, and nothing else; a hundred more mappings show that no table of
/// 64 bounds a domain.
fn iodomain_maps_exactly_what_was_granted() -> Outcome {
    with_domain(|res, f, d, pa| {
        let buf = ok(sys::vmo_create_dma_in(res, 2, 0), "the buffer")?.a1 as u64;
        let ro = ok(sys::vmo_create_dma_in(res, 1, 0), "the read-only page")?.a1 as u64;
        let r = (|| {
            map_here(buf, BUF_VA, 2, PF_R | PF_W)?;
            map_here(ro, RO_VA, 1, PF_R | PF_W)?;
            for i in 0..512 {
                wr64(RO_VA + 8 * i, pattern_word(i));
            }
            let ro_handle = ok(
                sys::handle_duplicate(ro, Rights::READ.0 | Rights::DMA.0),
                "a read-only DMA handle",
            )?
            .a1 as u64;
            let wide = sys::iodomain_map(d.drv, ro_handle, 0, 1, 3);
            let ro_io = sys::iodomain_map(d.drv, ro_handle, 0, 1, 1);
            sys::handle_close(ro_handle);
            ensure_eq!(
                st(wide),
                Some(S::AccessDenied),
                "IO4: never wider than the handle"
            );
            let ro_io = ok(ro_io, "map the page for reading")?.a1 as u64;
            let buf_io = ok(sys::iodomain_map(d.drv, buf, 0, 2, 3), "map the buffer")?.a1 as u64;
            // the driver's duplicate does nothing else
            ensure_eq!(
                st(sys::iodomain_destroy(d.drv)),
                Some(S::AccessDenied),
                "the driver cannot destroy"
            );
            ensure_eq!(
                st(sys::iodomain_fault_read(d.drv)),
                Some(S::AccessDenied),
                "the driver cannot read faults"
            );
            ensure_eq!(
                st(sys::object_wait(d.drv, sig::DEVICE_FAULT, 1)),
                Some(S::AccessDenied),
                "the driver cannot wait for faults"
            );
            // positive DMA: the device reads the descriptor the test wrote and writes random bytes into the buffer's second page
            for i in 0..512 {
                wr64(BUF_VA + 4096 + 8 * i, 0);
            }
            ensure!(
                rng_request(f, d.rings_io, buf_io + 4096, 64, 2000)?,
                "the device did not return the descriptor (used idx {})",
                rd16(RINGS_VA + 0x202)
            );
            ensure!(
                (0..8).any(|i| rd64(BUF_VA + 4096 + 8 * i) != 0),
                "the device's write did not reach the buffer"
            );
            ensure!(
                (8..512).all(|i| rd64(BUF_VA + 4096 + 8 * i) == 0),
                "the device wrote past its 64 bytes"
            );
            // what the device must not reach: a write to the read-only page, one page past the buffer, a physical address, address 0
            // (the pattern frame `with_test_function` made, at PATTERN_VA)
            {
                // (QEMU's virtio maps a buffer it may not write through a bounce buffer: the request completes, the write-back
                // is refused and reported. So a refused transaction shows as memory unchanged and a fault record, not as the
                // used ring: `rng_request`'s answer is not the check here.)
                for (at, what) in [
                    (ro_io, "a write to the read-only mapping"),
                    (buf_io + 2 * 4096, "a write one page past the buffer"),
                    (pa, "a write to a frame's physical address"),
                    (0, "a write to address 0"),
                ] {
                    let _ = rng_request(f, d.rings_io, at, 64, 300)?;
                    expect_fault(d.sup, at, DIR_WRITE, what)?;
                }
                // rings at an address nothing maps (the page past the buffer: the lowest-fit allocator put the rings, the
                // read-only page and the buffer below it): the device's first fetch from them is refused
                let _ = rng_request(f, buf_io + 2 * 4096, buf_io, 64, 300)?;
                expect_fault(
                    d.sup,
                    buf_io + 2 * 4096,
                    DIR_READ,
                    "a read of rings nothing maps",
                )?;
                if let Some(i) = pattern_intact() {
                    ensure!(false, "the pattern frame changed at word {i}");
                }
                ensure!(
                    (0..512).all(|i| rd64(RO_VA + 8 * i) == pattern_word(i)),
                    "the read-only page changed"
                );
            }
            let mut more = Vec::new();
            for _ in 0..100 {
                more.push(
                    ok(
                        sys::iodomain_map(d.drv, buf, 0, 1, 1),
                        "one of a hundred more",
                    )?
                    .a1 as u64,
                );
            }
            ensure!(
                sys::debug_stat(160) >= 104,
                "the I/O-mapped pages are counted ({})",
                sys::debug_stat(160)
            );
            for io in more {
                ok(
                    sys::iodomain_unmap(d.drv, io, 1),
                    "unmap one of the hundred",
                )?;
            }
            ok(sys::iodomain_unmap(d.drv, buf_io, 2), "unmap the buffer")?;
            ok(
                sys::iodomain_unmap(d.drv, ro_io, 1),
                "unmap the read-only page",
            )?;
            Ok(())
        })();
        unmap_here(BUF_VA);
        unmap_here(RO_VA);
        sys::handle_close(buf);
        sys::handle_close(ro);
        r
    })
}

/// IOMMU-SPEC 4.4 and 6.1 (IO3): when `iodomain_unmap` returns, the device can no longer reach the frames, even though it used the
/// translation just before (so the IOMMU's translation cache held it). The negative control `fault-iommu-skip-inval` must fail here.
fn iodomain_unmap_stops_the_device() -> Outcome {
    with_domain(|res, f, d, _| {
        let buf = ok(sys::vmo_create_dma_in(res, 1, 0), "the buffer")?.a1 as u64;
        let r = (|| {
            map_here(buf, BUF_VA, 1, PF_R | PF_W)?;
            let io = ok(sys::iodomain_map(d.drv, buf, 0, 1, 3), "map the buffer")?.a1 as u64;
            ensure!(
                rng_request(f, d.rings_io, io, 64, 2000)?,
                "the first request was not returned"
            );
            ensure!(
                (0..8).any(|i| rd64(BUF_VA + 8 * i) != 0),
                "the first write did not land"
            );
            for i in 0..512 {
                wr64(BUF_VA + 8 * i, 0);
            }
            ok(sys::iodomain_unmap(d.drv, io, 1), "unmap")?;
            // (the request may complete through QEMU's bounce buffer: what matters is that its write-back is refused)
            let _ = rng_request(f, d.rings_io, io, 64, 300)?;
            expect_fault(d.sup, io, DIR_WRITE, "a write after the unmap")?;
            ensure!(
                (0..512).all(|i| rd64(BUF_VA + 8 * i) == 0),
                "the device reached the frame after the unmap returned (a stale translation)"
            );
            Ok(())
        })();
        unmap_here(BUF_VA);
        sys::handle_close(buf);
        r
    })
}

/// IOMMU-SPEC 7.4: the ring keeps the sixteen oldest unread records, counts the rest as dropped, and DEVICE_FAULT follows it.
fn iodomain_faults_reach_the_supervisor() -> Outcome {
    with_domain(|_, f, d, _| {
        let before = iommu_faults();
        // twenty refused reads, each at its own page past the rings (the device fetches its available ring there)
        for i in 0..20u64 {
            let at = d.rings_io + (1 + i) * 4096;
            ensure!(
                !rng_request(f, at, d.rings_io, 64, 0)?,
                "read {i} was taken"
            );
            let t0 = sys::clock_get();
            while iommu_faults() == before + i && sys::clock_get() < t0 + 2000 {
                sys::thread_yield();
            }
        }
        // The device faults more than once for one request (QEMU 11.1.3's virtio reads the available ring's index three times), and the loop
        // above waits only for the first fault of each, so the last requests' later faults can still be on their way: a count read at any one
        // moment can be short of what the domain has stored and dropped by the time it is read. What holds at every moment is the ledger
        // (IOMMU-SPEC 7.4: the count and the ring change under one lock): every fault is either a record read from the ring or counted as
        // dropped. So the count is read once it has stayed still, the ring is drained, the count is let settle again and anything stored since
        // is drained too, and the sum is checked against the final count.
        let total = iommu_wait_faults(before).unwrap_or(before) - before;
        ensure!(total >= 20, "twenty faults at least ({total})");
        ensure!(
            sys::object_signals(d.sup).a1 as u32 & sig::DEVICE_FAULT != 0,
            "DEVICE_FAULT is up"
        );
        let recs = drain_domain_faults(d.sup);
        // (a fault that arrives while the ring is being read finds room in it: the drain reads it too, so the ring holds at least sixteen)
        ensure!(recs.len() >= 16, "the ring holds sixteen ({})", recs.len());
        // (the first read reports what was dropped before it: at least the faults counted so far beyond the sixteen it kept)
        ensure!(
            recs[0].3 >= total - 16,
            "the dropped count comes with the first read: {} for {total} faults",
            recs[0].3
        );
        let settled = iommu_wait_faults(before).unwrap_or(before) - before;
        let late = drain_domain_faults(d.sup);
        let dropped: u64 = recs.iter().chain(late.iter()).map(|r| r.3).sum();
        ensure_eq!(
            recs.len() as u64 + late.len() as u64 + dropped,
            settled,
            "every fault was read or counted as dropped ({} read, {} late, {dropped} dropped)",
            recs.len(),
            late.len()
        );
        ensure!(
            recs.windows(2).all(|w| w[0].1 <= w[1].1),
            "oldest first: {recs:x?}"
        );
        ensure_eq!(
            recs[0].1 & !0xfff,
            d.rings_io + 4096,
            "the oldest is the first read"
        );
        ensure!(recs.iter().all(|r| r.2 == DIR_READ), "reads: {recs:x?}");
        ensure_eq!(
            st(sys::iodomain_fault_read(d.sup)),
            Some(S::ShouldWait),
            "empty"
        );
        ensure_eq!(
            sys::object_signals(d.sup).a1 as u32 & sig::DEVICE_FAULT,
            0,
            "and the signal is down"
        );
        ensure_eq!(
            st(sys::iodomain_fault_read(d.drv)),
            Some(S::AccessDenied),
            "the driver cannot read them"
        );
        Ok(())
    })
}

/// IOMMU-SPEC 10.2 and 10.3 (the T42 scenario): a killed driver's mappings stay with the domain and hold their VMO, the device's write
/// still lands; the supervisor's destroy gives everything back. With the supervisor killed too, the last handle destroys the domain,
/// the device is quarantined: a late write fails, reaches nothing and reports nothing.
fn iodomain_outlives_its_makers() -> Outcome {
    let domains = sys::debug_stat(159);
    let pages = sys::debug_stat(160);
    with_test_function(|res, f, _| {
        let rings = ok(sys::vmo_create_dma_in(res, 1, 0), "the rings")?.a1 as u64;
        map_here(rings, RINGS_VA, 1, PF_R | PF_W)?;
        let r = (|| {
            for run in 0..2 {
                let sup = ok(
                    sys::iodomain_create(res, IOMMU_TEST_DID, 0),
                    "iodomain_create",
                )?
                .a1 as u64;
                // (TRANSFER as well: the supervisor hands it over a channel)
                let drv = ok(
                    sys::handle_duplicate(
                        sup,
                        Rights::IO_MAP.0 | Rights::IO_UNMAP.0 | Rights::TRANSFER.0,
                    ),
                    "the driver's duplicate",
                )?
                .a1 as u64;
                let buf = ok(sys::vmo_create_dma_in(res, 1, 0), "the driver's buffer")?.a1 as u64;
                let rings_io =
                    ok(sys::iodomain_map(drv, rings, 0, 1, 3), "map the rings")?.a1 as u64;
                let buf_io = ok(sys::iodomain_map(drv, buf, 0, 1, 3), "map the buffer")?.a1 as u64;
                map_here(buf, BUF_VA, 1, PF_R | PF_W)?;
                // the driver: a child that holds the duplicate and the buffer, and is killed
                let (driver, ch) = launch(15)?;
                let hs = [drv, buf];
                let w = sys::channel_write(ch, [0u8; 4].as_ptr(), 4, hs.as_ptr(), 2);
                let mapped = sys::debug_stat(160);
                let killed = sys::process_kill(driver, 9);
                let ended = wait_ended(driver, 1000);
                sys::handle_close(driver);
                sys::handle_close(ch);
                ensure_eq!(st(w), Some(S::Ok), "the driver is given its handles");
                ok(killed, "kill the driver")?;
                ended?;
                ensure_eq!(
                    sys::debug_stat(160),
                    mapped,
                    "the mappings stay with the domain"
                );
                for i in 0..512 {
                    wr64(BUF_VA + 8 * i, 0);
                }
                ensure!(
                    rng_request(f, rings_io, buf_io, 64, 2000)?,
                    "the device's request after the driver died"
                );
                ensure!(
                    (0..8).any(|i| rd64(BUF_VA + 8 * i) != 0),
                    "the write lands in the buffer the mapping holds"
                );
                unmap_here(BUF_VA); // (the buffer's last holder is now the domain's mapping)
                if run == 0 {
                    ok(
                        sys::iodomain_destroy(sup),
                        "the supervisor destroys the domain",
                    )?;
                    sys::handle_close(sup);
                } else {
                    // the supervisor dies too: the last handle goes with its teardown
                    let (supervisor, ch) = launch(15)?;
                    let w = sys::channel_write(ch, [0u8; 4].as_ptr(), 4, [sup].as_ptr(), 1);
                    let killed = sys::process_kill(supervisor, 9);
                    let ended = wait_ended(supervisor, 1000);
                    sys::handle_close(supervisor);
                    sys::handle_close(ch);
                    ensure_eq!(
                        st(w),
                        Some(S::Ok),
                        "the supervisor is given the control handle"
                    );
                    ok(killed, "kill the supervisor")?;
                    ended?;
                }
                let t0 = sys::clock_get();
                while sys::debug_stat(159) != domains && sys::clock_get() < t0 + 2000 {
                    sys::thread_yield();
                }
                ensure_eq!(
                    sys::debug_stat(159),
                    domains,
                    "run {run}: the domain is gone"
                );
                ensure_eq!(
                    sys::debug_stat(160),
                    pages,
                    "run {run}: the I/O-mapped pages are back"
                );
                // quarantined: allocate and fill memory (the buffer's frames are free again), then the late write at the old address
                let fill = ok(sys::vmo_create_dma_in(res, 16, 0), "fill memory")?.a1 as u64;
                map_here(fill, SCRATCH_VA, 16, PF_R | PF_W)?;
                for i in 0..16 * 512 {
                    wr64(SCRATCH_VA + 8 * i, pattern_word(i));
                }
                let faults = iommu_faults();
                let late = rng_request(f, rings_io, buf_io, 64, 300)?;
                let t1 = sys::clock_get();
                while sys::clock_get() < t1 + 50 {
                    sys::thread_yield();
                }
                let intact = (0..16 * 512).all(|i| rd64(SCRATCH_VA + 8 * i) == pattern_word(i));
                unmap_here(SCRATCH_VA);
                sys::handle_close(fill);
                ensure!(
                    !late,
                    "run {run}: the quarantined device's request was served"
                );
                ensure!(intact, "run {run}: the late write reached reused memory");
                ensure_eq!(
                    iommu_faults(),
                    faults,
                    "run {run}: a quarantined device's faults are not reported (A14)"
                );
            }
            Ok(())
        })();
        unmap_here(RINGS_VA);
        sys::handle_close(rings);
        r
    })
}

/// IOMMU6 (SYSCALLS.md "Device domains", `vmo_create_dma_in`, `job_set_io_limits`): each call's handle, rights and argument errors on
/// the real kernel (the host's `iodomain_calls.rs` has every row in order; this is the machine's side of it).
fn iodomain_calls_check_handles_rights_and_arguments() -> Outcome {
    with_test_function(|res, _, _| {
        let ev = ok(sys::event_create(), "event")?.a1 as u64;
        let plain = ok(sys::vmo_create(1), "a plain VMO")?.a1 as u64;
        let weak_res = ok(
            sys::handle_duplicate(res, Rights::DUPLICATE.0),
            "a resource without MANAGE",
        )?
        .a1 as u64;
        let root = root_job()?;
        let small = job(root, u64::MAX, u64::MAX, 2)?;
        // (the domain's job: its tables are charged as pages, so not the small one; it maps nothing)
        let iojob = job(root, u64::MAX, u64::MAX, u64::MAX)?;
        let r = (|| {
            // vmo_create_dma_in
            ensure_eq!(
                st(sys::vmo_create_dma_in(BAD, 1, 0)),
                Some(S::BadHandle),
                "dma_in: no resource"
            );
            ensure_eq!(
                st(sys::vmo_create_dma_in(ev, 1, 0)),
                Some(S::WrongType),
                "dma_in: an event"
            );
            ensure_eq!(
                st(sys::vmo_create_dma_in(weak_res, 1, 0)),
                Some(S::AccessDenied),
                "dma_in: no MANAGE"
            );
            ensure_eq!(
                st(sys::vmo_create_dma_in(res, 0, 0)),
                Some(S::InvalidArgs),
                "dma_in: no pages"
            );
            ensure_eq!(
                st(sys::vmo_create_dma_in(res, 4, small)),
                Some(S::NoMemory),
                "dma_in: past the job's pages"
            );
            // iodomain_create
            ensure_eq!(
                st(sys::iodomain_create(BAD, IOMMU_TEST_DID, 0)),
                Some(S::BadHandle),
                "create: no resource"
            );
            ensure_eq!(
                st(sys::iodomain_create(ev, IOMMU_TEST_DID, 0)),
                Some(S::WrongType),
                "create: an event"
            );
            ensure_eq!(
                st(sys::iodomain_create(weak_res, IOMMU_TEST_DID, 0)),
                Some(S::AccessDenied),
                "create: no MANAGE"
            );
            ensure_eq!(
                st(sys::iodomain_create(res, 1 << 30, 0)),
                Some(S::InvalidArgs),
                "create: a device id past the tree's"
            );
            ensure_eq!(
                st(sys::job_set_io_limits(iojob, 0, 0)),
                Some(S::Ok),
                "the domain's job maps nothing"
            );
            let d = ok(sys::iodomain_create(res, IOMMU_TEST_DID, iojob), "create")?.a1 as u64;
            let r = (|| {
                ensure_eq!(
                    st(sys::iodomain_create(res, IOMMU_TEST_DID, 0)),
                    Some(S::BadState),
                    "create: a second domain for a live device"
                );
                let buf = ok(sys::vmo_create_dma_in(res, 1, 0), "a buffer")?.a1 as u64;
                let no_map = ok(
                    sys::handle_duplicate(d, Rights::IO_UNMAP.0 | Rights::IO_DESTROY.0),
                    "no IO_MAP",
                )?
                .a1 as u64;
                let drv = ok(
                    sys::handle_duplicate(d, Rights::IODOMAIN_DRIVER.0),
                    "the driver's",
                )?
                .a1 as u64;
                // iodomain_map
                ensure_eq!(
                    st(sys::iodomain_map(BAD, buf, 0, 1, 3)),
                    Some(S::BadHandle),
                    "map: no domain"
                );
                ensure_eq!(
                    st(sys::iodomain_map(ev, buf, 0, 1, 3)),
                    Some(S::WrongType),
                    "map: an event"
                );
                ensure_eq!(
                    st(sys::iodomain_map(d, plain, 0, 1, 3)),
                    Some(S::WrongType),
                    "map: a VMO that is not DMA"
                );
                ensure_eq!(
                    st(sys::iodomain_map(no_map, buf, 0, 1, 3)),
                    Some(S::AccessDenied),
                    "map: no IO_MAP"
                );
                ensure_eq!(
                    st(sys::iodomain_map(d, buf, 0, 0, 3)),
                    Some(S::InvalidArgs),
                    "map: no pages"
                );
                ensure_eq!(
                    st(sys::iodomain_map(d, buf, 0, 1, 3)),
                    Some(S::NoMemory),
                    "map: past the job's I/O pages"
                );
                // iodomain_unmap
                ensure_eq!(
                    st(sys::iodomain_unmap(BAD, 0, 1)),
                    Some(S::BadHandle),
                    "unmap: no domain"
                );
                ensure_eq!(
                    st(sys::iodomain_unmap(ev, 0, 1)),
                    Some(S::WrongType),
                    "unmap: an event"
                );
                ensure_eq!(
                    st(sys::iodomain_unmap(drv, 0, 1)),
                    Some(S::InvalidArgs),
                    "unmap: no such mapping"
                );
                let no_unmap =
                    ok(sys::handle_duplicate(d, Rights::IO_MAP.0), "no IO_UNMAP")?.a1 as u64;
                ensure_eq!(
                    st(sys::iodomain_unmap(no_unmap, 0, 1)),
                    Some(S::AccessDenied),
                    "unmap: no IO_UNMAP"
                );
                // iodomain_fault_read, iodomain_destroy
                ensure_eq!(
                    st(sys::iodomain_fault_read(BAD)),
                    Some(S::BadHandle),
                    "fault_read: no domain"
                );
                ensure_eq!(
                    st(sys::iodomain_fault_read(ev)),
                    Some(S::WrongType),
                    "fault_read: an event"
                );
                ensure_eq!(
                    st(sys::iodomain_destroy(BAD)),
                    Some(S::BadHandle),
                    "destroy: no domain"
                );
                ensure_eq!(
                    st(sys::iodomain_destroy(ev)),
                    Some(S::WrongType),
                    "destroy: an event"
                );
                ensure_eq!(
                    st(sys::iodomain_destroy(drv)),
                    Some(S::AccessDenied),
                    "destroy: the driver's"
                );
                ok(sys::iodomain_destroy(no_map), "destroy")?;
                ensure_eq!(
                    st(sys::iodomain_destroy(d)),
                    Some(S::Ok),
                    "a second destroy does nothing"
                );
                ensure_eq!(
                    st(sys::iodomain_map(drv, buf, 0, 1, 3)),
                    Some(S::BadState),
                    "map: a destroyed domain"
                );
                ensure_eq!(
                    st(sys::iodomain_unmap(drv, 0, 1)),
                    Some(S::BadState),
                    "unmap: a destroyed domain (OD171: before INVALID_ARGS)"
                );
                for h in [buf, no_map, drv, no_unmap] {
                    sys::handle_close(h);
                }
                Ok(())
            })();
            sys::handle_close(d);
            r
        })();
        // job_set_io_limits
        let no_limits = ok(
            sys::handle_duplicate(small, Rights::INSPECT.0),
            "a job without SET_LIMITS",
        )?
        .a1 as u64;
        let r2 = (|| {
            ensure_eq!(
                st(sys::job_set_io_limits(BAD, 1, 1)),
                Some(S::BadHandle),
                "set_io_limits: no job"
            );
            ensure_eq!(
                st(sys::job_set_io_limits(ev, 1, 1)),
                Some(S::WrongType),
                "set_io_limits: an event"
            );
            ensure_eq!(
                st(sys::job_set_io_limits(no_limits, 1, 1)),
                Some(S::AccessDenied),
                "set_io_limits: no SET_LIMITS"
            );
            Ok(())
        })();
        for h in [ev, plain, weak_res, no_limits, small, iojob, root] {
            sys::handle_close(h);
        }
        r.and(r2)
    })
}

// ---- interrupt vectors (IOMMU7; IOMMU-SPEC 8.2 to 8.5) -------------------------------------------------------------------------
//
// The same virtio-rng function and domain; its configuration-change message (`virtio_message`) is the device's MSI-X write. The test
// is the supervisor: it grants a vector and programs MSI-X entry 0 with the pair the kernel returned (or with a forged one). A granted
// vector's message sets the pending bit in its own interrupt file and, if the file is enabled, the IOMMU sends the kernel's notice.

/// Fault causes of the MSI translation (the IOMMU specification; `iommucore::faultq`): entry not valid, entry misconfigured.
const CAUSE_MSI_INVALID: u64 = 262;
const CAUSE_MSI_MISCONFIGURED: u64 = 263;

/// Where vector handle `h` sends its notices (`debug_stat` 148): (interrupt file << 16) | identity, all ones for none.
fn vector_route(h: u64) -> u64 {
    let r = rt::syscall(
        rt::abi::syscall::DEBUG_STAT,
        [rt::debug::handle() as usize, 148, h as usize, 0, 0, 0],
    );
    if r.status == 0 {
        r.a1 as u64
    } else {
        u64::MAX
    }
}

/// The device's MSI-X entry 0 says `(address, data)`, then the device sends it (the function reset and brought up again first, so
/// each message is a fresh configuration change).
fn msi_send(f: &PciFn, d: &Domain, address: u64, data: u32) -> Outcome {
    virtio_up(
        f,
        (d.rings_io, d.rings_io + 0x100, d.rings_io + 0x200),
        (address, data),
    )?;
    virtio_message(f);
    Ok(())
}

/// Let the device finish: no new IOMMU fault and no new notice for 50 ms.
fn msi_settle() {
    let mut last = (iommu_faults(), sys::debug_stat(162));
    let mut still = sys::clock_get();
    let t0 = still;
    while sys::clock_get() < still + 50 && sys::clock_get() < t0 + 2000 {
        let now = (iommu_faults(), sys::debug_stat(162));
        if now != last {
            last = now;
            still = sys::clock_get();
        }
        sys::thread_yield();
    }
}

/// The leak oracle of a vector test: once the handles are closed, no vector is live and every interrupt identity is free again (the
/// retirements are the harts' work: waited for, briefly).
fn vectors_at_baseline(live: u64, ids: u64) -> Outcome {
    let t0 = sys::clock_get();
    while (sys::debug_stat(146), sys::debug_stat(141)) != (live, ids)
        && sys::clock_get() < t0 + 1000
    {
        sys::thread_yield();
    }
    ensure_eq!(
        sys::debug_stat(146),
        live,
        "live vectors (debug_stat 146) back at the baseline"
    );
    ensure_eq!(
        sys::debug_stat(141),
        ids,
        "free interrupt identities (debug_stat 141) back at the baseline"
    );
    ensure_eq!(
        sys::debug_stat(140),
        0,
        "no message on an identity with no route"
    );
    Ok(())
}

/// `with_domain` with vector 0 granted: `body` gets the function, the domain, the interrupt handle and the MSI-X address. Afterwards
/// the leak oracle.
fn with_vector(body: impl FnOnce(&PciFn, &Domain, u64, u64) -> Outcome) -> Outcome {
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let r = with_domain(|_, f, d, _| {
        let g = ok(sys::iodomain_grant_vector(d.sup, 0), "grant vector 0")?;
        let (irq, address) = (g.a1 as u64, g.a2 as u64);
        let r = (|| {
            ensure_eq!(
                sys::debug_stat(146),
                live + 1,
                "one live vector (debug_stat 146)"
            );
            ensure_eq!(
                address & 0xfff,
                0,
                "the MSI-X address is a page of the window"
            );
            body(f, d, irq, address)
        })();
        sys::handle_close(irq);
        r
    });
    r.and(vectors_at_baseline(live, ids))
}

/// IOMMU-SPEC 8.3 and 8.5 (D5, IO11): the granted vector's message fires its object; while fired, the device's next message only sets
/// the file's pending bit (no notice) and the acknowledgement finds it and fires again at once; armed again, nothing more. The vector
/// can be placed on the calling hart (`interrupt_deliver_here`), moves off a hart that goes offline, and is still delivered. No IOMMU
/// fault.
fn msi_granted_vector_is_delivered() -> Outcome {
    with_vector(|f, d, irq, address| {
        let faults = iommu_faults();
        let notices = sys::debug_stat(162);
        ensure_eq!(
            st(sys::iodomain_grant_vector(d.drv, 1)),
            Some(S::AccessDenied),
            "a driver's handle cannot grant"
        );
        ensure_eq!(
            st(sys::iodomain_grant_vector(d.sup, 0)),
            Some(S::BadState),
            "vector 0 is granted and live"
        );
        msi_send(f, d, address, 1)?;
        ensure_eq!(
            st(sys::object_wait(
                irq,
                sig::INTERRUPT,
                sys::clock_get() + 2000
            )),
            Some(S::Ok),
            "the granted vector fires (notices {} to {})",
            notices,
            sys::debug_stat(162)
        );
        let route = vector_route(irq);
        ensure!(route != u64::MAX, "the vector has a route (debug_stat 148)");
        // masked: the message while fired sends nothing, and the acknowledgement fires it again from the pending bit
        msi_send(f, d, address, 1)?;
        msi_settle();
        ensure_eq!(
            sys::debug_stat(162),
            notices + 1,
            "masked: one notice only (D5)"
        );
        ok(sys::interrupt_ack(irq), "acknowledge")?;
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 1)),
            Some(S::Ok),
            "the message that came while masked fires again at the acknowledgement"
        );
        ok(sys::interrupt_ack(irq), "acknowledge again")?;
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 50)),
            Some(S::TimedOut),
            "armed, and nothing pending"
        );
        ensure_eq!(
            st(sys::interrupt_ack(irq)),
            Some(S::BadState),
            "an acknowledgement before it fired again"
        );
        // the explicit placement, then the hart it is on goes offline (unless it is hart 0, which never does): it moves, and still fires
        ok(sys::interrupt_deliver_here(irq), "interrupt_deliver_here")?;
        let here = vector_route(irq);
        ensure!(here != u64::MAX, "placed (debug_stat 148)");
        vector_moves_off_an_offline_hart(f, d, irq, address, here)?;
        msi_send(f, d, address, 1)?;
        ensure_eq!(
            st(sys::object_wait(
                irq,
                sig::INTERRUPT,
                sys::clock_get() + 2000
            )),
            Some(S::Ok),
            "delivered where it was placed"
        );
        ok(sys::interrupt_ack(irq), "the last acknowledgement")?;
        msi_settle();
        ensure_eq!(iommu_faults(), faults, "no IOMMU fault");
        Ok(())
    })
}

/// The argument and rights errors of `iodomain_grant_vector` and `interrupt_deliver_here` (SYSCALLS.md), on a live domain and vector;
/// a refused call changes nothing.
fn vector_calls_check_handles_rights_and_arguments() -> Outcome {
    with_vector(|_, d, irq, _| {
        let ev = ok(sys::event_create(), "event")?.a1 as u64;
        let waiter = ok(
            sys::handle_duplicate(irq, Rights::WAIT.0),
            "a WAIT-only duplicate",
        )?
        .a1 as u64;
        let r = (|| {
            ensure_eq!(
                st(sys::iodomain_grant_vector(BAD, 1)),
                Some(S::BadHandle),
                "grant: no domain"
            );
            ensure_eq!(
                st(sys::iodomain_grant_vector(ev, 1)),
                Some(S::WrongType),
                "grant: an event"
            );
            ensure_eq!(
                st(sys::iodomain_grant_vector(irq, 1)),
                Some(S::WrongType),
                "grant: an interrupt"
            );
            ensure_eq!(
                st(sys::iodomain_grant_vector(d.sup, 256)),
                Some(S::InvalidArgs),
                "grant: index 256"
            );
            ensure_eq!(
                st(sys::interrupt_deliver_here(BAD)),
                Some(S::BadHandle),
                "deliver_here: no interrupt"
            );
            ensure_eq!(
                st(sys::interrupt_deliver_here(ev)),
                Some(S::WrongType),
                "deliver_here: an event"
            );
            ensure_eq!(
                st(sys::interrupt_deliver_here(d.sup)),
                Some(S::WrongType),
                "deliver_here: a domain"
            );
            ensure_eq!(
                st(sys::interrupt_deliver_here(waiter)),
                Some(S::AccessDenied),
                "deliver_here: no MANAGE"
            );
            ensure_eq!(
                st(sys::iodomain_grant_vector(d.drv, 1)),
                Some(S::AccessDenied),
                "grant: the driver's duplicate"
            );
            ensure_eq!(
                vector_route(waiter),
                vector_route(irq),
                "a refused call changed nothing"
            );
            Ok(())
        })();
        sys::handle_close(waiter);
        sys::handle_close(ev);
        r
    })
}

/// The hart vector `irq` is placed on (`here`, from debug_stat 148) goes offline: the vector moves to another hart and is still
/// delivered; the hart comes back. `debug_stat` 148 names the interrupt file, and files follow the hart ids, not the logical harts
/// (hart 0 is the boot hart, whichever id that is), so the hart is found by taking the others offline one at a time until the route
/// leaves `here`. A route on hart 0, which never goes offline, moves for none of them: nothing to check.
fn vector_moves_off_an_offline_hart(
    f: &PciFn,
    d: &Domain,
    irq: u64,
    address: u64,
    here: u64,
) -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let harts = hart_count(res);
    let mut r = Ok(());
    for hart in 1..harts {
        let off = sys::hart_set_online(res, hart, 0);
        let mut moved_here = false;
        r = (|| {
            ensure_eq!(st(off), Some(S::Ok), "hart {hart} offline");
            let moved = vector_route(irq);
            if moved == here {
                return Ok(());
            }
            moved_here = true;
            ensure!(
                moved != u64::MAX && moved >> 16 != here >> 16,
                "the vector moved off hart {hart}'s file ({here:#x} to {moved:#x})"
            );
            msi_send(f, d, address, 1)?;
            ensure_eq!(
                st(sys::object_wait(
                    irq,
                    sig::INTERRUPT,
                    sys::clock_get() + 2000
                )),
                Some(S::Ok),
                "delivered after the move"
            );
            ok(sys::interrupt_ack(irq), "acknowledge after the move")?;
            Ok(())
        })();
        let back = sys::hart_set_online(res, hart, 1);
        if r.is_ok() && st(back) != Some(S::Ok) {
            r = Err(msg_at(line!()));
        }
        if r.is_err() || moved_here {
            break;
        }
    }
    sys::handle_close(res);
    r
}

/// IOMMU-SPEC 8.4, first row: a message to a page of the window whose vector is not granted is an IOMMU fault (MSI entry not valid)
/// naming the device; nothing is delivered and the granted vector does not fire.
fn msi_ungranted_vector_faults() -> Outcome {
    with_vector(|f, d, irq, address| {
        let notices = sys::debug_stat(162);
        let before = iommu_faults();
        msi_send(f, d, address + 5 * 4096, 1)?;
        ensure!(
            iommu_wait_faults(before).is_some(),
            "the message to vector 5 raised no fault"
        );
        let (did, _, cause, ttype) = iommu_last_fault();
        ensure_eq!(did, IOMMU_TEST_DID, "the fault names the function");
        ensure_eq!(cause, CAUSE_MSI_INVALID, "the MSI entry is not valid");
        ensure_eq!(ttype, TTYPE_WRITE, "a message is a write");
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 50)),
            Some(S::TimedOut),
            "vector 0 did not fire"
        );
        ensure_eq!(sys::debug_stat(162), notices, "no notice");
        // the record reaches the supervisor
        ensure_eq!(
            st(sys::object_wait(
                d.sup,
                sig::DEVICE_FAULT,
                sys::clock_get() + 2000
            )),
            Some(S::Ok),
            "DEVICE_FAULT rises"
        );
        let recs = drain_domain_faults(d.sup);
        ensure!(
            recs.iter()
                .any(|&(c, _, dir, _)| c == CAUSE_MSI_INVALID && dir == DIR_WRITE),
            "the supervisor reads the record: {recs:x?}"
        );
        Ok(())
    })
}

/// IOMMU-SPEC 8.4 and IO11: what a device can forge. A data word other than 1 on the granted page sets a bit in the vector's own file,
/// which sends nothing and is found and counted (debug_stat 147) at the acknowledgement; a data word no file holds is an IOMMU fault
/// (misconfigured); a message aimed at a real interrupt file is a translation fault. Nothing reaches a hart: no notice, no message on
/// an identity with no route, the object fires only for data 1. The negative control `fault-iommu-msi-basic` (basic-translate
/// entries, the device's data word passed to the hart) must fail here.
fn msi_forgery_faults() -> Outcome {
    with_vector(|f, d, irq, address| {
        let (notices, no_route, strays) = (
            sys::debug_stat(162),
            sys::debug_stat(140),
            sys::debug_stat(147),
        );
        // a stray data word
        let before = iommu_faults();
        msi_send(f, d, address, 5)?;
        msi_settle();
        ensure_eq!(iommu_faults(), before, "a word the file holds is no fault");
        ensure_eq!(sys::debug_stat(162), notices, "data 5 sends no notice");
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 20)),
            Some(S::TimedOut),
            "data 5 does not fire the vector"
        );
        // a word no file holds
        msi_send(f, d, address, 4096)?;
        ensure!(
            iommu_wait_faults(before).is_some(),
            "data 4096 raised no fault"
        );
        let (did, _, cause, _) = iommu_last_fault();
        ensure_eq!(did, IOMMU_TEST_DID, "the fault names the function");
        ensure_eq!(
            cause,
            CAUSE_MSI_MISCONFIGURED,
            "data 4096: the entry is misconfigured for it"
        );
        // a misaligned address on the granted page (8.4: cause 263; QEMU, observed: no fault and nothing delivered, records note
        // 2026-11-25 section 13): nothing reaches a hart either way, and a fault, if one is recorded, is the misconfiguration
        let before = iommu_faults();
        msi_send(f, d, address + 2, 1)?;
        msi_settle();
        if iommu_faults() != before {
            let (did, _, cause, _) = iommu_last_fault();
            ensure_eq!(
                (did, cause),
                (IOMMU_TEST_DID, CAUSE_MSI_MISCONFIGURED),
                "a misaligned message's fault"
            );
        }
        ensure_eq!(
            sys::debug_stat(162),
            notices,
            "a misaligned message sends no notice"
        );
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 20)),
            Some(S::TimedOut),
            "a misaligned message does not fire the vector"
        );
        // a real interrupt file, from the device
        let before = iommu_faults();
        msi_send(f, d, IMSIC_S, 1)?;
        ensure!(
            iommu_wait_faults(before).is_some(),
            "a message to the interrupt file raised no fault"
        );
        let (did, iova, _, ttype) = iommu_last_fault();
        ensure_eq!(
            (did, iova, ttype),
            (IOMMU_TEST_DID, IMSIC_S, TTYPE_WRITE),
            "the translation fault"
        );
        msi_settle();
        ensure_eq!(sys::debug_stat(162), notices, "no notice from a forgery");
        ensure_eq!(
            sys::debug_stat(140),
            no_route,
            "no message on an identity with no route"
        );
        // data 1 fires; its acknowledgement finds the stray bit of data 5 and counts it
        msi_send(f, d, address, 1)?;
        ensure_eq!(
            st(sys::object_wait(
                irq,
                sig::INTERRUPT,
                sys::clock_get() + 2000
            )),
            Some(S::Ok),
            "data 1 fires the vector"
        );
        ok(sys::interrupt_ack(irq), "acknowledge")?;
        ensure_eq!(
            sys::debug_stat(147),
            strays + 1,
            "the stray bit is found and counted once (debug_stat 147)"
        );
        ensure_eq!(
            st(sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 20)),
            Some(S::TimedOut),
            "a stray bit does not fire the vector"
        );
        Ok(())
    })
}

/// IOMMU-SPEC 8.3 and 8.5, revoke (IO10): when the last handle closes, the entry is invalid before the call returns, so the device's
/// next message is a fault and nothing is delivered; the identity and the file are given back (the leak oracle), and the index can be
/// granted again.
fn msi_revoke_stops_the_vector() -> Outcome {
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let r = with_domain(|_, f, d, _| {
        let g = ok(sys::iodomain_grant_vector(d.sup, 2), "grant vector 2")?;
        let (irq, address) = (g.a1 as u64, g.a2 as u64);
        let dup = sys::handle_duplicate(irq, Rights::WAIT.0);
        msi_send(f, d, address, 1)?;
        let fired = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 2000);
        sys::handle_close(irq);
        ensure_eq!(st(fired), Some(S::Ok), "the vector fires before the revoke");
        ensure_eq!(st(dup), Some(S::Ok), "a duplicate");
        let dup = dup.a1 as u64;
        ensure_eq!(sys::debug_stat(146), live + 1, "a duplicate keeps it live");
        sys::handle_close(dup);
        ensure_eq!(sys::debug_stat(146), live, "the last handle revoked it");
        let (notices, before) = (sys::debug_stat(162), iommu_faults());
        msi_send(f, d, address, 1)?;
        ensure!(
            iommu_wait_faults(before).is_some(),
            "the revoked vector's message raised no fault"
        );
        let (did, _, cause, _) = iommu_last_fault();
        ensure_eq!(
            (did, cause),
            (IOMMU_TEST_DID, CAUSE_MSI_INVALID),
            "the entry is invalid"
        );
        msi_settle();
        ensure_eq!(
            sys::debug_stat(162),
            notices,
            "nothing is delivered after the revoke"
        );
        vectors_at_baseline(live, ids)?;
        // granted again, it works again
        let g = ok(sys::iodomain_grant_vector(d.sup, 2), "grant vector 2 again")?;
        let irq = g.a1 as u64;
        msi_send(f, d, g.a2 as u64, 1)?;
        let fired = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 2000);
        sys::handle_close(irq);
        ensure_eq!(st(fired), Some(S::Ok), "the vector granted again fires");
        Ok(())
    });
    r.and(vectors_at_baseline(live, ids))
}

/// IOMMU-SPEC 6.3 and 10.6: destroying a domain with live vectors (one fired, one armed) revokes them all, and before the call returns
/// (no waiting afterwards) their identities are free again and no vector is live. The handles stay and name dead objects:
/// `interrupt_ack` and `interrupt_deliver_here` are BAD_STATE and the objects never fire again.
fn msi_destroy_revokes_every_vector() -> Outcome {
    let (live, ids) = (sys::debug_stat(146), sys::debug_stat(141));
    let r = with_domain(|_, f, d, _| {
        let a = ok(sys::iodomain_grant_vector(d.sup, 0), "grant vector 0")?;
        let b = ok(sys::iodomain_grant_vector(d.sup, 1), "grant vector 1")?;
        let (x, y) = (a.a1 as u64, b.a1 as u64);
        let r = (|| {
            msi_send(f, d, a.a2 as u64, 1)?;
            ensure_eq!(
                st(sys::object_wait(x, sig::INTERRUPT, sys::clock_get() + 2000)),
                Some(S::Ok),
                "vector 0 fires before the destruction"
            );
            ensure_eq!(sys::debug_stat(146), live + 2, "two live vectors");
            ok(sys::iodomain_destroy(d.sup), "iodomain_destroy")?;
            ensure_eq!(
                sys::debug_stat(146),
                live,
                "no live vector when the destroy returns"
            );
            ensure_eq!(
                sys::debug_stat(141),
                ids,
                "every identity free when the destroy returns"
            );
            for (h, what) in [(x, "the fired vector"), (y, "the armed vector")] {
                ensure_eq!(
                    st(sys::interrupt_ack(h)),
                    Some(S::BadState),
                    "ack on {what} after the destruction"
                );
                ensure_eq!(
                    st(sys::interrupt_deliver_here(h)),
                    Some(S::BadState),
                    "deliver_here on {what} after the destruction"
                );
            }
            ensure_eq!(
                st(sys::object_wait(y, sig::INTERRUPT, sys::clock_get() + 50)),
                Some(S::TimedOut),
                "the armed vector never fires again"
            );
            ensure_eq!(
                st(sys::iodomain_grant_vector(d.sup, 2)),
                Some(S::BadState),
                "a destroyed domain grants nothing"
            );
            Ok(())
        })();
        sys::handle_close(x);
        sys::handle_close(y);
        r
    });
    r.and(vectors_at_baseline(live, ids))
}

// ---- interrupts through devmgr's claim (IOMMU9; IOMMU-SPEC 8, 9 and 10) --------------------------------------------------------
//
// The suite is the driver here and `devmgr` the supervisor, as a real driver will be in IOMMU10: the claim reply carries the BAR, the
// domain handle with IO_MAP and IO_UNMAP, the DMA VMOs and one interrupt handle per vector of the class; `devmgr` granted the vectors
// and programmed the MSI-X table, and the driver only maps each event to its entry (`config_msix_vector`, `queue_msix_vector`).
// Every request below completes by interrupt: the used ring is read only after the queue's object fired.

/// The block function the harness puts at slot 6 (`addr=0x6`, `PCI_DEVICES`): the second disk (1 MiB), whose sector 0 holds byte i =
/// (i * 11 + 5) mod 256 until the filesystem tests (which run after the `pci_*` tests) format it, so a sector read from it is told from the first disk's.
const PCI_BLK2_SLOT: u64 = 6;
/// The entropy function's device id (`IOMMU_TEST_SLOT`, 00:04.0), claimed through `devmgr` by the forging test.
const PCI_RNG_RID: u16 = (IOMMU_TEST_SLOT << 3) as u16;
/// Where a claimed function's BAR, rings and data page are mapped here: slot `k` at CLAIM_VA + k * CLAIM_STRIDE.
const CLAIM_VA: u64 = 0x7e0_0000;
const CLAIM_STRIDE: u64 = 0x4_0000;
const CLAIM_RINGS: u64 = 0x1_0000;
const CLAIM_DATA: u64 = 0x2_0000;

/// One page of queue memory mapped at a page-aligned address.
struct PageMem(u64);
impl PageMem {
    fn at(&self, off: u64) -> u64 {
        self.0 + (off & 0xfff)
    }
}
impl virtio::Mem for PageMem {
    fn read16(&self, off: u64) -> u16 {
        rd16(self.at(off))
    }
    fn read32(&self, off: u64) -> u32 {
        // SAFETY: `at` keeps the address inside the page mapped read-write at `self.0` (a claimed function's rings), and the queue
        // layout aligns it.
        unsafe { core::ptr::read_volatile(self.at(off) as *const u32) }
    }
    fn write16(&mut self, off: u64, v: u16) {
        wr16(self.at(off), v)
    }
    fn write32(&mut self, off: u64, v: u32) {
        // SAFETY: as `read32`.
        unsafe { core::ptr::write_volatile(self.at(off) as *mut u32, v) }
    }
    fn write64(&mut self, off: u64, v: u64) {
        wr64(self.at(off), v)
    }
}

/// A function claimed through `devmgr` and brought up by the suite as its driver.
struct Claimed {
    /// Every handle the claim carried, in the reply's order (closed at the end), and the driver's job.
    hs: Vec<u64>,
    job: u64,
    c: proto::devmgr::PciClaim,
    base: u64,
    t: virtio::PciTransport<VaRegs>,
    q: virtio::SplitQueue,
    data_io: u64,
}

impl Claimed {
    fn dom(&self) -> u64 {
        self.hs[1]
    }
    /// The interrupt handle that means `what` (`IRQ_CONFIG` or a queue).
    fn irq(&self, what: u16) -> Option<u64> {
        self.c.irq(what).map(|(i, _)| self.hs[i])
    }
    fn data_va(&self) -> u64 {
        self.base + CLAIM_DATA
    }
    fn rings_va(&self) -> u64 {
        self.base + CLAIM_RINGS
    }

    /// Give everything back: the device reset, the mappings and the handles (the claim is released when the connection closes).
    fn drop_all(self) {
        use virtio::Transport;
        let mut t = self.t;
        t.set_status(0);
        unmap_here(self.base + CLAIM_DATA);
        unmap_here(self.base + CLAIM_RINGS);
        unmap_here(self.base);
        for h in self.hs {
            sys::handle_close(h);
        }
        sys::handle_close(self.job);
    }
}

/// Claim `rid` on `ch` for a new driver job and bring it up as its driver at slot `k`'s addresses: the BAR, the rings and the data
/// page mapped here and (the rings and the data page) in the domain, features negotiated, the configuration change and queue 0
/// mapped to the MSI-X entries `vectors` names, queue 0 of 8 entries enabled, DRIVER_OK.
fn claim_and_bring_up(
    ch: u64,
    txid: u32,
    rid: u16,
    k: u64,
    vectors: &virtio::VectorMap,
) -> Result<Claimed, Detail> {
    use virtio::Transport;
    let (job, send) = driver_job()?;
    let (s, c, hs) = match pci_claim(ch, txid, rid, Some(send)) {
        Ok(r) => r,
        Err(e) => {
            sys::handle_close(job);
            return Err(e);
        }
    };
    let base = CLAIM_VA + k * CLAIM_STRIDE;
    let up = (|| {
        ensure_eq!(s, proto::devmgr::Status::Ok, "the claim of {rid:#x}");
        let c = c.clone().ok_or_else(|| msg_at(line!()))?;
        let (bar, dom, rings, data) = (hs[0], hs[1], hs[2], hs[3]);
        let me = ok(sys::process_self(), "process_self")?.a1 as u64;
        let m = sys::process_map(me, bar, base, 0, c.bar_bytes, PF_R | PF_W);
        sys::handle_close(me);
        ensure_eq!(st(m), Some(S::Ok), "map the BAR VMO");
        map_here(rings, base + CLAIM_RINGS, 1, PF_R | PF_W)?;
        map_here(data, base + CLAIM_DATA, 1, PF_R | PF_W)?;
        let rings_io = ok(sys::iodomain_map(dom, rings, 0, 1, 3), "map the rings")?.a1 as u64;
        let data_io = ok(sys::iodomain_map(dom, data, 0, 1, 3), "map a data page")?.a1 as u64;
        for i in 0..512 {
            wr64(base + CLAIM_RINGS + 8 * i, 0);
            wr64(base + CLAIM_DATA + 8 * i, 0);
        }
        let mut t = virtio::PciTransport::new(
            VaRegs,
            virtio::PciLocations {
                common: base + u64::from(c.common.0),
                notify: base + u64::from(c.notify.0),
                notify_len: u64::from(c.notify.1),
                notify_multiplier: c.notify_multiplier,
                isr: base + u64::from(c.isr.0),
                device: (c.device_cfg.1 != 0)
                    .then(|| (base + u64::from(c.device_cfg.0), u64::from(c.device_cfg.1))),
            },
        );
        let t0 = sys::clock_get();
        vio(t.reset(&mut || sys::clock_get() < t0 + 1000), "reset")?;
        vio(t.negotiate(virtio::feature::REQUIRED), "negotiate")?;
        vio(t.set_config_vector(vectors.config), "config_msix_vector")?;
        let q = vio(virtio::SplitQueue::new(8, rings_io, 4096), "the queue")?;
        vio(
            t.setup_queue(0, 8, q.addresses(), vectors.queue(0)),
            "queue 0",
        )?;
        t.driver_ok();
        Ok(Claimed {
            hs: hs.clone(),
            job,
            c,
            base,
            t,
            q,
            data_io,
        })
    })();
    if up.is_err() {
        for &h in &hs {
            sys::handle_close(h);
        }
        unmap_here(base + CLAIM_DATA);
        unmap_here(base + CLAIM_RINGS);
        unmap_here(base);
        sys::handle_close(job);
    }
    up
}

/// Read sector 0 of a claimed block function, asking for an interrupt: the header and status in its data page, the sector after
/// them. The head's id.
fn claimed_blk_submit(b: &mut Claimed) -> Result<u16, Detail> {
    use virtio::Transport;
    let h = virtio::blk::header(virtio::blk::T_IN, 0);
    for (i, w) in h.as_chunks::<8>().0.iter().enumerate() {
        wr64(b.data_va() + BLK_HDR + 8 * i as u64, u64::from_le_bytes(*w));
    }
    for i in 0..64 {
        wr64(b.data_va() + BLK_DATA + 8 * i, 0);
    }
    wr64(b.data_va() + BLK_STATUS, 0xff);
    let chain = [
        virtio::Buf {
            addr: b.data_io + BLK_HDR,
            len: virtio::blk::HEADER_BYTES,
            device_writes: false,
        },
        virtio::Buf {
            addr: b.data_io + BLK_DATA,
            len: virtio::blk::SECTOR,
            device_writes: true,
        },
        virtio::Buf {
            addr: b.data_io + BLK_STATUS,
            len: 1,
            device_writes: true,
        },
    ];
    let mut mem = PageMem(b.rings_va());
    let head = vio(b.q.add(&mut mem, &chain, true), "add the request")?;
    b.t.notify(0);
    Ok(head)
}

/// The used element a claimed function returned for request `head`: OK status and its disk's pattern `(mul, add)` in the sector.
fn claimed_blk_done(b: &mut Claimed, head: u16, (mul, add): (u64, u64)) -> Outcome {
    let u = vio(b.q.take_used(&PageMem(b.rings_va())), "the used ring")?;
    ensure_eq!(
        u.map(|u| u.0),
        Some(head),
        "the device returned the request it was interrupting for"
    );
    ensure_eq!(
        rd8_dma(b.data_va() + BLK_STATUS),
        virtio::blk::S_OK,
        "the request's status"
    );
    let wrong = (0..512u64)
        .find(|&i| rd8_dma(b.data_va() + BLK_DATA + i) != ((i * mul + add) & 0xff) as u8);
    ensure_eq!(
        wrong,
        None,
        "sector 0 is its own disk's (byte i = i * {mul} + {add})"
    );
    Ok(())
}

/// Wait for the used ring to reach `n` entries (a request completed while its interrupt was masked: no notice to wake for).
fn claimed_used_reaches(b: &Claimed, n: u16) -> Outcome {
    let t0 = sys::clock_get();
    while rd16(b.rings_va() + b.q.layout.used_idx()) != n {
        ensure!(
            sys::clock_get() < t0 + 2000,
            "the device did not return the request"
        );
        sys::thread_yield();
    }
    Ok(())
}

/// `interrupt_deliver_here` from a thread of its own (`place_here`'s status, u64::MAX until it ran).
static PLACE_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);
extern "C" fn place_here(irq: usize) -> ! {
    PLACE_RESULT.store(
        sys::interrupt_deliver_here(irq as u64).status as u64,
        Ordering::SeqCst,
    );
    sys::thread_exit(0)
}

/// The sector-0 patterns of the two PCI disks, the scratch disk and the second disk (`pci_disk` in the script).
const BLK1_PATTERN: (u64, u64) = (7, 3);
const BLK2_PATTERN: (u64, u64) = (11, 5);
/// Block: the configuration change on entry 0, queue 0 on entry 1 (the class plan of `pcicore::claim`).
fn blk_vectors() -> virtio::VectorMap {
    virtio::VectorMap {
        config: 0,
        queues: vec![1],
    }
}

/// Run `body` with `devmgr` started (its service channel) and the root resource; the manager ends when the channel closes, which
/// releases every claim, and the test waits for that.
fn with_devmgr(body: impl FnOnce(u64, u64) -> Outcome) -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let (mine, mgr, mj) = start_devmgr_in_job(res)?;
    let r = body(res, mine);
    sys::handle_close(mine);
    let end = wait_ended(mgr, 5000);
    sys::handle_close(mgr);
    sys::handle_close(mj);
    sys::handle_close(res);
    r.and(end)
}

/// IOMMU9 (IOMMU-SPEC 8.3, 8.5, 9; D5 for a vector): the block function claimed through `devmgr`'s real path. The reply carries one
/// interrupt handle per vector of the block class (the configuration change on entry 0, the queue on entry 1); a request completes by
/// **interrupt** (the used ring is read only after the queue's object fired); while fired the object stays masked (a second request
/// completes with no notice) and the acknowledgement fires it again at once from the pending bit; armed, nothing more. Placed on the
/// hart that asks (`interrupt_deliver_here`, made with every other hart offline so the asking hart is known: hart 0), it stays there
/// and is delivered there. The configuration object never fires, and the IOMMU takes no fault. The negative-control kernels
/// `fault-vector-no-unmask` and `fault-vector-no-repend` (the vector twins of `interrupt_object_fires_and_masks`'s) must fail here.
fn pci_virtio_blk_interrupts_through_msix() -> Outcome {
    if !pci_disks_present()? {
        return Ok(()); // the no-disk boot
    }
    with_devmgr(|res, mgr| {
        let mut b = claim_and_bring_up(mgr, 1, PCI_BLK_DID as u16, 0, &blk_vectors())?;
        let r = (|| {
            use proto::devmgr::{PciIrq, IRQ_CONFIG};
            ensure_eq!(
                b.c.irqs,
                vec![
                    PciIrq {
                        entry: 0,
                        what: IRQ_CONFIG
                    },
                    PciIrq { entry: 1, what: 0 }
                ],
                "the block class's vectors: the configuration change on entry 0, the queue on entry 1"
            );
            let (cfg, q) = (
                b.irq(IRQ_CONFIG).ok_or_else(|| msg_at(line!()))?,
                b.irq(0).ok_or_else(|| msg_at(line!()))?,
            );
            let (faults, notices) = (iommu_faults(), sys::debug_stat(162));
            // the first request, by interrupt
            let head = claimed_blk_submit(&mut b)?;
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                Some(S::Ok),
                "the request's interrupt (notices {notices} to {}, faults {faults} to {})",
                sys::debug_stat(162),
                iommu_faults()
            );
            claimed_blk_done(&mut b, head, BLK1_PATTERN)?;
            // masked until acknowledged: the second request completes with no notice, and the acknowledgement fires it again
            let head = claimed_blk_submit(&mut b)?;
            claimed_used_reaches(&b, 2)?;
            msi_settle();
            ensure_eq!(
                sys::debug_stat(162),
                notices + 1,
                "fired: the second message sent no notice (D5)"
            );
            let a = ok(sys::interrupt_ack(q), "acknowledge")?;
            ensure_eq!(
                a.a1,
                2,
                "two claims: the first request, and the second's message taken from the pending bit at the acknowledgement"
            );
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 1)),
                Some(S::Ok),
                "the message that came while masked fires again at the acknowledgement"
            );
            claimed_blk_done(&mut b, head, BLK1_PATTERN)?;
            ok(sys::interrupt_ack(q), "acknowledge again")?;
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 50)),
                Some(S::TimedOut),
                "armed, nothing pending"
            );
            // placed on the hart that asks: with the other harts offline the asking thread runs on hart 0, whose interrupt file is
            // then the route's (`debug_stat` 148 names the file, and file numbers follow the hart ids, not the logical harts: hart 0 is
            // the boot hart, whichever id that is)
            let harts = hart_count(res);
            let mut off = Vec::new();
            let placed = (|| {
                for h in 1..harts {
                    ensure_eq!(
                        st(sys::hart_set_online(res, h, 0)),
                        Some(S::Ok),
                        "hart {h} offline"
                    );
                    off.push(h);
                }
                ok(sys::interrupt_deliver_here(q), "interrupt_deliver_here")?;
                Ok(vector_route(q))
            })();
            let mut back = Ok(());
            for &h in &off {
                if st(sys::hart_set_online(res, h, 1)) != Some(S::Ok) && back.is_ok() {
                    back = Err(msg_at(line!()));
                }
            }
            let route = placed?;
            back?;
            ensure!(route != u64::MAX, "placed on hart 0, which asked");
            let file0 = route >> 16;
            let head = claimed_blk_submit(&mut b)?;
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                Some(S::Ok),
                "delivered where it was placed"
            );
            claimed_blk_done(&mut b, head, BLK1_PATTERN)?;
            ensure_eq!(
                vector_route(q),
                route,
                "it stays where it was asked to be (no automatic follow)"
            );
            ok(sys::interrupt_ack(q), "acknowledge")?;
            // and with more harts, the call moves it: a thread that runs elsewhere (the asking thread spins here meanwhile) asks
            // for it, and the route leaves hart 0's file for the file of the hart that asked; a request is delivered there
            if harts > 1 {
                let mut moved = None;
                for _ in 0..50 {
                    PLACE_RESULT.store(u64::MAX, Ordering::SeqCst);
                    let t = new_thread(place_here, q as usize)?;
                    let t0 = sys::clock_get();
                    while sys::object_signals(t).a1 as u32 & sig::TERMINATED == 0
                        && sys::clock_get() < t0 + 2000
                    {
                        core::hint::spin_loop();
                    }
                    sys::handle_close(t);
                    ensure_eq!(
                        PLACE_RESULT.load(Ordering::SeqCst),
                        0,
                        "interrupt_deliver_here from another thread"
                    );
                    let r = vector_route(q);
                    if r != u64::MAX && r >> 16 != file0 {
                        moved = Some(r);
                        break;
                    }
                }
                let moved = moved.ok_or_else(|| msg_at(line!()))?;
                let head = claimed_blk_submit(&mut b)?;
                ensure_eq!(
                    st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                    Some(S::Ok),
                    "delivered on the hart that asked ({moved:#x})"
                );
                claimed_blk_done(&mut b, head, BLK1_PATTERN)?;
                ensure_eq!(vector_route(q), moved, "and it stays there");
                ok(sys::interrupt_ack(q), "acknowledge")?;
            }
            msi_settle();
            ensure_eq!(
                st(sys::object_wait(cfg, sig::INTERRUPT, sys::clock_get() + 1)),
                Some(S::TimedOut),
                "the configuration-change object never fired"
            );
            ensure_eq!(iommu_faults(), faults, "no IOMMU fault");
            Ok(())
        })();
        b.drop_all();
        r
    })
}

/// IOMMU9 (IOMMU-SPEC 8.3, 9): two block functions claimed at once, each with its two vectors; requests on both, one after the other
/// and together. Every interrupt arrives on its own device's queue object and none on another's: each queue object's claims equal
/// its device's requests, the configuration objects never fire, and each sector is its own disk's.
fn pci_two_devices_do_not_share() -> Outcome {
    if !pci_disks_present()? {
        return Ok(()); // the no-disk boot
    }
    with_devmgr(|_, mgr| {
        let mut a = claim_and_bring_up(mgr, 1, PCI_BLK_DID as u16, 0, &blk_vectors())?;
        let mut b = match claim_and_bring_up(mgr, 2, (PCI_BLK2_SLOT << 3) as u16, 1, &blk_vectors())
        {
            Ok(b) => b,
            Err(e) => {
                a.drop_all();
                return Err(e);
            }
        };
        let r = (|| {
            use proto::devmgr::IRQ_CONFIG;
            let faults = iommu_faults();
            let irq = |x: &Claimed, w| x.irq(w).ok_or_else(|| msg_at(line!()));
            let (aq, ac, bq, bc) = (
                irq(&a, 0)?,
                irq(&a, IRQ_CONFIG)?,
                irq(&b, 0)?,
                irq(&b, IRQ_CONFIG)?,
            );
            ensure!(
                [aq, ac, bq, bc]
                    .iter()
                    .all(|&h| vector_route(h) != u64::MAX),
                "four live vectors"
            );
            const N: u64 = 4;
            for i in 0..N {
                // one at a time: A's request interrupts A's queue object only
                for (x, q, others, pat) in [
                    (&mut a, aq, [ac, bq, bc], BLK1_PATTERN),
                    (&mut b, bq, [bc, aq, ac], BLK2_PATTERN),
                ] {
                    let head = claimed_blk_submit(x)?;
                    ensure_eq!(
                        st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                        Some(S::Ok),
                        "request {i}'s interrupt"
                    );
                    msi_settle();
                    for o in others {
                        ensure_eq!(
                            sys::object_signals(o).a1 as u32 & sig::INTERRUPT,
                            0,
                            "request {i}: another object fired"
                        );
                    }
                    claimed_blk_done(x, head, pat)?;
                    let n = ok(sys::interrupt_ack(q), "acknowledge")?.a1;
                    ensure_eq!(n as u64, 2 * i + 1, "one claim per request");
                }
                // and together
                let (ha, hb) = (claimed_blk_submit(&mut a)?, claimed_blk_submit(&mut b)?);
                for (x, q, h, pat) in [
                    (&mut a, aq, ha, BLK1_PATTERN),
                    (&mut b, bq, hb, BLK2_PATTERN),
                ] {
                    ensure_eq!(
                        st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                        Some(S::Ok),
                        "request {i}'s interrupt, both devices busy"
                    );
                    claimed_blk_done(x, h, pat)?;
                    let n = ok(sys::interrupt_ack(q), "acknowledge")?.a1;
                    ensure_eq!(
                        n as u64,
                        2 * i + 2,
                        "one claim per request, both devices busy"
                    );
                }
            }
            msi_settle();
            for (h, what) in [(ac, "A's configuration"), (bc, "B's configuration")] {
                ensure_eq!(
                    st(sys::interrupt_ack(h)),
                    Some(S::BadState),
                    "{what} object never fired"
                );
            }
            for q in [aq, bq] {
                ensure_eq!(
                    st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 20)),
                    Some(S::TimedOut),
                    "nothing more after the last request"
                );
            }
            ensure_eq!(iommu_faults(), faults, "no IOMMU fault");
            Ok(())
        })();
        a.drop_all();
        b.drop_all();
        r
    })
}

/// IOMMU9 (IOMMU-SPEC 8.4, 8.7, 9): what a driver is handed cannot reach past its device. The entropy function is claimed through
/// `devmgr` (its class has one vector, queue 0 on entry 1; entry 0 of its two is left masked and cleared). The suite then uses only
/// the claim's handles, as a driver would: the BAR VMO is the virtio structures' BAR, not the MSI-X table's (which the configuration
/// space, read here through the root resource as an observer, puts in another BAR), and the entry `devmgr` programmed, read by the
/// observer, appears nowhere in what the BAR VMO maps; the domain handle cannot grant a vector, destroy the domain, read its faults or
/// be duplicated; the interrupt handle cannot be duplicated or grant. The driver then points `queue_msix_vector` at entry 0, which
/// `devmgr` did not program: the device's message for the queue is not sent (or, were it sent, would fault in the IOMMU with cause
/// 262), and nobody is interrupted. Pointed at its own entry 1, the same request interrupts (the negative is not vacuous).
///
/// What this does not show, by construction: the test runs in the suite's process, which also holds the root resource (it is the
/// observer above), so "a driver has no handle for the configuration space or the table" rests on the claim's handle list (five
/// handles, each checked by type and rights here), not on a process that holds nothing else. And because entry 0 is masked and
/// cleared, QEMU does not send the message at all: this proves the table's mask, not the IOMMU's refusal of an ungranted vector,
/// which `msi_forgery_faults` (IOMMU7) proves with a message actually sent.
fn pci_driver_cannot_forge_through_devmgr() -> Outcome {
    with_devmgr(|res, mgr| {
        let mut x = claim_and_bring_up(
            mgr,
            1,
            PCI_RNG_RID,
            0,
            &virtio::VectorMap {
                config: virtio::NO_VECTOR,
                queues: vec![0],
            },
        )?;
        let r = (|| {
            use proto::devmgr::PciIrq;
            ensure_eq!(
                (x.c.dma_pages.clone(), x.c.irqs.clone()),
                (vec![1, 1], vec![PciIrq { entry: 1, what: 0 }]),
                "the entropy class: two DMA pages, queue 0 on entry 1"
            );
            ensure_eq!(
                x.hs.len(),
                5,
                "the BAR, the domain, two DMA VMOs, one interrupt"
            );
            let q = x.irq(0).ok_or_else(|| msg_at(line!()))?;
            // the configuration space as an observer sees it: the MSI-X table is in a BAR of its own
            let me = ok(sys::process_self(), "process_self")?.a1 as u64;
            let cfgv = ok(
                sys::vmo_create_physical(res, PCI_ECAM + (IOMMU_TEST_SLOT << 15), 4096),
                "the configuration page",
            )?
            .a1 as u64;
            let m = sys::process_map(me, cfgv, PCI_CFG_VA, 0, 4096, PF_R);
            sys::handle_close(cfgv);
            sys::handle_close(me);
            ensure_eq!(st(m), Some(S::Ok), "map it read-only");
            let mut cfg = [0u8; pcicore::CFG_BYTES];
            for (i, w) in cfg.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *w = cfg_rd32(4 * i as u64).to_le_bytes();
            }
            unmap_here(PCI_CFG_VA);
            let caps = pcicore::capabilities(&cfg).map_err(|_| msg_at(line!()))?;
            let msix = pcicore::msix_cap(&cfg, &caps)
                .ok()
                .flatten()
                .ok_or_else(|| msg_at(line!()))?;
            let vbar = pcicore::virtio_caps(&cfg, &caps)
                .ok()
                .and_then(|v| {
                    v.iter()
                        .find(|c| c.cfg_type == pcicore::virtio_cfg::COMMON)
                        .map(|c| c.bar)
                })
                .ok_or_else(|| msg_at(line!()))?;
            ensure!(
                msix.table_bar != vbar && msix.pba_bar != vbar,
                "the MSI-X table (BAR {}) is not in the driver's BAR ({vbar})",
                msix.table_bar
            );
            ensure_eq!(
                msix.table_size,
                2,
                "QEMU's entropy function has two entries"
            );
            // the table's entry 1 as the observer reads it (devmgr's grant: an address in the MRIF window, the ABI's data), and
            // nowhere in what the driver can read through its BAR VMO: the driver's mapping does not contain the table
            let bar_reg = |i: u8| {
                let at = 0x10 + 4 * usize::from(i);
                let w = |o: usize| u32::from_le_bytes([cfg[o], cfg[o + 1], cfg[o + 2], cfg[o + 3]]);
                let lo = w(at);
                let hi = if lo & 0x6 == 0x4 { w(at + 4) } else { 0 };
                (u64::from(hi) << 32) | u64::from(lo & !0xf)
            };
            let entry1 = bar_reg(msix.table_bar) + u64::from(msix.table_offset) + 16;
            let me = ok(sys::process_self(), "process_self")?.a1 as u64;
            let tv = ok(
                sys::vmo_create_physical(res, entry1 & !0xfff, 4096),
                "the table's page",
            )?
            .a1 as u64;
            let m = sys::process_map(me, tv, PCI_CFG_VA, 0, 4096, PF_R);
            sys::handle_close(tv);
            sys::handle_close(me);
            ensure_eq!(st(m), Some(S::Ok), "map the table's page read-only");
            let e = PCI_CFG_VA + (entry1 & 0xfff);
            let (lo, hi, data) = (mmio_rd32(e), mmio_rd32(e + 4), mmio_rd32(e + 8));
            unmap_here(PCI_CFG_VA);
            let granted = (u64::from(hi) << 32) | u64::from(lo);
            ensure!(
                granted != 0 && u64::from(data) == rt::abi::iodomain::MSI_DATA,
                "devmgr programmed entry 1 ({granted:#x}, data {data})"
            );
            let isr = u64::from(x.c.isr.0)..u64::from(x.c.isr.0) + u64::from(x.c.isr.1).max(4);
            let seen = (0..x.c.bar_bytes / 4)
                .map(|i| 4 * i)
                .filter(|o| !isr.contains(o))
                .find(|&o| {
                    o + 8 < x.c.bar_bytes
                        && mmio_rd32(x.base + o) == lo
                        && mmio_rd32(x.base + o + 4) == hi
                        && mmio_rd32(x.base + o + 8) == data
                });
            ensure_eq!(
                seen,
                None,
                "the granted entry is not readable through the driver's BAR VMO"
            );
            // the domain handle maps and unmaps; the interrupt handle waits and acknowledges
            for (r, what) in [
                (sys::iodomain_grant_vector(x.dom(), 0), "grant vector 0"),
                (sys::iodomain_grant_vector(x.dom(), 1), "grant vector 1"),
                (sys::iodomain_destroy(x.dom()), "destroy the domain"),
                (
                    sys::iodomain_fault_read(x.dom()),
                    "read the domain's faults",
                ),
                (
                    sys::handle_duplicate(x.dom(), Rights::IO_MAP.0),
                    "duplicate the domain",
                ),
                (
                    sys::handle_duplicate(q, Rights::WAIT.0),
                    "duplicate the interrupt",
                ),
            ] {
                ensure_eq!(st(r), Some(S::AccessDenied), "the driver cannot {what}");
            }
            ensure_eq!(
                st(sys::iodomain_grant_vector(q, 0)),
                Some(S::WrongType),
                "an interrupt handle grants nothing"
            );
            // queue 0 pointed at entry 0, which devmgr left masked and cleared: one request
            let (faults, notices, no_route) =
                (iommu_faults(), sys::debug_stat(162), sys::debug_stat(140));
            claimed_rng_request(&mut x)?;
            claimed_used_reaches(&x, 1)?;
            msi_settle();
            ensure_eq!(sys::debug_stat(162), notices, "no notice reached a hart");
            ensure_eq!(
                sys::debug_stat(140),
                no_route,
                "no message on an identity with no route"
            );
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 20)),
                Some(S::TimedOut),
                "the granted vector did not fire for a message aimed elsewhere"
            );
            if iommu_faults() != faults {
                let (did, _, cause, _) = iommu_last_fault();
                ensure_eq!(
                    (did, cause),
                    (u64::from(PCI_RNG_RID), CAUSE_MSI_INVALID),
                    "a message that left the device faults as an ungranted vector"
                );
            }
            let _ = vio(x.q.take_used(&PageMem(x.rings_va())), "the used ring")?;
            // and pointed at its own entry: the same request interrupts
            {
                use virtio::Transport;
                let t0 = sys::clock_get();
                vio(x.t.reset(&mut || sys::clock_get() < t0 + 1000), "reset")?;
                vio(x.t.negotiate(virtio::feature::REQUIRED), "negotiate")?;
                let rings_io = x.q.io_base;
                x.q = vio(virtio::SplitQueue::new(8, rings_io, 4096), "the queue")?;
                for i in 0..512 {
                    wr64(x.rings_va() + 8 * i, 0);
                }
                vio(
                    x.t.setup_queue(0, 8, x.q.addresses(), 1),
                    "queue 0 on entry 1",
                )?;
                x.t.driver_ok();
            }
            claimed_rng_request(&mut x)?;
            ensure_eq!(
                st(sys::object_wait(q, sig::INTERRUPT, sys::clock_get() + 2000)),
                Some(S::Ok),
                "the queue on its own entry interrupts"
            );
            ok(sys::interrupt_ack(q), "acknowledge")?;
            Ok(())
        })();
        x.drop_all();
        r
    })
}

/// One entropy request on a claimed virtio-rng function: 64 device-writable bytes of its data page, an interrupt asked for.
fn claimed_rng_request(x: &mut Claimed) -> Outcome {
    use virtio::Transport;
    let chain = [virtio::Buf {
        addr: x.data_io,
        len: 64,
        device_writes: true,
    }];
    let mut mem = PageMem(x.rings_va());
    vio(x.q.add(&mut mem, &chain, true), "add the request")?;
    x.t.notify(0);
    Ok(())
}

/// IOMMU4: identities are allocated per hart, next-fit, so an object re-created again and again walks the whole identity space of a
/// file; an object routed through an identity of 64 or more (every `eie` register, up to `riscv,num-ids`) fires, stays masked until it
/// is acknowledged and fires again after (D5). Before IOMMU4 the identity was source + 1 and `claim` dropped every identity above 63.
fn interrupt_identities_above_63_work() -> Outcome {
    with_rtc(identities_above_63)
}

fn identities_above_63(res: u64) -> Outcome {
    let Some(probe) = rtc_object(res)? else {
        return Ok(()); // (no interrupt controller, or the RTC is not admitted: the harness checks which machine this is)
    };
    sys::handle_close(probe);
    let num_ids = sys::debug_stat(144);
    ensure!(
        (63..=2047).contains(&num_ids),
        "riscv,num-ids reads {num_ids}"
    );
    let (no_route, misrouted) = (sys::debug_stat(140), sys::debug_stat(5));
    let harts = hart_count(res);
    rtc_raise();
    let limit = harts * num_ids + 64;
    let mut i = 0;
    while sys::debug_stat(142) < num_ids {
        ensure!(
            i < limit,
            "after {i} objects the highest identity a message was taken on is {}, not {num_ids}",
            sys::debug_stat(142)
        );
        let irq = ok(sys::interrupt_create(res, RTC_IRQ), "interrupt_create")?.a1 as u64;
        let r = one_identity(irq, i);
        sys::handle_close(irq);
        r?;
        i += 1;
    }
    ensure_eq!(sys::debug_stat(140), no_route, "messages with no route");
    ensure_eq!(sys::debug_stat(5), misrouted, "misrouted messages");
    sys::debug_write(
        alloc::format!(
            "irq: {i} objects walked the identities up to {} of {num_ids}\n",
            sys::debug_stat(142)
        )
        .as_bytes(),
    );
    Ok(())
}

/// The `i`-th object of `identities_above_63`: it fires, takes one claim until acknowledged, and fires again after.
fn one_identity(irq: u64, i: u64) -> Outcome {
    let r = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 2000);
    ensure_eq!(
        st(r),
        Some(S::Ok),
        "object {i} fired (highest identity so far {})",
        sys::debug_stat(142)
    );
    if i.is_multiple_of(32) {
        // masked while fired: the line stays high, and the kernel takes nothing more until the acknowledgement
        nap_ticks(2);
    }
    let ack = ok(sys::interrupt_ack(irq), "interrupt_ack")?;
    ensure_eq!(
        ack.a1,
        1,
        "object {i}: one claim before the acknowledgement"
    );
    let r = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 2000);
    ensure_eq!(
        st(r),
        Some(S::Ok),
        "object {i} fired again after its acknowledgement (the line is still high)"
    );
    Ok(())
}

static MV_IRQ: AtomicU64 = AtomicU64::new(0);
static MV_STOP: AtomicU32 = AtomicU32::new(0);
static MV_DONE: AtomicU64 = AtomicU64::new(0);
static MV_LOST: AtomicU32 = AtomicU32::new(0);

/// One of the acknowledging threads: wait for the interrupt and acknowledge it (whichever thread is first; the others find it
/// acknowledged already, BAD_STATE, and wait again). A wait that times out is a request that never completed: the object did not fire
/// within 3 s although the line stays high. (`object_wait` says OK when the signal ended the wait, even if another thread acknowledged
/// it before this one returned: before IOMMU4 it read the signals again after the sleep and could say TIMED_OUT early.)
extern "C" fn mv_acker(_: usize) -> ! {
    let irq = MV_IRQ.load(Ordering::SeqCst);
    while MV_STOP.load(Ordering::SeqCst) == 0 {
        let r = sys::object_wait(irq, sig::INTERRUPT, sys::clock_get() + 3000);
        if st(r) != Some(S::Ok) {
            if MV_STOP.load(Ordering::SeqCst) == 0 {
                MV_LOST.store(1, Ordering::SeqCst);
            }
            break;
        }
        if st(sys::interrupt_ack(irq)) == Some(S::Ok) {
            MV_DONE.fetch_add(1, Ordering::SeqCst);
        }
    }
    sys::thread_exit(0)
}

/// IOMMU4: an object acknowledged from changing harts a few thousand times while its device keeps firing, and moved off harts taken
/// offline under it: every request completes, no message reaches an identity with no route, and every identity comes back (the free
/// count at the end equals the start). At two harts and more the route must have moved.
fn interrupt_moves_do_not_lose_or_leak() -> Outcome {
    with_rtc(moves_do_not_lose_or_leak)
}

fn moves_do_not_lose_or_leak(res: u64) -> Outcome {
    let free0 = sys::debug_stat(141);
    let (no_route, misrouted, moves) = (
        sys::debug_stat(140),
        sys::debug_stat(5),
        sys::debug_stat(143),
    );
    let Some(irq) = rtc_object(res)? else {
        return Ok(());
    };
    let harts = hart_count(res);
    MV_IRQ.store(irq, Ordering::SeqCst);
    MV_STOP.store(0, Ordering::SeqCst);
    MV_DONE.store(0, Ordering::SeqCst);
    MV_LOST.store(0, Ordering::SeqCst);
    rtc_raise();
    let mut threads = Vec::new();
    let mut failed: Option<alloc::string::String> = None;
    for _ in 0..harts.clamp(2, 4) {
        match new_thread(mv_acker, 0) {
            Ok(t) => threads.push(t),
            Err(_) => {
                failed = Some("an acknowledging thread could not be made".into());
                break;
            }
        }
    }
    const WANT: u64 = 3000;
    let until = sys::clock_get() + 60_000;
    let mut cycled = 0u64;
    while failed.is_none()
        && MV_DONE.load(Ordering::SeqCst) < WANT
        && MV_LOST.load(Ordering::SeqCst) == 0
    {
        if sys::clock_get() > until {
            failed = Some(alloc::format!(
                "only {} of {WANT} acknowledgements in a minute",
                MV_DONE.load(Ordering::SeqCst)
            ));
            break;
        }
        // every 500 acknowledgements a hart other than 0 goes offline and comes back: the routes on it move (IOMMU-SPEC 8.5), and
        // the threads on it run elsewhere, so the acknowledgements come from other harts
        let done = MV_DONE.load(Ordering::SeqCst);
        if harts > 1 && done / 500 > cycled {
            cycled = done / 500;
            let h = 1 + cycled % (harts - 1);
            if st(sys::hart_set_online(res, h, 0)) != Some(S::Ok) {
                failed = Some(alloc::format!("hart {h} did not go offline"));
                break;
            }
            nap_ticks(2);
            if st(sys::hart_set_online(res, h, 1)) != Some(S::Ok) {
                failed = Some(alloc::format!("hart {h} did not come back"));
                break;
            }
        }
        nap_ticks(1);
    }
    MV_STOP.store(1, Ordering::SeqCst);
    for t in &threads {
        let r = sys::object_wait(*t, sig::TERMINATED, sys::clock_get() + 10_000);
        if st(r) != Some(S::Ok) && failed.is_none() {
            failed = Some("an acknowledging thread did not end".into());
        }
        sys::handle_close(*t);
    }
    rtc_quiet();
    sys::handle_close(irq);
    if let Some(f) = failed {
        ensure!(false, "{f}");
    }
    ensure_eq!(
        MV_LOST.load(Ordering::SeqCst),
        0,
        "a request never completed (no interrupt within 3 s of the acknowledgement), after {} acknowledgements",
        MV_DONE.load(Ordering::SeqCst)
    );
    // the identities come back: the old ones are retired by their harts after the last move and the close
    let until = sys::clock_get() + 2000;
    while sys::debug_stat(141) != free0 && sys::clock_get() < until {
        nap_ticks(2);
    }
    let free1 = sys::debug_stat(141);
    let moved = sys::debug_stat(143) - moves;
    sys::debug_write(
        alloc::format!(
            "irq: {} acknowledgements on {harts} harts, {moved} identities retired after a move, free identities {free0} then {free1}\n",
            MV_DONE.load(Ordering::SeqCst)
        )
        .as_bytes(),
    );
    ensure_eq!(
        free1,
        free0,
        "free interrupt identities at the end and at the start"
    );
    ensure_eq!(sys::debug_stat(140), no_route, "messages with no route");
    ensure_eq!(sys::debug_stat(5), misrouted, "misrouted messages");
    if harts > 1 {
        ensure!(moved > 0, "the route never moved on {harts} harts");
    }
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

/// T284: windows of the probe taken again because a pause of the host covered them, the worst such pause (as `debug_stat` 216 packs
/// it), and the pause that covered the third window in a row when the probe gave up (0: it did not).
static RT_RETAKEN: AtomicU64 = AtomicU64::new(0);
static RT_WORST_PAUSE: AtomicU64 = AtomicU64::new(0);
static RT_GAVE_UP: AtomicU64 = AtomicU64::new(0);
/// The probe's bound in microseconds: a window that a pause later than this covered says nothing about the scheduler.
const RT_BOUND_US: u64 = 4_000;
/// The negative control's own bound (an equal-priority probe must be later than this): its windows are covered only by a pause later
/// than it, as the kernel twin's (`the_latency_test_can_fail`).
const RT_CONTROL_BOUND_US: u64 = 6_000;

/// The pause detector (T284, `debug_stat` 216): whether a pause of the host covered the window since `since_ns` (the `clock_ns` clock) for
/// a test whose bound is `bound_us` (two harts took a timer interrupt more than that late inside it), as (hart, microseconds late) of the
/// worst, or None.
fn host_pause_since(since_ns: u64, bound_us: u64) -> Option<(u64, u64)> {
    let r = rt::syscall(
        nr_debug_stat(),
        [
            rt::debug::handle() as usize,
            216,
            since_ns as usize,
            bound_us as usize,
            0,
            0,
        ],
    );
    let v = if r.status == 0 { r.a1 as u64 } else { 0 };
    (v != 0).then_some((v >> 48, v & ((1 << 48) - 1)))
}

/// How long the harts were not taking their timer interrupts since `since_ns`, summed over the harts, counting samples at least 2 ms late,
/// in microseconds (`debug_stat` 217).
fn host_paused_us_since(since_ns: u64) -> u64 {
    let r = rt::syscall(
        nr_debug_stat(),
        [
            rt::debug::handle() as usize,
            217,
            since_ns as usize,
            0,
            0,
            0,
        ],
    );
    if r.status == 0 {
        r.a1 as u64
    } else {
        0
    }
}

/// For the next `ms` milliseconds (0: stop) every hart keeps a timer deadline at most 1 ms ahead (`debug_stat` 218).
fn host_pause_watch(ms: u64) -> bool {
    rt::syscall(
        nr_debug_stat(),
        [rt::debug::handle() as usize, 218, ms as usize, 0, 0, 0],
    )
    .status
        == 0
}

/// The heartbeat on for at most `ms`, and off again when this is dropped, however the test returns (an early `?` or a failed
/// check must not leave every hart taking an interrupt a millisecond for the tests after it).
struct PauseWatch;

/// Whether a [`PauseWatch`] is alive (the kernel's heartbeat is one switch, not a count: an inner watch must not end an outer one).
static WATCH_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

impl PauseWatch {
    fn start(ms: u64) -> Option<PauseWatch> {
        let w = host_pause_watch(ms).then_some(PauseWatch);
        if w.is_some() {
            WATCH_ON.store(true, Ordering::SeqCst);
        }
        w
    }
}

impl Drop for PauseWatch {
    fn drop(&mut self) {
        WATCH_ON.store(false, Ordering::SeqCst);
        let _ = host_pause_watch(0);
    }
}

fn nr_debug_stat() -> usize {
    rt::abi::syscall::DEBUG_STAT
}

/// `##PAUSE <test> hart <h> late <us> us`: a window of `test` that a pause of the host covered (the harness keeps the line in the log).
fn pause_line(test: &str, hart: u64, late_us: u64) {
    println!("##PAUSE {test} hart {hart} late {late_us} us");
}

extern "C" fn rt_probe(_: usize) -> ! {
    sys::thread_set_sched(0, RT_PROBE_PRIO.load(Ordering::SeqCst) as u8, 0);
    let gate = sys::event_create().a1 as u64;
    let (mut worst, mut second) = (0u64, 0u64);
    let (mut kept, mut in_a_row, mut retaken, mut worst_pause) = (0, 0, 0u64, 0u64);
    let name = "a_real_time_thread_stays_responsive_while_hogs_run_and_a_normal_one_does_not";
    // (T284: a wait that a pause of the host covered, a timer interrupt of any hart more than the bound late inside it, is taken
    // again, at most three in a row: the third fails the test, since a host that does not run the hart is a failure too)
    while kept < 30 {
        let start = sys::clock_ns();
        let target = sys::clock_get() + 3; // milliseconds: the wait's own unit
        sys::object_wait(gate, 1 << 24, target);
        // (the same clock before and after, in nanoseconds: the time counter has another origin, the firmware's start)
        let late = sys::clock_ns().saturating_sub(target * 1_000_000);
        let late = late / 1000; // microseconds
        let bound = if RT_PROBE_PRIO.load(Ordering::SeqCst) == 40 {
            RT_BOUND_US
        } else {
            RT_CONTROL_BOUND_US
        };
        if let Some((h, us)) = host_pause_since(start, bound) {
            pause_line(name, h, us);
            in_a_row += 1;
            if worst_pause & ((1 << 48) - 1) < us {
                worst_pause = (h << 48) | us;
            }
            if in_a_row == 3 {
                RT_GAVE_UP.store((h << 48) | us, Ordering::SeqCst);
                break;
            }
            retaken += 1;
            continue;
        }
        in_a_row = 0;
        kept += 1;
        if late > worst {
            second = worst;
            worst = late;
        } else {
            second = second.max(late);
        }
    }
    RT_RETAKEN.store(retaken, Ordering::SeqCst);
    RT_WORST_PAUSE.store(worst_pause, Ordering::SeqCst);
    RT_SECOND_LATE_US.store(second, Ordering::SeqCst);
    RT_WORST_LATE_US.store(worst, Ordering::SeqCst);
    sys::handle_close(gate);
    sys::thread_exit(0)
}

/// What the probe's re-sampling did, for a failure message: "" when no window was taken again.
fn rt_pauses_said() -> alloc::string::String {
    let p = RT_WORST_PAUSE.load(Ordering::SeqCst);
    if p == 0 {
        return alloc::string::String::new();
    }
    alloc::format!(
        "; {} windows taken again for a host pause, the worst {} us on hart {}",
        RT_RETAKEN.load(Ordering::SeqCst),
        p & ((1 << 48) - 1),
        p >> 48
    )
}

/// The probe's worst and second-worst lateness (microseconds) at `prio` with a hog on every hart.
fn worst_lateness_us(prio: u32) -> Result<(u64, u64), Detail> {
    let harts = job_use(root_job()?, 7)?.0;
    RT_STOP.store(0, Ordering::SeqCst);
    RT_PROBE_PRIO.store(prio, Ordering::SeqCst);
    RT_WORST_LATE_US.store(u64::MAX, Ordering::SeqCst);
    RT_GAVE_UP.store(0, Ordering::SeqCst);
    // (T284: every hart keeps a timer deadline near while the probe measures, so a pause of the host makes one late wherever it lands;
    // asked before any hog exists, so a refusal leaves nothing running)
    let watch = PauseWatch::start(20_000);
    ensure!(
        watch.is_some(),
        "debug_stat 218 (the heartbeat) was refused"
    );
    let mut hogs = Vec::new();
    for _ in 0..harts {
        hogs.push(new_thread(rt_hog, 0)?);
    }
    nap_ticks(5); // the hogs are running, one per hart
    let probe = new_thread(rt_probe, 0)?;
    let w = sys::object_wait(probe, sig::TERMINATED, sys::clock_get() + 20_000);
    drop(watch);
    RT_STOP.store(1, Ordering::SeqCst);
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    sys::handle_close(probe);
    ensure_eq!(w.status, 0, "the probe finished");
    let gave_up = RT_GAVE_UP.load(Ordering::SeqCst);
    ensure!(
        gave_up == 0,
        "host pause: three windows in a row were covered by a pause (the last: a timer interrupt {} us late on hart {}), so the probe could not measure the scheduler",
        gave_up & ((1 << 48) - 1),
        gave_up >> 48
    );
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
        second <= RT_BOUND_US && rt <= 30_000,
        "a real-time thread woke {rt} us late (second worst {second} us) with a hog on every hart (bound 4 ms, worst 30 ms){}",
        rt_pauses_said()
    );
    // the negative control: at the hogs' priority the wake waits for a slice to end
    let (normal, _) = worst_lateness_us(10)?;
    ensure!(
        normal > RT_CONTROL_BOUND_US,
        "an equal-priority probe was only {normal} us late: the hogs are not loading the harts{}",
        rt_pauses_said()
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
    // (the worst of (billed, what host pauses explain) over the readings: the job's real-time time in the current 100 ms window,
    // less what the harts were billed while the host did not run them)
    let mut worst_rt = 0u64;
    let mut worst_paused = 0u64;
    let mut trace: Vec<(u64, u64, u64)> = Vec::new(); // (ms into the 100 ms window, RT ms billed so far, ms of host pause in it)
    let mut naps_done = 0;
    let until = sys::clock_get() + 20_000;
    let watch = PauseWatch::start(20_000); // (T284: every hart keeps a deadline near, so a pause of the host is seen on each)
    while naps_done < 30 && sys::clock_get() < until {
        nap_ticks(10); // the hogs hold every hart: this sleep needs a hart to wake on
        let used = job_use(root, 9)?.0;
        // T284: the scheduler bills a thread by the clock, so a hart the host did not run for 20 ms bills its hog 20 ms it never
        // ran (the host-stall injector shows it: 254 ms billed in one window with one stall of 20 to 30 ms on all four harts). What
        // the pause detector saw in this budget window (since its start on the same clock) is allowed for, hart by hart; a
        // scheduler that overruns the budget by itself still fails.
        let now = sys::clock_ns();
        let paused_ns = host_paused_us_since(now - now % 100_000_000) * 1000;
        if used.saturating_sub(paused_ns) > worst_rt.saturating_sub(worst_paused) {
            (worst_rt, worst_paused) = (used, paused_ns);
        }
        trace.push((
            sys::clock_get() % 100,
            used / 1_000_000,
            paused_ns / 1_000_000,
        ));
        naps_done += 1;
    }
    drop(watch);
    RT_STOP.store(1, Ordering::SeqCst);
    ok(sys::thread_set_sched(0, 10, 0), "main back to normal")?;
    for h in hogs {
        sys::object_wait(h, sig::TERMINATED, sys::clock_get() + 20_000);
        sys::handle_close(h);
    }
    ensure_eq!(naps_done, 30, "thirty naps finished while the hogs ran");
    // (without the budget the job's real-time time in a window would be about `harts` windows long; allow a tick or two of overshoot)
    ensure!(
        worst_rt.saturating_sub(worst_paused) <= 100_000_000 + harts * 20_000_000,
        "the job was billed {worst_rt} ns of real-time time in one 100 ms window, {worst_paused} ns of it while the host did not run the harts (budget 100 ms, {harts} harts); (ms into window, RT ms, host pause ms): {trace:?}"
    );
    Ok(())
}

static CPU_SPIN_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static CPU_SLEEP_NS: AtomicU64 = AtomicU64::new(u64::MAX);
/// The spinner's own figure when its window opened, and the clock's length of the window (see `cpu_spinner`).
static CPU_SPIN_START_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static CPU_SPIN_WALL_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Spins for 100 ms of the clock and reports the CPU time its own thread was charged (handle 0: the caller): at the window's opening
/// and at its close, and the window's length by the same clock, read outside both figures (so the figures' difference is charged
/// within it).
extern "C" fn cpu_spinner(_: usize) -> ! {
    let t0 = sys::clock_ns();
    let c0 = sys::thread_cpu_ns(0).a1 as u64;
    while sys::clock_ns() < t0 + 100_000_000 {
        core::hint::spin_loop();
    }
    let c1 = sys::thread_cpu_ns(0).a1 as u64;
    let t1 = sys::clock_ns();
    CPU_SPIN_START_NS.store(c0, Ordering::SeqCst);
    CPU_SPIN_WALL_NS.store(t1 - t0, Ordering::SeqCst);
    CPU_SPIN_NS.store(c1, Ordering::SeqCst);
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
    CPU_SPIN_START_NS.store(u64::MAX, Ordering::SeqCst);
    CPU_SPIN_WALL_NS.store(u64::MAX, Ordering::SeqCst);
    let me_p = ok(sys::process_self(), "process_self")?.a1 as u64;
    let before = ok(sys::process_cpu_ns(me_p), "process cpu")?.a1 as u64;
    // One after the other: the spinner's 100 ms are of the clock, so at one hart anything else that runs in them (the sleeper's
    // start-up and its end, a few milliseconds under emulation: T250) is time the spinner is not charged. The suite's thread waits.
    let spin = new_thread(cpu_spinner, 0)?;
    ensure_eq!(
        sys::object_wait(spin, sig::TERMINATED, sys::clock_get() + 20_000).status,
        0,
        "the spinner ended"
    );
    let sleep = new_thread(cpu_sleeper, 0)?;
    ensure_eq!(
        sys::object_wait(sleep, sig::TERMINATED, sys::clock_get() + 20_000).status,
        0,
        "the sleeper ended"
    );
    let spun = CPU_SPIN_NS.load(Ordering::SeqCst);
    let slept = CPU_SLEEP_NS.load(Ordering::SeqCst);
    let (c0, wall) = (
        CPU_SPIN_START_NS.load(Ordering::SeqCst),
        CPU_SPIN_WALL_NS.load(Ordering::SeqCst),
    );
    ensure!(
        c0 <= spun && (100_000_000..u64::MAX).contains(&wall),
        "the spinner's figures: {c0} ns at the start, {spun} ns at the end, a window of {wall} ns"
    );
    // What was charged inside the window, against the window by the same clock (the scheduler charges by the clock `clock_get` reads).
    // The thread's whole figure also holds its start-up (entry, stack faults, the first run of its code under emulation, a host that
    // stalls the hart: milliseconds on a CI runner, T250), which is outside the window, so it is not compared with 100 ms. Not more
    // than the window: one thread is designated on one hart at a time and charged from its designation, which is made under the lock
    // of the clock reading it is charged from (AC1; a pull that designated with an older reading billed a stretch on both harts, which
    // `kcore/tests/charge_migrate_concurrency.rs` pins). Not less than 95 percent of it (the spec's 2 percent needs a quieter machine
    // than a shared CI runner: interrupts and the suite's other harts' work may take this hart).
    let win = spun - c0;
    ensure!(
        win <= wall && win >= wall / 100 * 95,
        "a thread that spun for {wall} ns of the clock was charged {win} ns in that time ({c0} ns before it, {spun} ns in all)"
    );
    // (the thread's start-up, its event and its two calls cost a few milliseconds on a loaded CI machine under emulation, 5.5 to 7.9 ms seen (T250): the point
    // is that 50 ms of sleep were not charged, which 25 ms still tells)
    ensure!(
        slept < 25_000_000,
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
        st(sys::job_info(root, 14)),
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
        st(sys::job_info(root, 14)),
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
    // (T265: the credit to the ancestors is checked on a parent job of this test's own, not on the root: the root also counts processes
    // of earlier tests that are still being torn down, and one whose teardown finished during this test made the root's count one lower
    // than it was at the start, "got 5, want 6", once under a planted pause)
    let parent = job(root, u64::MAX, u64::MAX, u64::MAX)?;
    let (base, _) = job_use(parent, 0)?;
    let j = job(parent, 4, u64::MAX, u64::MAX)?;
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
        job_use(parent, 0)?.0,
        base,
        "the parent job's count is back where it was"
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
    sys::handle_close(parent);
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
    let base = next_fixed(0x10_0000)?;
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

/// T249: the page index of a VMO is charged to its job as kernel bytes, one 528-byte leaf per 64 pages touched, and given back with the VMO. A job
/// too small for a leaf has the write refused (NO_MEMORY) with its usage unchanged.
fn the_page_index_is_charged_to_the_job() -> Outcome {
    const LEAF: u64 = 528;
    let root = root_job()?;
    let j = job(root, 4, 4, 64)?;
    let (before, _) = job_use(j, 8)?;
    let v = ok(sys::vmo_create_in(64 * 40, j), "a VMO of 40 leaves")?.a1 as u64;
    // (the VMO's own object body and handle are kernel bytes too: the leaves are what moves from here)
    let (base, _) = job_use(j, 8)?;
    for k in 0..8u64 {
        ok(
            sys::vmo_write(v, k * 64 * 4096, b"x".as_ptr(), 1),
            "touch one page of a new leaf",
        )?;
    }
    // (another page of a leaf that exists costs nothing more)
    ok(sys::vmo_write(v, 4096, b"y".as_ptr(), 1), "same leaf")?;
    ensure_eq!(job_use(j, 8)?.0, base + 8 * LEAF, "eight leaves charged");
    // half the VMO gone: its leaves with it
    ok(
        sys::vmo_decommit(v, 0, 4 * 64 * 4096),
        "decommit four leaves",
    )?;
    ensure_eq!(job_use(j, 8)?.0, base + 4 * LEAF, "four leaves credited");
    sys::handle_close(v);
    job_use_settles(j, 8, before, 500)?;
    // a job whose one page allows 512 kernel bytes cannot hold a leaf
    let tiny = job(root, 1, 1, 1)?;
    let t = ok(sys::vmo_create_in(64, tiny), "a VMO in a tiny job")?.a1 as u64;
    let tb = job_use(tiny, 8)?.0;
    let r = sys::vmo_write(t, 0, b"x".as_ptr(), 1);
    ensure_eq!(st(r), Some(S::NoMemory), "no room for the leaf");
    ensure_eq!(job_use(tiny, 8)?.0, tb, "nothing was charged");
    ensure_eq!(job_use(tiny, 2)?.0, 0, "no page either");
    for h in [t, tiny, j, root] {
        sys::handle_close(h);
    }
    Ok(())
}

/// MM2, IOMMU12 (IOMMU-SPEC 4.7): DMA VMOs (`vmo_create_dma_in`, not promised contiguous and with no per-object limit) are made and
/// closed many times, at sizes above the old call's 64 pages, several live at once up to the system's DMA backstop (`debug_stat` 124:
/// then NO_MEMORY), and odd sizes. Each is zeroed; every page comes back (what was taken can be taken again, as one object, after the
/// last close), and the kernel's final audit says nothing leaked.
fn dma_runs_are_allocated_and_freed_repeatedly() -> Outcome {
    let res = ok(sys::resource_root(), "resource_root")?.a1 as u64;
    let r = (|| {
        let total = sys::debug_stat(124);
        ensure!(total >= 256, "a DMA backstop of {total} pages");
        for round in 0..40u64 {
            let pages = 65 + (round % 4) * 61; // 65, 126, 187, 248
            let v = ok(
                sys::vmo_create_dma_in(res, pages, 0),
                "a DMA VMO above 64 pages",
            )?
            .a1 as u64;
            let mut w = [0xffu8; 8];
            let rd = sys::vmo_read(v, (pages - 1) * 4096 + 4088, w.as_mut_ptr(), 8);
            sys::handle_close(v);
            ensure_eq!(st(rd), Some(S::Ok), "read its last word, round {round}");
            ensure_eq!(w, [0u8; 8], "zeroed, round {round}");
        }
        // what is left of the backstop (other programs may hold some) taken at once, in objects of 100 pages and then single pages,
        // until it is NO_MEMORY
        let mut live = alloc::vec::Vec::new();
        let mut got = 0u64;
        let r = (|| {
            for n in [100u64, 1] {
                loop {
                    let v = sys::vmo_create_dma_in(res, n, 0);
                    match st(v) {
                        Some(S::Ok) => {
                            live.push(v.a1 as u64);
                            got += n;
                        }
                        Some(S::NoMemory) => break,
                        other => ensure!(false, "a DMA VMO of {n} pages: {other:?}"),
                    }
                    ensure!(got <= total, "{got} pages past the backstop of {total}");
                }
            }
            ensure!(got >= 128, "only {got} of {total} pages were free to take");
            Ok(())
        })();
        for v in live {
            sys::handle_close(v);
        }
        r?;
        // an odd length, repeatedly; and the backstop is whole again
        for _ in 0..40 {
            let v = ok(sys::vmo_create_dma_in(res, 37, 0), "a 37-page DMA VMO")?.a1 as u64;
            sys::handle_close(v);
        }
        let t0 = sys::clock_get();
        loop {
            let v = sys::vmo_create_dma_in(res, got, 0);
            if st(v) == Some(S::Ok) {
                sys::handle_close(v.a1 as u64);
                break;
            }
            // (a large VMO's frames go back in batches: KT9)
            ensure!(
                sys::clock_get() < t0 + 2000,
                "the {got} pages never came back: {:?}",
                st(v)
            );
            sys::thread_yield();
        }
        Ok(())
    })();
    sys::handle_close(res);
    r
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
    // The child is stopped for the measured window (T238): it may still be starting up, and every page it touches is charged to the job, so a page
    // it touched between the read of the use below and the thread's creation was counted against the thread's room (seen at two harts: 68 pages
    // after the refusal, 69 by the second call, NO_MEMORY at a limit of 72). A stopped thread parks at its next return to user mode and touches
    // nothing more; one already in a fault finishes it first, which the reads below wait out. A thread made in a stopped process is charged when
    // it is made and runs no user code (SYSCALLS.md, `process_suspend`), which is all this test needs of it.
    ensure_eq!(st(sys::process_suspend(p)), Some(S::Ok), "stop the child");
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
    // (nothing is charged: with the caller's stack already mapped the only charge is the thread's, all or nothing, and the stopped child
    // charges nothing more: T238)
    let after = job_use(j, 2)?.0;
    ensure_eq!(after, used, "a refused thread charges nothing");
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
    let k = sys::process_kill(p, 3);
    if k.status != 0 {
        // (T270: BAD_STATE once, before the child was stopped here: say whether it had ended by itself and how)
        let info = sys::process_info(p);
        let ti = sys::thread_info(t);
        let mut d = Detail::new();
        let _ = core::fmt::Write::write_fmt(
            &mut d,
            format_args!(
                "kill: status {}, want 0; process_info status {} a1 (exit code) {} a2 {:#x}; process signals {:#x}; thread signals {:#x}, \
                 thread_info status {} a1 {:#x} a2 {:#x}; pages {} threads {}",
                k.status,
                info.status,
                info.a1 as i64,
                info.a2,
                sys::object_signals(p).a1,
                sys::object_signals(t).a1,
                ti.status,
                ti.a1,
                ti.a2,
                job_use(j, 2)?.0,
                job_use(j, 1)?.0
            ),
        );
        return Err(d);
    }
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
// The reported cost is the median of PS_REPS releases of a fifth of the round trips each (T232): one release swung by 30 times with
// whether the two threads of a pair happened to share a hart (T231), so a single figure compared with a baseline was noise.

const PS_MAX: usize = 32;
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

/// How many times each `pair_scaling` measurement is released; the median of them is reported.
const PS_REPS: u32 = 5;

/// Run `pairs` pairs of the given kind for `rounds` round trips in all, as [`PS_REPS`] releases of a fifth each, and report and return the
/// median cost in ns per round trip over all pairs (`ps:` gives the fastest and slowest release beside it, so a swing shows).
fn bench_pairs(kind: &str, pairs: usize, rounds: u32) -> Result<u64, Detail> {
    let each = (rounds / PS_REPS).max(1);
    let mut cycles_of = Vec::new();
    let mut stats = [0u64; 4];
    for _ in 0..PS_REPS {
        let (c, d) = pairs_once(kind, pairs, each)?;
        cycles_of.push(c);
        for (s, x) in stats.iter_mut().zip(d) {
            *s += x;
        }
    }
    cycles_of.sort_unstable();
    let median = cycles_of[cycles_of.len() / 2];
    let name = alloc::format!("pair_scaling_{kind}_n{pairs}");
    let trips = u64::from(each) * pairs as u64;
    let ns = bench_line(&name, median, trips);
    let per = |c: u64| c * 1_000_000_000 / sys::debug_stat(105).max(1) / trips.max(1);
    println!(
        "ps: {name} median of {PS_REPS} releases of {each} round trips: {ns} ns (fastest {} ns, slowest {} ns)",
        per(cycles_of[0]),
        per(cycles_of[cycles_of.len() - 1])
    );
    // (per thousand round trips, so that a rate below one is not rounded away; over every release)
    let all = trips * u64::from(PS_REPS);
    for (i, what) in ["ipis", "suspends", "wfis", "idle_ns"].iter().enumerate() {
        println!(
            "##BENCH ps_{kind}_n{pairs}_{what} {} per_1000_rt",
            stats[i] * 1000 / all.max(1)
        );
    }
    Ok(ns)
}

/// One release of `pairs` pairs doing `rounds` round trips each: the cycles from the release to the last exit, and what the harts did
/// meanwhile (IPIs received, retentive suspends, `wfi` idles, idle time: `debug_stat` 209 to 212, T213 M2).
fn pairs_once(kind: &str, pairs: usize, rounds: u32) -> Result<(u64, [u64; 4]), Detail> {
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
                  // (T213 M2: what the harts did while the pairs ran: IPIs received, retentive suspends, `wfi` idles, idle time)
    let stat0 = [209, 210, 211, 212].map(sys::debug_stat);
    let t0 = cycles();
    PS_GO.store(1, Ordering::SeqCst);
    sys::futex_wake(PS_GO.as_ptr(), u32::MAX);
    for t in &threads {
        let w = sys::object_wait(*t, sig::TERMINATED, sys::clock_get() + 120_000);
        ensure_eq!(w.status, 0, "a pair thread ended");
    }
    let c = cycles() - t0;
    let stat1 = [209, 210, 211, 212].map(sys::debug_stat);
    let d: [u64; 4] = core::array::from_fn(|i| stat1[i].saturating_sub(stat0[i]));
    for t in threads {
        sys::handle_close(t);
    }
    for h in closing {
        sys::handle_close(h); // the channel ends make the servers exit; the server handles are closed with them
    }
    Ok((c, d))
}

fn bench_pair_scaling(k: u32) -> Result<u64, Detail> {
    let mut last = 0;
    // (SC2: the full run also measures one pair per hart at 8, 16 and 32 harts, where the machine has them; a boot with fewer harts measures 1, 2 and 4 as before)
    let harts = job_use(root_job()?, 7)?.0 as usize;
    for pairs in [1, 2, 4, 8, 16, 32] {
        if pairs > 4 && (k < 10 || pairs > harts) {
            continue;
        }
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
    let base = next_fixed(0x100_0000)?;
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
        (
            "a_two_gib_vmo_committed_whole_stays_within_the_latency_bound",
            a_two_gib_vmo_committed_whole_stays_within_the_latency_bound,
        ),
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
            "no_dma_device_outside_the_iommu",
            no_dma_device_outside_the_iommu,
        ),
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
        ("blk_driver_killed_mid_write", blk_driver_killed_mid_write),
        (
            "blk_driver_cannot_reach_outside",
            blk_driver_cannot_reach_outside,
        ),
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
            "pci_devices_are_found_and_claimed",
            pci_devices_are_found_and_claimed,
        ),
        (
            "a_child_has_no_device_authority",
            a_child_has_no_device_authority,
        ),
        (
            "interrupt_object_fires_and_masks",
            interrupt_object_fires_and_masks,
        ),
        (
            "device_interrupts_arrive_where_asked",
            device_interrupts_arrive_where_asked,
        ),
        (
            "interrupt_identities_above_63_work",
            interrupt_identities_above_63_work,
        ),
        (
            "interrupt_moves_do_not_lose_or_leak",
            interrupt_moves_do_not_lose_or_leak,
        ),
        (
            "iommu_is_on_and_denies_by_default",
            iommu_is_on_and_denies_by_default,
        ),
        (
            "iommu_denies_messages_by_default",
            iommu_denies_messages_by_default,
        ),
        (
            "iodomain_maps_exactly_what_was_granted",
            iodomain_maps_exactly_what_was_granted,
        ),
        (
            "iodomain_unmap_stops_the_device",
            iodomain_unmap_stops_the_device,
        ),
        (
            "iodomain_faults_reach_the_supervisor",
            iodomain_faults_reach_the_supervisor,
        ),
        (
            "iodomain_outlives_its_makers",
            iodomain_outlives_its_makers,
        ),
        (
            "iodomain_calls_check_handles_rights_and_arguments",
            iodomain_calls_check_handles_rights_and_arguments,
        ),
        (
            "msi_granted_vector_is_delivered",
            msi_granted_vector_is_delivered,
        ),
        ("msi_ungranted_vector_faults", msi_ungranted_vector_faults),
        ("msi_forgery_faults", msi_forgery_faults),
        ("msi_revoke_stops_the_vector", msi_revoke_stops_the_vector),
        ("msi_destroy_revokes_every_vector", msi_destroy_revokes_every_vector),
        (
            "pci_virtio_blk_reads_through_the_iommu",
            pci_virtio_blk_reads_through_the_iommu,
        ),
        (
            "pci_virtio_outside_its_mapping_faults",
            pci_virtio_outside_its_mapping_faults,
        ),
        (
            "vector_calls_check_handles_rights_and_arguments",
            vector_calls_check_handles_rights_and_arguments,
        ),
        (
            "pci_virtio_blk_interrupts_through_msix",
            pci_virtio_blk_interrupts_through_msix,
        ),
        ("pci_two_devices_do_not_share", pci_two_devices_do_not_share),
        (
            "pci_driver_cannot_forge_through_devmgr",
            pci_driver_cannot_forge_through_devmgr,
        ),
        // (IOMMU12: the hostile-driver matrix after every test that needs a device id never given a domain: a destroyed domain leaves its
        // device quarantined, unreported (A14), so `iommu_is_on_and_denies_by_default` and its neighbours must come first)
        ("hostile_dma_at_a_kernel_address", hostile_dma_at_a_kernel_address),
        ("hostile_dma_at_another_process_frame", hostile_dma_at_another_process_frame),
        ("hostile_dma_at_another_device_buffer", hostile_dma_at_another_device_buffer),
        ("hostile_dma_at_another_device_bar", hostile_dma_at_another_device_bar),
        ("hostile_dma_at_an_interrupt_file", hostile_dma_at_an_interrupt_file),
        ("hostile_dma_one_page_past_its_buffer", hostile_dma_one_page_past_its_buffer),
        ("hostile_dma_through_a_read_only_mapping", hostile_dma_through_a_read_only_mapping),
        ("hostile_dma_after_its_own_unmap", hostile_dma_after_its_own_unmap),
        ("hostile_dma_after_the_domain_is_destroyed", hostile_dma_after_the_domain_is_destroyed),
        ("hostile_dma_after_its_supervisor_died", hostile_dma_after_its_supervisor_died),
        ("hostile_msi_at_another_device_vector", hostile_msi_at_another_device_vector),
        ("hostile_msi_at_the_inter_hart_identity", hostile_msi_at_the_inter_hart_identity),
        ("hostile_msi_at_the_iommu_identity", hostile_msi_at_the_iommu_identity),
        ("hostile_msi_on_an_ungranted_vector", hostile_msi_on_an_ungranted_vector),
        ("hostile_msi_with_a_wrong_data_word", hostile_msi_with_a_wrong_data_word),
        ("hostile_msi_after_the_vector_is_revoked", hostile_msi_after_the_vector_is_revoked),
        ("hostile_cannot_map_without_the_dma_right", hostile_cannot_map_without_the_dma_right),
        ("hostile_cannot_map_writable_without_write", hostile_cannot_map_writable_without_write),
        ("hostile_cannot_destroy_its_domain", hostile_cannot_destroy_its_domain),
        ("hostile_cannot_grant_itself_a_vector", hostile_cannot_grant_itself_a_vector),
        ("hostile_cannot_read_or_wait_for_its_faults", hostile_cannot_read_or_wait_for_its_faults),
        ("hostile_learns_no_physical_address", hostile_learns_no_physical_address),
        ("hostile_maps_until_its_job_refuses", hostile_maps_until_its_job_refuses),
        ("hostile_child_job_is_held_to_the_parents_limits", hostile_child_job_is_held_to_the_parents_limits),
        ("every_driver_job_has_finite_io_limits", every_driver_job_has_finite_io_limits),
        ("dma_survives_a_dead_supervisor", dma_survives_a_dead_supervisor),
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
            "runtime_heap_stops_at_its_cap_after_giving_memory_back",
            runtime_heap_stops_at_its_cap_after_giving_memory_back,
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
            "the_heap_gives_memory_back_when_it_is_freed",
            the_heap_gives_memory_back_when_it_is_freed,
        ),
        (
            "a_close_that_frees_a_chain_takes_interrupts_between_its_slices",
            a_close_that_frees_a_chain_takes_interrupts_between_its_slices,
        ),
        (
            "a_decommit_under_way_refuses_the_calls_that_would_race_it",
            a_decommit_under_way_refuses_the_calls_that_would_race_it,
        ),
        (
            "a_decommit_the_job_cannot_pay_the_holes_for_is_no_memory",
            a_decommit_the_job_cannot_pay_the_holes_for_is_no_memory,
        ),
        (
            "a_two_gib_sparse_vmo_is_mapped_touched_and_given_back",
            a_two_gib_sparse_vmo_is_mapped_touched_and_given_back,
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
        ("the_page_index_is_charged_to_the_job", the_page_index_is_charged_to_the_job),
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
            "a_kill_storm_on_a_channel_loses_no_message",
            stream_proto::a_kill_storm_on_a_channel_loses_no_message,
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
            "rng_seeds_through_the_iommu",
            net_tests::rng_seeds_through_the_iommu,
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
            "net_device_cannot_write_the_transmit_pool",
            net_tests::net_device_cannot_write_the_transmit_pool,
        ),
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
    // and each test's IOMMU faults (`debug_stat` 150) as an `iofaults:` line when there were any: the script allows them only for the
    // tests that make them on purpose, and checks the boot's total against the sum (IOMMU10: no unexpected fault over a boot)
    let timed = |name: &str, test: fn() -> Outcome| -> bool {
        let t0 = sys::clock_get();
        let f0 = sys::debug_stat(150);
        let r = test();
        let ticks = sys::clock_get() - t0;
        let faults = sys::debug_stat(150) - f0;
        let ok = report(name, r);
        println!("tt: {name} {ticks}");
        if faults != 0 {
            println!("iofaults: {name} {faults}");
        }
        ok
    };
    // Behind the lossy relay (`net=lossy`, S9-SPEC Q30) only the tests that move bulk TCP data in a way slirp copes with run: the others assume a clean link
    let lossy = sys::debug_stat(9) != 0;
    let suite_faults = sys::debug_stat(150);
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
        // the kernel-only boot (`shard=k`, KT1): the Kernel tests, which need no network (`init/ktests.txt`)
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
    println!("iofaults: suite {}", sys::debug_stat(150) - suite_faults);
    println!(
        "init: {} of {} user tests failed",
        failed,
        tests.len() + LUA_UPSTREAM.len()
    );
    failed
}
