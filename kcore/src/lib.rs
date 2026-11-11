//! Pure kernel logic for the OS track. Everything here is `no_std`, forbids
//! `unsafe`, and builds on the host, where it is tested thoroughly (property and
//! model tests, loom and shuttle for interleavings, Miri). The hardware-facing
//! kernel binary only wires these pieces to the machine.
#![no_std]
#![forbid(unsafe_code)]

pub mod latch;
pub mod sync;
pub mod testlog;
