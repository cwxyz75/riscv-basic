# The variadic entry points of posix2.rs: a0-a7 are saved below the entry stack pointer so that they and the caller's stack
# arguments are one run of 8-byte slots (see stdio.rs); the `v*` function gets a pointer to the first variadic slot.
.macro variadic name, target, nfixed, offset
.section .text.\name,"ax",@progbits
.globl \name
\name:
    addi sp, sp, -80
    sd ra, 0(sp)
    sd a0, 16(sp)
    sd a1, 24(sp)
    sd a2, 32(sp)
    sd a3, 40(sp)
    sd a4, 48(sp)
    sd a5, 56(sp)
    sd a6, 64(sp)
    sd a7, 72(sp)
    addi a\nfixed, sp, \offset
    call \target
    ld ra, 0(sp)
    addi sp, sp, 80
    ret
.endm
variadic error, verror, 3, 40
variadic error_at_line, verror_at_line, 5, 56
variadic execl, vexecl, 2, 32
variadic execlp, vexeclp, 2, 32
variadic execle, vexecle, 2, 32
variadic fscanf, vfscanf, 2, 32
variadic scanf, vscanf, 1, 24
