//! The native runtime for user programs (OS-ARCHITECTURE.md, section 14; S3b): raw
//! syscalls, a `_start` that calls the program's `app_main`, a panic handler, and a
//! small formatted-output and test-reporting layer, all without an allocator.
//!
//! Only builds for the RISC-V user target (it contains `ecall`).
#![no_std]

use abi::status::Status;
use core::arch::{asm, global_asm};
use core::fmt::{self, Write};

pub use abi;

#[cfg(feature = "heap")]
pub mod fs;
#[cfg(feature = "heap")]
pub mod heap;
#[cfg(feature = "heap")]
pub mod io;
#[cfg(feature = "heap")]
pub mod launch;
#[cfg(feature = "heap")]
pub mod spawn;
#[cfg(feature = "heap")]
pub mod startup;

/// The result of a raw syscall: the status code and the two result registers.
#[derive(Debug, Clone, Copy)]
pub struct Ret {
    pub status: i32,
    pub a1: usize,
    pub a2: usize,
}

impl Ret {
    pub fn status(&self) -> Option<Status> {
        Status::from_code(self.status)
    }
}

/// A raw syscall (SYSCALLS.md, "ABI").
pub fn syscall(number: usize, args: [usize; 6]) -> Ret {
    let (status, a1, a2): (usize, usize, usize);
    // SAFETY: `ecall` traps to the kernel, which preserves every register except the
    // results in a0 to a2 (SYSCALLS.md). No memory is touched except through pointers the
    // caller passed, which the kernel validates.
    unsafe {
        asm!(
            "ecall",
            in("a7") number,
            inlateout("a0") args[0] => status,
            inlateout("a1") args[1] => a1,
            inlateout("a2") args[2] => a2,
            in("a3") args[3],
            in("a4") args[4],
            in("a5") args[5],
            options(nostack),
        );
    }
    Ret {
        status: status as i32,
        a1,
        a2,
    }
}

pub mod sys {
    use super::{syscall, Ret};
    use abi::syscall::*;

    pub fn debug_write(bytes: &[u8]) -> Ret {
        syscall(
            DEBUG_WRITE,
            [bytes.as_ptr() as usize, bytes.len(), 0, 0, 0, 0],
        )
    }
    pub fn exit(code: i32) -> ! {
        syscall(PROCESS_EXIT, [code as usize, 0, 0, 0, 0, 0]);
        loop {
            core::hint::spin_loop();
        }
    }
    pub fn thread_yield() -> Ret {
        syscall(THREAD_YIELD, [0; 6])
    }
    pub fn clock_get() -> u64 {
        syscall(CLOCK_GET, [0; 6]).a1 as u64
    }
    pub fn handle_close(h: u32) -> Ret {
        syscall(HANDLE_CLOSE, [h as usize, 0, 0, 0, 0, 0])
    }
    pub fn handle_duplicate(h: u32, rights: u32) -> Ret {
        syscall(HANDLE_DUPLICATE, [h as usize, rights as usize, 0, 0, 0, 0])
    }
    pub fn handle_replace(h: u32, rights: u32) -> Ret {
        syscall(HANDLE_REPLACE, [h as usize, rights as usize, 0, 0, 0, 0])
    }
    pub fn channel_create() -> Ret {
        syscall(CHANNEL_CREATE, [0; 6])
    }
    pub fn channel_write(
        ch: u32,
        bytes: *const u8,
        nbytes: usize,
        handles: *const u32,
        nhandles: usize,
    ) -> Ret {
        syscall(
            CHANNEL_WRITE,
            [
                ch as usize,
                bytes as usize,
                nbytes,
                handles as usize,
                nhandles,
                0,
            ],
        )
    }
    pub fn channel_read(
        ch: u32,
        bytes: *mut u8,
        max_bytes: usize,
        handles: *mut u32,
        max_handles: usize,
    ) -> Ret {
        syscall(
            CHANNEL_READ,
            [
                ch as usize,
                bytes as usize,
                max_bytes,
                handles as usize,
                max_handles,
                0,
            ],
        )
    }
    pub fn channel_read_txid(
        ch: u32,
        txid: u32,
        bytes: *mut u8,
        max_bytes: usize,
        handles: *mut u32,
        max_handles: usize,
    ) -> Ret {
        syscall(
            CHANNEL_READ_TXID,
            [
                ch as usize,
                txid as usize,
                bytes as usize,
                max_bytes,
                handles as usize,
                max_handles,
            ],
        )
    }
    pub fn event_create() -> Ret {
        syscall(EVENT_CREATE, [0; 6])
    }
    pub fn event_signal(ev: u32, clear: u32, set: u32) -> Ret {
        syscall(
            EVENT_SIGNAL,
            [ev as usize, clear as usize, set as usize, 0, 0, 0],
        )
    }
    pub fn object_signals(h: u32) -> Ret {
        syscall(OBJECT_SIGNALS, [h as usize, 0, 0, 0, 0, 0])
    }
    /// Wait until `signals(h) & mask != 0` or tick `deadline` (0: do not wait, u64::MAX: forever).
    pub fn object_wait(h: u32, mask: u32, deadline: u64) -> Ret {
        syscall(
            OBJECT_WAIT,
            [h as usize, mask as usize, deadline as usize, 0, 0, 0],
        )
    }
    pub fn object_info(h: u32) -> Ret {
        syscall(OBJECT_INFO, [h as usize, 0, 0, 0, 0, 0])
    }
    pub fn vmo_create(pages: u64) -> Ret {
        syscall(VMO_CREATE, [pages as usize, 0, 0, 0, 0, 0])
    }
    pub fn vmo_read(vmo: u32, offset: u64, buf: *mut u8, len: usize) -> Ret {
        syscall(
            VMO_READ,
            [vmo as usize, offset as usize, buf as usize, len, 0, 0],
        )
    }
    pub fn vmo_write(vmo: u32, offset: u64, buf: *const u8, len: usize) -> Ret {
        syscall(
            VMO_WRITE,
            [vmo as usize, offset as usize, buf as usize, len, 0, 0],
        )
    }
    pub fn vmo_get_size(vmo: u32) -> Ret {
        syscall(VMO_GET_SIZE, [vmo as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_create() -> Ret {
        syscall(PROCESS_CREATE, [0; 6])
    }
    pub fn process_self() -> Ret {
        syscall(PROCESS_SELF, [0; 6])
    }
    pub fn process_map(
        process: u32,
        vmo: u32,
        vaddr: u64,
        vmo_offset: u64,
        length: u64,
        flags: u64,
    ) -> Ret {
        syscall(
            PROCESS_MAP,
            [
                process as usize,
                vmo as usize,
                vaddr as usize,
                vmo_offset as usize,
                length as usize,
                flags as usize,
            ],
        )
    }
    pub fn process_unmap(process: u32, vaddr: u64) -> Ret {
        syscall(
            PROCESS_UNMAP,
            [process as usize, vaddr as usize, 0, 0, 0, 0],
        )
    }
    pub fn process_start(process: u32, entry: u64, sp: u64, bootstrap: u32, arg: u64) -> Ret {
        syscall(
            PROCESS_START,
            [
                process as usize,
                entry as usize,
                sp as usize,
                bootstrap as usize,
                arg as usize,
                0,
            ],
        )
    }
    pub fn socket_create() -> Ret {
        syscall(SOCKET_CREATE, [0; 6])
    }
    pub fn socket_write(socket: u32, bytes: *const u8, len: usize) -> Ret {
        syscall(
            SOCKET_WRITE,
            [socket as usize, bytes as usize, len, 0, 0, 0],
        )
    }
    pub fn socket_read(socket: u32, buf: *mut u8, max: usize) -> Ret {
        syscall(SOCKET_READ, [socket as usize, buf as usize, max, 0, 0, 0])
    }
    pub fn port_create() -> Ret {
        syscall(PORT_CREATE, [0; 6])
    }
    pub fn port_bind(port: u32, object: u32, mask: u32, key: u64) -> Ret {
        syscall(
            PORT_BIND,
            [
                port as usize,
                object as usize,
                mask as usize,
                key as usize,
                0,
                0,
            ],
        )
    }
    pub fn port_unbind(port: u32, key: u64) -> Ret {
        syscall(PORT_UNBIND, [port as usize, key as usize, 0, 0, 0, 0])
    }
    pub fn port_wait(port: u32, deadline: u64) -> Ret {
        syscall(PORT_WAIT, [port as usize, deadline as usize, 0, 0, 0, 0])
    }
    pub fn thread_create(process: u32, entry: u64, sp: u64, arg: u64) -> Ret {
        syscall(
            THREAD_CREATE,
            [
                process as usize,
                entry as usize,
                sp as usize,
                arg as usize,
                0,
                0,
            ],
        )
    }
    pub fn thread_exit(code: i32) -> ! {
        syscall(THREAD_EXIT, [code as usize, 0, 0, 0, 0, 0]);
        loop {
            core::hint::spin_loop();
        }
    }
    pub fn thread_info(thread: u32) -> Ret {
        syscall(THREAD_INFO, [thread as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_kill(process: u32, code: i32) -> Ret {
        syscall(PROCESS_KILL, [process as usize, code as usize, 0, 0, 0, 0])
    }
    pub fn process_info(process: u32) -> Ret {
        syscall(PROCESS_INFO, [process as usize, 0, 0, 0, 0, 0])
    }
    pub fn channel_call(ch: u32, args: &super::CallArgs) -> Ret {
        syscall(
            CHANNEL_CALL,
            [
                ch as usize,
                args as *const super::CallArgs as usize,
                0,
                0,
                0,
                0,
            ],
        )
    }
}

/// The handle value of the bootstrap channel every process starts with (SYSCALLS.md).
pub const BOOTSTRAP: u32 = 1;

/// The argument record of `channel_call` (SYSCALLS.md; 56 bytes).
#[repr(C)]
pub struct CallArgs {
    pub req_bytes: u64,
    pub req_handles: u64,
    pub req_nbytes: u32,
    pub req_nhandles: u32,
    pub rep_bytes: u64,
    pub rep_handles: u64,
    pub rep_max_bytes: u32,
    pub rep_max_handles: u32,
    pub deadline: u64,
}

/// A fixed-size text buffer: formatted output without an allocator.
pub struct Buf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Self {
        Buf {
            bytes: [0; N],
            len: 0,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len < N {
                self.bytes[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// Print formatted text to the serial console (one `debug_write` per call).
pub fn print(args: fmt::Arguments) {
    let mut buf = Buf::<512>::new();
    let _ = buf.write_fmt(args);
    sys::debug_write(buf.as_bytes());
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::print(format_args!("{}\n", format_args!($($arg)*)));
    }};
}

/// Why a test failed: a one-line message in a fixed buffer.
pub type Detail = Buf<200>;

/// Fail a test with a formatted message unless `cond` holds.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            let mut d = $crate::Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!($($arg)*));
            return Err(d);
        }
    };
}

/// Compare two values; fail with both in the message.
#[macro_export]
macro_rules! ensure_eq {
    ($got:expr, $want:expr, $($arg:tt)*) => {{
        let (g, w) = (&$got, &$want);
        if *g != *w {
            let mut d = $crate::Detail::new();
            let _ = core::fmt::Write::write_fmt(&mut d, format_args!("{}: got {:?}, want {:?}", format_args!($($arg)*), g, w));
            return Err(d);
        }
    }};
}

/// Report one test in the structured form the boot harness reads.
pub fn report(name: &str, result: Result<(), Detail>) -> bool {
    match result {
        Ok(()) => {
            println!("##TEST {} PASS", name);
            true
        }
        Err(detail) => {
            let text = core::str::from_utf8(detail.as_bytes()).unwrap_or("(not utf-8)");
            println!("##TEST {} FAIL {}", name, text);
            false
        }
    }
}

extern "Rust" {
    fn app_main() -> i32;
}

global_asm!(
    r#"
    .section .text._start
    .globl _start
_start:
    call rt_start
1:
    j 1b
"#
);

/// The first Rust code of every program: call the program's `app_main` and exit with
/// its result. (The kernel starts a process with `a0` = its start argument.)
#[no_mangle]
extern "C" fn rt_start(arg: usize) -> ! {
    START_ARG.store(arg, core::sync::atomic::Ordering::Relaxed);
    // SAFETY: `app_main` is defined by the program being linked, with this signature.
    let code = unsafe { app_main() };
    sys::exit(code)
}

static START_ARG: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The start argument the kernel passed in `a0`.
pub fn start_arg() -> usize {
    START_ARG.load(core::sync::atomic::Ordering::Relaxed)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("##PANIC user program: {}", info);
    sys::exit(101)
}
