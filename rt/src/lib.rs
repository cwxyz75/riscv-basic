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
pub mod blk;
#[cfg(feature = "heap")]
pub mod clock;
#[cfg(feature = "heap")]
pub mod fs;
#[cfg(feature = "heap")]
pub mod heap;
#[cfg(feature = "heap")]
pub mod io;
#[cfg(feature = "heap")]
pub mod launch;
#[cfg(feature = "heap")]
pub mod net;
pub mod random;
pub mod resolve;
#[cfg(feature = "heap")]
pub mod service;
#[cfg(feature = "heap")]
pub mod spawn;
#[cfg(feature = "heap")]
pub mod startup;
pub mod sync;

/// The argument record of `thread_spawn` (SYSCALLS.md): 64 bytes, little endian. Zero a field to take its default.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnArgs {
    pub entry: u64,
    pub arg: u64,
    pub tp: u64,
    pub stack_bytes: u64,
    pub guard_bytes: u64,
    pub priority: u32,
    pub flags: u32,
    pub sp: u64,
    pub reserved: u64,
}

/// One record of `port_wait_many` (SYSCALLS.md): a ready binding's key and the signals observed under its mask.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortPacket {
    pub key: u64,
    pub observed: u32,
    pub zero: u32,
}

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
    use super::{syscall, Ret, SpawnArgs};
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
    /// The kernel clock in nanoseconds since boot (the ABI's unit, SYSCALLS.md "Time").
    pub fn clock_ns() -> u64 {
        syscall(CLOCK_GET, [0; 6]).a1 as u64
    }
    /// The kernel clock in **milliseconds** since boot: the convenience unit of the servers and tests (`clock_ns() / 1e6`). Deadlines
    /// built on it (`clock_get() + 500`) are milliseconds too, and the wrappers below convert them to the ABI's nanoseconds.
    pub fn clock_get() -> u64 {
        clock_ns() / abi::time::NS_PER_MS
    }
    /// A deadline in absolute milliseconds as the ABI wants it: 0 (poll) and `u64::MAX` (never) stay, the rest becomes nanoseconds.
    pub fn deadline_ns(ms: u64) -> u64 {
        match ms {
            0 | u64::MAX => ms,
            _ => ms.saturating_mul(abi::time::NS_PER_MS),
        }
    }
    /// A kernel counter (see SYSCALLS.md `debug_stat`): 0 blocking waits entered, 1 waits that were woken
    /// without being satisfied (spurious wakes), 2 threads woken.
    pub fn debug_stat(which: u64) -> u64 {
        syscall(DEBUG_STAT, [which as usize, 0, 0, 0, 0, 0]).a1 as u64
    }
    pub fn handle_close(h: u64) -> Ret {
        syscall(HANDLE_CLOSE, [h as usize, 0, 0, 0, 0, 0])
    }
    pub fn handle_duplicate(h: u64, rights: u32) -> Ret {
        syscall(HANDLE_DUPLICATE, [h as usize, rights as usize, 0, 0, 0, 0])
    }
    pub fn handle_replace(h: u64, rights: u32) -> Ret {
        syscall(HANDLE_REPLACE, [h as usize, rights as usize, 0, 0, 0, 0])
    }
    pub fn channel_create() -> Ret {
        syscall(CHANNEL_CREATE, [0; 6])
    }
    pub fn channel_write(
        ch: u64,
        bytes: *const u8,
        nbytes: usize,
        handles: *const u64,
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
        ch: u64,
        bytes: *mut u8,
        max_bytes: usize,
        handles: *mut u64,
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
        ch: u64,
        txid: u32,
        bytes: *mut u8,
        max_bytes: usize,
        handles: *mut u64,
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
    pub fn event_signal(ev: u64, clear: u32, set: u32) -> Ret {
        syscall(
            EVENT_SIGNAL,
            [ev as usize, clear as usize, set as usize, 0, 0, 0],
        )
    }
    pub fn object_signals(h: u64) -> Ret {
        syscall(OBJECT_SIGNALS, [h as usize, 0, 0, 0, 0, 0])
    }
    /// Wait until `signals(h) & mask != 0` or the absolute time `deadline` in **milliseconds** on the `clock_get` clock (0: do not
    /// wait, u64::MAX: forever).
    pub fn object_wait(h: u64, mask: u32, deadline: u64) -> Ret {
        object_wait_ns(h, mask, deadline_ns(deadline))
    }
    /// `object_wait` with the deadline in nanoseconds on the `clock_ns` clock (the ABI's own unit).
    pub fn object_wait_ns(h: u64, mask: u32, deadline: u64) -> Ret {
        syscall(
            OBJECT_WAIT,
            [h as usize, mask as usize, deadline as usize, 0, 0, 0],
        )
    }
    pub fn object_info(h: u64) -> Ret {
        syscall(OBJECT_INFO, [h as usize, 0, 0, 0, 0, 0])
    }
    pub fn vmo_create(pages: u64) -> Ret {
        syscall(
            VMO_CREATE,
            [pages.saturating_mul(4096) as usize, 0, 0, 0, 0, 0],
        )
    }
    /// A VMO of `bytes` bytes (rounded up to whole pages), charged to `job` (0: the caller's own), with `flags` (`abi::vmo_flags`).
    pub fn vmo_create_bytes(bytes: u64, job: u64, flags: u64) -> Ret {
        syscall(
            VMO_CREATE,
            [bytes as usize, job as usize, flags as usize, 0, 0, 0],
        )
    }
    /// A VMO whose pages are charged to `job` (a handle with MANAGE) instead of the caller's own.
    pub fn vmo_create_in(pages: u64, job: u64) -> Ret {
        syscall(
            VMO_CREATE,
            [
                pages.saturating_mul(4096) as usize,
                job as usize,
                0,
                0,
                0,
                0,
            ],
        )
    }
    /// A VMO with `flags` (`abi::vmo_flags`: RESIZABLE), charged to `job` (0: the caller's own).
    pub fn vmo_create_flags(pages: u64, job: u64, flags: u64) -> Ret {
        syscall(
            VMO_CREATE,
            [
                pages.saturating_mul(4096) as usize,
                job as usize,
                flags as usize,
                0,
                0,
                0,
            ],
        )
    }
    /// Give back the committed pages of `[offset, offset + len)` of a VMO (needs WRITE; page aligned): every mapping of them goes, and they read as zeros.
    pub fn vmo_decommit(vmo: u64, offset: u64, len: u64) -> Ret {
        syscall(
            VMO_DECOMMIT,
            [vmo as usize, offset as usize, len as usize, 0, 0, 0],
        )
    }
    /// Change the size of a resizable VMO to `bytes` (a multiple of the page size; needs WRITE).
    pub fn vmo_set_size(vmo: u64, bytes: u64) -> Ret {
        syscall(VMO_SET_SIZE, [vmo as usize, bytes as usize, 0, 0, 0, 0])
    }
    /// A copy-on-write child of `bytes` bytes of `vmo` from `offset`, charged to `job` (0: the caller's own): `flags` is `child_flags::SNAPSHOT` or `PRIVATE` (MM8). Returns the handle.
    pub fn vmo_create_child(vmo: u64, offset: u64, bytes: u64, job: u64, flags: u64) -> Ret {
        syscall(
            VMO_CREATE_CHILD,
            [
                vmo as usize,
                offset as usize,
                bytes as usize,
                job as usize,
                flags as usize,
                0,
            ],
        )
    }
    pub fn vmo_read(vmo: u64, offset: u64, buf: *mut u8, len: usize) -> Ret {
        syscall(
            VMO_READ,
            [vmo as usize, offset as usize, buf as usize, len, 0, 0],
        )
    }
    /// Commit pages `[offset, offset + len)` of a VMO now (needs the COMMIT right); the creator's job pays.
    pub fn vmo_commit(vmo: u64, offset: u64, len: u64) -> Ret {
        syscall(
            VMO_COMMIT,
            [vmo as usize, offset as usize, len as usize, 0, 0, 0],
        )
    }
    pub fn vmo_write(vmo: u64, offset: u64, buf: *const u8, len: usize) -> Ret {
        syscall(
            VMO_WRITE,
            [vmo as usize, offset as usize, buf as usize, len, 0, 0],
        )
    }
    /// The root resource (the process the kernel started only).
    pub fn resource_root() -> Ret {
        syscall(RESOURCE_ROOT, [0; 6])
    }
    pub fn vmo_create_physical(resource: u64, pa: u64, bytes: u64) -> Ret {
        syscall(
            VMO_CREATE_PHYSICAL,
            [resource as usize, pa as usize, bytes as usize, 0, 0, 0],
        )
    }
    pub fn vmo_create_dma(resource: u64, pages: u64) -> Ret {
        syscall(
            VMO_CREATE_DMA,
            [resource as usize, pages as usize, 0, 0, 0, 0],
        )
    }
    pub fn vmo_phys_addr(vmo: u64) -> Ret {
        syscall(VMO_PHYS_ADDR, [vmo as usize, 0, 0, 0, 0, 0])
    }
    /// A read-only VMO holding the device tree; `a2` is its length in bytes.
    pub fn resource_dtb(resource: u64) -> Ret {
        syscall(RESOURCE_DTB, [resource as usize, 0, 0, 0, 0, 0])
    }
    /// Sleep if the 32-bit word at `addr` still equals `expected`; `deadline` is an absolute time in milliseconds on the `clock_get`
    /// clock (0: poll, `u64::MAX`: forever). OK when woken, SHOULD_WAIT if the word differed, TIMED_OUT.
    pub fn futex_wait(addr: *const u32, expected: u32, deadline: u64) -> Ret {
        futex_wait_ns(addr, expected, deadline_ns(deadline))
    }
    /// `futex_wait` with the deadline in nanoseconds.
    pub fn futex_wait_ns(addr: *const u32, expected: u32, deadline: u64) -> Ret {
        syscall(
            FUTEX_WAIT,
            [addr as usize, expected as usize, deadline as usize, 0, 0, 0],
        )
    }
    /// Wake up to `count` threads of this process sleeping on `addr`; returns how many.
    pub fn futex_wake(addr: *const u32, count: u32) -> Ret {
        syscall(FUTEX_WAKE, [addr as usize, count as usize, 0, 0, 0, 0])
    }
    pub fn interrupt_create(resource: u64, irq: u32) -> Ret {
        syscall(
            INTERRUPT_CREATE,
            [resource as usize, irq as usize, 0, 0, 0, 0],
        )
    }
    pub fn interrupt_ack(interrupt: u64) -> Ret {
        syscall(INTERRUPT_ACK, [interrupt as usize, 0, 0, 0, 0, 0])
    }
    pub fn vmo_get_size(vmo: u64) -> Ret {
        syscall(VMO_GET_SIZE, [vmo as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_create() -> Ret {
        syscall(PROCESS_CREATE, [0; 6])
    }
    /// A process charged to `job` (a handle with MANAGE) instead of the caller's own job.
    pub fn process_create_in(job: u64) -> Ret {
        syscall(PROCESS_CREATE, [job as usize, 0, 0, 0, 0, 0])
    }
    /// The limits of a job: processes, threads, pages, jobs, objects, ipc bytes (`u64::MAX` means
    /// no limit), in that order.
    pub type Limits = [u64; 6];
    /// No limit on anything.
    pub const NO_LIMITS: Limits = [u64::MAX; 6];
    pub fn job_create(parent: u64, limits: &Limits) -> Ret {
        syscall(
            JOB_CREATE,
            [parent as usize, limits.as_ptr() as usize, 0, 0, 0, 0],
        )
    }
    pub fn job_set_limits(job: u64, limits: &Limits) -> Ret {
        syscall(
            JOB_SET_LIMITS,
            [job as usize, limits.as_ptr() as usize, 0, 0, 0, 0],
        )
    }
    /// `which`: 0 processes, 1 threads, 2 pages, 3 jobs, 4 objects, 5 ipc bytes. Returns `a1` = used, `a2` = limit.
    pub fn job_info(job: u64, which: u64) -> Ret {
        syscall(JOB_INFO, [job as usize, which as usize, 0, 0, 0, 0])
    }
    /// Set a job's scheduling limits (`max_prio`, RT budget in ns per 100 ms window, CPU weight): SET_LIMITS.
    pub fn job_set_sched(job: u64, max_prio: u8, rt_budget_ns: u64, cpu_weight: u32) -> Ret {
        syscall(
            JOB_SET_SCHED,
            [
                job as usize,
                max_prio as usize,
                rt_budget_ns as usize,
                cpu_weight as usize,
                0,
                0,
            ],
        )
    }
    /// Set a thread's base priority (1 to 63) and flags (bit 0: round robin); `thread` 0 is the caller.
    pub fn thread_set_sched(thread: u64, priority: u8, flags: u8) -> Ret {
        syscall(
            THREAD_SET_SCHED,
            [thread as usize, priority as usize, flags as usize, 0, 0, 0],
        )
    }
    /// `a1` packs the base priority (bits 0-7), flags (8-15) and effective priority (16-23); `a2` is the thread id.
    pub fn thread_get_sched(thread: u64) -> Ret {
        syscall(THREAD_GET_SCHED, [thread as usize, 0, 0, 0, 0, 0])
    }
    pub fn job_kill(job: u64, code: i32) -> Ret {
        syscall(JOB_KILL, [job as usize, code as usize, 0, 0, 0, 0])
    }
    pub fn job_root() -> Ret {
        syscall(JOB_ROOT, [0; 6])
    }
    pub fn process_self() -> Ret {
        syscall(PROCESS_SELF, [0; 6])
    }
    pub fn process_map(
        process: u64,
        vmo: u64,
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
    pub fn process_unmap(process: u64, vaddr: u64) -> Ret {
        syscall(
            PROCESS_UNMAP,
            [process as usize, vaddr as usize, 0, 0, 0, 0],
        )
    }
    /// Unmap every page of `[vaddr, vaddr + len)`: holes are fine and mappings are split at the edges (MM7).
    pub fn process_unmap_range(process: u64, vaddr: u64, len: u64) -> Ret {
        syscall(
            PROCESS_UNMAP,
            [process as usize, vaddr as usize, len as usize, 0, 0, 0],
        )
    }
    /// Change the permissions of a fully mapped range (`flags`: R 1, W 2, X 4; MM7).
    pub fn process_protect(process: u64, vaddr: u64, len: u64, flags: u64) -> Ret {
        syscall(
            PROCESS_PROTECT,
            [
                process as usize,
                vaddr as usize,
                len as usize,
                flags as usize,
                0,
                0,
            ],
        )
    }
    /// Map `length` bytes of `vmo` where the kernel chooses; the address is in `a1` (MM7).
    pub fn process_map_anywhere(
        process: u64,
        vmo: u64,
        vmo_offset: u64,
        length: u64,
        flags: u64,
    ) -> Ret {
        process_map(
            process,
            vmo,
            0,
            vmo_offset,
            length,
            flags | abi::map_flags::ANYWHERE,
        )
    }
    /// The mapping at or after `vaddr`: six words in `buf` (base, length, permissions, ceiling, VMO id, VMO offset); `a1` is the next base or 0 (MM7).
    pub fn process_map_info(process: u64, vaddr: u64, buf: *mut u64) -> Ret {
        syscall(
            PROCESS_MAP_INFO,
            [process as usize, vaddr as usize, buf as usize, 0, 0, 0],
        )
    }
    /// Map the region of `source` that starts at `vaddr` into `target` at the same address: `clone_mode::SHARE` or `SNAPSHOT` (MM8).
    pub fn process_clone_region(source: u64, target: u64, vaddr: u64, mode: u64) -> Ret {
        syscall(
            PROCESS_CLONE_REGION,
            [
                source as usize,
                target as usize,
                vaddr as usize,
                mode as usize,
                0,
                0,
            ],
        )
    }
    pub fn process_start(process: u64, entry: u64, sp: u64, bootstrap: u64, arg: u64) -> Ret {
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
    pub fn socket_write(socket: u64, bytes: *const u8, len: usize) -> Ret {
        syscall(
            SOCKET_WRITE,
            [socket as usize, bytes as usize, len, 0, 0, 0],
        )
    }
    pub fn socket_read(socket: u64, buf: *mut u8, max: usize) -> Ret {
        syscall(SOCKET_READ, [socket as usize, buf as usize, max, 0, 0, 0])
    }
    pub fn socket_shutdown(socket: u64) -> Ret {
        syscall(SOCKET_SHUTDOWN, [socket as usize, 0, 0, 0, 0, 0])
    }
    pub fn port_create() -> Ret {
        syscall(PORT_CREATE, [0; 6])
    }
    pub fn port_bind(port: u64, object: u64, mask: u32, key: u64) -> Ret {
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
    /// `port_bind` with flags (`abi::port_flags::ONESHOT`).
    pub fn port_bind_flags(port: u64, object: u64, mask: u32, key: u64, flags: u64) -> Ret {
        syscall(
            PORT_BIND,
            [
                port as usize,
                object as usize,
                mask as usize,
                key as usize,
                flags as usize,
                0,
            ],
        )
    }
    pub fn port_rearm(port: u64, key: u64) -> Ret {
        syscall(PORT_REARM, [port as usize, key as usize, 0, 0, 0, 0])
    }
    pub fn port_unbind(port: u64, key: u64) -> Ret {
        syscall(PORT_UNBIND, [port as usize, key as usize, 0, 0, 0, 0])
    }
    /// Wait for a binding to be ready; `deadline` in absolute milliseconds as for `object_wait`.
    pub fn port_wait(port: u64, deadline: u64) -> Ret {
        port_wait_ns(port, deadline_ns(deadline))
    }
    /// Wait for bindings to be ready and take up to `buf.len()` (1 to 64) of them at once; `deadline` as for `port_wait`. `a1` of the
    /// result is how many records were written.
    pub fn port_wait_many(port: u64, buf: &mut [super::PortPacket], deadline: u64) -> Ret {
        syscall(
            PORT_WAIT_MANY,
            [
                port as usize,
                buf.as_mut_ptr() as usize,
                buf.len(),
                deadline_ns(deadline) as usize,
                0,
                0,
            ],
        )
    }
    /// `port_wait` with the deadline in nanoseconds.
    pub fn port_wait_ns(port: u64, deadline: u64) -> Ret {
        syscall(PORT_WAIT, [port as usize, deadline as usize, 0, 0, 0, 0])
    }
    /// A thread on a stack the caller provides (`thread_spawn` with CALLER_STACK; the call `thread_create` is retired, AB5): it starts at
    /// `entry` with `a0 = arg` and `sp` as given. `a1` of the result is its handle, `a2` its tid.
    pub fn thread_on_stack(process: u64, entry: u64, sp: u64, arg: u64) -> Ret {
        let rec = SpawnArgs {
            entry,
            arg,
            flags: abi::spawn::CALLER_STACK,
            sp,
            ..SpawnArgs::default()
        };
        thread_spawn(process, &rec)
    }
    /// Create and start a thread (SYSCALLS.md `thread_spawn`): `a1` is its handle (0 if detached), `a2` its tid.
    pub fn thread_spawn(process: u64, rec: &SpawnArgs) -> Ret {
        syscall(
            THREAD_SPAWN,
            [
                process as usize,
                rec as *const SpawnArgs as usize,
                0,
                0,
                0,
                0,
            ],
        )
    }
    /// The user end of the channel the exceptions of `process`'s threads are delivered on (EX2; needs MANAGE): a message of
    /// `abi::exception::MESSAGE_BYTES` bytes with two handles (the thread, the exception) per fault the kernel cannot resolve.
    pub fn process_exception_channel(process: u64) -> Ret {
        syscall(PROCESS_EXCEPTION_CHANNEL, [process as usize, 0, 0, 0, 0, 0])
    }
    /// The same for the processes of `job` (and of the jobs below it) that have no channel of their own.
    pub fn job_exception_channel(job: u64) -> Ret {
        syscall(JOB_EXCEPTION_CHANNEL, [job as usize, 0, 0, 0, 0, 0])
    }
    /// Say what becomes of the thread that took `exception`: `abi::exception::RETRY`, `NEXT` or `KILL`.
    pub fn exception_resume(exception: u64, how: u64) -> Ret {
        syscall(
            EXCEPTION_RESUME,
            [exception as usize, how as usize, 0, 0, 0, 0],
        )
    }
    /// Raise a thread's suspend count (EX1; needs MANAGE): it stops at its next return to user mode, or where it is if it is blocked in a call.
    /// Its object then raises SUSPENDED.
    pub fn thread_suspend(thread: u64) -> Ret {
        syscall(THREAD_SUSPEND, [thread as usize, 0, 0, 0, 0, 0])
    }
    /// Lower a thread's suspend count (EX1); at 0 it runs again. BAD_STATE when it is not suspended.
    pub fn thread_resume(thread: u64) -> Ret {
        syscall(THREAD_RESUME, [thread as usize, 0, 0, 0, 0, 0])
    }
    /// Read a stopped thread's registers into `buf` (EX1): `kind` is `abi::thread_state::GENERAL`, `FP` or `VECTOR`; `len` is exactly the layout's size.
    pub fn thread_read_state(thread: u64, kind: u64, buf: *mut u8, len: usize) -> Ret {
        syscall(
            THREAD_READ_STATE,
            [thread as usize, kind as usize, buf as usize, len, 0, 0],
        )
    }
    /// Write a stopped thread's registers from `buf` (EX1); refused while the thread is inside a blocking call.
    pub fn thread_write_state(thread: u64, kind: u64, buf: *const u8, len: usize) -> Ret {
        syscall(
            THREAD_WRITE_STATE,
            [thread as usize, kind as usize, buf as usize, len, 0, 0],
        )
    }
    /// Reserved (T56): always NOT_SUPPORTED for a valid thread handle.
    pub fn thread_interrupt(thread: u64) -> Ret {
        syscall(THREAD_INTERRUPT, [thread as usize, 0, 0, 0, 0, 0])
    }
    pub fn thread_exit(code: i32) -> ! {
        syscall(THREAD_EXIT, [code as usize, 0, 0, 0, 0, 0]);
        loop {
            core::hint::spin_loop();
        }
    }
    pub fn thread_info(thread: u64) -> Ret {
        syscall(THREAD_INFO, [thread as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_kill(process: u64, code: i32) -> Ret {
        syscall(PROCESS_KILL, [process as usize, code as usize, 0, 0, 0, 0])
    }
    /// Stop every thread of `process` until `process_resume` (needs KILL).
    pub fn process_suspend(process: u64) -> Ret {
        syscall(PROCESS_SUSPEND, [process as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_resume(process: u64) -> Ret {
        syscall(PROCESS_RESUME, [process as usize, 0, 0, 0, 0, 0])
    }
    pub fn process_info(process: u64) -> Ret {
        syscall(PROCESS_INFO, [process as usize, 0, 0, 0, 0, 0])
    }
    /// `channel_call`; `args.deadline` is an absolute time in **milliseconds** on the `clock_get` clock (0: poll, `u64::MAX`: never).
    pub fn channel_call(ch: u64, args: &super::CallArgs) -> Ret {
        let ns = super::CallArgs {
            req_bytes: args.req_bytes,
            req_handles: args.req_handles,
            req_nbytes: args.req_nbytes,
            req_nhandles: args.req_nhandles,
            rep_bytes: args.rep_bytes,
            rep_handles: args.rep_handles,
            rep_max_bytes: args.rep_max_bytes,
            rep_max_handles: args.rep_max_handles,
            deadline: deadline_ns(args.deadline),
        };
        channel_call_ns(ch, &ns)
    }
    /// `channel_call` with `args.deadline` in nanoseconds (the ABI's own unit).
    pub fn channel_call_ns(ch: u64, args: &super::CallArgs) -> Ret {
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
pub const BOOTSTRAP: u64 = 1;

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
