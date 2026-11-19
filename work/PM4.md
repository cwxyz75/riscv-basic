---
id: PM4
kind: slice
title: Placement is a policy: `spread` and `pack`, `system_set_placement`
area: sched
horizon: long
blocked_by: [SC2]
refs: [KERNEL-V1-SPEC, OD132]
---

**How.** KERNEL-V1-SPEC 5.6. `kcore::sched_smp`: `trait Placement { fn select(&self, view: &HartView, thread: &ThreadSched, hint: WakeHint) -> usize; fn should_pull(&self, view: &HartView, me: usize) -> bool; }` over the published hints and masks; `Spread` is SC2's rule; `Pack` picks the lowest-indexed online hart whose published running priority is below the thread's or that is idle with a lower index than any busy one, falling back to `Spread` when every hart is busy, and pulls only when idle and the source hart has more than one queued thread. The policy is an atomic selector read at each decision; `system_set_placement(resource, policy)` (root resource) changes it. Both must satisfy GP1 at quiescence (a higher-priority thread is never left queued while a lower one runs on a hart it may use).

**Acceptance, written first.** The scheduler conformance and GP1 models run for both policies. A QEMU test with two periodic light threads on four harts: under `pack` at most two harts leave idle over a second (per-hart idle counters), under `spread` the threads sit on separate harts. RT latency (L1's `rt_latency`) under `pack` stays within B3.

**Review.** Default.
