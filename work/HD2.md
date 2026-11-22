---
id: HD2
kind: slice
title: Position-independent programs and address randomisation for them
area: security
horizon: long
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 3.7, 7.2. (1) `kcore::elf` accepts `ET_DYN` static-PIE images (no `PT_INTERP`, no `DT_NEEDED`; the dynamic section may hold `DT_RELA` with `R_RISCV_RELATIVE` only) as well as `ET_EXEC`, and reports the image's span. (2) Startup code relocates itself: `rt`'s entry and the C library's `crt0` walk their own `DT_RELA` before anything else (about 40 lines of position-independent assembly or carefully written Rust with no relocated references). (3) Builds: user Rust with `-C relocation-model=pie` and a static-pie link, C with `-fPIE -static-pie`. (4) Placement: the kernel's built-in loader (the first process) and `rt::spawn` map a PIE image's segments at a base obtained by one ANYWHERE reservation of the image's span (reserve, then map segments at fixed offsets inside it); the first thread's stack is mapped ANYWHERE instead of below `STACK_TOP`; `thread_spawn`'s stack arena hint starts at a random offset per process. `ET_EXEC` images and fixed addresses still work. Test builds honour `aslr=off`.

**Acceptance, written first.** `kcore/tests/elf_loader.rs`: PIE images accepted, a PIE with `PT_INTERP` or a non-relative relocation refused, the mutation fuzz extended to `ET_DYN`. QEMU: a program prints the addresses of its code, stack, a heap block and an ANYWHERE mapping; across ten launches each takes at least eight distinct values; with `aslr=off` they are constant. Every program in the image is PIE (a build check on `e_type`). The Lua, C, BusyBox and BASIC suites pass.

**Review.** Fable, soak.

## Progress (2026-11-22)

**Step (1) is built** (PR on `epic/phase-11`): `kcore::elf` accepts `ET_DYN` static-PIE images: link addresses are offsets from a base the loader chooses, the dynamic section may hold only the `DT_RELA` family (a tolerated few: empty symbol table and hash, flags, `DT_RELACOUNT`), every relocation must be `R_RISCV_RELATIVE` on an 8-byte word inside a writable segment, no `PT_INTERP`/`DT_NEEDED`, one `PT_DYNAMIC` and only in an `ET_DYN`; `Image` gains `pie` and `span`; host tests (`kcore/tests/elf_loader.rs`: acceptance, every rejection, a mutation fuzz over a static-PIE, one mutant checked). Until step (4) places them, the kernel's loader and `rt::spawn` refuse a static-PIE (BAD_TYPE / BAD_ELF). **Open:** (2) the self-relocating startup of `rt` and the C library, (3) the PIE builds of every program, (4) placement by an ANYWHERE reservation, the ANYWHERE stack and the random `thread_spawn` arena hint, `aslr=off`, and the QEMU acceptance (ten launches, eight distinct values).

