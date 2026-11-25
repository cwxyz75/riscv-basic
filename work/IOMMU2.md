---
id: IOMMU2
kind: slice
title: iommucore: the RISC-V IOMMU's data structures as a pure crate (directory table, page tables, MSI page tables and MRIFs, queues, I/O address allocator)
area: drivers
horizon: long
complexity: H
change_risk: M
epic: iommu-kernel
blocked_by: [IOMMU0]
refs: [T42, OD148, TESTING, IOMMU1, IOMMU7, Q-IOMMU-IRQ]
---

Child 2 of the IOMMU epic (`work/T42.md`). The hardware formats are fixed by the ratified RISC-V IOMMU specification, not by the
owner's choices, so this slice waits only for the facts record (IOMMU0) and may run beside the spec (IOMMU1). First feature PR on
`epic/iommu-kernel` (cut the epic for it; a pure crate, so the feature gate is its whole test).

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B): the crate also owns the MSI translation formats and the simulated device's MSI
path (module 7 and the additions marked "MSI" below); IOMMU7 builds on them. They are hardware formats too, so the slice still
waits only for IOMMU0 (its items 10 to 12 for these parts).

## Plan (2026-11-25)

### What is built

A new top-level crate `iommucore/`: `no_std`, `#![forbid(unsafe_code)]`, no dependency on `kcore` (so its mutation run is small and
fast), added to the workspace, to `make os-test`, to the Miri list and to `make os-mutants` (`PKG=iommucore`). It owns every bit
layout; the kernel (IOMMU5) owns every volatile access. Narrow traits are its only window out:

- `PhysMem64` (read and write a 64-bit word of physical memory by address, and, for MSI, atomic or-and-and-fetch on a word: the
  kernel's direct map, the tests' fake),
- `TableFrames` (give one zeroed frame, take one back: the kernel's frame allocator with the charge already taken),
- `Regs` (read and write a 32-bit or 64-bit register by offset: MMIO in the kernel, a simulated device in tests).

Modules:

1. `regs`: register offsets and field codecs (`capabilities`, `fctl`, `ddtp`, `cqb`/`cqh`/`cqt`/`cqcsr`, `fqb`/`fqh`/`fqt`/`fqcsr`,
   `ipsr`, `icvec`, the debug translation registers), a `Caps` decoder with a `check(required) -> Result<(), Missing>` that names
   the first missing capability (the boot refusal's text comes from here).
2. `ddt`: the device directory table in 1, 2 and 3 levels, base (32-byte) and extended (64-byte) device contexts, the device-id
   split for each, `set(device_id, DeviceContext)`, `clear`, `get`, lazy leaf pages that are returned when empty. A
   `DeviceContext` type whose constructors make only three shapes: **invalid**, **deny** (valid, an empty table, fault reporting
   off) and **translate(root, mode)**; there is no way to build a context with both stages Bare. MSI: `translate`
   takes the MSI page table's root, mask and pattern (the extended format's `msiptp`, `msi_addr_mask`, `msi_addr_pattern`); `deny`
   has `msiptp` off.
3. `iopt`: I/O page tables for Sv39, Sv48 and Sv57 (first stage) and the x4 second-stage variants (the 16 KiB root), 4 KiB leaves:
   `map(iova, pa, access)`, `unmap(iova) -> Option<pa>`, `translate`, `for_each_leaf`, `destroy`; leaves always written with A and
   D set (IOMMU0: the device does not set them) and with the U bit as the specification requires for the stage (IOMMU0 item 3);
   empty tables are freed, so a map and unmap sequence returns every frame. `unmap` and `destroy` never ask `TableFrames` for a
   frame. Compare with `kcore::pagetable` (generic over `PageTableFormat`, Sv39 only): reuse the shape, not the code, because the
   leaf bits and the root size differ and the crate must not depend on `kcore`.
4. `cmdq`: command encodings (`IOTINVAL.VMA`, `IOTINVAL.GVMA`, `IODIR.INVAL_DDT`, `IOFENCE.C` with PR and PW) and the ring
   arithmetic of the command queue (full, empty, wrap) as a pure state machine: `push(cmd) -> Result<tail, Full>`,
   `completed(head)`.
5. `faultq`: the fault record decoder (cause, transaction type, device id, I/O address) and the consumer side of the ring,
   including the overflow and memory-fault bits of `fqcsr`.
6. `iova`: the I/O address allocator: ranges of pages in a fixed window, never page 0, a **pending** state for a range that was
   unmapped but whose fence has not completed (it cannot be handed out again), no allocation on free (a bitmap sized at domain
   creation: the T177 rule). The allocator does not bound the number of mappings: a mapping's own record lives in `kcore`
   (IOMMU3; OD166 point 5 removed the fixed 64-entry mapping table).
7. `msi` (MSI): the address check (mask, pattern) and the interrupt-file-number extraction (the specification's bit-extract
   function, also in QEMU as `riscv_iommu_pext_u64`); the flat MSI page table (`set(index, entry)`, `clear`, `get`); the 16-byte
   entry, whose only constructors are **invalid** and **mrif(file address, notice page, notice identity)**: there is no
   constructor for a basic-translate entry outside a `cfg(test)` or negative-control feature (IO11 by construction, as the
   context type does for pass-through); the 512-byte memory-resident interrupt file as pending and enable words per 64
   identities, with `enable(id)`, `disable(id)`, `take_pending(id) -> bool` and `stray() -> bitmap` over the atomic operations of
   `PhysMem64`.
8. `sim` (behind `cfg(test)` or a `sim` feature used by `kcore`'s and `ksys`'s tests later): a small simulated IOMMU over `Regs`
   and `PhysMem64` that walks the real structures the way a device access would (directory, context, page table), keeps a
   translation cache that is only dropped by the invalidation commands, and executes the command queue. IOMMU3's model tests use
   it as the "hardware". MSI: a device **write** that passes the address check goes through the MSI page table exactly as QEMU's
   `riscv_iommu_msi_write` does (invalid entry, basic entry, MRIF update, notice, the fault causes), with an **MSI-entry cache**
   that only the invalidation commands drop (QEMU has none; real hardware may), and it records every message that reaches a
   simulated hart file (page, identity). IOMMU7's model uses that record as its oracle. The simulated device can be **handed
   over in any mode** (Off, Bare, each directory mode, with its queues on or off) and models the `ddtp` busy bit, so that the
   bring-up sequence of IOMMU5 (IOMMU-SPEC 2.4 step 1: an IOMMU not Off is written Off first; OD166 point 11) has a host test;
   it records every value written to `ddtp`, so a test can assert that Bare was never among them.

### Oracles (independent where possible)

- **Spec-derived vectors.** Encodings of every command, device context and fault record taken from the specification's tables
  and from QEMU's own qtest (`tests/qtest/riscv-iommu-test.c`) and headers (`hw/riscv/riscv-iommu-bits.h`), transcribed as bytes
  into `iommucore/tests/vectors.rs` with their source line (IOMMU0 item 9). QEMU's headers are the independent implementation. MSI: the entry and context field
  positions from `riscv-iommu-bits.h` (`MSI_PTE_V`, `_M`, `_MRIF_ADDR`, `_PPN`, `_C`, `MSI_MRIF_NID`, `_NPPN`, `_NID_MSB`, the
  `MSIPTP` fields) and the file layout from the AIA specification's table.
- **A reference model.** `tests/iopt_model.rs` and `tests/ddt_model.rs`: random map, unmap, set and clear sequences against a
  `BTreeMap` reference; after every step `translate` agrees for sampled addresses, and the frame count equals what the reference
  predicts (exact table-frame counts, baseline at the end: the M3 shape).
- **Two walkers.** The `sim` walker (written from the specification's translation algorithm, top down) against `iopt::translate`
  (written with the mapper): a differential test over random tables, including deliberately corrupt ones (a non-leaf with R set, a
  leaf at the wrong level, reserved bits): the two must agree on fault or address.
- **MSI model** (`tests/msi_model.rs`): random tables, files and device writes; the simulated device's result (delivered where,
  with what identity, or which fault) against a reference written from the specification's "process to translate addresses of
  MSIs"; the bit-extract function against a naive loop over every mask of a byte.
- **Property tests** for `iova` (no two live ranges overlap; a pending range is never returned; everything comes back) and `cmdq`
  (never overwrites an unconsumed command; wrap at every size).
- **Later, in the machine** (IOMMU5): the device's debug translation interface, if IOMMU0 finds it works, checks the same tables
  from QEMU's side.

### Mutation-check plan

`cargo mutants -p iommucore` in full once, at the end of the slice (a targeted experiment, not a routine; afterwards it is part of
`make os-mutants`, which the overnight tier runs when the owner asks). Survivors are fixed or listed in `mutants-accepted.txt` with
a reason. Hand-written mutations that must each fail a named test, recorded in the PR: a leaf written without A or D; W without R;
the U bit flipped; `unmap` that leaves the leaf; a table not freed when empty; the device-id split of the extended format used for
the base format; a deny context encoded as Bare; `IOFENCE.C` without PR and PW; the queue's full test off by one; `iova` handing
out a pending range; a fault record's device-id field shifted; (MSI) the notice identity's high bit dropped; the enable test
skipped; the mask and pattern check inverted; an MRIF entry encoded with mode 3; the file's pending word for identities 64 to 127
written at the wrong offset.

### Acceptance

The feature gate is green with the new crate in it (format, clippy, host tests, Miri is soak-tier and not run for this); the
vectors, the two models, the differential test and the property tests exist and pass; the hand mutations above were each seen to
fail a test; the full mutants run has no unexplained survivor. No kernel code changes; no boot.

### Hazards

- The base and extended context formats: QEMU reports MSI_FLAT, so the machine uses the extended one; the crate must still encode
  both, and choosing by capability is IOMMU5's job. A mix-up shows as every device faulting with "DDT entry misconfigured".
- The first-stage U-bit and A/D rules are from memory in this plan; if IOMMU0 leaves them "not established", this slice stops
  before `iopt` is finalised rather than guessing.
- A simulator written by the same context as the tables can share a misreading: that is why the vectors come from QEMU's headers
  and why IOMMU5 re-checks in the machine.

PR size: large (a crate of roughly 2,500 to 3,500 lines with its tests). Review: default reviewer (pure code, no kernel state).
