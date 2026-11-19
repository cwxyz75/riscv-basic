---
id: KT7
kind: slice
title: Schedule perturbation: seeded random delays at lock and inter-hart-interrupt points
area: testing
horizon: long
blocked_by: []
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** QEMU's scheduling of harts is too regular to shake out races. Perturb it on purpose.

**How.** A kernel feature `chaos` (no code without it). A per-hart xorshift generator seeded from the boot argument `chaos=<seed>` and the hart index. Hook points: `klock::Machine::acquired` and `released` (the kernel's implementation), before `send_ipi`, at `schedule()` entry, at the return to user mode. At each, with probability 1/8, spin for a random 0 to 2000 iterations of `spin_loop` (interrupts stay as they are). `make os-chaos SEEDS="1..200"`: the `k` shard at `-smp 2`, `4` and `8`, one boot per seed and hart count, `SOAK_JOBS` parallel; a failure prints the seed. It is an overnight target.

**Acceptance, written first.** With `chaos`, the existing negative-control kernels that hide a race (`fault-skip-poke` when L11d has it, `fault-skip-shootdown`) fail within 20 seeds at `-smp 4`; record how many seeds each took. The same seed gives the same delay sequence on one hart (a host test of the generator).

**Review.** Default.
