# Testing

How this project tests itself, and the rules that keep it fast. The decision is OD83 (`OS-ARCHITECTURE.md`); the
roadmap slice that built it is TS. The measurements the choices rest on are in "Profile" below. The rules are
checked by `scripts/check_docs.py` (part of tier 0), so they cannot quietly drift back to "run everything every time",
which is what happened after the previous adjustment.

## The tiers

| Tier | What | When | Budget | Make target | CI |
|---|---|---|---|---|---|
| 0, unit | fmt, docs check, host unit/model/property tests of every crate, clippy, the OS host tests with loom and shuttle | every edit; the inner loop | about 2 minutes | `make quick` (fast part), `make test lint os-test` | the `host` job |
| 1, smoke | tier 0, then the whole suite on the default machine (AIA) at four harts as two parallel boots (shards `a` and `b`), the release smoke, the instruction counts (`os-count`, KT5), the home computer's boot | every PR: the merge gate | 7 minutes per CI job | `make smoke` (`make check` is an alias) | `ci.yml` |
| 2, soak | the full matrix (1, 2 and 4 harts, round-robin, release, RVA23, plain `virt` polling, no disk, the serial sessions), every negative control, the fail-demo harness check, Miri | **only when the owner asks for it** (OD142): no schedule, no label, no run started by anyone else; **never required to merge** | none (jobs run in parallel) | `make soak` (parallel, `SOAK_JOBS`, default 3), `make check-all` | `soak.yml` |
| 3, overnight | long runs: 20 consecutive boots of the smp suite to start with (`overnight-repeat-smp`), and the whole suite on the checked release kernel (`os-boot-test-checked`: the `release-checked` profile, optimised with debug assertions and overflow checks on, at four harts, plus the kernel-only shard at one, two and eight); schedule perturbation (`os-chaos`: the kernel-only shard on the `chaos` kernel, one boot per seed in `CHAOS_SEEDS` and per hart count, a failing seed named and its log kept; delays at lock, IPI, switch and user-return points, 1/64 at the lock points and 1/8 at the others: at 1/8 everywhere a long critical section outgrows the big lock's spin watchdog and the harness's stall window); mutation testing of `kcore` and `klock` (`os-mutants`: cargo-mutants with the host tests as the oracle, eight shards for `kcore`; a survivor not in `mutants-accepted.txt` with a reason fails the run); coverage (`os-coverage`: cargo-llvm-cov over `kcore`, `klock`, `abi` and `ksys`, a table in the job summary and an HTML report, never a gate); the host fuzzer of the syscall layer (`os-fuzz-host`: 20000 seeds of 600 calls each from `kfuzz-gen`, audits after every call); each long-run item of KERNEL-V1-SPEC section 8 adds its target | **only when the owner asks for it** (OD142): `workflow_dispatch` is the only trigger; **never required to merge**; a red run is a work item the same day (OD140) | up to 300 minutes per job | `OVERNIGHT_TARGETS` in the Makefile (`make overnight-repeat-smp`) | `overnight.yml` |

The **kernel-only boot** (KT1, 2026-11-18) is part of tier 1: `make os-ktest` runs `os-ktest-small` (the test build at one and two harts, without the tests
marked `slow` in `init/ktests.txt`) and `os-ktest-4` (four harts, every Kernel test); `make os-ktest-release` boots the optimised build (debug assertions and
overflow checks off, no fat LTO so that the build is short) at four. Each boot runs **only the Kernel tests** (`init/ktests.txt`: a test that starts no
server and needs no disk, network or console service; shard `k`, `debug_stat` 13 = 3, or 4 with `kfast`) and the kernel's own self-tests, with no disk
and no network device. A Kernel test is one of the 150 or so tests of the suite that exercise syscalls, handles, channels, ports, sockets, VMOs,
processes, threads, futexes, jobs and quotas, scheduling and SMP; the three heavy stress tests stay in shard b. The harness (`OS_SHARD=k`) requires
exactly those names, and `os_boot_test.py --selftest-k` (the first line of `os-ktest-small`) proves it fails a boot that lacks a Kernel test or ran a
System test. `make kquick` = fmt-check, the `kcore` and `klock` host tests and the four-hart boot: the inner loop for kernel edits.
Measured 2026-11-18 on the development machine: 150 suite tests plus the kernel's own, 8 to 13 s of guest time per boot; **both small boots together
69 s, the four-hart boot 116 s, the release build with its compile 115 s**. (On CI the first cut, one job, was cancelled at its 7-minute cap: three
boots of 72 to 116 s here took over six minutes there. Three of the kernel's own tests and four suite tests dominate a one-hart boot: `unmap_while_siblings_touch`
23 s of host time, `thread_limit_and_slots_are_reused` 19 s, `two_thousand_threads_live` 18 s. The slow suite tests are skipped at one and two harts.)
CI runs the three targets as parallel jobs `kernel (ktest)`, `kernel (ktest 4 harts)` and `kernel (ktest release)`.

**A boot is judged on progress, not on total time** (T157, OD140). `scripts/os_boot_test.py` fails a boot when the guest prints nothing for
`OS_BOOT_STALL` seconds (180: a hang, reported as "stalled" with the last line) or runs past `OS_BOOT_TIMEOUT` (1800: a runaway ceiling). A loaded host
(measured: a CPU burner on every core) slows a healthy suite more than three times, so a fixed 600 s budget read slowness as failure. A test must not assume
the boot is young either: guest time on a loaded host runs well ahead of the guest's tick count, so a window on the RTC or the clock since boot is wide
(`clock_service_reports_the_rtc` allows two hours after the base).

**Flake tooling** (KT2; OD140: an intermittent kernel failure is a defect until its cause is known). `OS_RECORD=<dir>` on `scripts/os_boot_test.py`
boots under QEMU's record mode (`-icount shift=auto,rr=record`) and keeps the recording only when the boot fails (the fail-demo kernel, which fails by
design, keeps it, for the acceptance check); `OS_REPLAY=<file>` replays it, with `OS_GDB=1` adding `-s -S` to wait for gdb on :1234. Recording needs a
boot with no disk and no network device, so use `OS_SHARD=k` (the kernel-only boot). **Record mode runs the harts in one TCG thread**: a race that needs
true parallel harts may not reproduce under it, and its instruction-counted time changes what timing-dependent tests see (the fail-demo recording
fails `threads_created_but_not_run_when_the_process_ends` with a timeout as well, in the recording and again identically in the replay). `make
os-repeat TARGET=<target> N=<n>` runs a target N times with `SOAK_JOBS=1`, prints `pass/N` and keeps the serial log of every failing run in `target/repeat/`;
every failing boot also leaves its serial log in `target/fail-logs/`, and the soak workflow uploads both as artifacts of a failed job. Checked by:
`make os-repeat TARGET=os-boot-test-fail-run N=3` reports `0/3` with three logs, and a recording of the fail-demo kernel replays to the same failing
`##TEST` lines. Running `os-repeat` over a soak target is a soak run, which only the owner starts (OD142).

Rules:

1. **The gate is tiers 0 and 1 and nothing else.** `make check` is exactly `smoke`. `ci.yml` runs only smoke targets (plus the
   fail-demo harness check when the harness itself changed, and `kernel (shell test)`: the interactive shell test, the one job that
   builds BusyBox, from sources fetched by sha256 and cached), has no build matrix, and every job's `timeout-minutes` is at most
   the budget (7 minutes). A job that outgrows its budget fails loudly: raise the budget on purpose here, or cut work, do not let it grow.
2. **Locally, run tier 0 and the one targeted test** while iterating (`make quick`, or `cargo test -p kcore --test X`). Run
   `make smoke` (about three minutes warm) before a push if you want the whole gate; CI runs it anyway. Do not run the soak
   tier, or any single soak target, yourself: see rule 4.
3. **Merge on a green gate.** A red soak run means "fix next", not "revert".
4. **The soak tier runs only when the owner asks for it** (OD142, 2026-11-18). Nobody else starts it: not `make soak` or one
   of its targets locally, not `soak.yml` (its only trigger is a manual start), not a schedule, not a label. The same holds
   for the long-run tier of KERNEL-V1-SPEC section 8. A change that would have had a soak run before (the kernel's locking,
   SMP, memory management, security-relevant paths, a negative-control mechanism) says so in its pull request and adds a
   line to "Runs owed to the owner" in `WORK-PLAN.md`; it merges on the green gate like any other. (The Fable review of such
   slices stays as before.) What a change needs at one and two harts is in the gate, not in soak (KERNEL-V1-SPEC section 8,
   tier 1).
5. **Where a new test goes.** Pure logic (a decision, a data structure, a codec, an accounting rule): a host test in a pure
   crate (`kcore`, `proto`, ...), model-based or property-based where it can be. A syscall or server behaviour: the QEMU suite
   (`init/src/bin/suite.rs`, the smoke boot runs it all). Only what the machine itself shows (traps, the MMU and TLB, SMP races,
   the interrupt controller, FP state, boot, real drivers) must be in QEMU at all. Long, exhaustive or soak-only things
   (churn tests, large seed counts, upstream conformance suites, benchmarks): see "When the smoke boot outgrows its budget".
6. **Negative controls and mutation checks** guard a specific mechanism; they run in the soak tier (`os-boot-test-smp-neg`,
   `os-boot-test-aia`) and a mutation check is run once per new test, by hand, not on every push.
7. **Independent oracles** (the upstream Lua suite, the C transcript against the host build, the BASIC golden corpus, real
   clients) stay in the suite: while the whole suite fits the budget (about 100 s of guest time at four harts, see below) they run in smoke. They are what
   the roadmap means by an oracle that the writing context did not produce.
8. **An intermittent failure of a kernel test is a defect until its cause is known** (OD140). It gets a work item at once
   and is investigated; it is not re-run and noted. (A job that fails for infrastructure reasons, such as a package mirror
   timing out, is still simply re-run.)

## When the smoke boot outgrows its budget

The smoke boot is the whole suite at four harts, run as **two shards in parallel CI jobs** since 2026-11-18 (OD121): `a`
(everything not in `b`; the guest has no network device) and `b` (the `net_*` tests, the C sockets test and the three heaviest
stress tests, `ten_thousand_threads_leak_nothing`, `a_thread_hog_cannot_exhaust_the_kernel` and `cycles_do_not_grow_memory`, which are
CPU-bound and slow on a CI runner, where the network tests mostly wait; first split 2026-11-18: shard a took 6 minutes and b 2.5; shard b also runs `fetch_the_program_image`, which the hog test needs, so a test that starts children carries its prerequisite along). The
guest filters by the `shard=a` / `shard=b` kernel argument (`debug_stat` 13); the harness (`OS_SHARD`) has the same rule, checks
each shard reports only its own required tests, and runs the host-side checks of the part it has (shard b does the network
ones, shard a the disk, C and Lua ones). `make os-boot-test-smoke` runs both one after the other, locally. The release smoke is
its own job, so a gate job is a boot of about half the suite or the release build, with a 7-minute budget (the docs check enforces it).

Measured 2026-11-16 (after S9): 217 tests, about 101 seconds of guest time on the development machine, a little under half of it in
the network tests, and, on a CI runner, which is about 2 to 3 times slower, 6.6 minutes of boot as one job; with the release smoke and
setup the one job took 9.6 minutes of its 10 (2026-11-18). The harness prints `suite guest time` and the six slowest tests after every
passing boot, so the growth is visible in every log. Split a shard again, by the same mechanism, before a job passes 6 minutes; after
that, in this order: (1) a third shard (the filter is a name rule in the suite and the harness: add a value of `debug_stat` 13);
(2) tag tests soak-only and add a filter, starting with the slowest in the profile (`profile_check.py` prints them); (3) move the
logic of QEMU tests to the host (below).
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
| Soak (tier 2), only when the owner asks (OD142) | (part of every PR) | everything else, about 25 minutes, in parallel jobs |

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

## Network TCP checks by tier (S9 step 8, S9-SPEC 14.7)

| Check | Tier | Where |
|---|---|---|
| `nettcp`, `netcore` TCP, `netwire::tcp`, `netsim` lossy-link suite (reduced seeds), `dns`, `http1`, `sockshim`, `rngcore` | 0 | `make os-test` |
| The streams, sockets, random, fetch and `httpd` tests in the boot suite, `net_tcp_churn` (short form: 100 in sequence, 3 rounds of 32) | 1 | the smoke boot |
| Three bulk TCP tests behind the lossy relay (`make os-boot-test-lossy`, `LOSSY_SEEDS="1 .. 20"` for the full run), `net_tcp_churn` at full length (`bench=full`: 2,000 in sequence, 40 rounds of 32), the `##BENCH tcp_*` lines | 2 | `make os-boot-test-lossy`, `make os-bench`; the soak workflow's "lossy relay" job |

Benchmarks added: `tcp_loopback_1mib` and `tcp_echo_rt_4k` (from `net_streams_over_loopback`), next to `net_ping_rtt_*`. Not yet
measured: the transfers from and to the host and `net_bkl_profile_tcp` (TIDY-UP T102). The lossy mode runs only three tests, so the
harness's checks for the others are off in it; the lossy tests' post-close waits are longer in it (`drain_ms`).

**Soak parallelism (2026-11-17).** `make soak` runs three boots at once (`SOAK_JOBS=3`). On a ten-core laptop that is also running anything else, the network tests and the benchmark comparison fail from load (a `net_tcp_to_the_host_server` timeout, a `thread_create_join` ratio over 3, QEMU's "Guest moved used index"), and the same targets pass when run alone. Use `SOAK_JOBS=1` for a trustworthy run, and rerun a failing target alone before calling it a failure.

## The kernel's regime (KERNEL-V1-SPEC section 8, OD140)

The target regime for the kernel crates (`kernel/`, `kcore`, `klock`, `abi`) is stated in `docs/specs/KERNEL-V1-SPEC.md`,
section 8: the syscall layer tested on the host, a kernel-only boot at one, two and four harts in the gate, and a third
tier of long runs (hours; started only by the owner, like the soak tier; never required to merge) for the long checks: schedule perturbation,
syscall fuzzing, allocation-failure sweeps, mutation testing, a release build with assertions, wide SMP and repeat runs.
Long-running tests stay out of the gate; that rule does not change. Each check is added to the tables of this file, with
its make target and its measured cost, in the change that builds it.

## Instruction counts (KT5)

Wall-time benchmarks move with the host (T75). `make os-count` boots the kernel-only shard at one hart under `-icount shift=0`, where the guest's retired-instruction counter
(`rdinstret`, opened to user mode through `scounteren` in test builds only) is deterministic, and compares the suite's `##BENCH count_<name> <n> instr` lines
(`instruction_counts_report`: `handle_duplicate`+`handle_close`, `channel_write`+`channel_read` of 64 bytes, a `channel_call` round trip between two threads, `event_signal`+`object_wait`+clear,
a page fault on a fresh page, `thread_create`+exit+join, `futex_wake` with no waiter; user and kernel instructions both count) with `bench/counts.tsv`: a rise of more than 5 % fails, a fall of
more than 5 % is printed with a request to record it (`make os-count-record`, then review the diff of the file). Two consecutive runs give identical numbers. A planted loop of 1000
iterations in `channel_write` raised that path by 16 % and failed the comparison. The check is its own CI job (`kernel (counts)`, about 1.5 minutes of boot). The recorded counts are those of the
`kfast` boot (the slow tests skipped): the full boot differs by under 1 % on two paths (heap state), so record and compare in the same configuration.

## Coverage (KT11)

`make os-coverage` (overnight; `cargo-llvm-cov` 0.6.16) measures the host tests of `kcore`, `klock`, `abi` and `ksys`. Not a gate: a number says where a test is missing, not that
the code is right (the mutation run, `os-mutants`, asks that). The first numbers, 2026-11-20, line coverage over the four crates: **91.7 %** (7713 lines, 639 missed), region
coverage 92.3 %, functions 90.7 %. By file, the lowest: `abi/src/lib.rs` 55.7 % (the `Rights` and `Status` helpers are exercised through other crates, not by `abi`'s own
tests), `ksys/src/vmo.rs` 59.1 % (the success paths of the physical and DMA VMO calls need a resource handle in the fake machine), `kcore/src/object.rs` 87.0 %, `kcore/src/ticket.rs`
86.1 % (the lock's contention paths are modelled under loom and shuttle, which this run does not include), `kcore/src/frame.rs` 86.2 %, `kcore/src/wait.rs` 88.4 %,
`kcore/src/sched.rs` 88.4 %, `kcore/src/sched_smp.rs` 89.1 %; everything else is above 90 %. Branch coverage needs a nightly compiler and is not measured. `ksys` is
the host build of the syscall layer: the files of `kernel/src/user.rs` that TS3 has moved appear here (`ksys/src/{lib,vmo,job,proc,lifecycle}.rs`), the rest of `user.rs` is
not on the host and not in this report. T175 names the functions the two files below 80 % do not reach.

## Fuzzing the syscall layer on the host (KT8, first half)

`kfuzz-gen` (pure, `no_std`) generates sequences of calls from a model of what the process holds: 70 % valid, 25 % near-valid (a wrong kind, a closed handle, a size off by one, a pointer
outside the window or in the read-only page, a wide argument, a widened right), 5 % garbage. The root job handle is pinned (never closed, sent, or given limits), handles received through a
channel enter the model, and every so often the harness acts for a child job it holds. `ksys/tests/fuzz.rs` runs the sequences against the real syscall layer (`ksys`, 27 calls: handles, channels,
sockets, events, ports, VMOs, job create/limits/info) on the fake machine, doing around each call what the kernel does (the wide-argument check first; the cleanup of dead objects, the sweep of
unreachable ones and the drain of the dirty log after) and checking after every call: **no call panics** (a panic is named with its phase and call index), `FrameDb::audit` holds, every handle
names a live object, a peer is alive and points back, **every object's reference count equals the handles and live objects that name it**, and the **job ledger equals what is held** (each job's
object usage and queued bytes equal what live objects charged to it or to a job below it hold). At the end of every sequence all handles are closed and the sweep runs: no object, no usage, no meter
may remain. `cargo test` runs 16 seeds of 400 calls (about 0.5 s; two short ones under Miri); `make os-fuzz-host` 20000 seeds of 600 in the checked release build (debug assertions and overflow checks
on). A failure prints the exact replay command (`KFUZZ_SEEDS=<seed> KFUZZ_CALLS=<n> cargo test --profile release-checked -p ksys --test fuzz sequences`; `KFUZZ_TRACE=1` prints every call), and a test
checks that a seed replays to the same hash of calls and replies. Run once here: 2500 seeds of 600 calls in the checked build found nothing. Planted defects, each found within the first seeds and
replayed: an unchecked index in the `event_signal` handler (a panic), a charge on an error path never credited (the object ledger), a queued-byte charge never credited (the byte ledger) and a
reference nobody releases (the reference audit). The Fable review's findings that this does not yet cover are `T176`. The guest half (a `kfuzz` program in the boot image driving the real kernel,
with the audits behind `debug_stat`) is not built.

## The allocation-failure sweep (KT9, host half)

`ksys/tests/allocfail.rs` runs each of seventeen scenarios (create each object kind, duplicate and replace, send and receive a message with a handle, read by transaction id, write and read a socket, bind
a port, write and read and commit a VMO, make a job, close a channel with a queued handle, and four that run thirty to seventy of the same call to cross every node and chunk boundary) with its N-th
allocation failing, for N = 1, 2, ... until the scenario finishes without reaching N. A test global allocator counts the allocations made while a scenario call runs (the fake machine's own are
exempt) and returns null for the N-th. Rust's infallible allocations abort on null, so each (scenario, N) runs in a **child process** (the test binary re-executed): a child that dies is reported with
the scenario, the N and the first `kcore`/`ksys` frames of the failing allocation's backtrace. A child that survives must have returned NO_MEMORY from the call that met the failure, the same call made again with memory must succeed (no work was lost), and the state must
pass the fuzzer's audits (references, ledger, peers) and, after everything is closed, hold nothing. It runs in `cargo test` (about 40 s, in the host job, the one-failure mode) and, with `KALLOC_SCENARIO=<name>` and
`KALLOC_MAX=<n>`, one scenario at a time; `KALLOC_CHILD=<scenario>:<n>[:persist]` replays one failing allocation. `make os-allocfail` (overnight) also runs the **persistent** mode, where every allocation from the N-th on fails (real exhaustion does not recover): about 5 minutes. Findings of the first run, all fixed: the registry's dirty log, free list, credits, work list and
graveyard grew on the destruction path (`reserve_one` makes room before an object is made: the graveyard and its twin grow with the live objects; the dirty log swaps with a twin on every take (`recycle_dirty` brings the emptied one back) and a full log overflows, which is always safe; `release`'s work list is reserved per dying channel and a reference that cannot be queued is leaked for `sweep` to reclaim, never a failed destruction; the job ledger's free list is sized once); the handle table's slot and free-list growth (`HandleTable::reserve`, called by every
creator and by `duplicate` and `channel_read` before anything is taken); `channel_read` built its result after the message had left the queue (and `ksys` its handle-number buffer); `socket_read` drained before
allocating; `prune_port` allocated a marking vector (now in place); the VMO store and the committed-page map were `BTreeMap`s whose node allocations cannot report failure (now a slab with
generations and the two-level `PageMap`, both fallible); `vmo_commit`'s undo list and `vmo_read`'s buffer. The page map also cut a fresh-page fault from 19.6 thousand to 10.9 thousand
instructions (`bench/counts.tsv`). A planted infallible `Vec::push` in the `vmo_commit` handler is reported by scenario and N. What the sweep does not reach is `T177`; the guest half (`fault-alloc` in the
kernel, a `kalloc` program) is not built.

## Before a push

`make preflight` (about five minutes) runs what CI has failed pull requests for: `fmt-check`, `docs-check`, `lint` (clippy on the host crates), `lint-kernel` (clippy on the kernel, with its own deny list) and the
host tests. `make preflight-full` adds `kquick` (the kernel-only boot at four harts) and `os-ktest-release` (the same in the optimised build, whose kernel stack budget changes with every move of code between
crates): run it when `kernel/`, `ksys/` or `kcore/` changed, about fifteen more minutes. CI additionally runs one and two harts, the instruction counts, the two-shard suite, the shell test and the release smoke.
`scripts/check_docs.py` caps the near-term work items at 15.
