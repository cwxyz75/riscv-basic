---
id: AR1
kind: slice
title: The architecture boundary: all RISC-V code behind `kernel/src/arch/`, page tables generic over a format, an AArch64 stub that compiles
area: platform
horizon: long
refs: [KERNEL-V1-SPEC, OD143]
---

**What.** KERNEL-V1-SPEC 2.6 (OD143): arrange the kernel so AArch64 can be added later as a directory. Nothing of AArch64 is implemented. Do this before phase 4's platform work, so that work lands on the right side of the boundary.

**How, in four PRs.**

* **AR1a, the audit (no code change).** A record in `docs/records/notes/`: every place outside what will be `arch/` that names a RISC-V register, CSR, instruction, cause number, SBI call or Sv39 bit (`kernel/src/*.rs`, `kcore`, `abi`, `ksys`, `rt`, `libc`), with where each goes. This list is the checklist for the next three.
* **AR1b, the move.** `kernel/src/arch/mod.rs` and `kernel/src/arch/riscv64/`: `boot.rs`, the assembly and frame of `trap.rs`, the context switch from `thread.rs`, `percpu`'s `tp` binding, `timer.rs`, the SBI calls and IPI of `smp.rs`, `aia.rs`, the save and restore of `fp.rs`, the `satp` and `sfence.vma` uses of `kmem.rs` and the syscall layer, `irq.rs`'s status-register bits, `finisher.rs`. The interface is the list in the spec; the trap handler outside `arch` receives a decoded `arch::Fault { kind, address, access }` or `arch::Syscall`, never a cause register. `trait IrqChip` with the AIA as its one implementation. No behaviour changes: this is a move plus a facade.
* **AR1c, page tables.** `kcore::pagetable::PageTable<F: PageTableFormat>`; `Sv39` implements the format (entry encode and decode, levels, large-page levels, software bits, canonical-address rule); `kcore::aspace` and `kcore::usercopy` name no PTE bit. A second, host-only test format `TestFmt48` (four levels, a different bit layout modelled on AArch64's descriptors) exists only in `kcore/tests/` to prove the code is generic.
* **AR1d, the ABI side.** `abi::arch` holds the user address range, the stack arena, the thread-state layouts and the meaning of an exception message's raw cause field. Fault **kinds** are defined here once (PAGE_FAULT, ILLEGAL, BREAKPOINT, MISALIGNED, ACCESS, PAGER; EX2 uses the same numbers) and the exit code of an unhandled fault becomes `-1000 - kind` (INVARIANTS P3, `SYSCALLS.md`, the shell's and the C library's tables of fault exits, and the eight fault tests change with it; the user-visible meaning "killed by a fault of this kind" is kept). `kernel/src/arch/aarch64/` is a stub: every interface item present, every body `unimplemented!()`, with a header comment saying it carries no behaviour.

**Acceptance, written first.** (1) A check in `scripts/check_docs.py`: `asm!`, `global_asm!`, `csrr`/`csrw`/`sfence`/`ecall` text, the SBI extension ids and the names `satp`, `sstatus`, `scause`, `stvec`, `sepc`, `stval`, `sscratch` do not appear in `kernel/src` outside `arch/`, nor in `kcore` outside the Sv39 format module, nor in `ksys`; it is shown to fail on a planted `csrr` in `ksys`. (2) `cargo check` of the kernel for `aarch64-unknown-none` with the stub passes, as a gate step in the `host` job (seconds; the target is added to the toolchain setup). (3) The page-table and address-space models run for both `Sv39` and `TestFmt48`. (4) The whole suite at four harts and the `k` shard at 1, 2 and 4 are unchanged; the instruction counts of KT5 do not rise by more than 1% (the facade must inline).

**Rule for every later item.** Architecture-specific code written after this goes in `arch/riscv64/` behind an interface item added to `arch/mod.rs` and to the AArch64 stub in the same PR: vector state (PL2), Sstc and the timer (L13 if it lands first: move it here), the IMSIC (PL6), instruction-cache sync (PL7), idle and suspend entry (PM2, PM3), hart start and stop (PM1), kernel relocation (HD3), the syscall entry check (AB8). `libsys` gets `libsys/src/arch/riscv64.rs` for its one `ecall` sequence.

**Review.** Fable for AR1b (a large move through the trap path) and AR1c; default for a and d. A soak run is owed for AR1b.
