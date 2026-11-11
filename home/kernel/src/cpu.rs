//! Thin wrappers over the handful of RISC-V CSRs the kernel touches. Each
//! `unsafe` block is a single CSR instruction with no memory effects.

use core::arch::asm;

const SSTATUS_SIE: usize = 1 << 1;
const SSTATUS_FS_MASK: usize = 0b11 << 13;
const SSTATUS_FS_INITIAL: usize = 0b01 << 13;
const SIE_STIE: usize = 1 << 5;

/// The `time` counter (ticks at `timer::TIMEBASE_HZ`).
pub fn rdtime() -> u64 {
    let value: u64;
    // SAFETY: reading the `time` CSR has no side effects; OpenSBI lets
    // S-mode read it.
    unsafe { asm!("rdtime {}", out(reg) value, options(nomem, nostack)) };
    value
}

/// Turn the floating-point unit on (`sstatus.FS` = Initial). Until this is
/// done, any floating-point instruction raises an illegal-instruction trap.
pub fn enable_fpu() {
    // SAFETY: only changes the FS field of `sstatus`, enabling the F/D units
    // that the riscv64gc target already assumes exist.
    unsafe {
        asm!(
            "csrc sstatus, {mask}",
            "csrs sstatus, {initial}",
            mask = in(reg) SSTATUS_FS_MASK,
            initial = in(reg) SSTATUS_FS_INITIAL,
            options(nomem, nostack),
        );
    }
}

/// Unmask supervisor timer interrupts (`sie.STIE`).
pub fn enable_timer_interrupt() {
    // SAFETY: sets one interrupt-enable bit; a handler is installed first.
    unsafe { asm!("csrs sie, {}", in(reg) SIE_STIE, options(nomem, nostack)) };
}

/// Allow interrupts to be taken in S-mode (`sstatus.SIE`).
pub fn enable_interrupts() {
    // SAFETY: sets the global interrupt-enable bit; callers install the trap
    // vector before calling this.
    unsafe { asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack)) };
}

pub fn read_scause() -> usize {
    let value: usize;
    // SAFETY: reading a CSR has no side effects.
    unsafe { asm!("csrr {}, scause", out(reg) value, options(nomem, nostack)) };
    value
}

pub fn read_sepc() -> usize {
    let value: usize;
    // SAFETY: reading a CSR has no side effects.
    unsafe { asm!("csrr {}, sepc", out(reg) value, options(nomem, nostack)) };
    value
}

pub fn read_stval() -> usize {
    let value: usize;
    // SAFETY: reading a CSR has no side effects.
    unsafe { asm!("csrr {}, stval", out(reg) value, options(nomem, nostack)) };
    value
}

/// Install `handler` as the trap vector (direct mode; must be 4-byte aligned).
pub fn write_stvec(handler: usize) {
    // SAFETY: points `stvec` at a function that saves state and returns with
    // `sret`; callers pass the address of `trap_entry`.
    unsafe { asm!("csrw stvec, {}", in(reg) handler, options(nomem, nostack)) };
}
