# The stability epic: an addendum for its manager and engineers

Read this **in addition to** `docs/prompts/manager.md` or `docs/prompts/engineer.md`, after them. Where it says so, it overrides
them for the stability epic (`epic/stability`, umbrella `work/STAB0.md`) and for nothing else. Everything else in `CLAUDE.md`,
`docs/reference/TESTING.md` and those prompts stands: feature PRs into the epic, no routine local tests, CI is the check, the soak
and long runs only when the owner asks (OD142), `docs/specs/` off limits, no shortcuts or stand-ins.

## Models (overrides the engineer prompt's reviewer rule)

- **Opus** does all the engineering, the experiments and the reviews of this epic, including the review of an H-complexity or
  kernel or security change, which `engineer.md` would give to Fable.
- **Escalate to Fable** only when an Opus agent has honestly tried and cannot find a solution (a cause it cannot name, a fix it
  cannot make correct, a review question it cannot settle). The report says so: what was tried, how long, which runs and seeds,
  and what the Fable agent was asked. A Fable agent is a second opinion on the stuck point, not a reviewer of the whole PR.
- The manager starts one Opus engineer per ticket, as `manager.md` says.

## Two hours per run

No single test run exceeds **two hours** of wall clock: a boot, an `os-repeat` loop, a chaos seed range, a model-checker run, a
mutation shard, a Miri run. A longer experiment is split into pieces that each fit (seed ranges, `N` per loop, `SHARD=k/n`), and
the report gives each piece's length. Set `timeout` on anything that could run away. The owner's runs (soak, long runs, chaos,
mutants) are requested at the points the umbrella names, never started by an agent, and are dispatched as two-hour pieces too.

## The focus: reproduce, bisect, name, fix, prove

This epic is testing and debugging, not feature work. For every failure:

1. **Read the evidence first.** The failing log (`gh run view <id> --log`, `target/fail-logs/`, `target/repeat/`), the item's
   history, `git log` for the paths it names. Check whether the item is already overtaken by a later change.
2. **Reproduce it.** Make it fail on purpose: the failing seed, a record-and-replay capture, the host-stall injector, a throttled
   or failing disk, a planted delay, chaos points, a forced placement, or a host model that reaches the state directly. A bounded
   `os-repeat` over a smoke target is permitted (OD160) inside the two-hour cap. Five failed attempts at the same test, target and
   hart count make a failure intermittent (TESTING.md rule 11); keep going with a better reproduction rather than more of the same.
3. **Bisect** where there is a range: `make epic-bisect TARGET=<target>` on the epic, `git bisect` with one targeted boot per step
   elsewhere, a CI dispatch per commit when only the runner shows it (T223's lesson: a laptop and a runner differ).
4. **Name the cause**: the line, the interleaving or the missing step, and why the symptom follows from it. "Load", "a slow
   runner" or "a flake of the fixed races" is not a cause until a run shows it (the host-stall injector exists to show it).
5. **Fix it with proof.** The fix comes with a regression test or a negative control that fails without the fix, and a
   **mutation check**: revert the fix (or plant the bug) once by hand and watch the test fail, then restore it; say so in the PR.
   A test that cannot fail is not evidence (T193, T164). The fix goes where `TESTING.md` rule 8 says: logic on the host, syscall
   behaviour in the suite, only what the machine shows in QEMU; nothing grows tier 1.
6. **Strike the part** in its item with a dated note naming the cause, the PR and the test. A count of green runs never strikes a
   part (OD160).

## The tools, and when to reach for each

| Tool | Reach for it when | How |
|---|---|---|
| The failure's own report | Always first | `##PANIC ... [stack ...]` with `scripts/resolve_trace.py`; the `leak:` report (`OS_SHOW=leak:`); `iofaults:` lines; the per-hart deltas of the SMP tests |
| `os-repeat` over a smoke target | To count a failure rate or catch a rare one | `make os-repeat TARGET=<smoke target> N=<n>`, N chosen to fit two hours; never over a soak target (that is a soak run) |
| Record and replay | A failure in the kernel-only shard that appears at all under record mode | `OS_SHARD=k OS_RECORD=<dir>`, then `OS_REPLAY`, `OS_GDB=1`; it runs all harts in one TCG thread, so a failure there is not a host pause of one hart |
| Host-stall injector (T284) | Anything blamed on host load | `OS_HOST_STALL=<ms>:<period>:<seed>`: if the injector reproduces it, the cause is a pause and the test must tell it apart; if not, look in the kernel |
| Seeded chaos | Races in lock, IPI, switch and return paths | The `chaos` kernel, `OS_CHAOS_POINTS=<mask>`, a few seeds by hand inside two hours; the long seed sweeps are the owner's |
| Deterministic devices | Storage timing | QEMU throttling (`throttling.iops-total`), `blkdebug` rules, `readonly=on` |
| `-icount` | A bound that only makes sense in guest time | `OS_ICOUNT` at one hart, no disks under `sleep=off` |
| loom, shuttle | A protocol of a few threads over atomics or locks (futex, park, semaphore, the lock) | Beside the existing models in `kcore/tests` and `klock/tests`; keep the seed and schedule of a failure (`shuttle::replay`) |
| Miri | Unsafe code in the pure crates, aliasing | A targeted `cargo +nightly miri test -p <crate> --test <one>`; `make os-miri` is the soak's |
| Static analysis | Before reading a large path by eye | Extra clippy groups and lints as an experiment (`pedantic`, `nursery`, `indexing_slicing`, `arithmetic_side_effects`, `undocumented_unsafe_blocks`); `-Z emit-stack-sizes` for stack depth; the lock-order table and checker (`klock`); the release-checked kernel for dropped assertions |
| The host fuzzer and the allocation-failure sweep | A syscall path that may leak or panic under odd sequences or memory pressure | `KFUZZ_SEEDS`, `KFUZZ_CALLS`, `KALLOC_SCENARIO`; bound the seeds to fit two hours |
| Kani, TLA+/TLC, cargo-careful, ThreadSanitizer | Bounded proofs of pure functions; protocols across harts (kill and teardown, peek-copy-commit, wait/wake) | **Only after the owner records an OD for the tool** (the umbrella lists them); until then, the loom or shuttle model is the fallback |
| A targeted review | A path where the race is in the reasoning | A fresh Opus agent given only the paths and the question ("every interleaving of a kill, a close and a second hart between this check and that use"); its answer is a lead to test, not a cause |

## A hunt that did not find the cause

If the bounded runs end without a named cause, do not strike the part and do not write "probably load". Write in the item, under a
dated heading: what was tried (each reproduction method, the build and the hart count), **how many attempts** of each and how long
each took, the seeds, the captures and where their logs are (a CI run id or a kept file; `target/` is not kept), what was ruled
out and by what, the diagnostics now in place so the next occurrence names its cause, and the next step you would take. Say in the
report that the part stays open, and whether a Fable escalation was made and what it said. The PR with the diagnostics merges; the
part stays open (OD160).

## The report (in addition to engineer.md's)

For each part: the cause named (or "not found", with the record above), the reproduction that showed it, the regression test or
control, the mutation check and its result, every run with its length (none over two hours), and the model used for the work and
for the review (Opus, or the Fable escalation and why).
