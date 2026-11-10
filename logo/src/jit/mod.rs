//! Compiling the IR to RV64 and running it (see "Compilation to RV64" in
//! `LOGO.md`). This module is safe code: it produces machine code bytes and
//! services the calls that code makes. *Running* the bytes is the job of an
//! [`ExecuteFn`] supplied from outside: the kernel's, which calls the code on
//! the real machine, or [`emulated_execute`], which uses the `rv64` emulator.

mod boxing;
mod codegen;
mod emulated;
mod runtime;

pub(crate) use codegen::{compile, Compiled};
pub use emulated::emulated_execute;
pub(crate) use runtime::run_unit;

/// A service result: a value (or count, or truth) and an error flag, returned
/// to generated code in `a0` and `a1`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ret {
    pub value: u64,
    pub err: u64,
}

/// Runs generated machine code. `code` is a function taking `ctx` and returning
/// a status; `frame_bytes` is the stack it needs; `host` services the runtime
/// calls the code makes (through the table in `ctx`). Returns the status.
pub type ExecuteFn = fn(code: &[u8], frame_bytes: usize, host: &mut dyn JitHost) -> u64;

/// The layout shared by the code generator and whoever runs its output.
pub mod abi {
    /// Service numbers: generated code calls the address in the table entry
    /// `ctx + 8 * service`, passing `ctx` as the first argument.
    pub const SVC_VAR: usize = 0;
    pub const SVC_CONST: usize = 1;
    pub const SVC_NEG: usize = 2;
    pub const SVC_INFIX: usize = 3;
    pub const SVC_CALL: usize = 4;
    pub const SVC_DISCARD: usize = 5;
    pub const SVC_REPEAT_COUNT: usize = 6;
    pub const SVC_TRUTH: usize = 7;
    pub const SVC_FAIL: usize = 8;
    /// How many services there are (the table's length).
    pub const SERVICE_COUNT: usize = 9;
}

/// The runtime services generated code calls. Each takes the register
/// arguments of the call (after `ctx`) and returns a [`Ret`].
pub trait JitHost {
    fn var(&mut self, name: u64) -> Ret;
    fn constant(&mut self, index: u64) -> Ret;
    fn neg(&mut self, value: u64) -> Ret;
    fn infix(&mut self, op: u64, a: u64, b: u64) -> Ret;
    /// `args` are the boxed values of the call's inputs.
    fn call(&mut self, name: u64, args: &[u64]) -> Ret;
    fn discard(&mut self, value: u64) -> Ret;
    fn repeat_count(&mut self, value: u64) -> Ret;
    fn truth(&mut self, value: u64, name: u64) -> Ret;
    fn fail(&mut self, index: u64) -> Ret;
}
