---
id: PL1
kind: slice
title: RVA23 in the gate: `rva23s64` the only test CPU, a boot-time extension check, the kernel compiled for the profile
area: platform
horizon: long
refs: [KERNEL-V1-SPEC, OD127]
---

**What.** OD127: the commitment goes into the gate so it cannot drift again.

**How.** (1) `scripts/os_boot_test.py`: `-cpu rva23s64` always; the `OS_CPU` override and the `os-boot-test-rva23` target go (it is now every boot). CI's QEMU must have the CPU model (the local QEMU 11.1 has it): check the runner's version first, and if it lacks `rva23s64`, install a QEMU that has it in the workflow (a cached build) in this PR; that was the condition OD47 left open. (2) `REQUIRED_EXTENSIONS` in the kernel: for each hart, parse `riscv,isa-extensions` (or the `riscv,isa` string) from the device tree and stop with `##PANIC missing extension <name>` when one of the list is absent: `sv39`, `svade`, `sstc`, `zicbom`, `zicboz`, `zba`, `zbb`, `zbs`, `v`, `zicntr`, and the AIA nodes. Confirm each name against the RVA23 profile text before it enters the table (OD47), and record the confirmation in the PR. (3) The kernel's target: `riscv64gc-unknown-none-elf` with `-C target-feature=+zba,+zbb,+zbs,+zicond` and without F, D, V use in kernel code (it already traps on use: FP3). (4) Sstc is used from here on where a timer is programmed in new code; the SBI timer leaves with L13.

**Acceptance, written first.** A boot with `-cpu rv64` (no profile) ends in `##PANIC missing extension` and the harness has a target that expects exactly that (`os-boot-test-cpu-neg`, soak). Every other target passes on `rva23s64`.

**Review.** Default, soak.

## Progress (2026-11-20)

Built on `epic/phase-4`: `kcore::isa` (`REQUIRED_EXTENSIONS`: svade, sstc, zicbom, zicboz, zba, zbb, zbs, v, zicond, zicntr, ssaia; Sv39 through `mmu-type`; a hart's `riscv,isa-extensions` or, failing that, its `riscv,isa` string) with host tests over two real QEMU trees (`virt-rva23s64.dtb`, `virt-rv64.dtb`, expectations cross-checked with `dtc`); `dtb::require_extensions()` called right after the kernel table and heap window are up, ending in `##PANIC missing extension <name> (hart <id>)`; `-cpu rva23s64` in every OS-kernel boot (the harness, `s4a_serial_test.py`, `bench_release.py`, the interactive targets); `os-boot-test-rva23` gone, `os-boot-test-cpu-neg` (soak) expects the refusal on `-cpu rv64`; the kernel is built with `+zba,+zbb,+zbs,+zicond` (`kernel/.cargo/config.toml`). Names confirmed against the profile text (`riscv/riscv-profiles` `rva23-profile.adoc`): Sv39, Svade, Sstc, Zicbom, Zicboz, Zicntr, V and Zicond are mandatory in RVA23S64; B is Zba, Zbb and Zbs; `ssaia` is OD127's addition, not the profile's. CI's QEMU is 10.2.1 and lists `rva23s64` (read from the soak log of 2026-11-19), so no QEMU install is needed. Decisions: `OS_CPU` stays as the one override, for the negative control only; the check for the AIA's device-tree nodes (aplic, imsic) is left to PL6, which removes the polling fallback the `os-boot-test-polling` target still boots. Verified: the host tests; one boot of `os-boot-test-cpu-neg` (the refusal seen) and `make kquick` (236 tests at four harts, exit 0) after the target-feature change. Not run: smoke (the epic gate), soak.
