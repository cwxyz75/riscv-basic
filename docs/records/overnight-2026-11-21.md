# The overnight run of 2026-11-21: soak, overnight tier, fixes, and where phase 6 stands

A record (append-only). The owner asked for a full soak, then the overnight tier, `os-miri` and a full local gate, fixes as bugs were found, and
then the plan to go on. Everything ran on one Mac (ten cores, QEMU 11.1.3); CI installs QEMU 10.2.1. What was and was not run is stated for
each line. The decisions taken without the owner are in `WORK-PLAN.md`, "Decisions taken without the owner" (2026-11-21).

## Tier 2 (`make soak`)

First run, three boots at a time: eight of fifteen targets failed. Run again one at a time: `os-boot-test-aia`, `lossy`, `smp` and `latency`
pass (load, or host ports shared by parallel boots: T195). `os-bench` fails only against a baseline recorded on another machine on 2026-11-14
(and the instruction counts of the same build are about 70 percent above the CI-derived `bench/counts.tsv`: a different QEMU). Three failures
were real and are fixed (PR 333):

- `os-boot-test-cpu-neg`: on a CPU without the profile the kernel hung with no output. The device-tree reader is compiled with the RVA23
  extensions (Zicond), so it trapped in `console::init_from_tree`, before `dtb::require_extensions` could say anything, and the early trap
  restarted the boot in a loop. `_start` now installs an early trap handler first (base ISA only): `##PANIC missing extension ...` through the
  firmware console, then a failing exit through the test finisher (a test build) or SBI SRST.
- the `fault-early-reap` negative control (`os-boot-test-sched-neg`) passed although it lacked its mechanism: it depended on a race, and
  `reap_if_dead` has refused a thread in transit since the G4 review, so the control did nothing. It now reaps directly and writes to the freed
  stack slot at once.
- the `fault-shootdown` control (`os-boot-test-smp-neg`) passes on QEMU 11.1.3: not fixed, T193 (it depends on how long the emulator keeps a stale
  TLB entry; to be checked on CI's 10.2.1).

## Tier 3 (`make os-chaos`, `os-miri`, ... in a reduced form)

- `os-fuzz-host` (20,000 seeds of 600 calls): pass. `os-allocfail`: pass. `os-boot-test-checked` (the release kernel with the assertions on, the
  suite at four harts and the kernel shard at one, two and eight): pass. `overnight-repeat-smp`: ten boots of the smp suite, all pass (the tier
  says twenty).
- `os-chaos`: seeds 1 to 50 at two, four and eight harts (the default is 1 to 200), two jobs: 144 of 150 boots pass; all six failures are
  `realtime_thread_stays_responsive_under_load` (T178, it fails under host load). An earlier first attempt with three jobs at eight harts beside
  builds had two more failures (a starvation-watchdog panic and `kill_a_process_that_is_creating_threads`); they did not reproduce in four
  reruns and the 150 valid boots: T197. That first attempt was also invalidated by my own builds and boots on the same machine.
- `os-miri`: `abi`, `kcore`, `frame_model`, `ksys` and `klock` pass under Miri, no undefined behaviour reported; `ksys`'s allocation sweep
  starts child processes and is skipped (it runs in `os-allocfail`).
- `os-coverage` reported 0.00 percent everywhere: `cargo-llvm-cov` 0.6.16 cannot read rustc 1.100's coverage data. 0.9.2 does: 90.4 percent of
  regions and 91.3 percent of lines over `kcore`, `klock`, `abi` and `ksys` (the pin is bumped: PR 342).
- `os-mutants` over `klock`: 53 mutants, 4 missed and 1 timed out; killed by new tests (PR 341). Over `kcore`: stopped after 44 minutes (145 of
  3,686 mutants, 22 missed, 2 timed out; the whole crate would take about 18 hours here, CI shards it); eight survivors killed by new tests,
  the rest in T198 (19 listed).
- `make check` (the full local gate, standing in for the retired `preflight-full`): every step passes except `os-count`, which is 5 to 15 percent
  above its CI-derived baseline for the emulator reason above (CI's own count job passed at the phase 5 gate; the baseline is only to be
  re-recorded from CI). `os-release-smoke` run on its own: pass.

## Findings that stand

T193 (shootdown control on a newer QEMU), T194 (QEMU's "zero sized buffers" message at several boots: a guest queues a zero-length
descriptor somewhere; not found by reading the drivers), T195 (the parallel soak reports load failures; host ports are shared by every boot),
T197 (two overload-only failures), T198 (the kcore mutation survivors), T196 (MM4's end state).

## Phase 5

Merged into `main` as a merge commit (#329). The epic gate failed twice on the four-hart jobs: the cause was older than phase 5 (every
thread exit woke every sibling and scanned every thread, quadratic: 2,047 exits took minutes at four harts); fixed in #331, found with
progress marks in `thread_limit_and_slots_are_reused`.

## Phase 6 (`epic/phase-6`, not finished, not merged)

Merged into the epic: the soak fixes; L15 (mappings hold a VMO reference, `reap_vmos` deleted); MM4 in two parts (`kcore::slab`, then the
kernel heap serving small objects from slabs behind per-hart magazines with a frame pool: the spec's inline frame runs wait for L16's `FRAME`
lock, T196); L16 part a (the job ledger out of the VMO store); T191 (a per-process set of live threads); T189 part 2 (the `ff=` run reports
what it started); T171 (channel_write checks the channel handle first); the `klock` and `kcore` mutation tests; the cargo-mutants output
that two of my PRs committed by mistake is removed again (#347). A full-suite boot at two harts on the epic head passes (329 tests); the
kernel-only shard passes at one, two and four harts. Not run on the epic: the epic gate (smoke), one and four harts of the full suite, the
C and Lua transcripts. MM4, L16, MM5, L17, L18 and PL7 are not done; L16's three real locks, its frame caches and page reservations are
the next piece, and they are the ones that want a fresh-context review and a soak.
