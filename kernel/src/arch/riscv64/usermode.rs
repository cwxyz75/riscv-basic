//! Entering user mode for the first time (the assembly of a new thread's first `sret`): from a register image (EX1).

core::arch::global_asm!(
    r#"
    .section .text
    .globl enter_user_frame
    .balign 4
enter_user_frame:                # a0 a TrapFrame to start from, a1 address-space token, a2 kernel stack top (16-aligned)
    csrci sstatus, 2             # interrupts off until we are in U-mode
    csrw  satp, a1               # no flush: the token's ASID is unique and this hart flushed for its generation (L18)
    addi  sp, a2, -288           # the frame goes where a trap from user mode puts one: the top of this thread's kernel stack
    li    t0, 36
    mv    t1, a0
    mv    t2, sp
1:  ld    t3, 0(t1)
    sd    t3, 0(t2)
    addi  t1, t1, 8
    addi  t2, t2, 8
    addi  t0, t0, -1
    bnez  t0, 1b
    j     trap_epilogue          # restores the registers, sets PerHart.kstack_top from sp, and srets (tp is this hart's PerHart)
"#
);

extern "C" {
    /// Drop to U-mode from the register image `frame` (the thread's first run, or its first run after being written before it started): switch
    /// to the address space `token` and copy the image to the top of the kernel stack `kernel_top`, from where the trap exit restores it.
    /// Interrupts are off until the drop; nothing returns.
    ///
    /// # Safety
    /// `frame` points at a valid `TrapFrame` that does not overlap the top 288 bytes of the stack (it is on the heap); `token` names a live
    /// process root whose upper half is the kernel's; `tp` holds this hart's `PerHart`; `kernel_top` is the calling thread's own kernel stack
    /// top, 16-aligned; the caller holds no lock.
    #[link_name = "enter_user_frame"]
    pub fn enter_user_frame_raw(
        frame: *const crate::trap::TrapFrame,
        token: u64,
        kernel_top: u64,
    ) -> !;
}
