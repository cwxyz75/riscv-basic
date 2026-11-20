---
id: MM3
kind: slice
title: The direct map uses 1 GiB and 2 MiB pages
area: kernel
horizon: long
blocked_by: [MM2]
refs: [KERNEL-V1-SPEC, OD138]
---

**How.** `kcore::pagetable`: `map_large(va, pfn, level, perms)` for leaf entries at levels 1 and 2 (kernel use only: user mappings stay 4 KiB), `translate` and `for_each_leaf` already report a level. `kmem::init` maps each RAM range with the largest aligned pages, 4 KiB at unaligned edges; the kernel image stays out of the direct map as today (M5: split the 2 MiB pages that would cover it down to 4 KiB and leave the image's frames unmapped there). Device pages stay 4 KiB.

**Acceptance, written first.** A host test of the range splitter (property: exactly the requested frames are mapped, each once, with the largest legal page). QEMU: `direct_map_does_not_alias_the_image` and `kernel_mappings_are_w_xor_x` (extended to walk large leaves); the count of page-table frames for the direct map at `-m 4G` is under 16. Boot time to the first `##TEST` line at `-m 4G` recorded before and after.

**Review.** Fable.

## Progress (2026-11-20)

Built on `epic/phase-4`: `PageTableFormat::block_entry` and `PageTable::map_large(va, pfn, level, perms)` (levels 1 and 2 in Sv39; aligned va and frame; refuses a leaf or a table in the way with `AlreadyMapped` or `HugeConflict`, a missing level with `BadLevel`, and changes nothing on `NoMemory`; user mappings never call it), `kcore::pagetable::plan_range` (the splitter: the largest aligned pages up to a maximum level, small pages at unaligned edges), and `kmem::init` maps the direct map with it. Host: `kcore/tests/large_pages.rs` (the splitter's property over random ranges; a 4 GiB direct map in both formats, the second being the test-only AArch64-like format: every frame translates to itself, the leaf walk equals the plan, fewer than 16 table frames; every refusal; one mutation check: a splitter that ignores the range end is caught by two tests). The two selftests that assumed 4 KiB leaves now allow a large leaf only in the direct map and only as read-write data (`kernel_mappings_are_w_xor_x`), and count pages rather than leaves (`direct_map_does_not_alias_the_image`). Verified: host tests; `make kquick` (237 tests at four harts, exit 0). **Not done: the boot-time measurement at `-m 4G`**: the kernel's RAM size is still the constant `abi::layout::RAM_SIZE` (256 MiB), so a 4 GiB machine cannot boot until PL4 (discovery); the table-frame bound is proved on the host at 4 GiB instead, and the boot-time before and after belongs to PL4's acceptance. With 256 MiB the direct map is now about 130 leaves instead of 65,000.

Fable review (2026-11-20), no bug found; fixed in the PR: the selftest now proves the direct map really uses 2 MiB pages (at least 100 of them, under 1200 leaves) and that no page of the image or firmware is reachable through it (the whole range, page by page); `map_large` refuses a user page (`BadPerms`); `plan_range` clamps its maximum level; the refusal tests gained level-2 alignment, a non-canonical address, a user page, and "one frame short for the tables changes nothing" (two mutants the review named, a level-independent span and a missing reservation, are caught; an earlier edit of the test had silently not applied, which the mutation check exposed); the `-m 4G` measurements are now in PL4's acceptance. Left: a per-format maximum block level (`BadLevel` allows the top level for every format; matters when the AArch64 format becomes real).
