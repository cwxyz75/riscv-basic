//! AB9: the conformance program of the system library. It is built against `libsys` and `abi` only (no `rt`: its own `_start` and panic handler, about
//! a hundred lines), so it checks the contract as a stranger to this repository would meet it: find the library from the base the kernel passes in `a2`,
//! call its `sys_` functions, and see the statuses `SYSCALLS.md` documents. The boot-time suite starts it and reports its verdict (the exit code is the
//! number of checks that failed; each failure is one message on the bootstrap channel).
//!
//! What it checks: every call of the table has its function; each function (but the two that end the caller) answers a call made with nothing valid
//! with a status the ABI defines and not `BAD_SYSCALL`; a number nothing defines is `BAD_SYSCALL`; `sys_abi_version` agrees with the data page; the
//! clock does not go back; a VMO, a channel and an event do what the reference says, with the errors it lists for a bad handle, a closed peer and a short buffer.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};

use abi::status::Status;
use libsys::Image;

global_asm!(
    r#"
    .section .text._start
    .globl _start
_start:
    call conformance_start
1:
    j 1b
"#
);

/// A word of writable data: the program's linker script starts the read-write segment on a page of its own only when there is something in it (a segment
/// that shared a page with the read-only one would overlap it, and the loader refuses that).
#[used]
static mut WRITABLE: [usize; 2] = [1, 2];

struct Lib<'a> {
    img: Image<'a>,
    base: usize,
}

#[derive(Clone, Copy)]
struct Ret {
    status: i32,
    a1: usize,
    a2: usize,
}

impl Lib<'_> {
    fn at(&self, symbol: &str) -> Option<usize> {
        self.img
            .lookup(symbol)
            .map(|s| self.base.wrapping_add(s.value as usize))
    }

    /// Call the function named `symbol` (the library has it: the table check above found every one).
    fn call(&self, symbol: &str, args: [usize; 6]) -> Ret {
        let f = self.at(symbol).unwrap_or(0);
        if f == 0 {
            return Ret {
                status: 1,
                a1: 0,
                a2: 0,
            };
        }
        self.jump(f, args, 0)
    }

    fn jump(&self, f: usize, args: [usize; 6], a6: usize) -> Ret {
        let (status, a1): (usize, usize);
        let a2: usize;
        // SAFETY: `f` is the address of a function in the library the kernel mapped (read-only, executable): it makes the call and returns, touching only
        // `a7` and the result registers (the ABI preserves the rest), which `clobber_abi` and the outputs declare.
        unsafe {
            asm!(
                "jalr {f}",
                f = in(reg) f,
                inlateout("a0") args[0] => status,
                inlateout("a1") args[1] => a1,
                inlateout("a2") args[2] => a2,
                in("a3") args[3],
                in("a4") args[4],
                in("a5") args[5],
                in("a6") a6,
                clobber_abi("C"),
                options(nostack),
            );
        }
        Ret {
            status: status as i32,
            a1,
            a2,
        }
    }

    /// The call for a number the library has no function for (`sys_raw`: the number in `a6`).
    fn raw(&self, number: usize) -> Ret {
        match self.at("sys_raw") {
            Some(f) => self.jump(f, [0; 6], number),
            None => Ret {
                status: 1,
                a1: 0,
                a2: 0,
            },
        }
    }
}

struct Report {
    bootstrap: usize,
    failed: i32,
    /// How many checks have run, and the number of the first that failed (the exit code carries both: failed | first << 8).
    seen: i32,
    first: i32,
}

impl Report {
    fn expect(&mut self, lib: &Lib, ok: bool, what: &str) {
        self.seen += 1;
        if !ok {
            if self.failed == 0 {
                self.first = self.seen;
            }
            self.failed += 1;
            lib.call(
                "sys_channel_write",
                [
                    self.bootstrap,
                    what.as_ptr() as usize,
                    what.len().min(200),
                    0,
                    0,
                    0,
                ],
            );
        }
    }
}

fn defined(status: i32) -> bool {
    Status::from_code(status).is_some() || status > 0
}

#[no_mangle]
extern "C" fn conformance_start(_arg: usize, bootstrap: usize, base: usize) -> ! {
    // SAFETY: the kernel mapped the library's first segment at `base` (read-only, at least a page), and nothing unmaps it.
    let head = unsafe { core::slice::from_raw_parts(base as *const u8, 4096) };
    let len = libsys::image::image_len(head).unwrap_or(0);
    // SAFETY: as above, for the segment's whole length.
    let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
    let Ok(img) = Image::new(bytes) else {
        exit_raw(100);
    };
    let lib = Lib { img, base };
    let mut r = Report {
        bootstrap,
        failed: 0,
        seen: 0,
        first: 0,
    };
    run(&lib, &mut r);
    let code = (r.failed | r.first << 8) as usize;
    lib.call("sys_process_exit", [code, 0, 0, 0, 0, 0]);
    exit_raw(102) // (process_exit does not return)
}

/// The library could not be read: the only way left to say so is to stop with a code (an illegal instruction: the process ends with its own).
fn exit_raw(_code: i32) -> ! {
    // SAFETY: `unimp` is an illegal instruction; the process ends.
    unsafe { asm!("unimp", options(noreturn)) }
}

fn run(lib: &Lib, r: &mut Report) {
    // every call of the table has its function, and the two that end the caller are named
    for &(name, _) in abi::syscall::TABLE {
        let has = lib
            .img
            .symbols()
            .any(|s| libsys::is_wrapper_of(s.name, name));
        r.expect(lib, has, name);
    }
    // a call made with nothing valid (all arguments zero) answers with a status the ABI defines
    for &(name, _) in abi::syscall::TABLE {
        if name == "PROCESS_EXIT" || name == "THREAD_EXIT" {
            continue;
        }
        let sym = symbol(name);
        let st = lib.call(sym.as_str(), [0; 6]).status;
        let ok = defined(st) && st != Status::BadSyscall.code();
        r.expect(lib, ok, name);
    }
    // numbers nothing defines
    r.expect(
        lib,
        lib.raw(999).status == Status::BadSyscall.code(),
        "number 999 is BAD_SYSCALL",
    );
    r.expect(
        lib,
        lib.raw(abi::syscall::number(5)).status == Status::BadSyscall.code(),
        "an unassigned number is BAD_SYSCALL",
    );
    // the version
    let v = lib.call("sys_abi_version", [0; 6]);
    r.expect(
        lib,
        v.status == 0 && v.a1 == libsys::page::ABI as usize,
        "sys_abi_version is the data page's",
    );
    // the clock does not go back
    let (t1, t2) = (
        lib.call("sys_clock_get", [0; 6]),
        lib.call("sys_clock_get", [0; 6]),
    );
    r.expect(
        lib,
        t1.status == 0 && t2.status == 0 && t2.a1 >= t1.a1,
        "the clock is monotonic",
    );
    memory(lib, r);
    channels(lib, r);
    events(lib, r);
}

/// `"sys_"` and the constant's name in lower case, in a buffer (no allocation here).
struct Sym {
    buf: [u8; 48],
    len: usize,
}

impl Sym {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

fn symbol(constant: &str) -> Sym {
    let mut s = Sym {
        buf: [0; 48],
        len: 0,
    };
    for b in b"sys_"
        .iter()
        .copied()
        .chain(constant.bytes().map(|c| c.to_ascii_lowercase()))
    {
        if s.len < 48 {
            s.buf[s.len] = b;
            s.len += 1;
        }
    }
    s
}

fn memory(lib: &Lib, r: &mut Report) {
    let c = lib.call("sys_vmo_create", [4096, 0, 0, 0, 0, 0]);
    r.expect(lib, c.status == 0, "vmo_create");
    let vmo = c.a1;
    let size = lib.call("sys_vmo_get_size", [vmo, 0, 0, 0, 0, 0]);
    r.expect(lib, size.status == 0 && size.a1 == 4096, "vmo_get_size");
    let src = *b"conformance";
    let w = lib.call(
        "sys_vmo_write",
        [vmo, 8, src.as_ptr() as usize, src.len(), 0, 0],
    );
    r.expect(lib, w.status == 0, "vmo_write");
    let mut back = [0u8; 11];
    let rd = lib.call(
        "sys_vmo_read",
        [vmo, 8, back.as_mut_ptr() as usize, back.len(), 0, 0],
    );
    r.expect(
        lib,
        rd.status == 0 && back == src,
        "vmo_read returns what was written",
    );
    let past = lib.call(
        "sys_vmo_read",
        [vmo, 4090, back.as_mut_ptr() as usize, 11, 0, 0],
    );
    r.expect(
        lib,
        past.status == Status::InvalidArgs.code(),
        "vmo_read past the end is INVALID_ARGS",
    );
    r.expect(
        lib,
        lib.call("sys_handle_close", [vmo, 0, 0, 0, 0, 0]).status == 0,
        "handle_close",
    );
    r.expect(
        lib,
        lib.call("sys_vmo_get_size", [vmo, 0, 0, 0, 0, 0]).status == Status::BadHandle.code(),
        "a closed handle is BAD_HANDLE",
    );
}

fn channels(lib: &Lib, r: &mut Report) {
    let c = lib.call("sys_channel_create", [0; 6]);
    r.expect(lib, c.status == 0, "channel_create");
    let (a, b) = (c.a1, c.a2);
    let msg = *b"ping";
    let w = lib.call(
        "sys_channel_write",
        [a, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
    );
    r.expect(lib, w.status == 0, "channel_write");
    let mut got = [0u8; 4];
    // a buffer that is too small leaves the message queued
    let short = lib.call(
        "sys_channel_read",
        [b, got.as_mut_ptr() as usize, 1, 0, 0, 0],
    );
    r.expect(
        lib,
        short.status == Status::BufferTooSmall.code(),
        "a short buffer is BUFFER_TOO_SMALL",
    );
    let rd = lib.call(
        "sys_channel_read",
        [b, got.as_mut_ptr() as usize, 4, 0, 0, 0],
    );
    r.expect(
        lib,
        rd.status == 0 && got == msg,
        "channel_read returns what was written",
    );
    let empty = lib.call(
        "sys_channel_read",
        [b, got.as_mut_ptr() as usize, 4, 0, 0, 0],
    );
    r.expect(
        lib,
        empty.status == Status::ShouldWait.code(),
        "an empty channel is SHOULD_WAIT",
    );
    lib.call("sys_handle_close", [a, 0, 0, 0, 0, 0]);
    let gone = lib.call(
        "sys_channel_read",
        [b, got.as_mut_ptr() as usize, 4, 0, 0, 0],
    );
    r.expect(
        lib,
        gone.status == Status::PeerClosed.code(),
        "a read from a channel whose peer closed is PEER_CLOSED",
    );
    let wg = lib.call(
        "sys_channel_write",
        [b, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
    );
    r.expect(
        lib,
        wg.status == Status::PeerClosed.code(),
        "a write to a channel whose peer closed is PEER_CLOSED",
    );
    lib.call("sys_handle_close", [b, 0, 0, 0, 0, 0]);
}

fn events(lib: &Lib, r: &mut Report) {
    let e = lib.call("sys_event_create", [0; 6]);
    r.expect(lib, e.status == 0, "event_create");
    let ev = e.a1;
    let set = lib.call("sys_event_signal", [ev, 0, 1 << 24, 0, 0, 0]);
    r.expect(lib, set.status == 0, "event_signal");
    let sig = lib.call("sys_object_signals", [ev, 0, 0, 0, 0, 0]);
    r.expect(
        lib,
        sig.status == 0 && sig.a1 & (1 << 24) != 0,
        "the signal that was set is seen",
    );
    let w = lib.call("sys_object_wait", [ev, 1 << 24, 0, 0, 0, 0]);
    r.expect(
        lib,
        w.status == 0,
        "object_wait on a signalled object returns",
    );
    let none = lib.call("sys_object_wait", [ev, 1 << 25, 0, 0, 0, 0]);
    r.expect(
        lib,
        none.status == Status::TimedOut.code(),
        "object_wait on a signal that is not set times out",
    );
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit_raw(101)
}
