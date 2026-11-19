---
id: KT9
kind: slice
title: Allocation-failure sweep: fail the Nth kernel allocation, for every N
area: testing
horizon: long
blocked_by: []
refs: [KERNEL-V1-SPEC, OD139, OD140]
---

**How.** Kernel feature `fault-alloc`: the allocator counts allocations made while a user thread of a marked job is current and returns null for the one numbered by `debug_stat`-settable `fail_at` (once). A guest program runs each scenario of a fixed list (create each object kind, map and touch, spawn a thread, send a message with handles, `channel_call`, start a process, a job tree) in a child job with `fail_at = 1, 2, 3, ...` until the scenario completes without reaching the count. After every run: the call returned NO_MEMORY or succeeded; the kernel did not panic; killing the job returns frames, objects and ledger to baseline. On the host, the same sweep over the syscall layer once TS3 has landed. Target `os-allocfail`, overnight.

**Acceptance, written first.** A planted infallible `Vec::push` on a syscall path is reported as `##PANIC` by the sweep with the scenario and N. The sweep completes on the unmodified kernel; every infallible allocation it finds is fixed in this item (T-SPEC decision 5's audit).

**Review.** Fable. Expect real findings.
