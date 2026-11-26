//! Pure kernel logic for the OS track. Everything here is `no_std` (plus
//! `alloc`), forbids `unsafe`, and builds on the host, where it is tested
//! thoroughly (model-based tests against reference models, loom and shuttle for
//! interleavings, Miri). The hardware-facing kernel binary only wires these
//! pieces to the machine.
#![no_std]
#![forbid(unsafe_code)]
// KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause. The compiler finds the candidates; each remaining site carries an
// `#[allow] // INVARIANT: ...` naming why it cannot fire (`scripts/kernel_allows.py` lists them). Unit tests are exempt.
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

pub mod aiatree;
pub mod asid;
pub mod aspace;
pub mod buddy;
pub mod chaos;
pub mod devwin;
pub mod donate;
pub mod elf;
pub mod fbox;
pub mod fcache;
pub mod fdt;
pub mod fmap;
pub mod fp;
pub mod frame;
pub mod futex;
pub mod handle;
pub mod harts;
pub mod iodomain;
pub mod iommutree;
pub mod ipc;
pub mod isa;
pub mod job;
pub mod jobsched;
pub mod kaslr;
pub mod latch;
pub mod limits;
pub mod linestate;
pub mod memmap;
pub mod msiid;
pub mod object;
pub mod pagemap;
pub mod pager;
pub mod pagetable;
pub mod park;
pub mod pifutex;
pub mod powerseq;
pub mod sched;
pub mod sched2;
pub mod sched_smp;
pub mod slab;
pub mod suspend;
pub mod sync;
pub mod testlog;
pub mod ticket;
pub mod timebase;
pub mod usercopy;
pub mod wait;

use frame::FrameDb;
use pagetable::PhysMem;

/// Physical memory management: the frame database and access to frame contents.
/// On the host `M` is a fake memory; in the kernel it is the direct map.
pub struct Mm<M: PhysMem> {
    pub db: FrameDb,
    pub mem: M,
}
