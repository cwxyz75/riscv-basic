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
| `OS_MACHINE` | the machine; default `virt,aia=aplic-imsic,iommu-sys=on` (the AIA and the platform IOMMU, both required by the kernel); only `os-boot-test-iommu-neg` sets it, to the machine without the IOMMU |
| `OS_PCI_TEST` | `0` leaves off the virtio-pci function behind the IOMMU (`virtio-rng-pci` at `00:04.0`, `iommu_platform=on`) that the suite's `iommu_*` tests drive by hand |
| `OS_CPU` | the CPU model; always `rva23s64` (the kernel refuses to boot below the profile); only `os-boot-test-cpu-neg` sets it, to `rv64` |
| `OS_BOOT_STALL` | seconds without output before a boot is called stalled (default 180) |
| `OS_BOOT_TIMEOUT` | the runaway ceiling in seconds (default 1800) |
| `OS_PROFILE=<file>` | append host seconds and guest ticks per test to a TSV |
| `OS_RECORD=<dir>`, `OS_REPLAY=<file>`, `OS_GDB=1` | record and replay (below) |
| `OS_HOST_STALL=<ms>[:<period ms>[:<seed>]]` | the host-stall injector (below): stop the emulator for `<ms>` at seeded moments about every `<period>` |

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
| `k` | the Kernel tests only (`init/ktests.txt`: a test that needs no network or console service; the IOMMU tests among them start `devmgr`, and the block rig `blkd` on the PCI disks) and the kernel's own self-tests | the two PCI disks, no network |

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

**IOMMU faults.** The suite prints `iofaults: <test> <n>` for each test during which the IOMMU counted faults
(`debug_stat` 150) and `iofaults: suite <n>` for the whole test run (from the first test to the last; the kernel's own
boot self-test is outside it). `os_boot_test.py` fails a boot whose faults fall in a test that is not in its
`DELIBERATE_FAULT_TESTS`, or whose suite total is not the sum of the tests' (a fault between tests), and puts the total
on its pass line.

**Record and replay.** `OS_RECORD=<dir>` boots under QEMU's record mode and keeps the recording only when the boot
fails; `OS_REPLAY=<file>` replays it, and `OS_GDB=1` waits for gdb on :1234. Recording needs a boot with no network
device, so use `OS_SHARD=k` (the PCI disks go through QEMU's `blkreplay` filter). Record mode runs the harts in one TCG thread: a race that needs truly parallel
harts may not reproduce under it, and its instruction-counted time changes what timing-dependent tests see.

**A host pause or a kernel delay?** (T284) A bound on host time fails when the host does not run an emulated hart, and the boot used
to say nothing about whether that happened. Two tools now say it:

- *The host-stall injector* (`scripts/host_stall.py`): `OS_HOST_STALL=<ms>[:<period>[:<seed>]]` makes the harness stop the QEMU process
  (SIGSTOP) for `<ms>` milliseconds, then continue it, again and again, the gaps drawn uniformly from half to one and a half periods
  (default period 1000 ms, seed 1). Each stall goes into the serial log as `##HOSTSTALL <t> <ms>` (seconds since QEMU started, the
  measured stop). The kernel line gets `hoststall=<ms>:<period>`, and the guest's self-test `a_host_stall_is_seen_by_the_guest` (run only
  then) must see a stall land: a 1 ms sleep across which the clock jumped by the stall, with the detector showing it. The harness fails a
  boot with the variable set that made no stall or lacks that test. `host_stall.py --self-test` (the docs check) stops a child process
  that reads its clock and sees the gap, and sees none without the injector. Reproduce "a loaded host" on purpose with
  `OS_HOST_STALL=20:200 OS_SHARD=k OS_SMP=4`: the kernel before T284 failed `realtime_thread_stays_responsive_under_load` in every
  such boot.
- *The pause detector* (`kernel/src/pause.rs`, the rule in `kcore::pause`, `debug_stat` 216 to 218): per hart, how late its own timer
  interrupts came against what it programmed. A window of a timing test is covered by a pause only when **two harts** were later than
  the test's bound inside it (the one hart on a one-hart machine): a pause of the host stops every vCPU at once, while a kernel that
  holds interrupts off or waits for a lock in its timer handler does it on one hart. While a test measures, every hart keeps a deadline
  at most 1 ms ahead (the heartbeat, `pause::watch`, `debug_stat` 218), and after taking a timer interrupt a hart arms a 1 ms sentinel
  until its handler arms the next event, so a stopped hart is late for something wherever the stop lands (without these, stalls were
  seen to land between a taken interrupt and the re-arm, unseen). The tests take a covered window again, print `##PAUSE <test> hart
  <h> late <us> us` for it, and fail after three covered windows in a row (`host pause: ...` in the message); a late window with no
  pause fails on the bound as before, and names a one-hart lateness it saw. Two planted delays are the controls, in
  `os-boot-test-sched-neg`: `fault-rt-wake-late` (every real-time wake 5 ms late, the interrupts on time) and
  `fault-rt-wake-late-irqoff` (the hart held 5 ms with interrupts off at every real-time wake); both must fail on the bound, with and
  without the injector (`OS_NEG_DETAIL` makes `--neg` insist on the failure's words). Under `-icount` with `sleep=off` at one hart the
  harness puts `guesttime` on the kernel line: nothing is excused there and the real-time probe's bound is 1 ms of guest time. What
  the two-hart rule still cannot tell from a pause is a kernel delay that shows on two harts at once (a lock held with interrupts off
  on one hart while another spins for it with interrupts off, for longer than the bound): only the three-in-a-row rule catches that,
  when it happens at every wake, and the `latency-trace` build (`make os-latency`) measures such spans directly. Record
  mode is a further discriminator: it runs every hart in one TCG thread, so a failure that also shows under `OS_RECORD` is not a pause
  of one hart.

On the development machine (macOS) the detector also showed the emulator taking a timer interrupt programmed T ahead about T / 2 late,
every time, with the hart running and interrupts on (a 3 ms sleep about 1.6 ms, a 10 ms slice end 5.04 to 5.09 ms): a host lateness that
the heartbeat keeps under a millisecond while a test measures.

**Repeat.** `make os-repeat TARGET=<target> N=<n>` runs a target N times, one at a time, prints `pass/N` and keeps the
serial log of every failing run in `target/repeat/`. Over a soak target this is a soak run (`TESTING.md` rule 6). The overnight
`overnight-repeat-smp` is `os-repeat` of `os-ktest-4`, the kernel-only boot at four harts that the epic gate runs (OD161), `N` times
(20 by default, about two hours on a hosted runner).

**Where a panic came from.** A kernel `##PANIC` line ends with `[stack 0x... 0x...]` (T266: whatever stack the panic ran on, found from `sp` by
`kcore::stackwalk`: a user or kernel thread's kernel stack, the hart's boot stack, which its idle loop runs on, or its emergency stack): the words on
that stack that point into the kernel's code, innermost first (the panic handler's own frames, then the return addresses of the calls that led
here, among a few stale ones). `python3 scripts/resolve_trace.py <kernel ELF> <log file>` names the symbol nearest each address (it needs only
`nm`); `llvm-addr2line -e <ELF> <addr>` gives the line. It is how a panic whose location is the standard library's (T233) is traced to its caller. Two debug detectors say more than where they fired
(T266): the big-lock spin panic (`has spun for ... 30 million iterations`) ends with `owner at the start: ..., owner takes since: <n>, waited <ms> ms` and a
reading (the lock is FIFO: the same owner with no take since held it the whole wait, at the site named, unless the host did not run its hart;
another owner means it passed through the harts queued ahead; takes since mean FIFO order broke), and the starvation watchdog counts a sample only
when the hart the thread waits on took an interrupt since the last one (`kcore::watchdog`), so `starved thread` means that hart ran and left the
thread, and `stalled hart` means it took no interrupt for 10 s.

**What outlived init.** When `no_process_or_vmo_survives_init` or `no_kernel_thread_survives_init` fails, the kernel prints `leak:` lines
first (T278): the live kernel objects by kind, every process left (its state, threads, handles by kind, job), and every thread that was not
there before init started, with its scheduler state and the kernel-text words on its stack as `[stack ...]` in **link** addresses (KASLR's
slide taken off), so `resolve_trace.py <ELF> 0x...` names them directly. `OS_SHOW=leak:` prints such lines from a passing run too.
`leak: dying <pid>` lines (T265) name every process on its way out, in the table or already out of it (a process the job still counts after
`teardown_take`): its stage (ending, closing handles, draining devices, freeing memory), the thread that noted it, and how long it has
been ending and out of the table (`kernel/src/user/dying.rs`). When `process_teardown_returns_every_frame` fails, it prints the per-hart
frame-cache fill, a frame the heap's pool has in hand, the live kernel stacks, the exited threads not yet reaped, the FP and vector
records, and the **walk oracle** for the kernel-stack and heap windows: what a walk of the kernel's page table finds under each window
(`PageTable::frames_in_range`) against what the leak oracle's formula counts (`kcore::kwindow`, proved by Kani; the boot self-test
`kernel_windows_hold_what_their_formulas_say` and its twin after init check that the two agree).

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
| `make docs-check` | `scripts/check_docs.py` (documents, the decision log, the work queue, the testing tiers, the atomics allow-list, the architecture boundary, the workflows, the syscall-error table) the self-tests of the benchmark comparison and the packet-capture checker, and `scripts/refused_devices.py --self-test` (the boot harness refuses a command line that names a device which reads or reports memory at a physical address, `iommu-testdev`: IOMMU-SPEC amendment 2026-11-27 part 2, L7; the feature gate runs it too) |
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

**Mutation testing.** `make os-mutants` (overnight; cargo-mutants) mutates `kcore`, `klock` and `iommucore` with their host tests as
the oracle. A survivor that is not in `mutants-accepted.txt` with a reason fails the run. `PKG=` and `SHARD=` run a slice.

**Coverage.** `make os-coverage` (overnight; cargo-llvm-cov) reports line and region coverage of the host tests of
`kcore`, `klock`, `iommucore`, `abi` and `ksys`. It is a report, never a gate: a number says where a test is missing, not that the
code is right.

## Kernel checks under QEMU

**Instruction counts.** `make os-count` boots the kernel-only shard at one hart under `-icount shift=0,sleep=off`, where the
guest's retired-instruction counter is deterministic, with no disks (`OS_DISK=0`: under `sleep=off` the guest's clock jumps to its next
timer deadline whenever it idles, so guest time bounds no host disk I/O and a disk test there times out at random; the harness refuses
`sleep=off` with disks, unless `OS_PCI_TEST=0`), and compares the suite's `##BENCH count_<name> <n> instr` lines
with `bench/counts.tsv`. A rise of more than 25 % fails (the slack absorbs the runner and unrelated changes, T223); a fall of more than 5 % is printed with a request to record it
(`make os-count-record`, then read the diff of the file). Record and compare in the same configuration (the `kfast` boot). The file holds the numbers of the CI run (2026-11-20: PL1 moved the test CPU to `rva23s64` and the kernel's code generation to the profile's extensions, and the epic gate's numbers then differed from a laptop's by up to 12 % in either direction, so the recorded values came from the gate's own log): a local `make os-count` is a reproduction, not the check.

**Benchmarks.** The suite prints `##BENCH <name> <ns> ns` lines (a malformed one fails the boot). Every boot runs the
benchmarks at a tenth of the iterations with sanity assertions only, so they cannot rot. `make os-bench` (soak) runs
them in full at one and four harts and compares with `bench/baseline.tsv` through `scripts/bench_compare.py`: a
threefold slowdown fails. The numbers are costs of the emulated machine and move with the host: read the ratios. Every run file
and baseline names where it was measured in `#@ machine:` and `#@ qemu:` lines (`bench_compare.py --provenance`; the make targets start
each run file with them, `make os-count-record` keeps them), and `bench_compare.py` says so when the run's machine or QEMU differs
from the baseline's, or when the baseline names neither (T195): a laptop's wall times against another machine's baseline were 1.2 to 3
times off, and one build's instruction counts under another QEMU 70% above `bench/counts.tsv`.
`pair_scaling_<kind>_n<N>` (futex and `channel_call` pairs, one, two and four) is the cost per round trip over all pairs;
`bench_compare.py` prints the scaling, the cost at one pair over the cost at N, and `--scaling-floor bench/scaling.tsv` (on in `make os-bench`; `make os-latency` uses `bench/scaling-latency.tsv`, channel_call only, because the traced futex pair is not comparable) fails if a ratio falls below its recorded floor. The floors are tripwires for a collapse (about 60% of the lowest of three full runs), not the design's goals of 3.0 and 2.5, which are not gates (SCHED-SMP-SPEC amendment of 2026-11-24, T252); the measured figures at four harts are futex 0.5 to 1.3 (it swings with the pair's placement) and `channel_call` 0.8. `rt_latency_p99` and `rt_latency_max` are the
real-time probe's lateness, printed by the kernel's scheduler test.

**The latency trace.** `make os-latency` (soak) builds the kernel with the `latency-trace` feature, runs the suite at
four harts and prints the longest interrupts-off span of any hart with where it began, and per syscall the big kernel
lock's hold and wait. The test `the_latency_trace_can_see_a_long_section` proves a planted section is reported. The run also
prints them as `##BENCH irq_off_max`, `bkl_hold_max` and, for the ten worst syscalls, `bkl_hold_max_sys<N>` and
`bkl_wait_max_sys<N>`, and compares the first two with `bench/latency.tsv` (`bench_compare.py`).

**Schedule perturbation.** `make os-chaos` (overnight) boots the kernel-only shard on the `chaos` kernel, one boot per
seed in `CHAOS_SEEDS` and per hart count, with delays at lock, IPI, switch and user-return points. A failing seed is
named and its log kept under `target/chaos/`; `OS_CHAOS_POINTS=<mask>` (1 acquire, 2 release, 4 IPI, 8 switch, 16 user
return, 32 copy-out, 64 the stage boundaries of a process teardown) narrows the point.

**The checked release kernel.** `make os-boot-test-checked` (overnight) runs the whole suite at four harts and the
kernel-only shard at one, two and eight on the `release-checked` profile: optimised, with debug assertions and
overflow checks on.

**The matrix and the negative controls** (soak): `os-boot-test` (the normal, fail-demo, round-robin and release
kernels), `os-boot-test-smp` (two and four harts), `os-boot-test-nodisk`,
`os-boot-test-cpu-neg` (a CPU without the profile ends in `##PANIC missing extension`), `os-boot-test-iommu-neg` (a machine without the platform IOMMU ends in `##PANIC ... no IOMMU`), `os-boot-test-overflow`, `os-serial-test`, and the negative controls, each a kernel built with one
mechanism disabled that the suite must then fail: `os-boot-test-smp-neg`, `os-boot-test-sched-neg`, `os-boot-test-aia` (also the vector twins `fault-vector-no-unmask` and `fault-vector-no-repend`, which `pci_virtio_blk_interrupts_through_msix` must fail, and `fault-msiid-free-at-move`), `os-boot-test-iommu-bare` (`ddtp` Bare: `iommu_is_on_and_denies_by_default` must fail), `os-boot-test-iommu-inval`
(`fault-iommu-skip-inval`, an unmap without its invalidation and fence: `iodomain_unmap_stops_the_device` and, since IOMMU12,
the hostile-driver matrix's `hostile_dma_after_its_own_unmap` must fail),
`os-boot-test-iommu-msi` (`fault-iommu-msi-basic`, basic-translate MSI entries that pass the device's data word to the hart:
`msi_forgery_faults` must fail; `make os-models` also checks that the host model `iodomain_vectors_model` fails with it).

**The network checks by tier.**

| Check | Tier | Where |
|---|---|---|
| `nettcp`, `netcore` TCP, `netwire::tcp`, the `netsim` lossy-link suite (reduced seeds), `dns`, `http1`, `sockshim`, `rngcore` | 0 | `make test` |
| the streams, sockets, random, fetch and `httpd` tests of the suite; `net_tcp_churn` in its short form | 1 | shard `b` |
| the bulk TCP tests behind the lossy relay (`make os-boot-test-lossy`, `LOSSY_SEEDS`), `net_tcp_churn` at full length, the `##BENCH tcp_*` lines | 2 | `make os-boot-test-lossy`, `make os-bench` |

## Running several boots at once

**Host ports.** The host side of the network tests the guest connects to (a TCP server, a scripted DNS server and an HTTP server:
`scripts/host_fixtures.py`) binds ports the host chooses, one set per boot, and the kernel line tells the guest where they are
(`hostports=<tcp>,<dns>,<http>`, read as `debug_stat` 15). The ports the host forwards into the guest were already chosen per boot. So two
network boots at once on one machine no longer collide (they did, at fixed ports 47802 to 47804, until T195); `host_fixtures.py
--self-test` (in the docs check) starts two sets at once and checks that their ports differ and that each answers on its own. A boot whose
guest ran the network tests must also have reached the fixtures: the harness fails it if the fetch or the host-server test skipped.

**Timing.** `make soak` runs its targets one at a time (`SOAK_JOBS`, default 1) and ends with one line per target: PASS or FAIL, its
wall time, and the log of a failing one under `target/tier/` (`scripts/tier.py run`). The tests follow the host's clock, so several
boots at once on one machine, or a machine doing anything else, report timing failures that are not bugs (the realtime and latency
bounds, the benchmark comparison): a larger `SOAK_JOBS` is a quick look, not a result to trust.

**Pieces of two hours** (STAB0). `make soak-piece PIECE=<soak target>` and `make overnight-piece PIECE=<overnight target>` run one
target of the tier, with its size where it has one: `CHAOS_SEEDS=A..B` for `os-chaos`, `N=<boots>` for `overnight-repeat-smp`,
`PKG=<crate> SHARD=k/n` for `os-mutants`. They refuse a target of another tier and a malformed size (`scripts/tier.py piece`). The
workflows run them in their `piece` job when the owner dispatches with inputs, each piece within 120 minutes:
`gh workflow run soak.yml --ref <branch> -f target=os-boot-test-smp`, `gh workflow run overnight.yml --ref <branch> -f
target=os-chaos -f chaos_seeds=1..40`, `-f target=overnight-repeat-smp -f n=20`, `-f target=os-mutants -f pkg=kcore -f shard=3/8`.
With no input a dispatch runs the whole tier as before. The docs check refuses any other trigger, any other input, a piece job without
its 120-minute limit or its guard, and a whole-tier job that would also run in a piece (`check_docs.py --self-test` plants each).

## CI's QEMU

CI boots on **QEMU 11.1.3**, the version the project develops on and the one the IOMMU facts were taken from
(`docs/records/notes/2026-11-25-iommu-qemu-facts.md`). It is not the distribution's package: Ubuntu 26.04 ships 10.2.1, whose `virt`
machine writes an `iommu-map` that leaves PCI requester id 0xffff outside the IOMMU, so the kernel (IOMMU-SPEC 2.2) keeps the PCI
windows closed and the IOMMU, MSI and device-capability tests fail. 11.1.0 is the first release with the single full entry.

**What is built.** `.github/actions/install-qemu/build-qemu.sh` pins the version, the tarball's sha256 (the same as Homebrew's
formula) and the fingerprint of the QEMU release key (`CEAC C9E1 5534 EBAB B82D 3FA0 3353 C9CE F108 B584`; the public key is
`qemu-release-key.asc` beside the script). It downloads the tarball from download.qemu.org, checks both before extracting it, and
builds `riscv64-softmmu` alone with `--without-default-features` plus TCG, slirp (`-netdev user`), pixman (`screendump`, ramfb) and the
system libfdt, with `--disable-download` (nothing is fetched at configure time); no GUI, VNC, docs, tools or guest agent, and
the UEFI images (about 300 MB) are deleted after the install. `-bios default` is the OpenSBI that ships with the release (1.8.1 in 11.1.3),
as on a developer's machine; the distribution's `opensbi` package is no longer installed.

**The cache.** The install prefix (`~/qemu`) is one Actions cache entry, keyed
`qemu-<version>-<distribution><release>-<arch>-<config>`, where `<config>` is a hash of the build script and the release key (so a new
version, checksum or configure flag is a new entry) and the distribution part follows the runner image whose libraries the binary
links against. The apt packages are cached by the same action, keyed by the distribution too. The install action
(`.github/actions/install-qemu`) has two modes:

- `mode: use` (default; every job that boots): restore the entry, install its run-time libraries from the apt cache, put it on the
  `PATH` and run `check-qemu.sh`. A miss **fails the job**; a job that boots never builds QEMU and never falls back to apt.
- `mode: fill` (the tool-build job only): look the entry up without downloading it; on a miss, build it (the build dependencies come
  through the same apt cache) and save it.

`check-qemu.sh` is the version assertion: the `qemu-system-riscv64` on the `PATH` must be `~/qemu/bin/qemu-system-riscv64` and must
print `QEMU emulator version 11.1.3` (the version is read from the build script, so the two cannot drift); it also requires each of the
`aia` and `iommu-sys` machine options, the `rva23s64` CPU and the `user` network backend. The action runs it after every restore, and
the smoke jobs `kernel (smoke a)` and `kernel (smoke b)` run it again as a step of their own.

**Who fills it.** GitHub scopes a cache to the ref that saved it: a run can restore entries of its own ref and of `main`, and a
pull request also those of its base branch. Two places build into the cache:

- **ci.yml's `qemu` job**, which every job that boots `needs` (the docs check refuses a booting job without it). On a warm cache it is
  a lookup (the job takes some tens of seconds with the runner's start, which every boot job now waits for); on a cold one it builds
  before any boot starts. So every smoke is correct with no step by hand, wherever
  it runs: a smoke dispatched on `epic/<name>` fills that epic's scope, the epic's pull request into `main` reads `main`'s entry (or
  builds into its own scope the first time), and the push to `main` that merges an epic fills `main`, after which every branch and
  pull request reads `main`'s entry. It is the tool-build exception to the gate's budget (`TESTING.md`, "Budgets"). The docs check
  also refuses, in `ci.yml`, `soak.yml` and `overnight.yml`, a job that boots without the install action (or with `qemu: 'false'`),
  and a job that uses CI's QEMU without needing this job (soak and overnight have their own `qemu` job).
- **`qemu.yml`**, which runs the same lookup-or-build and then checks the entry from a fresh runner as a boot job would (restore, the
  version assertion, the device tree's `iommu-map` must be the single full entry `<0 phandle 0 0x10000>`
  (`check-iommu-map.py`), the machine started with no kernel to OpenSBI's banner with the harness's devices and backends, the lossy
  relay's hub and socket backends, and record mode). It runs by itself on a push, to any branch but `main`, that changes
  `.github/actions/install-qemu/` or `qemu.yml`. On a feature branch that proves the build but warms only the branch itself (its
  pull request runs under `refs/pull/<n>/merge` and reads the base's entries, not the branch's); the push of the squash-merge then
  warms the epic. By hand: `gh workflow run qemu.yml --ref <branch>`.

**When to dispatch `qemu.yml` by hand.** Only to keep a cold build out of a smoke's wall time: before a smoke on an epic whose scope
has no entry (an epic cut before `main` had one, or after the entry was evicted: GitHub drops an entry unused for 7 days, and
evicts the least recently used first when the repository's 10 GB, shared with the Rust caches, is full), run
`gh workflow run qemu.yml --ref epic/<name>` and start the smoke when it is green. Nothing else needs it; a smoke without it is
still correct, only longer by the build (about 3 minutes cold, measured 2026-11-26).

**Changing the version.** Edit `QEMU_VERSION`, `QEMU_SHA256` (and the key file and `QEMU_KEY_FPR` if the release manager changed)
in `build-qemu.sh`; the key changes, the next tool-build job builds it, and `check-qemu.sh` then expects the new version.
