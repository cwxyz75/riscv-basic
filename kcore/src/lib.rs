//! Pure kernel logic for the OS track. Everything here is `no_std` (plus
//! `alloc`), forbids `unsafe`, and builds on the host, where it is tested
//! thoroughly (model-based tests against reference models, loom and shuttle for
//! interleavings, Miri). The hardware-facing kernel binary only wires these
//! pieces to the machine.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod aspace;
pub mod elf;
pub mod fdt;
pub mod frame;
pub mod futex;
pub mod handle;
pub mod ipc;
pub mod job;
pub mod latch;
pub mod linestate;
pub mod object;
pub mod pagetable;
pub mod sched;
pub mod sync;
pub mod testlog;
pub mod ticket;
pub mod usercopy;

use frame::FrameDb;
use pagetable::PhysMem;

/// Physical memory management: the frame database and access to frame contents.
/// On the host `M` is a fake memory; in the kernel it is the direct map.
pub struct Mm<M: PhysMem> {
    pub db: FrameDb,
    pub mem: M,
}
