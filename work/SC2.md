---
id: SC2
kind: slice
title: Placement without visiting every hart; scaling measured at 16 and 32 harts
area: sched
horizon: long
blocked_by: [L20]
refs: [KERNEL-V1-SPEC, SCHED-SMP-SPEC]
---

**How.** KERNEL-V1-SPEC 5.2. Global `AtomicU64` masks `idle` and `overloaded`, maintained under each hart's `RQ` lock when its state changes. `select_hart`: last hart if it would run there; else `idle & online & affinity` lowest set bit; else the minimum of the published `cur_eff` hints. `settle` no longer locks every other hart at each event (T156): it pulls only when its own queue is empty or `overloaded != 0`, from harts in that mask, in ascending order, re-checking under both locks as now. GP1 and the reference tables are unchanged and must still pass. Then measure: `pair_scaling` (L1) for futex pairs, `channel_call` pairs and page-fault pairs at 4, 16 and 32 harts, with the lock-wait trace; record the numbers in `TESTING.md`; evaluate SCHED-SMP-SPEC 3.13's trigger at 16 harts and record the result as a decision (split `OBJ`: a new plan; or not: T70 stays someday).

**Acceptance, written first.** A counter shows zero cross-hart lock acquisitions per scheduling event when no hart is overloaded. The shuttle models (`rq_concurrency.rs`, the idle model) pass with the masks, and a mutation that fails to set `overloaded` is caught (a thread starves in the model). The scaling numbers exist.

**Review.** Fable, soak. Closes T156.

## Progress (2026-11-23, phase 10)

**First step built:** the three masks (`kcore::sched_smp::Masks`, published in the run-queue guard's drop), `select_hart` through `idle & online`, the pull through `overloaded` only, the counter `pull_looks`, host tests `sched_masks.rs` (zero looks while no hart is overloaded at 16 harts; one look for one overloaded hart), the negative control `fault-skip-overloaded` (caught by the GP1 walk and by `only_the_hart_with_a_waiting_thread_is_looked_at`). **Left:** the scaling measurements at 16 and 32 harts (`pair_scaling` for futex, channel_call and page-fault pairs, with the lock-wait trace, recorded in `TESTING.md`) and the evaluation of SCHED-SMP-SPEC 3.13's trigger (a decision): QEMU runs at 16 and 32 harts and the owner's soak; `push_marks` still reads every hart's hint (no lock: the spec allows it). T156 closes with the measurements.
