# AR1a: where RISC-V leaks outside the architecture boundary (audit, 2026-11-20)

The checklist for AR1b (the move), AR1c (page tables) and AR1d (the ABI side). Produced by searching `kernel/src`, `kcore/src`,
`abi/src`, `ksys/src`, `rt/src` and `libc/src` for `asm!`/`global_asm!`, `csr*`, `sfence`, `ecall`, `wfi`, the CSR names `satp`, `sstatus`,
`scause`, `stvec`, `sepc`, `stval`, `sscratch`, `sie`, `stimecmp`, SBI, `Sv39`, and the literal cause numbers. Counts are matching lines.
No code changed in this slice.

## Result in one line

`ksys`: clean (no match; it was written behind the seam). `kcore`: only the page-table format (`pagetable.rs`, Sv39 in the doc and the
canonical-address rule) and doc comments naming `sfence.vma` or `sstatus.FS` (`aspace.rs`, `fp.rs`). `abi`: the Sv39 address layout
(`lib.rs` 387 to 450). Everything else is in the kernel crate (and the user-side crates `rt` and `libc`).

## Kernel crate: each file and where it goes (`kernel/src/arch/riscv64/` unless said)

| File (matches) | What is RISC-V in it | Destination |
|---|---|---|
| `boot.rs` | entry assembly, early paging (`satp`), hart bring-up, DTB handoff | `arch/riscv64/boot.rs`; the generic part (`kmain`) stays |
| `trap.rs` (47 CSR names, 20 `csr*`, 6 `asm!`) | trap vector assembly, `TrapFrame` with `scause`/`sepc`/`stval` fields, cause decode | assembly and frame to `arch/riscv64/trap.rs`; the handler outside receives `arch::Fault { kind, address, access }` or `arch::Syscall` |
| `user/trap.rs` (16) | reads `frame.scause`, `frame.sepc`, `frame.stval` to dispatch syscalls and faults; `csrw satp` on the switch; `ecall` step | decode moves behind `arch::Fault`/`Syscall`; the `satp` write becomes `arch::aspace_switch(root, asid)` |
| `user/lifecycle.rs` (13) | `enter_user` assembly (`csrw satp/sepc/sscratch`, `sret`) | `arch/riscv64/usermode.rs`: `arch::enter_user(entry, sp, arg, aspace, ...)` |
| `thread.rs` (7) | context-switch assembly, `csrc sstatus` (FS and VS off), `wfi`, `csrr sstatus` | `Context` and `switch` to `arch`; `cpu_idle` (wfi); FP/VS disable inside `MachineState` init |
| `fp.rs` (20) | FP save/restore, `sstatus.FS` | `arch/riscv64/fp.rs` as the `MachineState` unit; the policy (`kcore::fp`) stays |
| `kmem.rs` (13, 9 `sfence`) | `satp` read/write, `sfence.vma` (page and all), root install | `tlb_flush_page`, `tlb_flush_asid`, `aspace_switch`, `aspace_current` in `arch` |
| `kstack.rs`, `heap.rs` (1 `sfence` each) | `sfence.vma zero, zero` after mapping | `arch::tlb_flush_all` |
| `smp.rs` (11) | SBI HSM hart start, IPI through AIA, `ecall`, `sfence` shootdown | `cpu_start`, `ipi_send`, `tlb_shootdown` |
| `timer.rs` (12) | `time` CSR, `stimecmp` (0x14d), Sstc probe | `clock_read`, `timer_set` (the Sstc probe is the RVA23 check of 2.1, also in `arch`) |
| `irq.rs` (8) | `sstatus.SIE`, `sie.STIE` | `irq_save`, `irq_restore`, `irq_enable_timer` |
| `aia.rs` (9 `csr*`) | APLIC/IMSIC and the AIA CSRs | `arch/riscv64/aia.rs` behind `trait IrqChip` |
| `percpu.rs` (4) | `tp` is the per-hart pointer, `sscratch` holds it in user mode, last `satp` | `this_cpu()` and the `tp` binding in `arch`; the `PerHart` struct stays generic except the `satp` cache field (becomes `AspaceToken`) |
| `finisher.rs` | `wfi` after the exit device write | `arch::cpu_idle` / `system_reset` |
| `selftest.rs`, `user_tests.rs`, `smp_tests.rs`, `sched_tests.rs` (4+2+1+1) | tests that assert `satp` mode 8, raw `scause` 13 and 15, the Sv39 upper half | assert through `arch::Fault` kinds and `arch::aspace_current`; the Sv39 address checks move to `arch` selftests |
| `main.rs`, `chaos.rs`, `console.rs`, `dtb.rs`, `latency.rs`, `bkl.rs`, `interactive.rs` | none (comments only in `main.rs`) | stay |

## Other crates

| Where | What | Destination |
|---|---|---|
| `kcore/src/pagetable.rs` | the Sv39 entry layout, levels, canonical-address rule, the `sfence.vma` caller contract in a doc comment | AR1c: `PageTableFormat` with `Sv39`; the doc says "flush the TLB" |
| `kcore/src/aspace.rs:634` | the same doc comment | AR1c (reword) |
| `kcore/src/fp.rs` | `sstatus.FS` named in docs and a method doc (the policy is generic: Off/Clean/Dirty) | reword to "the FP unit state"; the RISC-V encoding belongs to `arch` |
| `abi/src/lib.rs` 387 to 450 | kernel address layout "(Sv39, upper half)" and its test | AR1d: `abi::arch` holds the user range, stack arena and thread-state layouts; the upper-half test stays with the Sv39 format |
| `ksys` | nothing | check added in AR1 acceptance (1) keeps it so |
| `rt/src/lib.rs` | the `ecall` stub, `global_asm!` start code | becomes `libsys/src/arch/riscv64.rs` (the one `ecall` sequence); `rt` keeps the call table |
| `libc/src/{thread,stdio,spawn,setjmp,posix2,wchar}.rs` | `asm!`/`global_asm!` (thread start, setjmp/longjmp, syscall stubs, one `csr`) | per-architecture directory in libc (`libc/src/arch/riscv64/`); the check covers `kernel/src`, `kcore`, `ksys` only, `libsys` and `libc/src/arch` are exempt |

## Facts the audit found that change the plan

1. `kernel/src/user/trap.rs` and `user/lifecycle.rs` hold RISC-V code (trap decode, `enter_user`). They were not "arch" in the old layout;
   AR1b moves them too, which is why the decode must come first (`arch::Fault`, `arch::Syscall`), then the move.
2. `PerHart` has a `satp` cache and its doc names `sscratch`: the field becomes an opaque `AspaceToken`.
3. The four test files above assert raw cause numbers (13, 15). They are rewritten against `Fault` kinds in AR1b, not left behind the check.
4. The SBI is used only from `smp.rs`; there is no console SBI call outside the boot path.
5. `libc` has six files of inline assembly. AR1's check does not cover `libc` (it is user-side, built per architecture from the same
   source tree), but the same rule applies: new assembly goes in `libc/src/arch/`.
