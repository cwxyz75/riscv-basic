---
id: KT1
kind: slice
title: A kernel-only boot in the gate, at one, two and four harts, test and release builds
area: testing
horizon: near
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** Today a kernel change is checked by the whole system's suite at four harts only (shards `a` and `b`); one and two harts and the release kernel are soak-only. Add a third shard, `k`: the kernel-facing tests of `init/src/bin/suite.rs` and nothing that needs a server.

**How.** Every suite test gets an area tag at its definition (`Kernel` or `System`; a test is `Kernel` when it starts no server and needs no disk, network or console service: syscalls and their errors, handles, channels, events, ports, sockets, VMOs, mapping, processes, threads, futexes, jobs and quotas, scheduling, FP, interrupts objects, SMP). `shard=k` (`debug_stat` 13) runs only `Kernel` tests; the harness (`OS_SHARD=k`) requires exactly those names. The in-kernel self-tests always run. Make targets: `os-ktest` (three boots: `-smp 1`, `2`, `4`, test build) and `os-ktest-release` (the release kernel at `-smp 4` with the `k` shard instead of the present one-second smoke). Both join `SMOKE_TARGETS`; CI gets one job `kernel (ktest)` running both within the 7-minute budget. `make kquick` = `fmt-check`, the `kcore` and `klock` host tests, `os-ktest` at `-smp 4` only: the inner loop for kernel edits.

**Acceptance, written first.** The harness fails when a `Kernel` test is missing from a `k` boot or a `System` test appears in one (a self-test of the harness, like the fail-demo). Each `k` boot is under 40 s of guest time on the development machine; the CI job is under 6 minutes. `TESTING.md` gains the row and the measured times.

**Review.** Default. No kernel behaviour changes.
