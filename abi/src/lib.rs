//! The contract between the OS-track kernel and everything that talks to it.
//! S0 holds only what the boot test needs; the syscall table arrives with S3 and
//! is specified in `SYSCALLS.md` first.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::panic,
        clippy::unreachable,
        clippy::undocumented_unsafe_blocks
    )
)] // KT3 (KERNEL-V1-SPEC 7.4); sites that remain carry `#[allow] // INVARIANT:`
#![no_std]
#![forbid(unsafe_code)]

/// The values of QEMU's test finisher (a SiFive test device; its address comes from the device tree): writing one ends the emulation with
/// an exit status.
pub mod arch;

pub mod finisher {
    /// Write this to exit QEMU with status 0.
    pub const PASS: u32 = 0x5555;
    /// The value that exits QEMU with status `code` (1 to 0xffff).
    pub const fn fail(code: u16) -> u32 {
        ((code as u32) << 16) | 0x3333
    }
}

/// Status codes returned in `a0` by every syscall (SYSCALLS.md, "Status codes").
pub mod status {
    #[repr(i32)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Status {
        Ok = 0,
        BadHandle = -1,
        WrongType = -2,
        AccessDenied = -3,
        InvalidArgs = -4,
        NoMemory = -5,
        ShouldWait = -6,
        PeerClosed = -7,
        BufferTooSmall = -8,
        TooBig = -9,
        BadState = -10,
        NotSupported = -11,
        TimedOut = -12,
        BadSyscall = -13,
        /// Reserved (slice T, T56): a blocking call that was told to stop by `thread_interrupt`. Nothing
        /// returns it yet; callers should treat it like a retry.
        Interrupted = -14,
    }

    impl Status {
        pub const fn code(self) -> i32 {
            self as i32
        }

        pub fn from_code(code: i32) -> Option<Status> {
            Some(match code {
                0 => Status::Ok,
                -1 => Status::BadHandle,
                -2 => Status::WrongType,
                -3 => Status::AccessDenied,
                -4 => Status::InvalidArgs,
                -5 => Status::NoMemory,
                -6 => Status::ShouldWait,
                -7 => Status::PeerClosed,
                -8 => Status::BufferTooSmall,
                -9 => Status::TooBig,
                -10 => Status::BadState,
                -11 => Status::NotSupported,
                -12 => Status::TimedOut,
                -13 => Status::BadSyscall,
                -14 => Status::Interrupted,
                _ => return None,
            })
        }
    }
}

/// A handle value as userland sees it (SYSCALLS.md, "ABI").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(pub u32);

impl Handle {
    pub const INVALID: Handle = Handle(0);
}

/// Rights carried by a handle (SYSCALLS.md, "Rights"). Rights only ever shrink.
pub mod rights {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Rights(pub u32);

    impl Rights {
        pub const NONE: Rights = Rights(0);
        pub const DUPLICATE: Rights = Rights(1 << 0);
        pub const TRANSFER: Rights = Rights(1 << 1);
        pub const READ: Rights = Rights(1 << 2);
        pub const WRITE: Rights = Rights(1 << 3);
        pub const EXECUTE: Rights = Rights(1 << 4);
        pub const MAP: Rights = Rights(1 << 5);
        pub const SIGNAL: Rights = Rights(1 << 6);
        pub const WAIT: Rights = Rights(1 << 7);
        pub const MANAGE: Rights = Rights(1 << 8);
        pub const INSPECT: Rights = Rights(1 << 9);
        pub const KILL: Rights = Rights(1 << 10);
        /// Change a job's limits (a job handle only). Apart from MANAGE so that a program can be
        /// given the power to make things inside a job without the power to lift the job's limits.
        pub const SET_LIMITS: Rights = Rights(1 << 11);
        /// Every assigned right bit.
        pub const ALL: Rights = Rights(0x3fff);
        /// Learn a DMA VMO's physical address (a DMA VMO handle only).
        pub const DMA: Rights = Rights(1 << 12);
        /// Commit a VMO's pages now (`vmo_commit`): the pages are charged to the VMO's creating job, so a
        /// holder with this right spends the creator's quota (slice V). A creator narrows it away before
        /// handing a VMO to someone else.
        pub const COMMIT: Rights = Rights(1 << 13);
        /// Not a right: "the same rights as the source", for duplicate and replace.
        pub const SAME: Rights = Rights(1 << 31);

        pub const CHANNEL: Rights = Rights(
            Self::DUPLICATE.0
                | Self::TRANSFER.0
                | Self::READ.0
                | Self::WRITE.0
                | Self::SIGNAL.0
                | Self::WAIT.0,
        );
        pub const VMO: Rights = Rights(
            Self::DUPLICATE.0
                | Self::TRANSFER.0
                | Self::READ.0
                | Self::WRITE.0
                | Self::EXECUTE.0
                | Self::MAP.0
                | Self::COMMIT.0,
        );
        pub const PROCESS: Rights = Rights(
            Self::DUPLICATE.0
                | Self::TRANSFER.0
                | Self::MANAGE.0
                | Self::KILL.0
                | Self::INSPECT.0
                | Self::WAIT.0,
        );
        pub const SOCKET: Rights = Rights(
            Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::READ.0 | Self::WRITE.0 | Self::WAIT.0,
        );
        pub const PORT: Rights =
            Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::MANAGE.0 | Self::WAIT.0);
        pub const THREAD: Rights = Rights(
            Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::MANAGE.0 | Self::INSPECT.0 | Self::WAIT.0,
        );
        /// A job: create children and processes in it, set limits (MANAGE), end everything in it
        /// (KILL), read usage (INSPECT).
        pub const JOB: Rights = Rights(
            Self::DUPLICATE.0
                | Self::TRANSFER.0
                | Self::MANAGE.0
                | Self::KILL.0
                | Self::INSPECT.0
                | Self::SET_LIMITS.0,
        );
        /// The root resource: mint device memory, DMA buffers and interrupts. Held only by early boot code.
        pub const RESOURCE: Rights = Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::MANAGE.0);
        /// An interrupt object: wait for it, inspect it, acknowledge it (MANAGE).
        pub const INTERRUPT: Rights = Rights(
            Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::WAIT.0 | Self::INSPECT.0 | Self::MANAGE.0,
        );
        pub const EVENT: Rights =
            Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::SIGNAL.0 | Self::WAIT.0);

        pub const fn union(self, other: Rights) -> Rights {
            Rights(self.0 | other.0)
        }

        /// Does `self` include every right in `other`?
        pub const fn contains(self, other: Rights) -> bool {
            self.0 & other.0 == other.0
        }

        /// Are all of `self`'s rights also in `other`?
        pub const fn is_subset_of(self, other: Rights) -> bool {
            self.0 & !other.0 == 0
        }

        /// Only assigned bits are set.
        pub const fn is_valid(self) -> bool {
            self.0 & !Self::ALL.0 == 0
        }
    }
}

/// Signal bits (SYSCALLS.md, "Signals").
/// Flags for `port_bind` (its fifth argument).
pub mod port_flags {
    /// The binding disarms itself when `port_wait` reports it, until `port_rearm`.
    pub const ONESHOT: u64 = 1;
}

pub mod signals {
    pub const READABLE: u32 = 1 << 0;
    pub const WRITABLE: u32 = 1 << 1;
    pub const PEER_CLOSED: u32 = 1 << 2;
    /// A process has ended.
    pub const TERMINATED: u32 = 1 << 3;
    /// The peer socket end called `socket_shutdown`: no more data will arrive beyond what is queued.
    pub const WRITE_CLOSED: u32 = 1 << 4;
    /// An interrupt object's source fired and has not been acknowledged.
    pub const INTERRUPT: u32 = 1 << 5;
    /// A process is stopped by `process_suspend` (level: cleared by `process_resume`; never set with TERMINATED).
    pub const STOPPED: u32 = 1 << 6;
    /// User signals: bits 24 to 31, changed only by `event_signal`.
    pub const USER_MASK: u32 = 0xff00_0000;
    /// Every signal a wait or a port binding may name.
    pub const WAITABLE: u32 = READABLE
        | WRITABLE
        | PEER_CLOSED
        | TERMINATED
        | WRITE_CLOSED
        | INTERRUPT
        | STOPPED
        | USER_MASK;
}

/// The handle value of the bootstrap channel every process starts with.
pub const BOOTSTRAP_HANDLE: Handle = Handle(1);

/// Size of the `channel_call` argument record (SYSCALLS.md).
pub const CHANNEL_CALL_ARGS_BYTES: usize = 56;

/// Limits (SYSCALLS.md, "Limits").
pub mod limits {
    pub const MAX_HANDLES: usize = 4096;
    pub const MAX_MSG_BYTES: usize = 65536;
    pub const MAX_MSG_HANDLES: usize = 64;
    pub const MAX_QUEUED_MSGS: usize = 128;
    /// Bytes a channel endpoint may have queued (so 128 large messages cannot pile up).
    pub const MAX_QUEUED_BYTES: usize = 256 * 1024;
    /// Kernel objects in existence at once; creating more is NO_MEMORY. A stand-in for
    /// per-job quotas (OS-ARCHITECTURE Q9).
    pub const MAX_OBJECTS: usize = 16384;
    /// Largest device tree `resource_dtb` will copy.
    pub const MAX_DTB_BYTES: usize = 256 * 1024;
    /// Pages in one DMA VMO, and in all DMA VMOs together (S6: a stand-in for per-job DMA quotas).
    pub const MAX_DMA_PAGES: u64 = 64;
    pub const MAX_DMA_TOTAL_PAGES: u64 = 1024;
    /// Pages in one VMO (64 MiB).
    pub const MAX_VMO_PAGES: u64 = 16384;
    /// Most pages one `vmo_commit` call may commit (the kernel zeroes them with the big lock held).
    pub const MAX_COMMIT_PAGES: u64 = 1024;
    /// Live processes at once.
    pub const MAX_PROCESSES: usize = 64;
    /// Live user threads in one process.
    pub const MAX_THREADS_PER_PROCESS: usize = 2048;
    /// Live user threads in the whole system (each costs a 32 KiB kernel stack from the
    /// kernel heap (an 8 MiB boot heap that grows from the frame database); a stand-in for per-job quotas, OS-ARCHITECTURE Q9).
    pub const MAX_USER_THREADS: usize = 4096;
    /// Bytes buffered in all channels and sockets together (another stand-in for per-job
    /// quotas, Q9): a write that would exceed it fails with NO_MEMORY.
    pub const MAX_IPC_BYTES: usize = 2 * 1024 * 1024;
    /// Bytes buffered per socket endpoint.
    pub const SOCKET_BUFFER: usize = 65536;
    /// Mapped regions in one address space.
    pub const MAX_REGIONS: usize = 16384;
}

/// Time (SYSCALLS.md, "Time"): `clock_get` returns nanoseconds since boot and every deadline argument is an absolute time in
/// nanoseconds on that clock (AB2). 0 means "do not wait", `DEADLINE_NEVER` means "no deadline".
pub mod time {
    pub const NS_PER_MS: u64 = 1_000_000;
    pub const NS_PER_SEC: u64 = 1_000_000_000;
    /// No deadline: wait for ever.
    pub const DEADLINE_NEVER: u64 = u64::MAX;
}

/// Syscall numbers (SYSCALLS.md, "Syscall numbers and registers").
pub mod syscall {
    pub const DEBUG_WRITE: usize = 0;
    pub const PROCESS_EXIT: usize = 1;
    pub const THREAD_YIELD: usize = 2;
    pub const CLOCK_GET: usize = 3;
    /// A kernel counter by index (a measuring aid for the boot-time suite, like `DEBUG_WRITE`).
    pub const DEBUG_STAT: usize = 4;
    pub const HANDLE_CLOSE: usize = 10;
    pub const HANDLE_DUPLICATE: usize = 11;
    pub const HANDLE_REPLACE: usize = 12;
    pub const CHANNEL_CREATE: usize = 20;
    pub const CHANNEL_WRITE: usize = 21;
    pub const CHANNEL_READ: usize = 22;
    pub const CHANNEL_READ_TXID: usize = 23;
    pub const CHANNEL_CALL: usize = 24;
    pub const EVENT_CREATE: usize = 30;
    pub const EVENT_SIGNAL: usize = 31;
    pub const OBJECT_SIGNALS: usize = 32;
    pub const OBJECT_WAIT: usize = 33;
    pub const OBJECT_INFO: usize = 34;
    pub const VMO_CREATE: usize = 40;
    pub const VMO_READ: usize = 41;
    pub const VMO_WRITE: usize = 42;
    pub const VMO_GET_SIZE: usize = 43;
    pub const VMO_CREATE_PHYSICAL: usize = 44;
    pub const VMO_CREATE_DMA: usize = 45;
    pub const VMO_PHYS_ADDR: usize = 46;
    pub const VMO_COMMIT: usize = 47;
    pub const PROCESS_CREATE: usize = 50;
    pub const PROCESS_SELF: usize = 51;
    pub const PROCESS_MAP: usize = 52;
    pub const PROCESS_UNMAP: usize = 53;
    pub const PROCESS_START: usize = 54;
    pub const PROCESS_KILL: usize = 55;
    pub const PROCESS_INFO: usize = 56;
    /// Stop every thread of a process until `PROCESS_RESUME` (SIG2): needs KILL.
    pub const PROCESS_SUSPEND: usize = 57;
    pub const PROCESS_RESUME: usize = 58;
    pub const THREAD_CREATE: usize = 60;
    pub const THREAD_EXIT: usize = 61;
    pub const THREAD_INFO: usize = 62;
    /// Reserved (T56): tell a thread to stop what it is blocked in. Validates its arguments, then
    /// answers NOT_SUPPORTED.
    pub const THREAD_INTERRUPT: usize = 63;
    /// Create and start a thread with a kernel-provisioned stack (SCHED-SMP-SPEC 5.3, task 9).
    pub const THREAD_SPAWN: usize = 64;
    /// Set a thread's base priority and flags (SCHED-SMP-SPEC 4.9, slice SC).
    pub const THREAD_SET_SCHED: usize = 65;
    pub const THREAD_GET_SCHED: usize = 66;
    pub const SOCKET_CREATE: usize = 70;
    pub const SOCKET_WRITE: usize = 71;
    pub const SOCKET_READ: usize = 72;
    pub const SOCKET_SHUTDOWN: usize = 73;
    pub const PORT_CREATE: usize = 80;
    pub const PORT_BIND: usize = 81;
    pub const PORT_UNBIND: usize = 82;
    pub const PORT_WAIT: usize = 83;
    pub const PORT_REARM: usize = 84;
    pub const JOB_CREATE: usize = 90;
    pub const JOB_SET_LIMITS: usize = 91;
    pub const JOB_INFO: usize = 92;
    /// Set a job's `max_prio`, `rt_budget` and `cpu_weight`.
    pub const JOB_SET_SCHED: usize = 96;
    pub const JOB_KILL: usize = 93;
    pub const JOB_ROOT: usize = 94;
    pub const RESOURCE_ROOT: usize = 95;
    pub const RESOURCE_DTB: usize = 102;
    pub const FUTEX_WAIT: usize = 110;
    pub const FUTEX_WAKE: usize = 111;
    pub const INTERRUPT_CREATE: usize = 100;
    pub const INTERRUPT_ACK: usize = 101;
}

/// The user address space (SYSCALLS.md, "User address space and process start").
pub mod user {
    /// Lowest mappable user address; page 0 is never mapped.
    pub const BASE: u64 = crate::arch::USER_BASE;
    /// First address above the user half.
    pub const TOP: u64 = crate::arch::USER_TOP;
    /// Initial `sp`.
    pub const STACK_TOP: u64 = crate::arch::USER_STACK_TOP;
    /// Pages of lazily committed stack below `STACK_TOP`.
    pub const STACK_PAGES: u64 = 64;
    /// Longest `debug_write`.
    pub const MAX_DEBUG_WRITE: usize = 4096;
    /// The stack arena: where `thread_spawn` maps the stacks it provisions (32 GiB nothing else is given).
    pub const STACK_ARENA_BASE: u64 = crate::arch::USER_STACK_ARENA_BASE;
    pub const STACK_ARENA_END: u64 = crate::arch::USER_STACK_ARENA_END;
}

/// `thread_spawn` (SYSCALLS.md): the argument record and its limits.
pub mod spawn {
    /// Bytes in the argument record.
    pub const ARGS_BYTES: usize = 64;
    /// Stack bytes when the record says 0.
    pub const DEFAULT_STACK: u64 = 256 * 1024;
    /// The most stack bytes one thread may ask for.
    pub const MAX_STACK: u64 = 64 * 1024 * 1024;
    /// Flag: round robin within the priority level.
    pub const ROUND_ROBIN: u32 = 1;
    /// Flag: no handle is returned (nothing to join).
    pub const DETACHED: u32 = 2;
    /// Flag: the stack is the caller's (`sp` at offset 48); the kernel provisions nothing.
    pub const CALLER_STACK: u32 = 4;
}

/// The kernel's address-space layout (Sv39, kernel in the upper half).
pub mod layout {
    /// The virtual address the kernel image is linked at (the start of its text). Where it sits in physical memory is the loader's choice,
    /// found at boot (`arch::kernel_delta`: virtual minus physical).
    pub const KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8020_0000;
    /// Start of the direct map: physical address `pa` is at `DIRECT_BASE + pa`
    /// (RAM outside the kernel image, plus the device pages the kernel uses).
    pub const DIRECT_BASE: u64 = 0xffff_ffc0_0000_0000;
    /// The direct map reaches physical addresses below this (64 GiB: top-level slots 256 to 319); the slots above it belong to other
    /// kernel windows (the self-test region at 320, the stacks at 384, the heap at 416, the image at 510). RAM above it is not used.
    pub const DIRECT_MAP_PHYS_LIMIT: u64 = 64 << 30;
    /// Highest PLIC source an interrupt object may name (QEMU `virt`: the eight virtio-mmio slots
    /// are sources 1..=8; the UART, 10, belongs to the kernel).
    pub const MAX_IRQ: u32 = 8;
    /// First virtual address of the region the self-tests use for lazily
    /// committed memory (an otherwise empty top-level slot in the upper half).
    pub const LAZY_TEST_BASE: u64 = 0xffff_ffd0_0000_0000;
    /// The kernel-stack window (slice T): one top-level slot (1 GiB) of its own, carved into slots of
    /// `KSTACK_SLOT_BYTES`. In each slot the lower half is **unmapped** (the guard: a stack that
    /// overflows runs into it and faults) and the upper half is the stack. The trap entry recognises
    /// a stack pointer in the guard half by arithmetic alone (see `kernel/src/arch/riscv64/trap.rs`), so the slot
    /// size and the guard/stack split must stay powers of two.
    pub const KSTACK_WINDOW_BASE: u64 = 0xffff_ffe0_0000_0000;
    pub const KSTACK_WINDOW_SIZE: u64 = 1 << 30;
    pub const KSTACK_SLOT_BYTES: u64 = 32 * 1024;
    /// The mapped, usable part of a slot (the top half).
    pub const KSTACK_BYTES: u64 = KSTACK_SLOT_BYTES / 2;
    /// The grown kernel heap's window (slice T step 2): the pages the heap adds after boot are mapped here,
    /// contiguously, so the allocator can extend. One top-level slot of its own.
    pub const HEAP_WINDOW_BASE: u64 = 0xffff_ffe8_0000_0000;
    pub const HEAP_WINDOW_SIZE: u64 = 256 * 1024 * 1024;
}

#[cfg(test)]
mod tests {
    use super::finisher;

    #[test]
    fn layout_is_consistent() {
        use super::layout::*;
        // Sv39 upper half: bit 38 and everything above set.
        for va in [DIRECT_BASE, KERNEL_VIRT_BASE, LAZY_TEST_BASE] {
            assert_eq!(
                (va as i64) >> 38,
                -1,
                "{va:#x} is not in the Sv39 upper half"
            );
        }
        // The direct map and the lazy test area use top-level slots of their own.
        let slot = |va: u64| (va >> 30) & 0x1ff;
        assert_eq!(slot(DIRECT_BASE), 256);
        assert_eq!(slot(KERNEL_VIRT_BASE), 510);
        assert_eq!(slot(LAZY_TEST_BASE), 320);
        // the direct map's span ends where the next window begins, and the early table's loop (`boot.rs`) maps exactly that span
        assert_eq!(DIRECT_BASE + DIRECT_MAP_PHYS_LIMIT, LAZY_TEST_BASE);
        assert_eq!(slot(KSTACK_WINDOW_BASE), 384);
        assert_eq!(slot(HEAP_WINDOW_BASE), 416);
        assert_eq!((HEAP_WINDOW_BASE as i64) >> 38, -1);
        assert_eq!((KSTACK_WINDOW_BASE as i64) >> 38, -1);
        // the trap entry's guard test: `srai 30` of an address in the window is -128, and the window is
        // exactly one top-level slot
        assert_eq!((KSTACK_WINDOW_BASE as i64) >> 30, -128);
        assert_eq!(KSTACK_WINDOW_SIZE, 1 << 30);
        assert!(KSTACK_SLOT_BYTES.is_power_of_two() && KSTACK_BYTES * 2 == KSTACK_SLOT_BYTES);
    }

    #[test]
    fn finisher_values_match_the_device_documentation() {
        assert_eq!(finisher::PASS, 0x5555);
        assert_eq!(finisher::fail(1), 0x0001_3333);
        assert_eq!(finisher::fail(0xffff), 0xffff_3333);
    }
}
