//! Console logic for riscv-basic that needs no hardware: the line editor (echo,
//! backspace, Ctrl-U, Ctrl-C) and ANSI escape-sequence helpers.
//!
//! The kernel feeds bytes from the UART into [`LineEditor`] and writes whatever
//! it asks to be echoed back to the UART; keeping the logic here means it is
//! unit-tested on the host. See `DESIGN.md` (Console model).

#![no_std]
#![forbid(unsafe_code)]

pub mod ansi;
mod editor;

pub use editor::{Event, LineEditor, MAX_LEN};
