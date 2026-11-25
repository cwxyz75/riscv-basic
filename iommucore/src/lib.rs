//! The RISC-V IOMMU's data structures as pure code (IOMMU2; IOMMU-SPEC sections 2 to 8). This crate owns every bit layout the
//! kernel writes for the IOMMU: the register fields, the device directory and its contexts, the I/O page tables, the command and
//! fault queues, the MSI page table and the memory-resident interrupt files, and the I/O address allocator. The kernel owns every
//! volatile access: it reaches memory and registers only through the three narrow traits below.
//!
//! The formats are the ratified RISC-V IOMMU specification's (version 1.0), as QEMU 11.1.3 implements them
//! (`hw/riscv/riscv-iommu-bits.h`; the facts record `docs/records/notes/2026-11-25-iommu-qemu-facts.md`). The forbidden states
//! of IOMMU-SPEC 2.4 (IO2, IO11) have no constructor here: `ddtp` cannot be encoded Bare, a device context cannot have both
//! stages Bare, and an MSI page-table entry cannot be in basic-translate mode, except behind the negative-control features
//! `fault-iommu-bare` and `fault-msi-basic`, which no real build enables.
//!
//! `no_std` plus `alloc` (the I/O address allocator's bitmaps, sized once at creation, and the simulator), no `unsafe`, and no
//! dependency on `kcore`, so its tests and its mutation run are small.
#![no_std]
#![forbid(unsafe_code)]
// KT3 (KERNEL-V1-SPEC 7.4): this code runs in the kernel, so no panic a device or a program can cause. Each remaining site carries
// an `#[allow] // INVARIANT: ...` naming why it cannot fire. Unit tests are exempt.
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
)]

extern crate alloc;

pub mod cmdq;
pub mod ddt;
pub mod faultq;
pub mod iopt;
pub mod iova;
pub mod msi;
pub mod regs;
#[cfg(any(test, feature = "sim"))]
pub mod sim;

/// The page size of every structure here: 4 KiB.
pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

/// The field that holds a physical page number in a directory entry, a page-table entry, `ddtp` and the queue bases: bits 53:10.
pub(crate) const PPN_SHIFT: u32 = 10;
pub(crate) const PPN_MASK: u64 = (1 << 44) - 1;

/// Physical memory, by physical address, one aligned 64-bit word at a time. The kernel's implementation is the direct map; the
/// tests' is [`sim::FakeMem`]. The IOMMU reads these words concurrently with the kernel's writes, and writes the memory-resident
/// interrupt files itself, so the kernel's implementation uses single-copy-atomic accesses; `fetch_or` and `fetch_and` are atomic
/// read-modify-writes (the interrupt files are changed from any hart, without a lock: IOMMU-SPEC 8.5).
pub trait PhysMem64 {
    fn read(&self, pa: u64) -> u64;
    fn write(&self, pa: u64, value: u64);
    /// Atomically `*pa |= bits`; returns the old value.
    fn fetch_or(&self, pa: u64, bits: u64) -> u64;
    /// Atomically `*pa &= bits`; returns the old value.
    fn fetch_and(&self, pa: u64, bits: u64) -> u64;
}

/// The frames of directory pages and page tables. `alloc` gives one zeroed 4 KiB frame (its physical address) with its charge
/// already taken, or `None`; `free` takes one back.
///
/// A frame given to `free` may still be in the IOMMU's caches until the invalidation and fence that follow the change that
/// emptied it (IOMMU-SPEC 6.1 step 3: "table frames the unmap emptied are detached"): the kernel's implementation keeps such a
/// frame off its free lists until the fence has completed. Removal (`unmap`, `destroy`, `clear`) never calls `alloc`.
pub trait TableFrames {
    fn alloc(&mut self) -> Option<u64>;
    fn free(&mut self, pa: u64);
}

/// The IOMMU's registers, by byte offset from the register page ([`regs`] has the offsets). MMIO in the kernel; [`sim::Sim`] in the
/// tests. Reads take `&mut self` because a register read can have an effect (a busy bit that clears).
pub trait Regs {
    fn read32(&mut self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    fn read64(&mut self, offset: usize) -> u64;
    fn write64(&mut self, offset: usize, value: u64);
}

/// The physical page number field of an entry (bits 53:10) for a page-aligned address.
pub(crate) const fn ppn_field(pa: u64) -> u64 {
    ((pa >> PAGE_SHIFT) & PPN_MASK) << PPN_SHIFT
}

/// The page-aligned address held in an entry's physical page number field (bits 53:10).
pub(crate) const fn ppn_addr(entry: u64) -> u64 {
    ((entry >> PPN_SHIFT) & PPN_MASK) << PAGE_SHIFT
}
