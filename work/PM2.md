---
id: PM2
kind: slice
title: Idle states: a long idle suspends the hart through firmware
area: sched
horizon: long
refs: [KERNEL-V1-SPEC, OD132]
---

**How.** KERNEL-V1-SPEC 5.5. In the idle loop, after programming the timer: if the next deadline is more than 1 ms away (or there is none), call SBI HSM `hart_suspend` with the default retentive type instead of `wfi`; on firmware without it, `wfi`. The threshold is one constant. Per-hart counters: time idle, entries by kind.

**Acceptance.** Counters show retentive suspends on an idle machine and none while a 100 us periodic sleeper runs; the idle shuttle model is unchanged (the suspend has `wfi`'s wake rule); `wake_latency_event` from idle is recorded with and without. On QEMU the two are equivalent in effect: say so.

**Review.** Default.
