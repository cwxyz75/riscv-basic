---
id: X-SPECULATION
kind: topic
title: Speculative-execution exposure and mitigations
area: security
horizon: someday
refs: [KERNEL-V1-SPEC, OD139]
---

Recorded by OD139, not designed: every address space maps the kernel half including the direct map of all RAM; there is no branch-predictor isolation between user and kernel or between processes; syscall arguments index tables with no speculation barrier; the `time` counter is readable from user mode at full resolution. QEMU cannot show whether a mitigation works, so this waits for a hardware target. First questions when it is taken up: which RVA23 cores are affected by which class, whether the direct map can leave user address spaces, and whether `scounteren` should gate `time` per job.
