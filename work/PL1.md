---
id: PL1
kind: slice
title: RVA23 in the gate: `rva23s64` the only test CPU, a boot-time extension check, the kernel compiled for the profile
area: platform
horizon: long
blocked_by: [KT1]
refs: [KERNEL-V1-SPEC, OD127]
---

**What.** OD127: the commitment goes into the gate so it cannot drift again.

**How.** (1) `scripts/os_boot_test.py`: `-cpu rva23s64` always; the `OS_CPU` override and the `os-boot-test-rva23` target go (it is now every boot). CI's QEMU must have the CPU model (the local QEMU 11.1 has it): check the runner's version first, and if it lacks `rva23s64`, install a QEMU that has it in the workflow (a cached build) in this PR; that was the condition OD47 left open. (2) `REQUIRED_EXTENSIONS` in the kernel: for each hart, parse `riscv,isa-extensions` (or the `riscv,isa` string) from the device tree and stop with `##PANIC missing extension <name>` when one of the list is absent: `sv39`, `svade`, `sstc`, `zicbom`, `zicboz`, `zba`, `zbb`, `zbs`, `v`, `zicntr`, and the AIA nodes. Confirm each name against the RVA23 profile text before it enters the table (OD47), and record the confirmation in the PR. (3) The kernel's target: `riscv64gc-unknown-none-elf` with `-C target-feature=+zba,+zbb,+zbs,+zicond` and without F, D, V use in kernel code (it already traps on use: FP3). (4) Sstc is used from here on where a timer is programmed in new code; the SBI timer leaves with L13.

**Acceptance, written first.** A boot with `-cpu rv64` (no profile) ends in `##PANIC missing extension` and the harness has a target that expects exactly that (`os-boot-test-cpu-neg`, soak). Every other target passes on `rva23s64`.

**Review.** Default, soak.
