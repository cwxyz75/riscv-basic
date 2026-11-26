# Testing

The testing policy: which checks run when, on which branch, and what is deliberately not run. The decisions are OD83 (the
tiers), OD140 (the kernel's regime), OD142 (soak and long runs only when the owner asks) and OD144 (epic branches, the
feature gate, the commit hook). What each check does and how to run or replay it is in `TEST-TOOLS.md`. Measurements,
profiles and the history of the regime are records (`docs/records/testing-2026-11-20.md` is the first).

**The principle.** The project is in primary development. Test time must be in proportion to the change: a small change
gets a build and the host tests, and the boots are paid once per phase. The cost accepted in return is a longer hunt
now and then when a phase's boots fail; "When an epic's smoke is red" keeps that hunt short.

## Branches

| Branch | What it is | What merges into it | Gate |
|---|---|---|---|
| `main` | Always builds and boots. | An epic branch, when its phase is done; a documentation-only change, directly. | The epic gate (tier 1). |
| `epic/<name>` | One epic: the children of an umbrella work item (a large one may be cut into two or three epics). Cut from `main`. | Feature branches. | The feature gate (tier 0). |
| a feature branch | One work item, or one named part of one, cut from the epic. | Commits. | The commit hook. |

- A feature branch is **squash-merged** into its epic, so an epic is one commit per piece of work.
- An epic is merged into `main` with a **merge commit**, so those commits survive on `main`; the first-parent history of
  `main` is the history that has booted.
- A tidy item, a fix or a CI change that touches code rides on the open epic. Only documentation-only changes go straight
  to `main` (CI skips the boots for them).
- When `main` moves under an epic, merge `main` into the epic. Never rebase or force-push an epic.
- A commit on an epic has built and passed the host tests. It has not booted. `main` is the only branch that promises a
  boot.

## The gates

| Tier | Gate | Runs | What | Budget | Make target | CI |
|---|---|---|---|---|---|---|
| | the commit hook | every commit, locally | format, the docs check, clippy (which does the whole of `cargo check`); **no tests** | seconds when warm | `make hooks` installs it; it runs `fmt-check`, `docs-check`, `lint` | none |
| 0 | the feature gate | every pull request (into an epic, and as part of tier 1) | format, the docs check, clippy on the host crates and on everything built for the target, the host tests of every crate, the loom and shuttle model checkers, the kernel against the AArch64 stub, a build of the OS kernel with its user programs, the home kernel and the optimised system image; **no QEMU** | 7 minutes per job | `make feature` | `ci.yml`: `host`, `models`, `build`, `build (release)` |
| 1 | the epic gate (smoke) | a pull request into `main`; a push to `main`; by hand on any branch | tier 0, then the boots: the whole suite on the default machine at four harts (two shards), the kernel-only boot at one, two and four harts and in the optimised build, the instruction counts, the release smoke, the shell test, the home computer's boot | 7 minutes per job, the jobs side by side: **10 minutes for the whole gate** | `make smoke` (`make check`) | `ci.yml`: the `kernel (...)` jobs |
| 2 | soak | **only when the owner asks** (OD142) | the full matrix (hart counts, round-robin, release, RVA23, no disk, the serial sessions), every negative control, the benchmarks, Miri | none | `make soak` | `soak.yml`, manual start only |
| 3 | overnight | **only when the owner asks** (OD142) | the long runs: repeat boots, the checked release kernel, schedule perturbation, mutation testing, coverage, the host fuzzer at length, the persistent allocation-failure sweep | 300 minutes per job | `OVERNIGHT_TARGETS` | `overnight.yml`, manual start only |

## Rules

1. **A feature merges on tier 0.** A pull request into an epic runs no boot, whatever it touches. Merge it when the
   feature gate is green.
2. **An epic merges on tier 1, and tier 1 is not to grow.** Smoke is the right amount of testing for a phase. Its
   budget is 10 minutes of wall time for the whole gate (7 per job, checked). A new boot, or a longer one, is added to
   smoke only by a decision of the owner; work that outgrows the budget is cut or moved (see "Budgets").
3. **No tests as a routine, locally.** The commit hook runs no tests and nothing is run "before a push": CI is the
   check. Not `make feature`, not `make smoke`, not the host test suite, as a habit.
4. **Experiments are welcome.** The one test being written (`cargo test -p kcore --test X`), one targeted boot to
   watch an acceptance check fail and then pass (`make kquick`, or one shard), a mutation check of a new test, a
   reproduction while debugging, static analysis: all fine, at the builder's judgement. The line is between answering a
   question and running a ritual.
5. **Smoke on an epic, by hand.** `gh workflow run ci.yml --ref epic/<name>` runs the epic gate on an epic branch. It
   is a checkpoint for a long or risky epic (after the change that reworks a lock, say), started by the owner or by the
   builder, never required and never routine: at most a few per epic.
6. **Soak and overnight run only when the owner asks** (OD142). Nobody else starts them: not `make soak` or one of its
   targets locally, not `soak.yml` or `overnight.yml`, not a schedule, not a label. Running `os-repeat` over a soak
   target is a soak run. Changes do not each record a run as owed: nothing is recorded per change, and one
   run of a tier covers everything merged before it. A red soak or overnight run means "fix next", not "revert".
7. **A work item closes when its epic's gate is green.** An item's acceptance check is written first, as before. `scripts/work.py done` runs in the epic's closing commit,
   so the record reaches `main` only with a green boot behind it. The kernel-only boots of tier 1 are the verification
   at one, two and four harts.
8. **Where a new test goes.** Pure logic (a decision, a data structure, a codec, an accounting rule): a host test in a
   pure crate (`kcore`, `ksys`, `proto`, ...), model-based or property-based where it can be. A syscall or server
   behaviour: the QEMU suite (`init/src/bin/suite.rs`). Only what the machine itself shows (traps, the MMU and TLB, SMP
   races, the interrupt controller, FP state, boot, real drivers) must be in QEMU at all. Long, exhaustive or
   seed-heavy things go to tier 2 or 3, not into smoke.
9. **Negative controls and mutation checks** guard a specific mechanism. A mutation check is run once per new test, by
   hand; the standing negative controls are tier 2.
10. **Independent oracles** (the upstream Lua suite, the C transcript against the host build, the BASIC golden corpus,
    real clients) stay in the suite, in smoke, while smoke fits its budget.
11. **An intermittent failure of a kernel test is a defect until its cause is known** (OD140, OD160). A failure that
    appears is investigated, not re-run and noted: first ask whether the change under test could cause it (if so, it
    is that change's defect, to fix in the same PR), then try to reproduce it. A failure counts as **intermittent**
    only after **five** attempts to reproduce it have failed (the same test, target and hart count, the failing seed
    or a record-and-replay capture where there is one: `os-repeat` loops over a smoke target are ordinary debugging, OD160). Then, and not
    before, it gets a work item (area `testing`) with where and when it was seen, the log, what was tried and how to
    trace it, and the PR it appeared on goes on. The item closes only on a named cause or a demonstrated test race,
    never on a count of green runs (OD160). (A job that fails for infrastructure reasons, such as a package mirror
    timing out, is simply re-run.)
12. **Reviews are separate from tests** and unchanged: a kernel or security slice gets its fresh-context review on its
    own feature pull request, before it merges into the epic.

## When an epic's smoke is red

The feature that broke the boot is one of the epic's commits, and none of them was booted. Find it mechanically:

1. Read the failing job's log first: the harness names the failing test and the last line printed.
2. `make epic-bisect` (on the epic branch, clean tree) bisects from the epic's fork point on `origin/main` to `HEAD`,
   one kernel-only boot at four harts per step: about four boots for fifteen commits. `make epic-bisect TARGET=<target>`
   bisects with the target that failed (`os-boot-test-smoke-a`, `os-ktest-release`, `os-count`, ...).
3. The fix is a feature branch on the epic like any other. The epic's pull request into `main` reruns the gate.

A failure that does not reproduce is rule 11's business, not a reason to rerun the gate.

## Budgets

Every job of `ci.yml` has a 7-minute timeout, which is its budget; the jobs run side by side, so the gate's wall time
is that of its slowest job, inside 10 minutes. A job that outgrows its budget fails loudly. Then, in this order:

1. split a suite shard again (the filter is a name rule in the suite and the harness);
2. tag the slowest tests soak-only (`scripts/profile_check.py` prints them);
3. move the logic of QEMU tests to the host (the syscall layer in `ksys`, server protocols over in-memory channels).

Measure before choosing (`TEST-TOOLS.md`, "Measuring"); do not guess. Raising a budget is a decision recorded here.

**The one exception: the tool-build job** (2026-11-26). CI's QEMU is built from its release tarball and cached (`TEST-TOOLS.md`,
"CI's QEMU"), because the distribution's QEMU is too old for the IOMMU. The `qemu` job of `ci.yml` looks the cache entry up and, only
when it is missing, builds it before the boots start. Its timeout is 20 minutes, not 7: a cold build does not fit the gate's budget,
and a build must never land inside a job that boots. It is a lookup of seconds on a warm cache, so the gate's wall time is unchanged
except for the first smoke in a cache scope. The exception is for building a tool into the cache and nothing else: the job runs no
make target, no test and no boot, and every job that boots restores the entry and fails on a miss rather than building it. No rule
about tests or boots changes.

## What enforces this

`scripts/check_docs.py` (part of the commit hook and of tier 0) refuses:

- a feature-gate target that starts QEMU, directly or through anything it depends on;
- a job of `ci.yml` that starts QEMU and is not limited to the epic gate;
- a `make` target in `ci.yml` that is not a smoke target, a job timeout over the budget, or a build matrix;
- a tool-build job (`qemu`) that does more than fill the cache, is not limited to the epic gate, or exceeds its own 20 minutes, and a
  job that uses CI's QEMU without needing it;
- a commit hook that runs a test or a boot;
- `make check` being anything but `smoke`;
- a soak or overnight target that its workflow does not run, or that a lower tier does;
- a schedule, push or pull-request trigger on `soak.yml` or `overnight.yml`.
