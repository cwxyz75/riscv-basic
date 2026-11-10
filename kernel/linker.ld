/* Memory layout for the kernel on QEMU's `virt` board.
 *
 * OpenSBI (M-mode firmware) lives at the bottom of RAM (0x8000_0000) and jumps
 * to us in S-mode at 0x8020_0000. See DESIGN.md for the full memory map.
 */
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;
STACK_SIZE   = 64K;

SECTIONS
{
    . = BASE_ADDRESS;

    .text : ALIGN(4K) {
        *(.text.init)          /* _start must come first */
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += STACK_SIZE;
        __stack_top = .;
    }

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
    }
}
