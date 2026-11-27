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
pub struct Handle(pub u64);

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
        pub const ALL: Rights = Rights(0x3_ffff);
        /// Learn a DMA VMO's physical address (a DMA VMO handle only).
        pub const DMA: Rights = Rights(1 << 12);
        /// Commit a VMO's pages now (`vmo_commit`): the pages are charged to the VMO's creating job, so a
        /// holder with this right spends the creator's quota (slice V). A creator narrows it away before
        /// handing a VMO to someone else.
        pub const COMMIT: Rights = Rights(1 << 13);
        /// Map a DMA VMO into a device domain (`iodomain_map`; a domain handle only, IOMMU-SPEC 4.1).
        pub const IO_MAP: Rights = Rights(1 << 14);
        /// Remove a mapping of a device domain (`iodomain_unmap`).
        pub const IO_UNMAP: Rights = Rights(1 << 15);
        /// Grant an interrupt vector of a device domain (`iodomain_grant_vector`, IOMMU7).
        pub const IO_GRANT: Rights = Rights(1 << 16);
        /// Destroy a device domain (`iodomain_destroy`).
        pub const IO_DESTROY: Rights = Rights(1 << 17);
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
        /// An exception (EX2): resolve it (`exception_resume`), pass it on, hand it to another thread.
        pub const EXCEPTION: Rights = Rights(Self::TRANSFER.0 | Self::MANAGE.0);
        /// A pager (MM9): create its VMOs, supply and fail their pages (MANAGE).
        pub const PAGER: Rights = Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::MANAGE.0);
        /// The VMO of a pager: every right of a VMO but COMMIT (committing would put zeros where the pager's data belongs).
        pub const PAGER_VMO: Rights = Rights(Self::VMO.0 & !Self::COMMIT.0);
        pub const THREAD: Rights = Rights(
            Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::MANAGE.0 | Self::INSPECT.0 | Self::WAIT.0,
        );
        /// A job: create children and processes in it, set limits (MANAGE), end everything in it
        /// (KILL), read usage (MANAGE).
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
        /// A debug resource (HD4): `debug_write` needs WRITE, `debug_stat` INSPECT; it can be duplicated and handed on.
        pub const DEBUG: Rights =
            Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::WRITE.0 | Self::INSPECT.0);
        /// An interrupt object: wait for it, inspect it, acknowledge it (MANAGE).
        pub const INTERRUPT: Rights = Rights(
            Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::WAIT.0 | Self::INSPECT.0 | Self::MANAGE.0,
        );
        pub const EVENT: Rights =
            Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::SIGNAL.0 | Self::WAIT.0);
        /// A device domain, as `iodomain_create` returns it (the supervisor's control handle, IOMMU-SPEC 4.1): map, unmap, grant,
        /// destroy, and wait for and read its faults.
        pub const IODOMAIN: Rights = Rights(
            Self::IO_MAP.0
                | Self::IO_UNMAP.0
                | Self::IO_GRANT.0
                | Self::IO_DESTROY.0
                | Self::DUPLICATE.0
                | Self::TRANSFER.0
                | Self::WAIT.0
                | Self::INSPECT.0,
        );
        /// What a driver is given (a duplicate of the control handle): map and unmap its own buffers, nothing else.
        pub const IODOMAIN_DRIVER: Rights = Rights(Self::IO_MAP.0 | Self::IO_UNMAP.0);

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
/// Flags for `vmo_create` (its third argument, MM6).
pub mod vmo_flags {
    /// The VMO's size may change (`vmo_set_size`).
    pub const RESIZABLE: u64 = 1;
}

/// Flags for `vmo_create_child` (its fifth argument, MM8): exactly one.
pub mod child_flags {
    /// The source and the child each see the source's contents as they were; later writes are private to the writer. The child is charged to the source's job.
    pub const SNAPSHOT: u64 = 1;
    /// The child sees the source's pages, including later changes to those it has not written; its writes are private to it.
    pub const PRIVATE: u64 = 2;
}

/// Flags of `process_map` beyond the permissions (R 1, W 2, X 4): MM7.
pub mod map_flags {
    /// The kernel picks a free place at random (`vaddr` must be 0); the address is returned in `a1`.
    pub const ANYWHERE: u64 = 8;
    /// Reserved (HD5, KERNEL-V1-SPEC 7.5): a shadow-stack mapping. Named so that adding the mechanism changes nothing that exists; refused with
    /// NOT_SUPPORTED.
    pub const SHADOW_STACK: u64 = 1 << 8;
    /// Reserved (HD5): refused with NOT_SUPPORTED.
    pub const RESERVED_9: u64 = 1 << 9;
}

/// The `features` word of `process_start` (its sixth argument, HD5): it must be zero. The named bits are mechanisms this kernel does not have, and
/// are refused with NOT_SUPPORTED; any other bit is INVALID_ARGS.
pub mod start_features {
    /// Forward-edge control-flow integrity (landing pads).
    pub const LANDING_PADS: u64 = 1;
    /// A shadow stack.
    pub const SHADOW_STACK: u64 = 2;
    /// Pointer masking.
    pub const POINTER_MASKING: u64 = 4;
    /// The named bits.
    pub const ALL: u64 = LANDING_PADS | SHADOW_STACK | POINTER_MASKING;
}

/// Modes of `process_clone_region` (its fourth argument, MM8).
pub mod clone_mode {
    /// The target maps the same VMO with the same permissions.
    pub const SHARE: u64 = 1;
    /// The target maps a SNAPSHOT child of the source's VMO.
    pub const SNAPSHOT: u64 = 2;
}

pub mod port_flags {
    /// The binding disarms itself when `port_wait` reports it, until `port_rearm`.
    pub const ONESHOT: u64 = 1;
}

/// Device domains (IOMMU-SPEC 4, 7.4): the `access` word of `iodomain_map` and the words `iodomain_fault_read` returns.
pub mod iodomain {
    /// The device may read the memory (`access`: READ, or READ | WRITE; there is no write-only access).
    pub const READ: u64 = 1;
    /// The device may also write it.
    pub const WRITE: u64 = 2;
    /// `iodomain_fault_read`'s `a1`: the cause in bits 15:0, the direction in bits 23:16, the faults dropped since the previous read in
    /// bits 63:32 (`a2` is the I/O address).
    pub const DIRECTION_SHIFT: u32 = 16;
    pub const DROPPED_SHIFT: u32 = 32;
    /// The directions: not a transaction, a read, a write (a message is a write), an instruction fetch.
    pub const DIR_NONE: u64 = 0;
    pub const DIR_READ: u64 = 1;
    pub const DIR_WRITE: u64 = 2;
    pub const DIR_EXEC: u64 = 3;
    /// The data word of every granted vector's MSI-X entry (IOMMU-SPEC 8.2): the kernel enables only identity 1 in a vector's own
    /// interrupt file. `iodomain_grant_vector` returns the address (`a2`); the data word is this constant.
    pub const MSI_DATA: u64 = 1;
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
    /// A device domain has a fault record waiting (IOMMU-SPEC 7.4: level, cleared by the `iodomain_fault_read` that empties its ring).
    /// The same bit as STOPPED: signal bits are per object kind.
    pub const DEVICE_FAULT: u32 = 1 << 6;
    /// A thread has stopped because of `thread_suspend` (level: cleared when its suspend count falls to 0; never set with TERMINATED; EX1). A
    /// thread blocked in a call has stopped where it is.
    pub const SUSPENDED: u32 = 1 << 7;
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
        | SUSPENDED
        | USER_MASK;
}

/// The handle value of the bootstrap channel every process starts with.
pub const BOOTSTRAP_HANDLE: Handle = Handle(1);

/// Size of the `channel_call` argument record (SYSCALLS.md).
pub const CHANNEL_CALL_ARGS_BYTES: usize = 56;

/// Limits (SYSCALLS.md, "Limits").
pub mod limits {
    /// Most packets one `port_wait_many` returns (AB4).
    pub const PORT_PACKETS_MAX: usize = 64;
    /// Bytes in one `port_wait_many` record: `key: u64`, `observed: u32`, four bytes of zero.
    pub const PORT_PACKET_BYTES: usize = 16;
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
    /// Most pages one `vmo_commit` call may commit (the kernel zeroes them with the big lock held).
    pub const MAX_COMMIT_PAGES: u64 = 1024;
    /// Live processes at once.
    pub const MAX_PROCESSES: usize = 64;
    /// Live user threads in the whole system (each costs a 32 KiB kernel stack from the
    /// kernel heap (an 8 MiB boot heap that grows from the frame database); a stand-in for per-job quotas, OS-ARCHITECTURE Q9).
    pub const MAX_USER_THREADS: usize = 4096;
    /// Bytes buffered in all channels and sockets together (another stand-in for per-job
    /// quotas, Q9): a write that would exceed it fails with NO_MEMORY.
    pub const MAX_IPC_BYTES: usize = 2 * 1024 * 1024;
    /// Bytes buffered per socket endpoint.
    pub const SOCKET_BUFFER: usize = 65536;
    /// Mapped regions in one address space: the hard backstop; a job's `job_set_region_limit` lowers it for its processes (PL8).
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

/// Syscall numbers (SYSCALLS.md, "Syscall numbers and registers"). One table (AB7): it makes the constants and `TABLE`, which `libsys` turns into one
/// `sys_<name>` wrapper per call (the name is the constant's, in lower case).
pub mod syscall {
    /// AB8: the number a call has in this build. Normally the number written in the table; when `ABI_SHUFFLE_SEED` is set in the environment at build time,
    /// every number below [`LIMIT`] is mapped by a bijection (`n * mult + add` modulo `LIMIT`, `mult` odd) chosen by the seed, so that code that
    /// writes a number of its own instead of using the table (or the library) fails the whole suite. Numbers at or above `LIMIT` are not calls and stay.
    #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i < bytes.len(); n < LIMIT = 128 and mult, add < 128, so the sum is small
    pub const fn number(n: usize) -> usize {
        match option_env!("ABI_SHUFFLE_SEED") {
            Some(seed) if n < LIMIT => {
                // FNV-1a of the seed text
                let bytes = seed.as_bytes();
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                let mut i = 0;
                while i < bytes.len() {
                    h ^= bytes[i] as u64;
                    h = h.wrapping_mul(0x0100_0000_01b3);
                    i += 1;
                }
                let mut mult = (((h >> 8) as usize) & (LIMIT - 1)) | 1;
                let add = (h as usize) & (LIMIT - 1);
                if mult == 1 && add == 0 {
                    mult = 3; // (never the identity: a seed means a shuffle)
                }
                (n * mult + add) % LIMIT
            }
            _ => n,
        }
    }

    macro_rules! syscalls {
        ($($(#[$m:meta])* $name:ident = $n:expr,)*) => {
            $($(#[$m])* pub const $name: usize = number($n);)*
            /// Every call: the constant's name and its number.
            pub const TABLE: &[(&str, usize)] = &[$((stringify!($name), number($n))),*];
        };
    }
    syscalls! {
        /// `debug_write(debug, ptr, len)`: needs a debug resource handle with WRITE (HD4).
        DEBUG_WRITE = 0,
        PROCESS_EXIT = 1,
        THREAD_YIELD = 2,
        CLOCK_GET = 3,
        /// A kernel counter by index (a measuring aid for the boot-time suite, like `DEBUG_WRITE`).
        /// `debug_stat(debug, which)`: needs a debug resource handle with INSPECT (HD4).
        DEBUG_STAT = 4,
        HANDLE_CLOSE = 10,
        HANDLE_DUPLICATE = 11,
        HANDLE_REPLACE = 12,
        CHANNEL_CREATE = 20,
        CHANNEL_WRITE = 21,
        CHANNEL_READ = 22,
        CHANNEL_READ_TXID = 23,
        CHANNEL_CALL = 24,
        EVENT_CREATE = 30,
        EVENT_SIGNAL = 31,
        OBJECT_SIGNALS = 32,
        OBJECT_WAIT = 33,
        OBJECT_INFO = 34,
        VMO_CREATE = 40,
        VMO_READ = 41,
        VMO_WRITE = 42,
        VMO_GET_SIZE = 43,
        VMO_CREATE_PHYSICAL = 44,
        VMO_CREATE_DMA = 45,
        VMO_PHYS_ADDR = 46,
        VMO_COMMIT = 47,
        /// Give back the committed pages of a range of a VMO (MM6): they read as zeros afterwards.
        VMO_DECOMMIT = 48,
        /// Change the size of a resizable VMO (MM6).
        VMO_SET_SIZE = 49,
        PROCESS_CREATE = 50,
        PROCESS_SELF = 51,
        PROCESS_MAP = 52,
        PROCESS_UNMAP = 53,
        PROCESS_START = 54,
        PROCESS_KILL = 55,
        PROCESS_INFO = 56,
        /// Stop every thread of a process until `PROCESS_RESUME` (SIG2): needs KILL.
        PROCESS_SUSPEND = 57,
        PROCESS_RESUME = 58,
        /// Change the permissions of a range of a process's mappings (MM7).
        PROCESS_PROTECT = 59,
        // 60 was `thread_create`: retired, not reused (AB5)
        THREAD_EXIT = 61,
        THREAD_INFO = 62,
        /// Reserved (T56): tell a thread to stop what it is blocked in. Validates its arguments, then
        /// answers NOT_SUPPORTED.
        THREAD_INTERRUPT = 63,
        /// Create and start a thread with a kernel-provisioned stack (SCHED-SMP-SPEC 5.3, task 9).
        THREAD_SPAWN = 64,
        /// Set a thread's base priority and flags (SCHED-SMP-SPEC 4.9, slice SC).
        THREAD_SET_SCHED = 65,
        THREAD_GET_SCHED = 66,
        /// Describe the region of a process at or after an address (MM7).
        PROCESS_MAP_INFO = 67,
        /// Make a copy-on-write child of a range of a VMO (MM8): SNAPSHOT or PRIVATE.
        VMO_CREATE_CHILD = 68,
        /// Map a region of one process into another, shared or as a SNAPSHOT child (MM8): the primitive a `fork` shim loops over.
        PROCESS_CLONE_REGION = 69,
        /// Raise a thread's suspend count: it stops at its next return to user mode (EX1).
        THREAD_SUSPEND = 74,
        /// Lower it; at 0 the thread runs again.
        THREAD_RESUME = 75,
        /// Read a stopped thread's registers.
        THREAD_READ_STATE = 76,
        /// Write a stopped thread's registers.
        THREAD_WRITE_STATE = 77,
        /// The user end of the channel exceptions of a process's threads are delivered on (EX2).
        PROCESS_EXCEPTION_CHANNEL = 78,
        /// The same for the processes of a job that have no handler of their own.
        JOB_EXCEPTION_CHANNEL = 79,
        /// Say what becomes of a thread that took an exception: RETRY, NEXT or KILL.
        EXCEPTION_RESUME = 97,
        /// `pager_create() -> (pager, channel)` (MM9).
        PAGER_CREATE = 103,
        /// `pager_create_vmo(pager, key, bytes) -> vmo`.
        PAGER_CREATE_VMO = 104,
        /// `pager_supply(pager, vmo, offset, length, source, source_offset)`.
        PAGER_SUPPLY = 105,
        /// `pager_fail(pager, vmo, offset, length, status)`.
        PAGER_FAIL = 106,
        /// Reserved for dirty tracking (MM9 names it, answers NOT_SUPPORTED).
        PAGER_QUERY_DIRTY = 107,
        /// Reserved for dirty tracking (MM9 names it, answers NOT_SUPPORTED).
        PAGER_WRITEBACK_DONE = 108,
        SOCKET_CREATE = 70,
        SOCKET_WRITE = 71,
        SOCKET_READ = 72,
        SOCKET_SHUTDOWN = 73,
        PORT_CREATE = 80,
        PORT_BIND = 81,
        PORT_UNBIND = 82,
        PORT_WAIT = 83,
        PORT_REARM = 84,
        /// Up to `PORT_PACKETS_MAX` ready bindings in one call (AB4).
        PORT_WAIT_MANY = 85,
        JOB_CREATE = 90,
        JOB_SET_LIMITS = 91,
        JOB_INFO = 92,
        /// Set a job's `max_prio`, `rt_budget` and `cpu_weight`.
        JOB_SET_SCHED = 96,
        /// Set a job's limit on the regions one address space of its processes may hold (PL8): `a0` job (SET_LIMITS), `a1` the limit.
        JOB_SET_REGION_LIMIT = 98,
        JOB_KILL = 93,
        JOB_ROOT = 94,
        RESOURCE_ROOT = 95,
        RESOURCE_DTB = 102,
        FUTEX_WAIT = 110,
        FUTEX_WAKE = 111,
        FUTEX_LOCK_PI = 112,
        FUTEX_UNLOCK_PI = 113,
        /// The debug resource (HD4): a privileged process (the one the kernel started) gets the handle that `debug_write` and `debug_stat` now need.
        DEBUG_ROOT = 114,
        /// Choose how a wake picks a hart (PM4): `a0` the root resource (MANAGE), `a1` the policy, 0 spread or 1 pack.
        SYSTEM_SET_PLACEMENT = 115,
        /// Take a hart out of the scheduler or bring it back (PM1): `a0` the root resource (MANAGE), `a1` the hart's index, `a2` 1 online or 0 offline.
        HART_SET_ONLINE = 116,
        /// A hart's state (PM1): `a0` the root resource (INSPECT), `a1` the index; returns 0 offline, 1 starting, 2 online, 3 draining.
        HART_INFO = 117,
        /// Suspend to RAM through the firmware (PM3): `a0` the root resource (MANAGE). NOT_SUPPORTED where it cannot be done.
        SYSTEM_SUSPEND = 118,
        INTERRUPT_CREATE = 100,
        INTERRUPT_ACK = 101,
        /// `iodomain_create(resource, device_id, job) -> domain` (IOMMU-SPEC 4.2).
        IODOMAIN_CREATE = 119,
        /// `iodomain_map(domain, vmo, vmo_offset, pages, access) -> io_address` (4.3).
        IODOMAIN_MAP = 120,
        /// `iodomain_unmap(domain, io_address, pages)` (4.4).
        IODOMAIN_UNMAP = 121,
        /// `iodomain_destroy(domain)` (4.5).
        IODOMAIN_DESTROY = 122,
        /// `iodomain_fault_read(domain) -> (cause | direction << 16 | dropped << 32, io_address)` (7.4; the packing is SYSCALLS.md's).
        IODOMAIN_FAULT_READ = 123,
        /// `job_set_io_limits(job, io_pages, vectors)` (5.3).
        JOB_SET_IO_LIMITS = 124,
        /// `vmo_create_dma_in(resource, pages, job) -> vmo` (4.7).
        VMO_CREATE_DMA_IN = 125,
        /// `iodomain_grant_vector(domain, index) -> (interrupt, msi_address)` (8.3; the data word is `iodomain::MSI_DATA`).
        IODOMAIN_GRANT_VECTOR = 126,
        /// `interrupt_deliver_here(interrupt)` (8.5): move the interrupt's delivery to the calling hart.
        INTERRUPT_DELIVER_HERE = 127,
    }
    /// One more than the largest number in the table.
    pub const LIMIT: usize = 128;
}

/// Pagers (MM9, KERNEL-V1-SPEC 4.4): the request message on a pager's channel.
pub mod pager {
    /// A request: five little-endian words (key, byte offset, byte length, kind, reserved 0).
    pub const REQUEST_BYTES: usize = 40;
    /// Read: supply these pages.
    pub const READ: u64 = 1;
    /// Reserved for dirty tracking: write these pages back (not produced).
    pub const WRITEBACK: u64 = 2;
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
    /// The stack arena: where `thread_spawn` maps the stacks it provisions and the loader a static-PIE's main stack (32 GiB nothing else is given).
    pub const STACK_ARENA_BASE: u64 = crate::arch::USER_STACK_ARENA_BASE;
    /// Where the arena that ANYWHERE mappings are placed in starts: above the runtime heap's range (`rt::heap`: 128 MiB from 0x8_0000_0000),
    /// which grows at fixed addresses and reserves nothing.
    pub const MAP_ARENA_BASE: u64 = 0x9_0000_0000;
    /// The end of the stack arena. Nothing the kernel places (an ANYWHERE mapping, a program's image, the system library, a stack) lands in
    /// `[STACK_ARENA_END, TOP)`: those addresses are only ever mapped where a program names them (T272).
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
    /// Flag: the thread exists but does not run until `thread_resume` (EX1): its registers can be written first.
    pub const SUSPENDED: u32 = 8;
}

/// Exceptions (EX2, KERNEL-V1-SPEC 3.6): the message the kernel writes to an exception channel and what a handler may do about it.
pub mod exception {
    /// Bytes of the message: five little-endian words, `kind` (`abi::arch::fault`), `tid`, `pc`, `address`, `cause` (the architecture's raw cause: on
    /// RISC-V 12, 13 and 15 for a page fault on an instruction fetch, a load and a store). Two handles come with it: the thread (INSPECT,
    /// MANAGE, WAIT) and the exception.
    pub const MESSAGE_BYTES: usize = 40;
    /// Handles in the message.
    pub const MESSAGE_HANDLES: usize = 2;
    /// `exception_resume`: the thread goes on from its state (which the handler may have changed).
    pub const RETRY: u64 = 1;
    /// Offer the exception to the next level (the job, then each ancestor job); with none left the process ends as it always did.
    pub const NEXT: u64 = 2;
    /// End the process with the usual fault exit code.
    pub const KILL: u64 = 3;
}

/// `thread_read_state` / `thread_write_state` (EX1): the kinds and, for RISC-V, the layouts (per architecture: `abi::arch`).
pub mod thread_state {
    /// x1 to x31, then pc: 32 words.
    pub const GENERAL: u64 = 1;
    /// f0 to f31, then fcsr: 33 words.
    pub const FP: u64 = 2;
    /// vtype, vl, vstart, vcsr, then v0 to v31 of `vlenb` bytes each.
    pub const VECTOR: u64 = 3;
    /// Reserved (HD5): the shadow-stack pointer and the landing-pad expectation. Refused with NOT_SUPPORTED.
    pub const CFI: u64 = 4;
    /// Words of the GENERAL layout.
    pub const GENERAL_WORDS: usize = 32;
    /// Words of the FP layout.
    pub const FP_WORDS: usize = 33;
    /// Words before the vector registers in the VECTOR layout.
    pub const VECTOR_HEADER_WORDS: usize = 4;
}

/// The kernel's address-space layout (Sv39, kernel in the upper half).
pub mod layout {
    /// The virtual address the kernel image is linked at (the start of its text). Where it sits in physical memory is the loader's choice,
    /// found at boot (`arch::kernel_delta`: virtual minus physical).
    pub const KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8020_0000;
    /// Start of the direct map: physical address `pa` is at `DIRECT_BASE + pa`
    /// (RAM outside the kernel image, plus the device pages the kernel uses). This is the **first** slot's address: the kernel moves the map up by a random
    /// 0 to 64 GiB at boot (HD3, `kmem::direct_base`).
    pub const DIRECT_BASE: u64 = 0xffff_ffc0_0000_0000;
    /// The direct map reaches physical addresses below this (64 GiB: 64 top-level slots from 256 + k, k = 0 to 64 chosen at boot, HD3); the slots above
    /// 384 belong to other kernel windows (the stacks at 384, the heap at 416, the self-test region at 448, the image at 510). RAM above it is not used.
    pub const DIRECT_MAP_PHYS_LIMIT: u64 = 64 << 30;
    /// First virtual address of the region the self-tests use for lazily
    /// committed memory (an otherwise empty top-level slot in the upper half).
    pub const LAZY_TEST_BASE: u64 = 0xffff_fff0_0000_0000;
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

    /// No two calls share a number (a collision made `job_set_region_limit` and `exception_resume` the same call, PL8).
    #[test]
    fn syscall_numbers_are_unique() {
        let t = super::syscall::TABLE;
        for (i, &(name, n)) in t.iter().enumerate() {
            for &(other, m) in &t[..i] {
                assert!(n != m, "{name} and {other} share the number {n}");
            }
        }
    }

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
        assert_eq!(slot(LAZY_TEST_BASE), 448);
        // the direct map's span, moved up by the most (64 GiB: `boot.rs` chooses k = 0 to 64), still ends where the stack window begins
        assert_eq!(
            DIRECT_BASE + (64 << 30) + DIRECT_MAP_PHYS_LIMIT,
            KSTACK_WINDOW_BASE
        );
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

#[cfg(test)]
mod syscall_table_tests {
    use super::syscall::*;
    #[test]
    fn table_is_complete_and_unique() {
        for (i, &(name, n)) in TABLE.iter().enumerate() {
            assert!(n < LIMIT, "{name} = {n} is not below LIMIT");
            assert!(
                TABLE[..i].iter().all(|&(_, m)| m != n),
                "number {n} used twice"
            );
        }
        assert!(TABLE.contains(&("DEBUG_WRITE", DEBUG_WRITE)));
        assert!(TABLE.len() >= 80);
        // the numbers that are not calls stay not calls, shuffled or not
        assert!(TABLE
            .iter()
            .all(|&(_, n)| n != number(5) && n != number(60)));
    }
}
