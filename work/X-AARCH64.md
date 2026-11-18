---
id: X-AARCH64
kind: topic
title: A second architecture (AArch64)
area: platform
horizon: someday
---

Approach if ever wanted: an in-tree `arch` layer, not kernel forks. Refactor on RISC-V first (arch code behind one module: boot, trap frame, context switch, per-CPU register, timer, IPI, TLB and cache maintenance, FP state; the page-table code in `kcore` made generic over a PTE format; an interrupt-controller trait; a barrier abstraction), then bring Arm up under `qemu-system-aarch64 -M virt` and run the same suite as the oracle. First step would be a read-only audit of RISC-V assumptions leaking into `kcore` and `user.rs`. Needs its own profile-floor decision beside OD47.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
