//! Entering user mode for the first time (the assembly of a new thread's first `sret`).

core::arch::global_asm!(
    r#"
    .section .text
    .globl enter_user
    .balign 4
enter_user:                      # a0 entry, a1 user sp, a2 arg, a3 address-space token, a4 kernel stack top, a5 tp, a6 a1 for the thread
    csrci sstatus, 2             # interrupts off until we are in U-mode
    csrw  satp, a3
    sfence.vma zero, zero
    csrw  sepc, a0
    sd    a4, 0(tp)              # PerHart.kstack_top: where the next trap from user mode lands
    csrw  sscratch, tp           # from now on a trap is a trap from user mode
    li    t0, 256
    csrc  sstatus, t0            # SPP = 0: sret goes to U-mode
    li    t0, 32
    csrs  sstatus, t0            # SPIE = 1: interrupts on in U-mode
    li    t0, 0x6600
    csrc  sstatus, t0            # FP and vector units off: a new thread has no FP state
    mv    sp, a1
    mv    a0, a2
    li x1, 0
    li x3, 0
    mv x4, a5
    li x5, 0
    li x6, 0
    li x7, 0
    li x8, 0
    li x9, 0
    mv x11, a6
    li x12, 0
    li x13, 0
    li x14, 0
    li x15, 0
    li x16, 0
    li x17, 0
    li x18, 0
    li x19, 0
    li x20, 0
    li x21, 0
    li x22, 0
    li x23, 0
    li x24, 0
    li x25, 0
    li x26, 0
    li x27, 0
    li x28, 0
    li x29, 0
    li x30, 0
    li x31, 0
    sret
"#
);

extern "C" {
    /// Switch to the address space `token`, make `kernel_top` the trap stack of this hart, and drop to U-mode at `entry`
    /// with stack `sp`, first argument `arg` and `a1` in the second argument register; `tp` is the user thread pointer.
    /// Interrupts are off until the drop; nothing returns.
    ///
    /// # Safety
    /// `token` names a live process root whose upper half is the kernel's; the caller holds no lock.
    #[link_name = "enter_user"]
    pub fn enter_user_raw(
        entry: u64,
        sp: u64,
        arg: u64,
        token: u64,
        kernel_top: u64,
        tp: u64,
        a1: u64,
    ) -> !;
}
