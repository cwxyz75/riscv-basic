//! The LOGO language for riscv-basic: reader, parser and reference interpreter,
//! specified in `LOGO.md`. Pure: no hardware, no `unsafe`. The machine reaches
//! the interpreter through [`Io`] (text, time, reboot) and the `turtle` crate's
//! `Canvas` (the screen).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod error;
mod exec;
mod interp;
mod ir;
mod jit;
mod lex;
mod parse;
mod prims;
mod rng;
mod value;

pub use interp::{Engine, Interpreter};
pub use jit::{abi, emulated_execute, ExecuteFn, JitHost, Ret};

use alloc::string::String;

/// What the console handed back when the interpreter asked for a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A finished line of text, without its terminator.
    Text(String),
    /// The user pressed BREAK (Ctrl-C) while the line was being typed.
    Break,
    /// No more input (scripted tests); never happens on the real machine.
    Eof,
}

/// Everything the interpreter needs from the outside world besides the screen.
pub trait Io {
    /// Write text to the console.
    fn write_str(&mut self, s: &str);
    /// Block until a full line is available.
    fn read_line(&mut self) -> Line;
    /// Non-blocking: has Ctrl-C been pressed since the last call?
    fn break_pressed(&mut self) -> bool;
    /// Milliseconds since boot (for `wait`).
    fn millis(&mut self) -> u64;
    /// Reboot the machine (`bye`); on real hardware this does not return.
    fn reboot(&mut self);
    /// The disk behind `save`, `load`, `dir` and `erasefile`; `None` if the
    /// machine has none.
    fn files(&mut self) -> Option<&mut dyn cartfs::Files> {
        None
    }
}
