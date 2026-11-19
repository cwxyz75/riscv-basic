---
id: AB2
kind: slice
title: Nanosecond time: every deadline and `clock_get` in nanoseconds, one flag-day change
area: sched
horizon: near
refs: [SCHED-SMP-SPEC, KERNEL-V1-SPEC, OD131]
---

**What.** Decided on 2026-11-14 (SCHED-SMP-SPEC 4.11, Q3) and restated by KERNEL-V1-SPEC 3.4; the kernel and `SYSCALLS.md` still use 1 ms ticks.

**How.** `clock_get` returns nanoseconds since boot from the `time` counter and the timer frequency (a constant 10 MHz until PL4 reads it from the device tree: use one function `timebase()` so PL4 changes one place). Every deadline argument (`object_wait`, `port_wait`, `futex_wait`, `channel_call`, kernel sleeps) is absolute nanoseconds; `u64::MAX` is never (`abi::DEADLINE_NEVER`). Inside the kernel, deadlines are stored in nanoseconds and, until L13, rounded **up** to the next tick when queued, so nothing fires early. `job_set_sched`'s budget is already nanoseconds. Userland in the same PR: `rt::time`, the C library's clocks, sleeps and timeouts, every server's deadlines (`netd` timers, `consoled`), the suite's timing tests. Remove `ticks_to_ns` shims.

**Acceptance, written first.** `sleep_never_wakes_early` (1000 sleeps of random length: each ends at or after its deadline); a deadline of `u64::MAX` waits; a deadline in the past returns TIMED_OUT at once; the C transcript's `clock_gettime` differences are in nanoseconds against the host oracle. The suite at 1, 2, 4 harts.

**Review.** Default, soak (it touches every wait).
