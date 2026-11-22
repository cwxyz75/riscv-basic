---
id: X-SPECULATION
kind: topic
title: Speculative-execution exposure
area: security
horizon: someday
---

What this kernel does not defend against, recorded so that it is a known gap and not a surprise (KERNEL-V1-SPEC 7.5, HD5): the direct map is present in every address space (a speculative load from a user process can reach kernel memory the architecture says is inaccessible, on a core that has the problem); there is no branch-predictor isolation between processes or between user and kernel (no barrier at a switch, no retpoline-style code); the `time` counter is readable from user mode (a high-resolution clock for cache attacks, `clock_get` is not the only source). The reserved ABI places of HD5 (shadow stacks, landing pads, pointer masking) are the control-flow half of the same hardening; this item is the data half.

(Not specified, not scheduled, not committed. A first slice would be to decide which cores the kernel targets (RVA23 has no known speculative-leak errata list yet), measure `time` access from user mode under `scounteren`, and decide whether the direct map can leave the user address spaces at the cost of a switch of tables at every trap.)
