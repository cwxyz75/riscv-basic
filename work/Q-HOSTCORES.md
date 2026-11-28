---
id: Q-HOSTCORES
kind: decision
title: Boots with more harts than the host has cores: change the runs, or the big lock
area: testing
horizon: near
epic: stability
refs: [T266, T212, OD95, STAB0]
---

**The question.** The owner's checkpoint of 2026-11-28 ran the eight-hart release-checked boot and the chaos pieces on hosted runners
(four cores) for the first time. Both stalled. The cause is in T266 ("Fixer (2026-11-28)"): with more harts than host cores, each
hand-over of the big lock waits until the host runs the hart whose turn it is, so lock-heavy tests run up to a thousand times slower.
The most likely mechanism is waiter preemption of the FIFO ticket lock (OD95). PR 625 bounded the exclusion test and made
`smp_churn_stress` report its progress. Even so, an eight-hart kernel-only boot on a four-core runner takes many minutes, and its
timing tests fail. Which way should it go?

1. **Change the runs.**
   - Run eight-hart boots only on runners with eight cores or more (a larger runner, or the owner's own machine).
   - Run the chaos pieces one boot at a time, or with no more harts in flight than the runner has cores (today: three boots of two,
     four and eight harts at once on four cores).
   - Optionally, the harness refuses or skips a boot with more harts than cores, instead of only noting it on failure.
2. **Change the lock.** A big lock that tolerates a waiter the host is not running: spin for a while, then wait for an interrupt and
   be woken by the release, as paravirtual spinlocks do. Or a test-and-set fallback, as Linux uses as a guest without them. This
   changes OD95's ticket lock and SCHED-SMP-SPEC, and it costs an IPI on a contended release.
3. **Both.**

**What would settle option 2 first.** Measure the exclusion test with a test-and-set lock beside the ticket lock, under the same host
load: sixteen CPU hogs on the laptop, eight harts. That shows whether the FIFO order is what multiplies the cost.
