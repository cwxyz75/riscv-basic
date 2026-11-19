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
