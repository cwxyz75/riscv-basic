---
id: STAB0
kind: slice
title: The stability epic (umbrella): find and fix the causes of the intermittent failures, so the freeze's soak and long runs can be green
area: testing
horizon: long
complexity: H
change_risk: M
epic: stability
refs: [FZ1, OD140, OD142, OD144, OD160, OD161, TESTING, TEST-TOOLS, docs/prompts/stability.md]
---

**Purpose.** FZ1 (the freeze) needs the soak tier and the long runs green on three consecutive owner-started runs, and no open
risk-H item in `kernel`, `sched` or `security`. Today the epic gate, the soak and the long runs fail now and then for causes that
nobody has named: frames or processes that outlive a teardown, a panic in a copy, a big-lock spin, a watchdog that fires on a host
pause, timing bounds that a loaded host breaks, storage services that miss a restart window, and negative controls that once could
not fail. TESTING.md rule 11 and OD160 make each of them a defect until its cause is known. This epic finds and fixes those causes.
It is testing, reproducing and debugging, not feature work: each child names a root cause, and its fix comes with a regression
test or a negative control and a mutation check that shows the test catches the bug.

**The branch** is `epic/stability`, cut from `main`; every child is a feature PR into it (TESTING.md, OD144). The manager and
engineer agents read `docs/prompts/stability.md` **in addition to** `docs/prompts/manager.md` and `docs/prompts/engineer.md`;
where it says so, it overrides them for this epic.

## The owner's constraints (2026-11-27)

1. **Models.** Opus for all engineering, experiments and reviews in this epic. Escalate to a Fable model only when an Opus agent
   has honestly tried and cannot find a solution, and say so in the report with what was tried. This overrides the engineer
   prompt's rule of a Fable reviewer for H complexity or kernel/security code, for this epic only.
2. **Tools.** Static analysis, model checkers and provers, loom and shuttle, record and replay, seeded chaos, targeted code
   reviews and extensive testing; each child below says which fits. A tool not yet in the tree needs its own OD (CLAUDE.md): they
   are listed for the owner below, and no child adopts one before the owner records it.
3. **Time.** No single test run exceeds **two hours** of wall clock. A repeated-run or soak-style experiment is split or bounded to
   fit, and each child states its budget in its acceptance.
4. **Focus.** Testing, reproducing and debugging. A child's fix names the root cause; a count of green runs never closes an
   intermittent part (OD160).
5. **Policy unchanged.** Feature PRs into the epic, no routine local tests, CI is the check; soak and long runs only when the owner
   asks (OD142), so the runs this plan needs are listed as owner requests at named points, never scheduled; `docs/specs/` is off
   limits (a spec problem becomes a work item); no shortcuts or stand-ins.

## The children, in order, and why

| # | Item | What | complexity / change_risk | Why here |
|---|---|---|---|---|
| 1 | T283 | Soak noise and vacuous controls; the owner's runs fit two hours a piece | M / L | The owner's checkpoint runs must be evidence: no port collisions, no foreign baselines, controls that can fail, `overnight-repeat-smp` on `os-ktest-4` (OD161), workflows that run a two-hour piece |
| 2 | T284 | Timing tests tell a host pause from a kernel delay | M / L | Builds the host-stall injector and the pause detector that T266 (T197), T287 and every timing bound need to name a cause instead of blaming load |
| 3 | T266 | Panics and spins made to name their cause (PR A of its plan) | H / M | The mutation run that ties T233 to PR 491's fix; the trace on any stack; spin and watchdog reports; afterwards every panic and spin names itself |
| 4 | T265 | Teardown, kill and frame accounting at several harts | H / M | The failures closest to FZ1's leak oracles (T227 with T278 seen together); the walk oracle says where a frame is before any kernel path changes |
| 5 | T286 | Storage services under restart and load | M / M | Deterministic slow and failing disks (QEMU throttling, `blkdebug`) replace "a loaded runner"; independent of 3 and 4 |
| 6 | T287 | Late threads and lost wakes | H / M | Needs T284's detector (blocked_by); the T49 shape was seen on `main` on 2026-11-27 |
| 7 | T285 | A failed copy-out leaves the message queued (OD161) | H / H | The fix behind T233's second symptom; after T266 (blocked_by), whose mutation run says whether another cause is live |
| 8 | T288 | klock: every ordered pair, strict release, the shuttle flake | M / M | Host-only; independent; strict release may expose double releases that the earlier children's boots would otherwise meet |
| 9 | T289 | Kernel stack depth: an 8 KiB budget in both builds | M / M | The budget has turned an epic smoke red; wide and mechanical, so after the hunts that read the same files |
| 10 | T290 | Freed kernel stacks poisoned, the faithful early-reap control, the frame-drain test | M / H | Detects use-after-free of a stack, today undetectable; after T289 (same files) |
| 11 | T259 | Spawn guards, the pthread_create hand-off, the stack arena, hart 0's clock | H / H | A livelock and a guard overlap; the arena refusal makes T272's class deterministic; last of the code changes |
| 12 | T150 | Review findings with no recorded outcome | L / L | A reading pass over old findings for live races and crashes; may feed any child above; last because the hunts above change what is still live |

`blocked_by` holds only the two real dependencies (T287 on T284, T285 on T266); the rest of the order is the manager's to change
for a reason (manager.md), with one rule: a child that changes a path another child is hunting in (T285, T289, T290 and T259
change kernel paths that T265 and T266 instrument) goes after that hunt has its diagnostics merged.

## The method, per child

Every child follows `docs/prompts/stability.md`: reproduce first, bisect where there is a range, name the cause, fix it with a
regression test or negative control, and mutation-check the test. What fits each:

| Item | Reproduce with | Static analysis, models and proofs | Review |
|---|---|---|---|
| T283 | Two network boots at once (port collision); single control kernels by hand, five boots | Docs-check self-test for the workflow inputs; a `kcore` shootdown model | Opus |
| T284 | `OS_HOST_STALL` (built here); a planted-delay `fault-` kernel; `OS_RECORD` as a discriminator (one TCG thread: no single-hart pause) | Property test of the detector's rule | Opus |
| T266 | One chaos boot with the reverted `current_space()`; the scratch revert of thread-exit charging; `OS_HOST_STALL` for the watchdog | Kani: the trace's slot arithmetic; host property tests of the watchdog rule | Opus |
| T265 | `os-repeat` of smoke a (12, about an hour), two or three at once; chaos points on the teardown path; the forced stack draw for T251 | TLA+ of kill and teardown; Kani of the oracle's formulas; the walk oracle as an independent oracle | Opus |
| T286 | QEMU throttling and `blkdebug`; `os-repeat` of `os-ktest-4` (15, about an hour); CI logs of the sightings | Host test of `volmgr`'s retry decision | Opus |
| T287 | `OS_HOST_STALL`, `OS_RECORD`, CPU burners on the host; `os-repeat` (15 per variant) | loom/shuttle model of `rt::sync::Semaphore` over a model futex; TLA+ of wait/wake if those pass and it recurs | Opus |
| T285 | T266's storms; the `ksys` allocation-failure harness | TLA+ (or shuttle) of peek, copy, commit with a killer and a second reader | Opus |
| T288 | Shuttle random and PCT schedules, seeds kept; loom; Miri on `klock` | Kani of the order table | Opus |
| T289 | One release boot with stack-use prints | `-Z emit-stack-sizes` with a call-graph script (cargo-call-stack would need an OD); clippy `large_stack_frames` on `ksys` and `kcore` | Opus |
| T290 | The stack-only control kernel, one boot by hand | Kani of the poison range and of the lock-free walk | Opus |
| T259 | The C transcript (the livelock), the `ksys` arithmetic test (the guard) | A shuttle model of the hand-off if the transcript cannot show it | Opus |
| T150 | (reading) | Targeted review, fresh context per review | Opus |

Broader static analysis that any child may run as an experiment (no gate is added by this epic): clippy's `pedantic` and
`nursery` groups and `clippy::undocumented_unsafe_blocks`, `clippy::indexing_slicing` and `clippy::arithmetic_side_effects` over
`kcore`, `ksys` and `klock` (read the findings, fix the real ones; do not turn on a lint group in CI without the owner);
Miri over the pure crates' tests where they run under it (`make os-miri` is soak; a targeted `cargo +nightly miri test -p <crate>
--test <one>` is an experiment); the release-checked kernel (`make os-kchecked-build`, then one boot by hand) for a failure that
might be overflow or an assertion the release build drops.

## What "done" means for the epic

1. Every child is merged into `epic/stability`, its acceptance met as its item says.
2. Every intermittent part of T265, T266, T286 and T287 is either struck on a named cause tied by a regression test, a negative
   control or a mutation run (OD160), or stays open with a record of the bounded hunt (what was run, how many times, the seeds,
   the logs or replay captures, what was ruled out) and its self-reporting diagnostics merged, so its next occurrence names its
   cause. A part is never struck on a count of green runs.
3. The epic gate is green on `epic/stability` (the kernel-only boots at one, two and four harts, the release boot, both suite
   shards at four harts), and the epic merges into `main` with a merge commit.
4. The owner has been asked for run point B (below); its result is FZ1's business, not this epic's: a red owner run means "fix
   next" (rule 6), with a new child on the next epic if the cause is new.

## Owner runs (requests at named points; the builder never starts them)

Every run is dispatched as pieces of **at most two hours** each (T283 adds the workflow inputs; until it merges, the owner runs the
targets locally or by `make` in pieces of that size).

- **Point A: after T283, T284, T266 and T265 have merged into the epic** and a checkpoint smoke on the epic is green
  (`gh workflow run ci.yml --ref epic/stability`, TESTING.md rule 5; the manager may start this one). Ask the owner for: the soak
  jobs `smp` (two and four harts), `negative controls and serial at four harts`, and `os-latency`, each a piece; `overnight-repeat-smp`
  (now `os-ktest-4`) with N bounded to two hours (about N=20); `os-chaos` with `CHAOS_SEEDS` split into ranges of two hours or less at
  two, four and eight harts (this is also OD160's chaos run for T222); `os-boot-test-checked`. The purpose is harvest: with the new
  diagnostics, what fails now names itself and feeds T286 and T287 and the still-open parts.
- **Point B: when every child has merged and the epic smoke is green, before the epic's PR into `main`.** Ask for the full soak
  tier and the long-run tier in two-hour pieces (`os-mutants` by `SHARD=k/n` pieces, `os-fuzz-host` and `os-allocfail` with seeds or
  calls bounded to fit). If green, it can count as the first of FZ1's three runs if the owner says so.

No other soak, long run, `os-repeat` over a soak target, chaos run or mutation run is part of this plan. An engineer's `os-repeat`
over a **smoke** target to reproduce a known failure is ordinary debugging (OD160), inside the two-hour cap.

## Tools that need an OD (for the owner; none is adopted before it is recorded)

| Tool | For | Note |
|---|---|---|
| **Kani** (bounded model checker for Rust) | T265 (oracle formulas), T266 (trace arithmetic), T288 (the order table), T290 (poison range, lock-free walk) | Host-only, on the pure crates; `cargo kani` harnesses beside the tests; not in CI unless the owner says |
| **TLA+ with TLC** (or Apalache) | T265 (kill and teardown), T285 (peek, copy, commit), T287 (wait/wake, if the host models pass) | Specs under `docs/records/` or a `models/` directory; a Java tool, run by hand |
| **Verus** | Not recommended for this epic | It needs the verified code written in its own dialect; the frame-accounting invariant (M3) would be its candidate later |
| **cargo-careful** | Any child, for host tests of `kcore`, `ksys`, `klock` with the standard library's debug assertions | Nightly toolchain (already used for Miri) |
| **cargo-call-stack** | T289 (the deepest static call chain) | Optional: `-Z emit-stack-sizes` with a script does most of it without a new tool |
| **ThreadSanitizer** (`-Zsanitizer=thread`, nightly) | T288 and the host concurrency tests | Optional; loom and shuttle already cover the models |

No OD is needed for: QEMU's block throttling, `blkdebug`, `-icount` and record and replay (QEMU itself); the host-stall injector and
the pause detector (written here, T284); clippy lint groups run as an experiment; Miri, loom, shuttle, cargo-mutants and
cargo-llvm-cov (already in the tree).

## Explicitly left out

- **T267** (instruction-count drift, T223; pair scaling, T252): measurement, not a flake (OD150 and the owner's acceptance of the
  scaling numbers); after FZ1.
- **T264** (what is left: untested error pairs, coverage, mutation survivors, harness trust, fault injection): test breadth.
- **T254** (what is left: T202 KernelBytes charging, T68 the slot shrink, whose Q2 the owner has not answered): accounting and
  memory return, not failures.
- **T256** (KT14 tail: aliasing, firmware addresses, the FP claim model) and **T261** (counters, ASIDs, shootdowns, the ASID-reuse
  control that QEMU cannot observe): soundness and design work with their own plans; candidates for the next epic. If a hunt here
  points into either, the manager raises it with the owner instead of widening this epic.
- **T257** (class N outside the scheduler) and **T268** (the scheduler overhaul): feature work; T265's "threads not yet reaped" term
  reads T257's state but does not change it.
- **T151** (re-check the migrated user-space items) and **T24** (reviews by a model of the same family): process, parked by plan 4.
- **The T206 window test** (`a_close_that_frees_a_chain_takes_interrupts_between_its_slices`: "no interrupt was taken inside a
  window in 40 rounds", seen on `main` at `ae08a87e`, run 45935032014, `kernel (ktest release)`, and on `epic/no-testdev`, run
  46018655715): another session is fixing it on `fix/t206-window-tick`. If that lands without a named cause, it joins T287.

## Closed or merged by this plan (2026-11-27)

T277 closed (cause named: the VMO table's doubling, PR 590, green on two epic smokes); T23 closed (stale: CI's QEMU is cached and
the boots run as parallel jobs); T278 and T270 merged into T265; T269 and T122 into T286; T135 into T288. Moved between items: T209,
T222, T226 out of T265; T167, T49, T219, T178 out of T267; T21, T40, T44, T193, T195, T232 out of T264; T85 out of T259; T172,
T173, T164, T160, T82 out of T254; PR B of T266 into T285.
