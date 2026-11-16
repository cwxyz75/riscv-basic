//! `ish`: the native shell (SHELL-SPEC; steps SH2 and SH3). A parser and an evaluator over a [`Host`] trait; everything the
//! machine does (processes, files, pipes) goes through the trait, so the whole language is tested on the host.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod ast;
pub mod builtins;
pub mod eval;
pub mod fake;
pub mod host;
pub mod parser;
pub mod value;

pub use parser::{parse, ParseError};

pub use eval::{Mode, Shell};
pub use host::Host;
pub use value::Val;
