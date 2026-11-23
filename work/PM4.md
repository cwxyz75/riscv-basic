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

## Built (2026-11-23, phase 10)

`kcore::sched_smp::Policy` (`Spread`, `Pack`), one byte read at each wake (`select_hart` dispatches to `select_spread` or `select_pack`), `system_set_placement` (115, the root resource with MANAGE; `debug_stat` 125 reads the policy), the GP1 and GP2 random walks run under both policies (`gp1_and_gp2_hold_under_the_pack_policy_too`), a placement test (`sched_masks.rs`), the suite test `system_set_placement_needs_the_resource_and_changes_the_policy`, and the QEMU acceptance (`pack_keeps_light_threads_on_few_harts`: two light periodic threads at four harts, at most two harts take wakes under pack, two under spread). Two deviations from the text above, both recorded in WORK-PLAN.md: the policy is an enum and two methods, not a trait (the trait comes with a third policy), and `pack` pulls exactly as `spread` does (the rule "an idle hart pulls only from a hart with more than one queued thread" would leave a waiting thread queued while a hart idles, which GP1 forbids; pack's saving is in where wakes go). `rt_latency` under `pack` stays to be measured with the soak.
