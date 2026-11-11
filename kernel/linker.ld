/* Memory layout of the OS-track kernel on QEMU's `virt` board.
 *
 * OpenSBI (M-mode firmware) jumps to us in S-mode at physical 0x8020_0000. The
 * kernel is LINKED in the upper half of the Sv39 address space, at
 * KERNEL_PHYS + KERNEL_OFFSET, and LOADED at KERNEL_PHYS (the AT() addresses). The
 * first instructions (`_start`, position-independent) build a tiny page table, turn
 * paging on and jump to the upper half; see src/boot.rs.
 */
OUTPUT_ARCH(riscv)
ENTRY(_start)

KERNEL_PHYS   = 0x80200000;
KERNEL_OFFSET = 0xffffffff00000000;
KERNEL_VIRT   = KERNEL_PHYS + KERNEL_OFFSET;
STACK_SIZE    = 256K;

SECTIONS
{
    . = KERNEL_VIRT;

    .text : AT(ADDR(.text) - KERNEL_OFFSET) ALIGN(4K) {
        __text_start = .;
        *(.text.init)          /* _start must come first */
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) ALIGN(4K) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        __rodata_end = .;
    }

    .data : AT(ADDR(.data) - KERNEL_OFFSET) ALIGN(4K) {
        __data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .stack (NOLOAD) : AT(ADDR(.stack) - KERNEL_OFFSET) ALIGN(4K) {
        __stack_bottom = .;
        . += STACK_SIZE;
        __stack_top = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
    }
}
