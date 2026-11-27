//! The backstops (PL8, KERNEL-V1-SPEC 2.5): the system-wide ceilings that protect kernel memory, derived from the RAM the machine has. They sit
//! behind the job quotas (which are what a program meets first); a machine with more memory has higher backstops, and none of them reserves memory
//! up front.

use abi::limits::{
    MAX_DMA_TOTAL_PAGES, MAX_IPC_BYTES, MAX_OBJECTS, MAX_PROCESSES, MAX_USER_THREADS,
};

/// The address spaces one hart may hold at once under the lock-order checker (`klock::Class::As`): the calls that look at every process
/// (`AsLocks::all`) take them all, so the process backstop may not pass it (T214 replaces those calls; then the figure of the table applies).
pub const AS_HELD_MOST: usize = 64;

/// A page, for the DMA figure.
const PAGE: u64 = 4096;

/// The system-wide ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backstops {
    /// Live kernel objects: RAM / 4096.
    pub objects: usize,
    /// Bytes queued in all channels and sockets: RAM / 32.
    pub ipc_bytes: usize,
    /// Live processes: RAM / 1 MiB, at most 2^20 (until the calls that take every process's address space at once are gone, T214, at most `AS_HELD_MOST`).
    pub processes: usize,
    /// Live user threads: RAM / 64 KiB, at most the slots of the kernel-stack window.
    pub user_threads: usize,
    /// Pages held by DMA VMOs: RAM / 16.
    pub dma_total_pages: u64,
}

impl Backstops {
    /// What the pure tests (and a kernel that has not read its memory map yet) use: the figures the constants of `abi::limits` have always had.
    pub const DEFAULT: Backstops = Backstops {
        objects: MAX_OBJECTS,
        ipc_bytes: MAX_IPC_BYTES,
        processes: MAX_PROCESSES,
        user_threads: MAX_USER_THREADS,
        dma_total_pages: MAX_DMA_TOTAL_PAGES,
    };

    /// The backstops of a machine with `ram` bytes of memory whose kernel-stack window has `stack_slots` slots (the table of KERNEL-V1-SPEC 2.5).
    /// Every figure is at least the old constant's value, so a small machine is not worse off than before.
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: divisions by constants; the casts are of values far below the target's usize
    pub fn for_ram(ram: u64, stack_slots: usize) -> Backstops {
        let d = Backstops::DEFAULT;
        let objects = (ram / 4096) as usize;
        let ipc_bytes = (ram / 32) as usize;
        let processes = ((ram >> 20) as usize).min(1 << 20).min(AS_HELD_MOST);
        let user_threads = ((ram >> 16) as usize).min(stack_slots);
        let dma_total_pages = ram / 16 / PAGE;
        Backstops {
            objects: objects.max(d.objects),
            ipc_bytes: ipc_bytes.max(d.ipc_bytes),
            processes: processes.max(d.processes),
            user_threads: user_threads.max(d.user_threads.min(stack_slots)),
            dma_total_pages: dma_total_pages.max(d.dma_total_pages),
        }
    }
}

/// The device backstops (IOMMU-SPEC 5.3) behind the two job limits of `kcore::job::IoResource`, which have no default: whatever a job's
/// limits are, a domain maps at most the DMA part of its window (`kcore::iodomain::IO_DMA_PAGES` pages, bounded again by the job's
/// kernel bytes, which pay for the mapping records and the tables), and a device has at most this many vectors (one MSI page-table
/// entry per page of the interrupt window). The pages DMA VMOs hold together stay behind `Backstops::dma_total_pages`.
pub const VECTORS_PER_DEVICE: u64 = 256;
