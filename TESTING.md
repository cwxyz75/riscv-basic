# Testing

The strategy (tiers, budgets, rules) is decided in OD83 and is written out here by slice TS step 2. This file
starts with what step 0 measured, because every later choice rests on it.

## Profile of `make check` (measured 2026-11-14, before slice TS; one machine, 10 cores, QEMU 11.1.3)

Reproduce with `python3 scripts/profile_check.py [OUT_DIR]` (about 30 minutes; it runs the targets of `make check` in
order and writes `phases.tsv` and `boots.tsv`). Per-test numbers come from `OS_PROFILE=<file>` on any boot
(`scripts/os_boot_test.py`): host seconds per test, and the guest ticks each test took (`tt:` lines from the suite).

| Phase (Makefile target) | Wall seconds |
|---|---|
| fmt-check, docs-check | 1 |
| test (host crates) | 31 |
| lint (clippy) | 8 |
| home-boot-test | 37 |
| os-test (host tests for the OS crates, loom, shuttle) | 84 |
| os-boot-test (normal, fail-demo, round-robin at 1 and 4 harts) | 391 |
| os-boot-test-smp (2 and 4 harts) | 229 |
| os-boot-test-smp-neg (negative controls, serial sessions at 4 harts) | 413 |
| os-boot-test-nodisk | 56 |
| os-boot-test-aia (AIA at 2 harts, two negative controls) | 258 |
| os-boot-test-polling | 56 |
| os-boot-test-rva23 | 61 |
| os-serial-test | 160 |
| os-release-smoke | 1 |
| **total** | **1785 (29.7 minutes)** |

77% of the time (1380 s) is QEMU boots: 16 distinct boot configurations, each running the whole suite. What the boots cost:

| Boot | Seconds |
|---|---|
| round-robin kernel, 1 hart | 261 |
| negative control `fault-skip-kernel-satp`, 4 harts (a hang until the timeout, by design) | 240 |
| negative control `fault-aia-no-unmask`, 1 hart (waits out timeouts, by design) | 163 |
| round-robin kernel, 4 harts | 107 |
| **normal kernel, 1 hart** | **102** |
| normal kernel, 2 harts | 70 |
| fail-demo kernel, 1 hart | 69 |
| normal kernel, RVA23 CPU, 1 hart | 56 |
| other negative controls (each 33 to 51) | 193 in all |
| normal kernel, plain `virt` (polling) | 51 |
| **normal kernel, 4 harts** | **35** |

What this says:

* The **whole normal suite at four harts takes 35 seconds**. A smoke gate does not need a reduced test subset to be fast;
  it needs one well-chosen boot instead of sixteen. (More harts is *faster*: tests that wait are overlapped.)
* Hart count, scheduler class, CPU profile and interrupt controller change the **kernel's** behaviour, so they matter for
  kernel tests, but the Lua, BASIC, libc and memfsd groups run unchanged in every one of them.
* Negative controls and the round-robin kernel are the biggest single items and guard specific mechanisms; they belong in
  tier 2 and in path-triggered runs (the files of the mechanism they guard).
* QEMU icount (`-icount shift=0,sleep=off`, `shift=auto,sleep=off`) is **slower** (117 s and 130 s against 101 s at one
  hart), so it is not a way to save time. Dropped from the plan.
* The slowest tests over all boots: `unmap_while_siblings_touch` (225 s, almost all of it the satp negative control
  hanging until its timeout), `lua_upstream_files` (111 s), `blk_round_trip` (68), `cycles_do_not_grow_memory` (68),
  `basic_golden_corpus_on_the_target` (67), `memfsd_survives_memory_pressure...` (42), `the_latency_test_can_fail` (33).
* Outside boots: the host test crates (31 s), the OS host tests with loom and shuttle (84 s) and clippy (8 s).

## What this means for the tiers (the numbers behind OD83)

| | Now | After (estimate from the table) |
|---|---|---|
| Local, every change (tier 0) | 30 minutes | fmt, docs, `test`, lint, `os-test`: about 2 minutes |
| PR gate (tier 1) | 30 minutes locally then about 12 in CI | the normal kernel at four harts (35 s), the release smoke, the home boot (37 s), plus builds: about 4 to 5 minutes in CI |
| Soak (tier 2), on demand and nightly | (part of every PR) | everything else, about 25 minutes, in parallel jobs |
