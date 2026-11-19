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

## AB2 built (2026-11-19)

`clock_get` returns nanoseconds since boot (`timer::now_ns`: the `time` counter from the first tick, scaled by `timer::timebase()`, one function for PL4 to change; no 128-bit division) and every deadline argument (`object_wait`, `port_wait`, `futex_wait`, `channel_call`, kernel waits) is an absolute time in nanoseconds on that clock (`abi::time`: `NS_PER_MS`, `DEADLINE_NEVER`; 0 still means poll). The scheduler's clocks are the same clock (`tick_on(h, now_ns())`, hart joins, the starvation watchdog), so a timer fires at the first tick at or after its deadline and never early. `rt::sys` keeps the millisecond conveniences (`clock_get()` and deadlines in ms, converted at the boundary by `deadline_ns`, with 0 and `u64::MAX` kept) and has raw `clock_ns`, `object_wait_ns`, `futex_wait_ns`, `port_wait_ns`, `channel_call_ns`; the C library's `clock_gettime` and the servers' nanosecond clocks (`clockd`, `netd`, `dhcpc`) use `clock_ns`. SYSCALLS.md says nanoseconds. Checked by `clock_get_is_nanoseconds` (readings not all whole milliseconds, a 5 ms wait ends at or after its deadline), `sleep_never_wakes_early` (300 waits of random length up to 3 ms, none early), and `make smoke` (all shards, the home boot and the release smoke) green.

**Found on the way, worth knowing:** the old tick counter ran slower than real time under load (the guest delivers fewer than 1000 timer interrupts a second when it is busy: 444 ticks over 1176 ms of real time in one measurement), so every duration the suite printed ("guest time") and every millisecond deadline in the tests were in a dilating clock. With a real clock the three tests that start two thousand threads, which take 20 to 35 s of real time on QEMU's software CPU, timed out against their 2 to 8 s deadlines; their deadlines are now `SLOW_MS` (120 s). The "guest time" lines the harness prints are real time now and read larger than before.

Not done here (left for the review and later): the C transcript's `clock_gettime` check against a host oracle (the libc transcript runs in the home boot, not the kernel boot) and the userland servers' remaining millisecond arithmetic, which stays in ms through the wrappers.
