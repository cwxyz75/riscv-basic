//! The first instructions: running at physical addresses with paging off, build a
//! tiny page table, turn paging on, and jump to the kernel's upper-half addresses.
//!
//! The kernel is linked at a fixed upper-half address (`abi::layout::KERNEL_VIRT_BASE`) and loaded wherever the loader puts it. `_start` is
//! position-independent: `la` is PC-relative, so with paging off it yields *physical* addresses, and the difference between where the
//! image is linked and where it is (`KERNEL_DELTA`, read back by `kernel_delta`) is all the rest of the kernel needs. The early table
//! holds only what the next few instructions and `kmain` need (1 GiB leaves for RAM and devices, so crude permissions; 2 MiB leaves for
//! the image, which must be loaded at a multiple of 2 MiB); `kmem::init` then builds the real kernel page table and abandons this one.
//!
//!   root[0]            phys 0 (devices)       RW   identity   (UART, test finisher)
//!   root[image / 1 GiB] the gigabyte the image runs in   RWX  identity   (keeps these instructions running across `satp`)
//!   root[256 + i]      phys i GiB, i = 0..63  RW   the direct map of the physical space below 64 GiB (RAM is wherever the tree says)
//!   root[510]          EARLY_L1               the kernel image at its linked address, as 2 MiB RWX pages from the image's physical start

use core::arch::global_asm;

#[repr(C, align(4096))]
pub struct EarlyRoot([u64; 512]);

/// Zero-initialised, so it lives in `.bss` and `_start` clears it with the rest.
#[no_mangle]
pub static mut EARLY_ROOT: EarlyRoot = EarlyRoot([0; 512]);

/// The level below `root[510]` in the early table: 2 MiB pages over the kernel image (PL4c).
#[no_mangle]
pub static mut EARLY_L1: EarlyRoot = EarlyRoot([0; 512]);

/// Where the image is linked minus where it was loaded: `_start` works it out from its own address and stores it here before paging is on.
#[no_mangle]
pub static mut KERNEL_DELTA: u64 = 0;

/// The difference between the image's virtual and physical addresses (`virtual - delta = physical`, for any address inside the image).
pub fn kernel_delta() -> u64 {
    // SAFETY: written once by `_start` before the kernel runs; read-only afterwards.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KERNEL_DELTA)) }
}

/// Where `early_trap` ends the run in a test build: QEMU's `virt` test finisher, at its fixed address (the tree is not read this early). A release
/// build writes nothing there and ends through the firmware.
const EARLY_FINISHER: u64 = if cfg!(feature = "release") {
    0
} else {
    0x10_0000
};

/// The failure code `early_trap` writes to the test finisher (`abi::finisher::fail(1)`).
const EARLY_FAIL: u64 = abi::finisher::fail(1) as u64;

global_asm!(
    r##"
    .section .text.init
    .globl _start
_start:
    # Before anything else: a trap handler of our own. This kernel is built for the RVA23 profile, so the compiler may use an extension
    # in any function, the device-tree reader included; on a CPU without the profile the first such instruction traps, long before
    # `dtb::require_extensions` could say so. `early_trap` (base ISA only) says it. It runs at its physical address, which the early table
    # identity-maps; `trap::init` replaces it.
    la   t0, early_trap
    csrw stvec, t0
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
    # Where is the image? `la` is PC-relative and paging is off, so this is its real, physical address, wherever the loader put it
    # (a multiple of 2 MiB: the early table maps it with 2 MiB pages). delta = where it is linked minus where it is, for the whole image.
    la   t0, __text_start
    li   t1, 0xffffffff80200000  # KERNEL_VIRT_BASE
    sub  s2, t1, t0
    la   t2, KERNEL_DELTA
    sd   s2, 0(t2)               # the kernel reads it back (`kernel_delta`)
    la   t2, EARLY_ROOT
    li   t3, 0xC7                # V R W A D
    sd   t3, 0(t2)               # root[0] = phys 0, RW (the devices, until `kmem::init`)
    # identity: the gigabytes the image runs from (its first and its last byte may be in different ones), RWX (keeps these instructions
    # running across `satp`, and lets a secondary hart read what it needs). An image that starts at 64 GiB or beyond is refused (`9:`).
    srli a4, t0, 36
    bnez a4, 9f
    la   a5, __kernel_end
    addi a5, a5, -1              # the image's last byte
    srli a5, a5, 30              # its gigabyte number
    srli t4, t0, 30              # the first byte's gigabyte number
    slli t5, t4, 3
    add  t5, t2, t5              # &root[number]
    slli t3, t4, 28              # (number << 18) << 10: the frame number of that gigabyte in the entry
    ori  t3, t3, 0xCF            # V R W X A D
    sd   t3, 0(t5)
    slli t5, a5, 3
    add  t5, t2, t5              # (the same slot again when the image sits in one gigabyte)
    slli t3, a5, 28
    ori  t3, t3, 0xCF
    sd   t3, 0(t5)
    # PL4: the direct map reaches every gigabyte of physical space the kernel supports (`DIRECT_MAP_PHYS_LIMIT`, 64 GiB), so that
    # `kmem::init` can write page tables into frames wherever the device tree puts RAM: root[256 + i] = phys i GiB, RW, for i = 0 to 63
    li   t5, 2048
    add  t5, t2, t5              # t5 = &root[256]
    li   t6, 0
    li   a2, 64
5:
    bgeu t6, a2, 6f
    slli t3, t6, 28
    ori  t3, t3, 0xC7            # V R W A D
    slli t4, t6, 3
    add  t4, t5, t4              # &root[256 + i]
    sd   t3, 0(t4)
    addi t6, t6, 1
    j    5b
6:
    # PL4c: the image at its linked address: root[510] points at EARLY_L1, whose entries (from index 1, where KERNEL_VIRT_BASE falls) are
    # 2 MiB read-write-execute pages over the image's physical frames, as many as the image needs
    la   a3, EARLY_L1
    srli t3, a3, 12
    slli t3, t3, 10
    ori  t3, t3, 1               # V only: a pointer to the next level
    sd   t3, 2032(t5)            # root[510]
    la   t4, __kernel_end
    sub  t4, t4, t0              # the image's size
    li   t6, 0x1fffff
    add  t4, t4, t6
    srli t4, t4, 21              # the number of 2 MiB pages
    li   t6, 511
    bgtu t4, t6, 9f              # an image that does not fit EARLY_L1 (over 1022 MiB) is refused
    slli a4, t0, 43              # a misaligned image (low 21 bits set) cannot be mapped: stop here (no console yet)
    bnez a4, 9f
    srli a4, t0, 21              # the first page's frame, in 2 MiB units
    li   a5, 0
7:
    bgeu a5, t4, 8f
    add  a6, a4, a5
    slli a6, a6, 19              # (pa >> 21) << 9 is the frame number; << 10 puts it in the entry
    ori  a6, a6, 0xCF            # V R W X A D
    addi a7, a5, 1               # L1 index: 1 + k
    slli a7, a7, 3
    add  a7, a3, a7
    sd   a6, 0(a7)
    addi a5, a5, 1
    j    7b
9:
    wfi
    j    9b
8:
    srli t3, t2, 12
    li   t4, 8
    slli t4, t4, 60              # satp.MODE = 8 (Sv39)
    or   t3, t3, t4
    csrw satp, t3
    sfence.vma zero, zero
    # Paging is on; we still run at the identity-mapped physical address. Jump to
    # the same code at its upper-half address.
    la   t0, 3f
    add  t0, t0, s2              # + delta: the same code at its linked address
    jr   t0
3:
    la   sp, __stack_top         # now PC-relative to an upper-half PC
    mv   a0, s0
    mv   a1, s1
    call kmain
4:
    wfi
    j    4b

    # The early trap handler: base ISA only (no extension may be assumed), no stack, no memory but the messages. Prints one line through the
    # firmware's console (SBI legacy putchar), then ends the run: failure through the test finisher in a test build, then the firmware's
    # system reset, then a stop.
    .balign 4
early_trap:
    csrr s3, scause
    csrr s4, sepc
    la   t0, 91f                 # the missing-extension line for an illegal instruction, the early-trap line otherwise
    li   t1, 2
    beq  s3, t1, 81f
    la   t0, 92f
81:
    jal  t6, 70f                 # print the string at t0
    mv   t2, s3
    jal  t6, 72f                 # scause
    la   t0, 93f
    jal  t6, 70f
    mv   t2, s4
    jal  t6, 72f                 # sepc
    la   t0, 94f
    jal  t6, 70f
    li   t0, {finisher}
    beqz t0, 85f
    li   t1, {fail}
    sw   t1, 0(t0)               # the test finisher: the run ends here with a failing exit status
85:
    li   a7, 0x53525354          # SBI SRST: shutdown, reason system failure
    li   a6, 0
    li   a0, 0
    li   a1, 1
    ecall
86:
    wfi
    j    86b
70:                              # print the NUL-terminated string at t0, return through t6
    lbu  a0, 0(t0)
    beqz a0, 71f
    li   a7, 1                   # SBI legacy console putchar
    ecall
    addi t0, t0, 1
    j    70b
71:
    jr   t6
72:                              # print t2 as 16 hex digits, return through t6
    li   t3, 60
73:
    srl  t4, t2, t3
    andi t4, t4, 15
    li   t5, 10
    bltu t4, t5, 74f
    addi a0, t4, 87              # 'a' - 10
    j    75f
74:
    addi a0, t4, 48              # '0'
75:
    li   a7, 1
    ecall
    addi t3, t3, -4
    bgez t3, 73b
    jr   t6
91:
    .asciz "\n##PANIC missing extension: an instruction this CPU lacks ran before the profile check (RVA23S64 and the AIA, OD127); scause 0x"
92:
    .asciz "\n##PANIC early trap before the kernel's own handler; scause 0x"
93:
    .asciz ", sepc 0x"
94:
    .asciz "\n"
"##,
    finisher = const EARLY_FINISHER,
    fail = const EARLY_FAIL,
);
