---
id: HD3
kind: slice
title: Address randomisation for the kernel
area: security
horizon: long
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 7.3. The kernel links position independent; early boot (already position independent after PL4) picks `slide` = a random multiple of 2 MiB such that the image stays inside its two top-level slots, applies the image's `R_RISCV_RELATIVE` relocations with it, and builds the early table at the slid address. The direct map's base becomes a boot-time value: a random multiple of 1 GiB among the positions where the RAM span fits between slot 256 and the kernel-stack window; `DIRECT_BASE` and `KERNEL_OFFSET` leave `abi` and become `kmem` values read once (every `DIRECT_BASE + pa` goes through `kmem::phys_to_virt`). The entropy for these two choices comes from the device tree seed read by a minimal parser in early boot (before the pool exists). Panic and fault reports print addresses minus the slide in test builds (so logs stay comparable) and nothing address-like in release builds. Test builds honour `kaslr=off`.

**Acceptance, written first.** Ten boots give at least six distinct slides and at least four distinct direct-map bases (reported through `debug_stat` in test builds). `kernel_mappings_are_w_xor_x`, `direct_map_does_not_alias_the_image` and the trap-entry stack-guard arithmetic pass at every slide (a soak run of 20 seeds). The stated entropy (about 9 and 7 bits) is recorded in `INVARIANTS.md` next to the check.

**Review.** Fable, soak.
