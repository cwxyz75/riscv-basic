---
id: HD5
kind: slice
title: Reserved ABI space for shadow stacks, landing pads and pointer masking
area: security
horizon: long
blocked_by: []
refs: [KERNEL-V1-SPEC, OD139]
---

**How.** KERNEL-V1-SPEC 7.5. No mechanism; only places, so adding the mechanisms later changes nothing that exists. `abi`: `process_map` flag bits 8 (SHADOW_STACK) and 9 (reserved) are named and refused with NOT_SUPPORTED; `thread_read_state` kind 4 (CFI: shadow stack pointer, landing-pad expectation) is named and NOT_SUPPORTED; the start arguments gain a 64-bit `features` word that must be zero (bit 0: landing pads, bit 1: shadow stack, bit 2: pointer masking), checked by `process_start`. `kcore::usercopy` already refuses a pointer with non-zero upper bits: add the test that pins it and a comment naming the reserved flag. Record the speculation exposure as a topic item (X-SPECULATION, someday): the direct map in every address space, no branch-predictor isolation, the user-readable `time` counter.

**Acceptance.** Each reserved value is refused with the stated status and changes nothing (suite tests; rows in `syscall-tests.tsv`).

**Review.** Default.
