//! Processor-level operations the rest of the kernel needs, as plain functions: TLB maintenance, address-space
//! installation, idle, and the per-hart pointer (AR1, KERNEL-V1-SPEC 2.6). An address-space token is what the
//! hardware register holds for one root (on RISC-V, the `satp` value); outside `arch` it is an opaque `u64`.

use core::arch::asm;

/// The token for the address space whose top-level table is at physical address `root` (Sv39).
#[inline(always)]
pub fn aspace_token(root: u64) -> u64 {
    (8u64 << 60) | root
}

/// The token currently installed on this hart.
#[inline(always)]
pub fn aspace_current() -> u64 {
    let v: u64;
    // SAFETY: reads a CSR.
    unsafe { asm!("csrr {}, satp", out(reg) v) };
    v
}

/// Install `token` on this hart and drop the old translations.
///
/// # Safety
/// `token` names a live top-level table that maps everything the kernel executes from and touches.
#[inline(always)]
pub unsafe fn aspace_install(token: u64) {
    // SAFETY: the caller guarantees the table; the flush drops the translations of the table being left.
    unsafe { asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) token) };
}

/// Flush the whole TLB of this hart.
#[inline(always)]
pub fn tlb_flush_all() {
    // SAFETY: `sfence.vma` with no operands flushes all translations; no other effect.
    unsafe { asm!("sfence.vma zero, zero") };
}

/// Flush the translations of the page at `va` in address space `asid` on this hart.
#[inline(always)]
pub fn tlb_flush_page(va: u64, asid: u16) {
    // SAFETY: `sfence.vma` with an address and an ASID drops cached translations of that page; no other effect.
    unsafe { asm!("sfence.vma {}, {}", in(reg) va, in(reg) u64::from(asid)) };
}

/// Wait for an interrupt (returns when one is pending, even with interrupts masked).
#[inline(always)]
pub fn cpu_idle() {
    // SAFETY: `wfi` only waits.
    unsafe { asm!("wfi") };
}

/// The per-hart pointer register's value.
#[inline(always)]
pub fn thread_pointer() -> usize {
    let p: usize;
    // SAFETY: reads `tp`; no memory is touched.
    unsafe { asm!("mv {}, tp", out(reg) p, options(nomem, nostack, preserves_flags)) };
    p
}

/// Set the per-hart pointer register.
///
/// # Safety
/// Kernel code reserves `tp` for the per-hart record; `p` must be that record's address.
#[inline(always)]
pub unsafe fn set_thread_pointer(p: usize) {
    // SAFETY: sets `tp`, which kernel code reserves for this purpose (LLVM never allocates it).
    unsafe { asm!("mv tp, {}", in(reg) p, options(nomem, nostack, preserves_flags)) };
}

/// The token for the address space `asid` whose top-level table is at physical address `root`.
#[inline(always)]
pub fn aspace_token_asid(root: u64, asid: u16) -> u64 {
    (8u64 << 60) | (u64::from(asid) << 44) | root
}

/// Turn the FP and vector units off on this hart, so any use traps (a thread starts with them off).
///
/// # Safety
/// Boot code only, before any user thread has run on this hart: the state of a unit left on is discarded behind the FP protocol (`fp.rs`).
#[inline(always)]
pub unsafe fn lazy_units_off() {
    // SAFETY: clears sstatus.FS and sstatus.VS only.
    unsafe { asm!("csrc sstatus, {}", in(reg) (3usize << 13) | (3usize << 9)) };
}

/// True when the FP and vector units are both off on this hart.
#[inline(always)]
pub fn lazy_units_are_off() -> bool {
    let v: usize;
    // SAFETY: reads a CSR.
    unsafe { asm!("csrr {}, sstatus", out(reg) v) };
    v >> 13 & 3 == 0 && v >> 9 & 3 == 0
}

/// Enable the software interrupt (the inter-hart interrupt) on this hart.
#[inline(always)]
pub fn software_interrupt_enable() {
    // SAFETY: sets sie.SSIE only (the supervisor software interrupt, used for IPIs).
    unsafe { asm!("csrs sie, {}", in(reg) 1usize << 1) };
}

/// Clear this hart's pending software interrupt.
#[inline(always)]
pub fn software_interrupt_clear() {
    // SAFETY: clears sip.SSIP, the pending bit of the software interrupt; nothing else.
    unsafe { asm!("csrc sip, {}", in(reg) 1usize << 1) };
}
