//! The first instructions: running at physical addresses with paging off, build a
//! tiny page table, turn paging on, and jump to the kernel's upper-half addresses.
//!
//! The kernel is linked at `KERNEL_PHYS + KERNEL_OFFSET` but loaded (and entered)
//! at `KERNEL_PHYS`. `_start` is position-independent: `la` is PC-relative, and
//! the link-time and load-time layouts differ by a constant, so before the jump
//! `la` yields *physical* addresses. The early table (1 GiB leaves, so crude
//! permissions) holds only what the next few instructions and `kmain` need;
//! `kmem::init` then builds the real kernel page table and abandons this one.
//!
//!   root[0]    phys 0 (devices)        RW   identity   (UART, test finisher)
//!   root[2]    phys 0x8000_0000 (RAM)  RWX  identity   (keeps these instructions
//!                                                       running across `satp`)
//!   root[256]  phys 0                  RW   direct map base
//!   root[258]  phys 0x8000_0000        RW   direct map of RAM
//!   root[510]  phys 0x8000_0000        RWX  the kernel image's upper-half address

use core::arch::global_asm;

#[repr(C, align(4096))]
pub struct EarlyRoot([u64; 512]);

/// Zero-initialised, so it lives in `.bss` and `_start` clears it with the rest.
#[no_mangle]
pub static mut EARLY_ROOT: EarlyRoot = EarlyRoot([0; 512]);

global_asm!(
    r#"
    .section .text.init
    .globl _start
_start:
    mv   s0, a0                  # hart id
    mv   s1, a1                  # device tree pointer
    # zero .bss (these are physical addresses: PC-relative `la`)
    la   t0, __bss_start
    la   t1, __bss_end
1:
    bgeu t0, t1, 2f
    sd   zero, 0(t0)
    addi t0, t0, 8
    j    1b
2:
    la   t2, EARLY_ROOT
    li   t3, 0xC7                # V R W A D
    sd   t3, 0(t2)               # root[0]   = phys 0, RW
    li   t3, 0x200000CF          # (0x80000 << 10) | V R W X A D
    sd   t3, 16(t2)              # root[2]   = phys 0x8000_0000, RWX
    li   t5, 2048
    add  t5, t2, t5              # t5 = &root[256]
    li   t3, 0xC7
    sd   t3, 0(t5)               # root[256] = phys 0, RW
    li   t3, 0x200000C7
    sd   t3, 16(t5)              # root[258] = phys 0x8000_0000, RW
    li   t3, 0x200000CF
    sd   t3, 2032(t5)            # root[510] = phys 0x8000_0000, RWX
    srli t3, t2, 12
    li   t4, 8
    slli t4, t4, 60              # satp.MODE = 8 (Sv39)
    or   t3, t3, t4
    csrw satp, t3
    sfence.vma zero, zero
    # Paging is on; we still run at the identity-mapped physical address. Jump to
    # the same code at its upper-half address.
    la   t0, 3f
    li   t1, 0xffffffff00000000  # KERNEL_OFFSET
    add  t0, t0, t1
    jr   t0
3:
    la   sp, __stack_top         # now PC-relative to an upper-half PC
    mv   a0, s0
    mv   a1, s1
    call kmain
4:
    wfi
    j    4b
"#
);
