//! The atomics everything in this crate uses. Normally `core`'s; under
//! `--cfg loom` or `--cfg shuttle` the model checker's, so the same code can be
//! explored over all (or many random) interleavings on the host.

#[cfg(not(any(loom, shuttle)))]
pub use core::sync::atomic::{fence, AtomicU32, Ordering};
#[cfg(loom)]
pub use loom::sync::atomic::{fence, AtomicU32, Ordering};
#[cfg(shuttle)]
pub use shuttle::sync::atomic::{fence, AtomicU32, Ordering};
