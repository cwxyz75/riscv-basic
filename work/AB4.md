---
id: AB4
kind: slice
title: `port_wait_many`: up to 64 packets in one call
area: ipc
horizon: long
blocked_by: [AB1]
refs: [KERNEL-V1-SPEC]
---

**How.** `port_wait_many(port, buffer, capacity, deadline) -> count`: `capacity` 1 to 64; each record is 16 bytes (`key: u64`, `observed: u32`, 4 bytes zero); blocks until at least one packet is ready, then takes as many as are ready up to `capacity`, in the order `port_wait` would have given them. Errors as `port_wait`, plus INVALID_ARGS for a bad capacity or buffer (checked before anything is dequeued). `port_wait` stays for now and is removed from the kernel when the system library wraps it (AB7). `rt`'s service loop and `netd` use it. Pure logic in `kcore::ipc` with the port model extended.

**Acceptance, written first.** The port model: `wait_many` equals that many `wait`s. QEMU: 64 ready bindings come back in one call, in order; a bad buffer changes nothing; the `channel_call_rt` benchmark is unchanged and a new `##BENCH port_drain_64` is recorded.

**Review.** Default.
