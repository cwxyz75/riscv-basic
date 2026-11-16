//! The format of the doofs disk filesystem (FS-SPEC section 3), as pure code over a block-device trait: 4 KiB blocks,
//! 32-byte **pointers** that carry the address, the generation the block was born in and a 16-byte **check field** (CRC-32C
//! in v1), and a **copy-on-write B+tree** of byte-string keys and values committed by generation. Nothing here does I/O of
//! its own; the in-memory [`MemDevice`] is the host's device and the crash harness (step 4) will wrap it.
//!
//! Step 1 of FS-SPEC section 8 builds the tree alone: no superblock, no files, no free-map rebuild. It is tested against a
//! `BTreeMap` model, with snapshots (a committed root stays readable, and none of its blocks is overwritten, while it is
//! retained: FI2) and mutation checks.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod crc;
pub mod device;
pub mod node;
pub mod pool;
pub mod superblock;
pub mod tree;
pub mod volume;

pub use device::{DevError, Device, MemDevice, BLOCK};
pub use pool::Pool;
pub use superblock::Superblock;
pub use tree::{Snapshot, Tree, TreeError, Verified};
pub use volume::{check, mkfs, mount, Report, VolError, Volume};

/// Longest key and longest value the tree stores (FS-SPEC section 12: names up to 255 bytes, inline values up to 1 KiB).
pub const MAX_KEY: usize = 300;
pub const MAX_VALUE: usize = 1100;
