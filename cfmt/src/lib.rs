//! The C library's formatting and parsing cores (S7c): the `printf` family's engine, `strtod`,
//! `strtol` and `strtoul`. Pure functions over bytes (`no_std` + `alloc`, `forbid(unsafe_code)`);
//! the C-ABI layer in `libc/` feeds them from `va_list`s and C strings. They are tested on the host
//! by differential comparison against the host's own libc (`tests/differential.rs`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod parse;
mod printf;

pub use parse::{strtod, strtol, strtoul, ParsedFloat, ParsedInt, ParsedUInt};
pub use printf::{format, Args, FmtError, Len};
