/* Memory layout of the OS-track kernel on QEMU's `virt` board.
 *
 * The kernel is LINKED at a fixed address in the upper half of the Sv39 address space (KERNEL_VIRT) and
 * LOADED wherever the loader puts it (KERNEL_LMA, the AT() addresses; 0x8020_0000 is only the default the
 * ELF asks QEMU for; `KERNEL_LMA=... cargo build` builds another, for the load-address boot test). The first
 * instructions (`_start`, position-independent) find where the image really is, build a tiny page table,
 * turn paging on and jump to the upper half; see src/arch/riscv64/boot.rs. The image must be loaded at a
 * multiple of 2 MiB.
 */
OUTPUT_ARCH(riscv)
ENTRY(_start)

KERNEL_VIRT   = 0xffffffff80200000;
KERNEL_LMA    = @KERNEL_LMA@;  /* build.rs fills it in: 0x80200000 unless $KERNEL_LMA says otherwise */
STACK_SIZE    = 256K;

SECTIONS
{
    . = KERNEL_VIRT;

    .text : AT(ADDR(.text) - KERNEL_VIRT + KERNEL_LMA) ALIGN(4K) {
        __text_start = .;
        *(.text.init)          /* _start must come first */
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VIRT + KERNEL_LMA) ALIGN(4K) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        __rodata_end = .;
    }

    .data : AT(ADDR(.data) - KERNEL_VIRT + KERNEL_LMA) ALIGN(4K) {
        __data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_VIRT + KERNEL_LMA) ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .stack (NOLOAD) : AT(ADDR(.stack) - KERNEL_VIRT + KERNEL_LMA) ALIGN(4K) {
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
