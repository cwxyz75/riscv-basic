//! The architecture boundary (KERNEL-V1-SPEC 2.6, OD143): the selected architecture's code, re-exported as `arch::*`.
//! Code outside this directory names no register, control-register, instruction or firmware call. RISC-V is the only
//! implementation; a second would be a sibling directory with the same items.

pub mod riscv64;
pub use riscv64::*;
