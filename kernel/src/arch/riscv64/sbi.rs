//! Firmware (SBI) calls and the secondary harts' entry assembly (AR1).

use core::arch::{asm, global_asm};

pub const EID_HSM: usize = 0x48_534D;
pub const FID_HART_START: usize = 0;
pub const FID_HART_GET_STATUS: usize = 2;
pub const FID_HART_SUSPEND: usize = 3;
/// SBI error: the call is not supported.
pub const SBI_ERR_NOT_SUPPORTED: isize = -2;
pub const EID_RFENCE: usize = 0x5246_4E43;
pub const FID_REMOTE_SFENCE_VMA: usize = 1;
pub const EID_IPI: usize = 0x73_5049;
pub const FID_SEND_IPI: usize = 0;
/// `hart_get_status` result for a hart that exists and is waiting to be started.
pub const HSM_STOPPED: usize = 1;

pub struct SbiRet {
    pub error: isize,
    pub value: usize,
}

pub fn sbi(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> SbiRet {
    sbi5(eid, fid, [a0, a1, a2, 0, 0])
}

pub fn sbi5(eid: usize, fid: usize, a: [usize; 5]) -> SbiRet {
    let (error, value): (isize, usize);
    // SAFETY: `ecall` from S-mode into OpenSBI with the standard calling convention (a7 =
    // extension, a6 = function, a0-a2 arguments, results in a0/a1); it changes no kernel state.
    unsafe {
        asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inlateout("a0") a[0] => error,
            inlateout("a1") a[1] => value,
            in("a2") a[2],
            in("a3") a[3],
            in("a4") a[4],
        );
    }
    SbiRet { error, value }
}

global_asm!(
    r#"
    .section .text
    .globl secondary_entry
    .balign 4
secondary_entry:                 # a0 = hart id, a1 = dense hart index; paging off, interrupts off
    mv   s0, a0
    mv   s1, a1
    # The kernel's own table has no identity mapping, so first use the early boot table (still
    # intact in .bss: identity for RAM, plus the upper-half image), then jump to the upper half.
    la   t2, EARLY_ROOT
    srli t3, t2, 12
    li   t4, 8
    slli t4, t4, 60              # satp.MODE = 8 (Sv39)
    or   t3, t3, t4
    csrw satp, t3
    sfence.vma zero, zero
    la   t0, 3f
    li   t1, 0xffffffff00000000  # KERNEL_OFFSET
    add  t0, t0, t1
    jr   t0
3:
    la   t0, SECONDARY_STACK_TOPS
    slli t1, s1, 3
    add  t0, t0, t1
    ld   sp, 0(t0)
    mv   a0, s0
    mv   a1, s1
    call secondary_main
4:
    wfi
    j    4b
"#
);

extern "C" {
    /// The entry point a secondary hart starts at (a0 = hart id, a1 = dense index; paging off).
    pub fn secondary_entry();
}
