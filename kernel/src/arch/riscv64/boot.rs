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
//!   root[256 + k + i]  phys i GiB, i = 0..63  RW   the direct map of the physical space below 64 GiB (RAM is wherever the tree says), `k` a random
//!                      number of gigabytes from 0 to 64 (HD3)
//!   root[510]          EARLY_L1               the kernel image at its slid address, as 2 MiB RWX pages from the image's physical start: it starts
//!                      `s` pages above where it is linked, `s` random (HD3), so that it stays inside the slot
//!
//! HD3: the kernel is linked as a static-PIE. Before paging goes on `_start` reads `/chosen/kaslr-seed` (else `/chosen/rng-seed`) from the device
//! tree with a small scanner of its own, chooses `s` and `k` (the formula is `kcore::kaslr`, which the boot self-test checks the choice against), and
//! applies the image's `R_RISCV_RELATIVE` relocations (`slide` added to each addend) to the image where it stands in memory. A test build's
//! `kaslr=off` on the kernel line (read by the same scanner) makes both zero.

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

/// The bytes the image is slid up from where it is linked (a multiple of 2 MiB; HD3), stored by `_start`.
#[no_mangle]
pub static mut KASLR_SLIDE: u64 = 0;

/// How many gigabytes the direct map is moved up from its first slot (0 to 64; HD3), stored by `_start`.
#[no_mangle]
pub static mut KASLR_DIRECT: u64 = 0;

/// The bytes the image is slid up from where it is linked.
pub fn slide() -> u64 {
    // SAFETY: written once by `_start` before the kernel runs; read-only afterwards.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KASLR_SLIDE)) }
}

/// The gigabytes the direct map is moved up.
pub fn direct_shift() -> u64 {
    // SAFETY: as for `slide`.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KASLR_DIRECT)) }
}

/// The difference between the image's virtual and physical addresses (`virtual - delta = physical`, for any address inside the image).
pub fn kernel_delta() -> u64 {
    // SAFETY: written once by `_start` before the kernel runs; read-only afterwards.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KERNEL_DELTA)) }
}

/// Where `early_trap` ends the run in a test build: QEMU's `virt` test finisher, at its fixed address (the tree is not read this early). A release
/// build writes nothing there and ends through the firmware.
const EARLY_FINISHER: u64 = if cfg!(all(feature = "release", not(feature = "release-suite"))) {
    0
} else {
    0x10_0000
};

/// Does `_start` look for `kaslr=off`? Test builds only: a release kernel does not even carry the string.
const KASLR_OFF_CHECK: u64 = if cfg!(all(feature = "release", not(feature = "release-suite"))) {
    0
} else {
    1
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
    lla   t0, early_trap
    csrw stvec, t0
    mv   s0, a0                  # hart id
    mv   s1, a1                  # device tree pointer
    # zero .bss (these are physical addresses: PC-relative `la`)
    lla   t0, __bss_start
    lla   t1, __bss_end
1:
    bgeu t0, t1, 2f
    sd   zero, 0(t0)
    addi t0, t0, 8
    j    1b
2:
    lla   t0, __text_start        # the image's real, physical address (a multiple of 2 MiB: the early table maps it with 2 MiB pages)
    # HD3: scan the device tree (big-endian, at its physical address) for the seeds: s7 = the first eight bytes of `kaslr-seed` (0: none), s8 = the
    # first eight bytes of `rng-seed` xor the next eight (0: none), s5 = 1 when the kernel line holds `kaslr=off` (test builds).
    li   s5, 0
    li   s7, 0
    li   s8, 0
    beqz s1, 49f                 # no tree pointer: no seed
    mv   a0, s1
    jal  t5, 60f
    li   a2, 0xd00dfeed
    bne  a1, a2, 49f             # not a tree
    addi a0, s1, 8
    jal  t5, 60f
    add  s9, s1, a1              # the structure block
    addi a0, s1, 12
    jal  t5, 60f
    add  s10, s1, a1             # the strings block
    addi a0, s1, 36
    jal  t5, 60f
    add  s11, s9, a1             # the end of the structure block
41:
    bgeu s9, s11, 49f
    mv   a0, s9
    jal  t5, 60f                 # a1 = the token
    addi s9, s9, 4
    li   a2, 1
    beq  a1, a2, 42f             # FDT_BEGIN_NODE: skip its name
    li   a2, 3
    beq  a1, a2, 43f             # FDT_PROP
    li   a2, 2
    beq  a1, a2, 41b             # FDT_END_NODE
    li   a2, 4
    beq  a1, a2, 41b             # FDT_NOP
    j    49f                     # FDT_END (or anything else)
42:
    lbu  a1, 0(s9)
    addi s9, s9, 1
    bnez a1, 42b
    addi s9, s9, 3
    andi s9, s9, -4
    j    41b
43:
    mv   a0, s9
    jal  t5, 60f
    mv   a3, a1                  # the property's length
    addi a0, s9, 4
    jal  t5, 60f
    add  a4, s10, a1             # its name, in the strings block
    addi s9, s9, 8               # its data
    mv   a5, s9
    add  s9, s9, a3
    addi s9, s9, 3
    andi s9, s9, -4              # the next token
    lla   a1, 181f                # "kaslr-seed"
    mv   a0, a4
    jal  t4, 61f
    bnez a2, 44f
    li   a2, 8
    bltu a3, a2, 41b
    lwu  a1, 0(a5)
    lwu  a2, 4(a5)
    slli a2, a2, 32
    or   s7, a1, a2
    j    41b
44:
    lla   a1, 182f                # "rng-seed"
    mv   a0, a4
    jal  t4, 61f
    bnez a2, 45f
    li   a2, 8
    bltu a3, a2, 41b
    lwu  a1, 0(a5)
    lwu  a2, 4(a5)
    slli a2, a2, 32
    or   s8, a1, a2
    li   a2, 16
    bltu a3, a2, 41b
    lwu  a1, 8(a5)
    lwu  a2, 12(a5)
    slli a2, a2, 32
    or   a1, a1, a2
    xor  s8, s8, a1
    j    41b
45:
    .if {off_check}
    lla   a1, 183f                # "bootargs"
    mv   a0, a4
    jal  t4, 61f
    bnez a2, 41b
    # look for "kaslr=off" anywhere in the property's data (a5, a3 bytes)
    li   a6, 9
46:
    bltu a3, a6, 41b
    lla   a1, 184f
    mv   a0, a5
    jal  t4, 62f                 # a2 = 0 when the nine bytes at a0 are "kaslr=off"
    bnez a2, 47f
    li   s5, 1
    j    41b
47:
    addi a5, a5, 1
    addi a3, a3, -1
    j    46b
    .else
    j    41b
    .endif
49:
    # choose: the seed is `kaslr-seed`, else `rng-seed`, else nothing (no slide)
    mv   t6, s7
    bnez t6, 50f
    mv   t6, s8
50:
    li   s7, 0                   # the slide in bytes
    li   s8, 0                   # the direct map's move in gigabytes
    beqz t6, 54f
    bnez s5, 54f                 # kaslr=off
    # the number of 2 MiB pages the image needs: choices for the slide = 511 - pages (the image stays below index 512 of EARLY_L1, from index 1 + s)
    lla   t4, __kernel_end
    sub  t4, t4, t0
    li   a2, 0x1fffff
    add  t4, t4, a2
    srli t4, t4, 21
    li   a2, 511
    bgeu t4, a2, 51f             # no room: no slide (the image is refused below if it does not fit)
    sub  a2, a2, t4              # choices
    slli a3, t6, 32
    srli a3, a3, 32              # the low 32 bits of the seed
    remu a3, a3, a2              # s
    slli s7, a3, 21
51:
    srli a3, t6, 32              # the high 32 bits
    li   a2, 65
    remu s8, a3, a2              # k
54:
    # relocate the image where it stands: every R_RISCV_RELATIVE entry (offset, info, addend) becomes `addend + slide` at the entry's offset
    lla   a2, __rela_start
    lla   a3, __rela_end
    li   a4, 0xffffffff80200000  # where the image is linked (KERNEL_VIRT_BASE)
55:
    bgeu a2, a3, 56f
    ld   a5, 0(a2)               # offset (a link address)
    ld   a6, 16(a2)              # addend
    sub  a5, a5, a4
    add  a5, a5, t0              # its physical address
    add  a6, a6, s7
    sd   a6, 0(a5)
    addi a2, a2, 24
    j    55b
56:
    # Where is the image? delta = where it is (linked + slide) minus where it is loaded (`t0`), for the whole image.
    li   t1, 0xffffffff80200000  # KERNEL_VIRT_BASE
    add  t1, t1, s7
    sub  s2, t1, t0
    lla   t2, KERNEL_DELTA
    sd   s2, 0(t2)               # the kernel reads it back (`kernel_delta`)
    lla   t2, KASLR_SLIDE
    sd   s7, 0(t2)
    lla   t2, KASLR_DIRECT
    sd   s8, 0(t2)
    lla   t2, EARLY_ROOT
    li   t3, 0xC7                # V R W A D
    sd   t3, 0(t2)               # root[0] = phys 0, RW (the devices, until `kmem::init`)
    # identity: the gigabytes the image runs from (its first and its last byte may be in different ones), RWX (keeps these instructions
    # running across `satp`, and lets a secondary hart read what it needs). An image that starts at 64 GiB or beyond is refused (`9:`).
    srli a4, t0, 36
    bnez a4, 9f
    lla   a5, __kernel_end
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
    slli a7, s8, 3
    add  a7, t5, a7              # a7 = &root[256 + k]: HD3 moves the direct map up by k gigabytes
    li   t6, 0
    li   a2, 64
5:
    bgeu t6, a2, 6f
    slli t3, t6, 28
    ori  t3, t3, 0xC7            # V R W A D
    slli t4, t6, 3
    add  t4, a7, t4              # &root[256 + k + i]
    sd   t3, 0(t4)
    addi t6, t6, 1
    j    5b
6:
    # PL4c: the image at its linked address: root[510] points at EARLY_L1, whose entries (from index 1, where KERNEL_VIRT_BASE falls) are
    # 2 MiB read-write-execute pages over the image's physical frames, as many as the image needs
    lla   a3, EARLY_L1
    srli t3, a3, 12
    slli t3, t3, 10
    ori  t3, t3, 1               # V only: a pointer to the next level
    sd   t3, 2032(t5)            # root[510]
    lla   t4, __kernel_end
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
    srli t6, s7, 21              # (HD3: the image sits `s` pages above where it is linked)
    add  a7, a5, t6
    addi a7, a7, 1               # L1 index: 1 + s + k
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
    lla   t0, 3f
    add  t0, t0, s2              # + delta: the same code at its linked address
    jr   t0
3:
    lla   sp, __stack_top         # now PC-relative to an upper-half PC
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
    lla   t0, 91f                 # the missing-extension line for an illegal instruction, the early-trap line otherwise
    li   t1, 2
    beq  s3, t1, 81f
    lla   t0, 92f
81:
    jal  t6, 70f                 # print the string at t0
    mv   t2, s3
    jal  t6, 72f                 # scause
    lla   t0, 93f
    jal  t6, 70f
    mv   t2, s4
    jal  t6, 72f                 # sepc
    lla   t0, 94f
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
    # (HD3) helpers of the seed scan, base ISA only, no stack: 60 = the big-endian word at a0 in a1 (return through t5, clobbers t3); 61 = a2 = 0 when
    # the NUL-terminated strings at a0 and a1 are equal (return through t4, clobbers t1 and t2); 62 = a2 = 0 when the nine bytes at a0 are those at a1
    .balign 4
60:
    lbu  a1, 0(a0)
    slli a1, a1, 8
    lbu  t3, 1(a0)
    or   a1, a1, t3
    slli a1, a1, 8
    lbu  t3, 2(a0)
    or   a1, a1, t3
    slli a1, a1, 8
    lbu  t3, 3(a0)
    or   a1, a1, t3
    jr   t5
61:
    lbu  t1, 0(a0)
    lbu  t2, 0(a1)
    bne  t1, t2, 63f
    beqz t1, 64f
    addi a0, a0, 1
    addi a1, a1, 1
    j    61b
63:
    li   a2, 1
    jr   t4
64:
    li   a2, 0
    jr   t4
    .if {off_check}
62:
    li   t3, 9
65:
    lbu  t1, 0(a0)
    lbu  t2, 0(a1)
    bne  t1, t2, 63b
    addi a0, a0, 1
    addi a1, a1, 1
    addi t3, t3, -1
    bnez t3, 65b
    li   a2, 0
    jr   t4
    .endif
181:
    .asciz "kaslr-seed"
182:
    .asciz "rng-seed"
    .if {off_check}
183:
    .asciz "bootargs"
184:
    .asciz "kaslr=off"
    .endif
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
    off_check = const KASLR_OFF_CHECK,
    fail = const EARLY_FAIL,
);
