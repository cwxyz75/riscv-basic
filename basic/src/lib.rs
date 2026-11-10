//! The BASIC interpreter core for riscv-basic.
//!
//! This crate is `no_std` (plus `alloc`) and contains **no hardware knowledge
//! and no `unsafe`**. Everything it needs from the machine arrives through the
//! [`Io`] trait, so the same code runs under the kernel (UART console) and
//! under `cargo test` on the host (scripted console). See `DESIGN.md` and
//! `BASIC.md` at the repository root.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod interp;
mod io;

pub use interp::Interpreter;
pub use io::{Io, Line};
