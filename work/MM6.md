---
id: MM6
kind: slice
title: VMOs by byte size with no cap; a VMO knows its mappings; `vmo_set_size`, `vmo_decommit`
area: kernel
horizon: long
refs: [KERNEL-V1-SPEC, OD133]
---

**How.** KERNEL-V1-SPEC 3.5, 4.3. `vmo_create(bytes, job, flags)` (RESIZABLE); `MAX_VMO_PAGES` and `MAX_REGION_PAGES` are gone (PL8 removes the constants; here the code stops needing them: region unmap and teardown are already chunked by L17/L19). Each VMO keeps a list of its mappings (address-space reference, base, length, VMO offset), maintained by map, unmap and split. `vmo_decommit(vmo, offset, length)`: WRITE; under the family lock collect the pages, then for each mapping (address-space lock, one at a time, ascending by process id) clear the PTEs, batch the shootdown, drop the mapping references, then drop the VMO's references and credit the job. `vmo_set_size(vmo, bytes)`: growing changes the size only; shrinking decommits the tail and shortens every mapping's backed length, so a later touch beyond it is a fault (exception kind ACCESS). Both refused for DMA and device VMOs and, for `set_size`, for a VMO with children.

**Acceptance, written first.** `kcore/tests/vmo_ops_model.rs`: random create, map, touch, decommit, resize, unmap against a byte-level reference (decommitted ranges read zero; frame count by formula; ledger exact), mutation-checked. QEMU: a 2 GiB VMO mapped and sparsely touched at `-m 4G`; decommit while a sibling thread touches the range (reads see old data or zeros, never another frame: a negative control `fault-decommit-no-shootdown` must fail); shrink then touch is a clean process end. The C library's `malloc` returns memory with decommit (a `##BENCH`-free check: a program that allocates and frees 64 MiB ends with its job's page usage near its start).

**Review.** Fable, soak.

**Precondition (2026-11-24, owner's decision):** before building the rest of this item, have Fable write a design plan and append it here (a section "Plan (Fable, date)"): the VMO size cap and per-VMO mapping lists, what they change in the frame accounting guarantees, the committed-page map's sparse cost and the VMO id generation wrap, and the order of the PRs.

**Plan (2026-11-24):** the design plan the precondition asks for is in `work/T207.md`, section "Plan (Fable, 2026-11-24)".
