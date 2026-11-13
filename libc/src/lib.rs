//! The C library (S7c-2, S7-SPEC section 5): C-ABI functions written in Rust over the native runtime.
//! The pure cores live in `cfmt` (and are tested against the host's libc there); this layer is thin
//! glue whose `unsafe` blocks follow the C contract: a pointer argument is valid for what the
//! function's documentation (the C standard) says, and a violation is the caller's undefined
//! behaviour, as in any C library.
//!
//! `no_builtins` is required: otherwise LLVM turns the loops that implement `strlen` or `memchr`
//! back into calls to themselves. `mem*` come from `compiler_builtins`.
#![no_std]
#![no_builtins]
#![allow(
    clippy::missing_safety_doc,
    non_camel_case_types,
    non_upper_case_globals
)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// `c_char` is `u8` on RISC-V: the casts document the C semantics (and keep the code portable).
#![allow(clippy::unnecessary_cast)]

extern crate alloc;

mod ctype;
mod errno;
mod fd;
mod file;
mod math;
mod misc;
mod setjmp;
mod start;
mod stdio;
mod stdlib;
mod string;
mod thread;
mod time;

pub use start::run_main;
