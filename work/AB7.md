---
id: AB7
kind: slice
title: The system library: `libsys.so` in the kernel image, mapped into every process; `rt` and the C library call through it
area: kernel
horizon: long
blocked_by: [HD2, AB6, EX2, MM9, PM1]
refs: [KERNEL-V1-SPEC, OD128]
---

**What.** KERNEL-V1-SPEC 3.1. Last of the ABI work, so the library is cut once from the finished call set.

**How.** A new crate `libsys` (`no_std`, `cdylib`-shaped, built for RVA23U64, position independent, no relocations other than none: PC-relative code only, checked by the build): one `#[no_mangle] extern "C" fn sys_<name>` per call, each a thin `ecall` wrapper generated from one table in `abi` that also generates the kernel's dispatch. It has `PT_LOAD` text (RX) and one read-only data page the kernel fills per boot (`abi_version`, `time_mult`, `time_shift`, `page_size`, `cbom_block`, `cboz_block`, `harts_online`, `flags`), plus `PT_DYNAMIC`, `.dynsym`, `.dynstr` and a SysV hash table. The kernel embeds the built object (`include_bytes!`), keeps its text in one shared VMO, and at `process_start` maps text and data page with ANYWHERE and records the text range in the process. The first thread gets the base in `a2`. `sys_clock_get` is computed in user mode. `rt`: a startup routine finds the symbols it needs by a linear walk of `.dynsym` (about 80 lines; `rt::sys` holds a table of function pointers filled once, before `main`); every existing wrapper calls through the table. The C library uses `rt`'s table. `rt::spawn` and the launcher need no change (the kernel maps the library). `proto` and the servers are untouched. Calls used only by the library's wrappers for older shapes (`port_wait`, `clock_get`) leave the kernel.

**Acceptance, written first.** A host test parses the built `libsys.so` and checks: no relocation sections, every call in `abi`'s table has a symbol, the data page layout matches `abi`. QEMU: every program in the image runs with no `ecall` instruction of its own (a build check greps the user binaries' disassembly for `ecall` outside `libsys`: none); `sys_clock_get` enters the kernel zero times (trap counter); two processes see different library bases (with ASLR on).

**Review.** Fable, soak.
