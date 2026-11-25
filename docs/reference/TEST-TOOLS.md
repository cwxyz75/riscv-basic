# Test tools

What each check is, how to run it and how to replay a failure. When and where a check runs is policy: `TESTING.md`.
Measured costs and first results are records (`docs/records/testing-2026-11-20.md`).

## The boot harness

`scripts/os_boot_test.py <kernel>` boots a kernel headlessly, reads the structured serial output (`##TEST` lines) and
exits with pass or fail. Its switches are environment variables:

| Variable | Meaning |
|---|---|
| `OS_SMP` | hart count |
| `OS_SHARD` | which part of the suite: `a`, `b`, `k` (below), empty for all |
| `OS_DTB` | boot with this device tree (`-dtb`); `scripts/entropy_boot_test.py` (`make os-boot-test-entropy`, soak) uses a copy of QEMU's own with `/chosen/rng-seed` removed |
| `OS_LOAD` | the physical address the image was built for (`make os-boot-test-loadaddr` sets it): the kernel line carries `load=<address>` and the test `the_image_is_where_the_loader_put_it` compares it with where the early boot code found the image |
| `OS_RAM` | the RAM QEMU is given (`-m`; default `256M`); the harness also puts `ram=<size>` on the kernel line, and the kernel test `ram_matches_the_machine` checks the frame count against it |
| `OS_CPU` | the CPU model; always `rva23s64` (the kernel refuses to boot below the profile); only `os-boot-test-cpu-neg` sets it, to `rv64` |
| `OS_BOOT_STALL` | seconds without output before a boot is called stalled (default 180) |
| `OS_BOOT_TIMEOUT` | the runaway ceiling in seconds (default 1800) |
| `OS_PROFILE=<file>` | append host seconds and guest ticks per test to a TSV |
| `OS_RECORD=<dir>`, `OS_REPLAY=<file>`, `OS_GDB=1` | record and replay (below) |

**A boot is judged on progress, not on total time.** A loaded host slows a healthy suite several times over, so the
harness fails a boot that prints nothing for the stall window, not one that is slow. A test must not assume the boot is
young either: a window on the RTC or on the clock since boot has to be wide.

Every failing boot leaves its serial log in `target/fail-logs/`.

## The suite and its shards

The suite is `init/src/bin/suite.rs`, run by the guest at boot. The guest filters by the `shard=` kernel argument and
the harness checks that each shard reported exactly its own tests.

| Shard | Holds | Machine |
|---|---|---|
| `a` | everything not in `b` | no network device |
| `b` | the `net_*` tests, the C sockets test, the three heaviest stress tests and their prerequisite | network |
| `k` | the Kernel tests only (`init/ktests.txt`: a test that starts no server and needs no disk, network or console service) and the kernel's own self-tests | no disk, no network |

A test that starts children carries its prerequisite along into its shard. `os_boot_test.py --selftest-k` proves the
harness fails a `k` boot that lacks a Kernel test or ran a System test.

| Target | What |
|---|---|
| `make os-boot-test-smoke-a`, `-b` | one shard of the whole suite at four harts (`os-boot-test-smoke`: both, one after the other) |
| `make os-ktest-small` | the kernel-only boot at one and two harts, without the tests marked `slow` |
| `make os-ktest-4` | the kernel-only boot at four harts, every Kernel test |
| `make os-ktest-release` | the kernel-only boot of the optimised build at four harts |
| `scripts/check_ecall.py DIR [--self-test]` | AB7: no embedded user program has an `ecall` instruction (every call goes through `libsys`); only `crash` is exempt (its AB8 test makes a call from outside the library on purpose); run by `os-ktest-release-content` |
| `cargo test -p libsys` | AB7: builds `libsys.so` and checks it (no relocations, a symbol per call in `abi::syscall::TABLE`, the data page's segment); the same check runs in the kernel's build script |
| `scripts/kaslr_check.py ELF` | HD3: ten boots of the kernel (each stopped at its `kaslr:` line) show at least six slides and four direct-map bases, and `kaslr=off` one layout; run by `os-ktest-small` |
| `scripts/unsafe_inventory.py [--list\|--check\|--self-test]` | KT14: every `unsafe` in `kernel/src` with a hash; the docs check fails for a block that is not in `docs/records/reviews/kt14-unsafe-audit.tsv` (a new or changed block is audited again) |
| `make os-ktest-shuffle` | AB8: the kernel-only boot (two harts) of a release build whose syscall numbers are shuffled by a seed (`ABI_SHUFFLE_SEED`): a program or kernel path that uses a literal number fails |
| `make os-ktest-entry-neg` | AB8: negative control `fault-skip-entry-check` (no `sepc` check); `child_faults_are_contained` must fail |
| `make os-ktest-release-content` | the kernel-only boot at four harts of a kernel with the release content (feature `release-suite`: no boot self-tests, no scheduler or SMP tests), run from the separately built suite (HD4 part 3) |
| `make kquick` | format, the pure kernel crates' host tests and the four-hart kernel-only boot: one targeted boot for a kernel change |
| `make os-release-smoke` | the release image boots to the shell and shows no test banners, and its symbol table holds no test-only code (`scripts/release_symbols.py`: the self-tests, the user-mode test driver, the SMP and scheduler tests are not compiled into a `release` kernel, HD4; the deny-list is in the script and its self-test plants a name) |
| `make home-boot-test` | the home computer boots and runs its checks |

After a passing boot the harness prints the suite's guest time and the six slowest tests.

## Finding a failure

**Which commit of an epic.** `make epic-bisect [TARGET=<target>]` (`scripts/epic_bisect.sh`) bisects the first-parent
commits from the epic's fork point on `origin/main` to `HEAD`, running one make target per step (default `os-ktest-4`).
It needs a clean tree and leaves the bisect open; `git bisect reset` returns to the branch.

**Record and replay.** `OS_RECORD=<dir>` boots under QEMU's record mode and keeps the recording only when the boot
fails; `OS_REPLAY=<file>` replays it, and `OS_GDB=1` waits for gdb on :1234. Recording needs a boot with no disk and no
network device, so use `OS_SHARD=k`. Record mode runs the harts in one TCG thread: a race that needs truly parallel
harts may not reproduce under it, and its instruction-counted time changes what timing-dependent tests see.

**Repeat.** `make os-repeat TARGET=<target> N=<n>` runs a target N times, one at a time, prints `pass/N` and keeps the
serial log of every failing run in `target/repeat/`. Over a soak target this is a soak run (`TESTING.md` rule 6).

**Where a panic came from.** A kernel `##PANIC` line from a thread that entered from user mode ends with `[stack 0x... 0x...]`: the words on its
kernel stack that point into the kernel's code, innermost first (the panic handler's own frames, then the return addresses of the calls that led
here, among a few stale ones). `python3 scripts/resolve_trace.py <kernel ELF> <log file>` names the symbol nearest each address (it needs only
`nm`); `llvm-addr2line -e <ELF> <addr>` gives the line. It is how a panic whose location is the standard library's (T233) is traced to its caller.

**The harness can see a failure.** `make os-boot-test-fail-demo` boots a kernel built with a deliberate fault and
requires the harness to fail it; CI runs it when the harness or the Makefile changed.

## Measuring

`OS_PROFILE=<file>` on any boot appends per-test host seconds and guest ticks. `python3 scripts/profile_check.py [dir]`
times every phase of the gate and prints the slowest boots and tests. Re-measure before changing a budget or moving a
test between tiers, and put the numbers in a dated record.

## Host checks

| Target | What |
|---|---|
| `make test` | the host tests of every crate, and the demo pictures against the independent renderer |
| `make os-models` | the lock, latch, futex, park, idle and run-queue protocols under the loom and shuttle model checkers |
| `make os-aarch64-check` | the kernel compiles against the AArch64 stub (the architecture boundary is complete) |
| `make lint` | clippy with warnings denied: `lint-host` (the host crates) and `lint-target` (the home kernel, the user programs, the OS kernel with its panic-finding deny list) |
| `make docs-check` | `scripts/check_docs.py` (documents, the decision log, the work queue, the testing tiers, the atomics allow-list, the architecture boundary, the workflows, the syscall-error table) and the self-tests of the benchmark comparison and the packet-capture checker |
| `make build` | the OS kernel (with its user programs) and the home kernel, then `scripts/check_vector_codegen.py` (PL3): the built C code (Lua, the C tests) must hold vector instructions, so a `-march` that lost the V extension cannot pass silently |
| `make os-miri` | the unit tests and the frame-database model under Miri (nightly toolchain; soak) |

**The fuzzer of the syscall layer.** `kfuzz-gen` generates call sequences from a model of what the process holds
(mostly valid, some near-valid, some garbage). `ksys/tests/fuzz.rs` runs them against the real syscall layer on the fake
machine and audits after every call: no panic, the frame database, every handle names a live object, reference counts
equal the handles and objects that name each object, the job ledger equals what is held, and nothing remains after
everything is closed. `cargo test` runs a few short sequences; `make os-fuzz-host` (overnight) runs many long ones in
the checked release build (`FUZZ_SEEDS`, `FUZZ_CALLS`). A failure prints its replay command
(`KFUZZ_SEEDS=<seed> KFUZZ_CALLS=<n> cargo test --profile release-checked -p ksys --test fuzz sequences`);
`KFUZZ_TRACE=1` prints every call.

The guest half is the program `init/src/bin/kfuzz.rs`: the same generator, its calls made through the system library against the real kernel, one seed per child process in a small job (the suite test `the_guest_fuzzer_leaves_nothing_behind` runs seeds 1 to 4 on every kernel boot; widen `SEEDS` there, or run the shard with a larger range, for a long hunt). A kernel panic is the boot test's `##PANIC`; after each child the job is killed and its counters (processes, threads, pages, jobs, objects, IPC bytes, kernel bytes) must settle at zero and the kernel heap must come back. A seed replays to the same calls (the window moves with ASLR, but the calls are relative to it).

**The allocation-failure sweep.** `ksys/tests/allocfail.rs` runs each scenario with its N-th allocation failing, for
every N until the scenario finishes without reaching it. Each case runs in a child process, because an infallible
allocation aborts on failure: a child that dies is reported with the scenario, the N and the frames of the failing
allocation. A child that survives must have returned NO_MEMORY from the call that met the failure, the same call must
succeed when repeated with memory, and the state must pass the fuzzer's audits. `cargo test` runs the one-failure mode;
`make os-allocfail` (overnight) adds the persistent mode, where every allocation from the N-th on fails.
The guest half (`make os-fault-alloc` family, `init/src/bin/suite.rs` "alloc sweep") does the same in the real kernel: the suite thread arms the
N-th allocation (`debug_stat` 200), runs a scenario, disarms (201) and checks that no VMO or object was left behind (204, 206, after a reachability
sweep, 205). The epilogue of every call (wake commit, cleanup, copy-on-write sync) is **counted**, not exempt: what it allocates is either
allocation-free (the wait table, the address-space scratch), refused with `NO_MEMORY` at an R point, or in `heap::nofail` (class N, counted per file by
`scripts/check_docs.py`). After the one-failure sweep of a scenario a **persistent pass** (`debug_stat` 208) fails every class-R allocation from the
first on, with class N exempt as the reserve makes it: nothing may panic or leak. (A scenario that touches a lazily committed page is left out of
that pass: its fault ends the process, which is the right answer, and the process is the one running the suite.)
`KALLOC_SCENARIO=<name>` and `KALLOC_MAX=<n>` run one scenario; `KALLOC_CHILD=<scenario>:<n>[:persist]` replays one case.

**Mutation testing.** `make os-mutants` (overnight; cargo-mutants) mutates `kcore` and `klock` with their host tests as
the oracle. A survivor that is not in `mutants-accepted.txt` with a reason fails the run. `PKG=` and `SHARD=` run a slice.

**Coverage.** `make os-coverage` (overnight; cargo-llvm-cov) reports line and region coverage of the host tests of
`kcore`, `klock`, `abi` and `ksys`. It is a report, never a gate: a number says where a test is missing, not that the
code is right.

## Kernel checks under QEMU

**Instruction counts.** `make os-count` boots the kernel-only shard at one hart under `-icount shift=0`, where the
guest's retired-instruction counter is deterministic, and compares the suite's `##BENCH count_<name> <n> instr` lines
with `bench/counts.tsv`. A rise of more than 25 % fails (the slack absorbs the runner and unrelated changes, T223); a fall of more than 5 % is printed with a request to record it
(`make os-count-record`, then read the diff of the file). Record and compare in the same configuration (the `kfast` boot). The file holds the numbers of the CI run (2026-11-20: PL1 moved the test CPU to `rva23s64` and the kernel's code generation to the profile's extensions, and the epic gate's numbers then differed from a laptop's by up to 12 % in either direction, so the recorded values came from the gate's own log): a local `make os-count` is a reproduction, not the check.

**Benchmarks.** The suite prints `##BENCH <name> <ns> ns` lines (a malformed one fails the boot). Every boot runs the
benchmarks at a tenth of the iterations with sanity assertions only, so they cannot rot. `make os-bench` (soak) runs
them in full at one and four harts and compares with `bench/baseline.tsv` through `scripts/bench_compare.py`: a
threefold slowdown fails. The numbers are costs of the emulated machine and move with the host: read the ratios.
`pair_scaling_<kind>_n<N>` (futex and `channel_call` pairs, one, two and four) is the cost per round trip over all pairs;
`bench_compare.py` prints the scaling, the cost at one pair over the cost at N, and `--scaling-floor bench/scaling.tsv` (on in `make os-bench` and `make os-latency`) fails if a ratio falls below its recorded floor. The floors are tripwires for a collapse (about 60% of the lowest of three full runs), not the design's goals of 3.0 and 2.5, which are not gates (SCHED-SMP-SPEC amendment of 2026-11-24, T252); the measured figures at four harts are futex 0.5 to 1.3 (it swings with the pair's placement) and `channel_call` 0.8. `rt_latency_p99` and `rt_latency_max` are the
real-time probe's lateness, printed by the kernel's scheduler test.

**The latency trace.** `make os-latency` (soak) builds the kernel with the `latency-trace` feature, runs the suite at
four harts and prints the longest interrupts-off span of any hart with where it began, and per syscall the big kernel
lock's hold and wait. The test `the_latency_trace_can_see_a_long_section` proves a planted section is reported. The run also
prints them as `##BENCH irq_off_max`, `bkl_hold_max` and, for the ten worst syscalls, `bkl_hold_max_sys<N>` and
`bkl_wait_max_sys<N>`, and compares the first two with `bench/latency.tsv` (`bench_compare.py`).

**Schedule perturbation.** `make os-chaos` (overnight) boots the kernel-only shard on the `chaos` kernel, one boot per
seed in `CHAOS_SEEDS` and per hart count, with delays at lock, IPI, switch and user-return points. A failing seed is
named and its log kept under `target/chaos/`; `OS_CHAOS_POINTS=<mask>` (1 acquire, 2 release, 4 IPI, 8 switch, 16 user
return) narrows the point.

**The checked release kernel.** `make os-boot-test-checked` (overnight) runs the whole suite at four harts and the
kernel-only shard at one, two and eight on the `release-checked` profile: optimised, with debug assertions and
overflow checks on.

**The matrix and the negative controls** (soak): `os-boot-test` (the normal, fail-demo, round-robin and release
kernels), `os-boot-test-smp` (two and four harts), `os-boot-test-nodisk`,
`os-boot-test-cpu-neg` (a CPU without the profile ends in `##PANIC missing extension`), `os-boot-test-overflow`, `os-serial-test`, and the negative controls, each a kernel built with one
mechanism disabled that the suite must then fail: `os-boot-test-smp-neg`, `os-boot-test-sched-neg`, `os-boot-test-aia`.

**The network checks by tier.**

| Check | Tier | Where |
|---|---|---|
| `nettcp`, `netcore` TCP, `netwire::tcp`, the `netsim` lossy-link suite (reduced seeds), `dns`, `http1`, `sockshim`, `rngcore` | 0 | `make test` |
| the streams, sockets, random, fetch and `httpd` tests of the suite; `net_tcp_churn` in its short form | 1 | shard `b` |
| the bulk TCP tests behind the lossy relay (`make os-boot-test-lossy`, `LOSSY_SEEDS`), `net_tcp_churn` at full length, the `##BENCH tcp_*` lines | 2 | `make os-boot-test-lossy`, `make os-bench` |

## Running several boots at once

`make soak` runs `SOAK_JOBS` boots at once (default 3). The tests follow the host's clock, so on a machine that is
doing anything else the network tests and the benchmark comparison fail from load. Use `SOAK_JOBS=1` for a result to
trust, and never run two network boots at once by hand: their host ports are fixed and collide.
