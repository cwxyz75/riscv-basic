# Testing

How this project tests itself, and the rules that keep it fast. The decision is OD83 (`OS-ARCHITECTURE.md`); the
roadmap slice that built it is TS. The measurements the choices rest on are in "Profile" below. The rules are
checked by `scripts/check_docs.py` (part of tier 0), so they cannot quietly drift back to "run everything every time",
which is what happened after the previous adjustment.

## The tiers

| Tier | What | When | Budget | Make target | CI |
|---|---|---|---|---|---|
| 0, unit | fmt, docs check, host unit/model/property tests of every crate, clippy, the OS host tests with loom and shuttle | every edit; the inner loop | about 2 minutes | `make quick` (fast part), `make test lint os-test` | the `host` job |
| 1, smoke | tier 0, then **one** boot of the whole suite on the default machine (AIA) at four harts, the release smoke, the home computer's boot | every PR: the merge gate | 10 minutes per CI job | `make smoke` (`make check` is an alias) | `ci.yml` |
| 2, soak | the full matrix (1, 2 and 4 harts, round-robin, release, RVA23, plain `virt` polling, no disk, the serial sessions), every negative control, the fail-demo harness check, Miri | on demand; nightly on `main`; on a PR labelled `soak` (recommended for kernel, SMP and security slices); before a release; **never required to merge** | none (jobs run in parallel) | `make soak` (parallel, `SOAK_JOBS`, default 3), `make check-all` | `soak.yml` |

Rules:

1. **The gate is tiers 0 and 1 and nothing else.** `make check` is exactly `smoke`. `ci.yml` runs only smoke targets (plus the
   fail-demo harness check when the harness itself changed), has no build matrix, and every job's `timeout-minutes` is at most
   the budget. A job that outgrows its budget fails loudly: raise the budget on purpose here, or cut work, do not let it grow.
2. **Locally, run tier 0 and the one targeted test** while iterating (`make quick`, or `cargo test -p kcore --test X`). Run
   `make smoke` (about three minutes warm) before a push if you want the whole gate; CI runs it anyway. Do not run the soak
   tier by hand before every push: that is what it is for on demand, and nightly.
3. **Merge on a green gate.** A red nightly soak means "fix next", not "revert".
4. **Soak when it matters:** put the `soak` label on a PR that changes the kernel's locking, SMP, memory-management or
   security-relevant paths, or that adds or changes a negative-control mechanism, and run `make soak` once for it. (The Fable
   review of such slices stays as before.)
5. **Where a new test goes.** Pure logic (a decision, a data structure, a codec, an accounting rule): a host test in a pure
   crate (`kcore`, `proto`, ...), model-based or property-based where it can be. A syscall or server behaviour: the QEMU suite
   (`init/src/bin/suite.rs`, the smoke boot runs it all). Only what the machine itself shows (traps, the MMU and TLB, SMP races,
   the interrupt controller, FP state, boot, real drivers) must be in QEMU at all. Long, exhaustive or soak-only things
   (churn tests, large seed counts, upstream conformance suites, benchmarks): see "When the smoke boot outgrows its budget".
6. **Negative controls and mutation checks** guard a specific mechanism; they run in the soak tier (`os-boot-test-smp-neg`,
   `os-boot-test-aia`) and a mutation check is run once per new test, by hand, not on every push.
7. **Independent oracles** (the upstream Lua suite, the C transcript against the host build, the BASIC golden corpus, real
   clients) stay in the suite: while the whole suite fits the budget (35 s at four harts) they run in smoke. They are what
   the roadmap means by an oracle that the writing context did not produce.

## When the smoke boot outgrows its budget

The smoke boot is the whole suite at four harts, 35 seconds in the profile below, against a 10-minute job (of which the
cold build is most). If it grows past the budget, in this order: (1) shard the suite across parallel QEMU instances; (2)
tag tests soak-only and add a filter (a `SOAK` list in the suite's registry and a flag the init program reads), starting
with the slowest in the profile (`profile_check.py` prints them); (3) move the logic of QEMU tests to the host (below).
The profile is the guide: do not guess.

## Moving logic out of QEMU (slice TS step 3, done incrementally)

* The syscall layer (`kernel/src/user.rs`, about 2,900 lines) behind a small `Machine` trait (copy in and out, the current
  process, the clock), so argument errors, rights, quotas and accounting run on the host against the real code with a fake
  machine; the matching suite tests move over as each piece lands.
* Server protocols (memfsd, devmgr, launcher, blkd): host harnesses that drive a server's logic over in-memory channels; the
  `Service` trait of slice R makes it natural.
* Optionally, bounded model checking (Kani or similar) of small pure pieces: the handle table, the slice T pool.

## Measuring

`OS_PROFILE=<file> python3 scripts/os_boot_test.py <kernel>` appends host seconds and guest ticks per test (and per boot) to a
TSV; `python3 scripts/profile_check.py [dir]` times every phase of the full matrix and prints the slowest boots and tests.
Re-measure when the smoke tier grows or the budget is questioned, and update the table below.

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

## What the tiers cost (estimates from the table above; measured results below)

| | Now | After (estimate from the table) |
|---|---|---|
| Local, every change (tier 0) | 30 minutes | fmt, docs, `test`, lint, `os-test`: about 2 minutes |
| PR gate (tier 1) | 30 minutes locally then about 12 in CI | the normal kernel at four harts (35 s), the release smoke, the home boot (37 s), plus builds: about 4 to 5 minutes in CI |
| Soak (tier 2), on demand and nightly | (part of every PR) | everything else, about 25 minutes, in parallel jobs |

## Measured after slice TS (same machine, warm build)

| | Time |
|---|---|
| `make smoke` (tiers 0 and 1: host tests, clippy, `os-test`, home boot, one boot at four harts, release smoke) | 2 min 33 s |
| `make soak` (`-j3`; everything else: round-robin, 1 and 2 harts, release, RVA23, polling, no disk, every negative control, the serial sessions) | 9 min 46 s, all green |
| `make check` before slice TS | 29 min 45 s |

## Benchmarks (slice B, SCHED-SMP-SPEC 6.1, OD96)

The suite measures a few costs and prints them as `##BENCH <name> <ns> ns` lines (smaller is better; a well-formed line passes
the kernel's `debug_write` filter like a `##TEST` result, a malformed one fails the boot). The clock is the `time` counter
through `debug_stat(6)`. Every boot runs them at a tenth of the iterations with sanity assertions only (so they cannot rot);
`make os-bench` (soak tier, in `soak.yml`) runs them in full (`-append bench=full`, which `debug_stat(7)` reports) at one and
four harts and compares with `bench/baseline.tsv` through `scripts/bench_compare.py` (a 3x slowdown fails; the self-test in
`make docs-check` proves it). The numbers are QEMU debug-build costs and move with the machine (T75): read the printed ratios.

| Benchmark | What it runs | smp1 | smp4 |
|---|---|---|---|
| `thread_create_join` | create, run an empty body, exit, join | 155 us | 234 us |
| `wake_latency_event` | two threads hand a baton through two events: a round trip (two wakes) | 142 us | 237 us |
| `channel_call_rt` | `channel_call` to a server thread, no bystanders | 165 us | 273 us |
| `channel_call_rt_100_bystanders` | the same with 100 threads blocked on an unrelated event | 205 us | 269 us |

First measurement 2026-11-14 (the baseline). Not built yet (later tasks, as their mechanisms land): `pair_scaling` (N independent
pairs on N harts), `bkl_profile` and `irq_off_max` (task 2, the latency trace), `rt_latency` (task 8, a user-level real-time probe).

## The latency trace (slice B task 2, SCHED-SMP-SPEC 3.12)

`make os-latency` (soak tier) builds the kernel with the `latency-trace` feature (no code at all without it), runs the suite at
four harts and prints, from the guest: the longest **interrupts-off span** of any hart and where it began, and per syscall
number the big-kernel-lock **hold** (maximum, count, total) and **wait**. Spans are measured at `irq::without` and around every
trap, and end at every context switch (a thread that sleeps inside a critical section does not count as holding interrupts
off); the test `the_latency_trace_can_see_a_long_section` proves a 5 ms section is reported within 20%, at the right file.

First measurements (2026-11-14, QEMU, debug build, four harts, the whole suite; costs of the emulated machine, not a real one):

| What | Worst | Where |
|---|---|---|
| longest interrupts-off span | 308 ms | a trap handler (`trap.rs`; the long operations of SCHED-SMP-SPEC 3.12 are in it) |
| BKL hold, syscall 47 (`vmo_commit`) | 87 ms | commit zeroes and maps up to 1024 pages under the lock |
| BKL hold, syscall 1 (`process_exit`) | 37 ms | teardown |
| BKL hold, syscall 0 (kernel threads and traps outside a syscall) | 34 ms; wait up to 87 ms | |
| BKL hold, syscall 10 (`handle_close`) | 34 ms | closing a handle that destroys a large object |

These are the numbers the lock peels (tasks 14 to 20) are judged against; the budget stated in SCHED-SMP-SPEC 3.12 is to be
restated from them once a release build and, ideally, hardware numbers exist (T75).
