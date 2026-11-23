/* The same layout as link.ld, as a static-PIE (HD2): linked at 0, so every address is an offset from the base the loader chooses; the loader applies the
 * R_RISCV_RELATIVE relocations in .rela.dyn to its copy of each segment's bytes before it maps them (kcore::elf), so read-only data stays read-only. */
OUTPUT_ARCH(riscv)
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(5);   /* R X */
    rodata  PT_LOAD FLAGS(4);   /* R   */
    data    PT_LOAD FLAGS(6);   /* R W */
    dynamic PT_DYNAMIC FLAGS(6);
}

SECTIONS
{
    . = 0;

    .text : ALIGN(4K) {
        *(.text._start)
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        *(.data.rel.ro .data.rel.ro.*)
    } :rodata

    .rela.dyn : { *(.rela.dyn) *(.rela.*) } :rodata

    .data : ALIGN(4K) {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
    } :data

    .dynamic : { *(.dynamic) } :data :dynamic

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .riscv.attributes .dynsym .dynstr .hash .gnu.hash .interp) }
}
