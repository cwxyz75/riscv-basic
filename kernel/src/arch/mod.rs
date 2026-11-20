//! The architecture boundary (KERNEL-V1-SPEC 2.6, OD143): the selected architecture's code, re-exported as `arch::*`.
//! Code outside this directory names no register, control-register, instruction or firmware call. RISC-V is the only
//! implementation; a second would be a sibling directory with the same items.

pub mod fault;
pub use fault::{Fault, FaultKind, UserTrap};

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

/// The AArch64 stub: every interface item present, every body `unimplemented!()`. It carries no behaviour and is never booted;
/// it exists so that `cargo check --target aarch64-unknown-none` proves the boundary is complete.
#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

// The interface the rest of the kernel uses (AR1b, KERNEL-V1-SPEC 2.6), by file under `riscv64/`:
//   cpu.rs      aspace_token, aspace_token_asid, aspace_current, aspace_install, tlb_flush_all, tlb_flush_page, cpu_idle,
//               thread_pointer, set_thread_pointer, lazy_units_off, lazy_units_are_off
//   switch.rs   Context (new), switch
//   usermode.rs enter_user
//   trap.rs     TrapFrame (arg, set_return, decode_user, skip_trap_instruction), Fault, FaultKind, UserTrap
//   sbi.rs      firmware calls and the secondary harts' entry
//   boot, timer, irq, fp, aia, finisher: the boot path, the one-shot timer, interrupt masking, the lazily switched FP unit,
//               the interrupt controller and the test finisher.
// Every later item that needs architecture code adds its item here and to the AArch64 stub (AR1d) in the same PR.
