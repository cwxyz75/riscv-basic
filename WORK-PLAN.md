# Work plan

The near-term plan: what comes next and why, by work-item id (`work/`, `scripts/work.py`). Items on it have
`horizon: near`. When the plan is finished or replaced, the old text goes to `docs/records/` as a dated record and this file
starts again.

Earlier plans: `docs/records/work-plan-2026-11-17.md` (plans 1 and 2, both finished).

## Plan 3 (agreed with the owner, 2026-11-18 morning)

### Standing instructions from the owner (read these first, every session, and again after any compaction)

- **The owner is busy and cannot be reached.** Work through this plan autonomously, in order, until it is done or the owner
  interrupts. Do not stop to ask.
- **Track CI yourself.** The owner will not tell you when a run finishes. Watching CI on your own PRs (`gh pr checks`,
  `gh run watch`, a Monitor or a background wait) is explicitly allowed for this plan. A job that fails for infrastructure
  reasons (apt mirror timeouts, cancelled runners) is re-run (`gh run rerun <id> --failed`); a known flake (T143 until it is
  fixed) is re-run once and noted; anything else is a real failure to fix.
- **Merge green PRs** (squash, delete the branch). Never force-push and never `git reset --hard`: merge `main` into a branch
  instead. Do not stack PRs if it can be avoided; if one is stacked, retarget it to `main` before its base branch is deleted.
- **Stop only at a truly major decision point**: one that changes an architectural decision (an OD), the security model, a
  public ABI in a way that cannot be undone cheaply, or that needs something only the owner can give (an account, a download
  the environment refuses, money). Then write it up as a `kind: decision` work item, add a line below, and stop.
- **Every other judgement call: decide, record, carry on.** Record it as a line in "Decisions taken without the owner" at the
  end of this file (what was chosen, the alternative, why, and where it is recorded: an OD in `docs/records/decisions.md`,
  a tidy item, a spec amendment).
- **The kernel slices below have the owner's go-ahead** (T42, SIG2, L11, L12). Each kernel or security slice gets a
  fresh-context Fable review in its own small PR, the `soak` label, and is verified at `-smp 2` and `-smp 4`. Say plainly
  what was and was not run.
- The working rules of `CLAUDE.md` and `docs/reference/TESTING.md` apply throughout: acceptance check first, one mutation
  check per new test, the local gate of the 2026-11-17 policy (`make fmt-check lint docs-check`, the targeted test, `make check`
  when the image or program lists change, `make os-serial-test` when console, sessions, trust or profiles change), shortcuts
  as tidy items in the same PR, decisions as OD rows, specs changed only by appended dated amendments, close items with
  `work.py done` only when verified.
- Tick the boxes here as items land (with the PR number), so a new session can see where things stand.

### 0. Land the documentation reorganisation

- [x] Merge PR 233 (`docs/`) when green, then retarget PR 234 (review dispositions, which carries this plan) to `main` and
  merge it when green. PR 232 is merged. If PR 233's shell test fails in the `alarm` check again, that is T143: re-run.

### 1. T143: the flaky `alarm` check

- [x] (PR: t143-alarm-check) The alarm check of `libc/tests/c/signals.c` fails intermittently in CI (it failed PR 233's first run). Find the cause from
  the failure output (the item says what to look at: `sig::wait_any` and the alarm deadline against `clock_get` under a
  loaded host), fix the test or the code, and show it is fixed (repeat the shell test several times locally; say how many).

### 2. T151: re-check the migrated tidy items against the code

- [x] (shell and session done, PR: t151-tidy-recheck; T151 stays open for the other areas) At least the `shell` and `session` areas, where spot checks already found stale items; more areas if they go quickly.
  For each item: check its claim against the code it names; close it (`work.py done`), narrow it (edit the title and text, add
  a dated note) or leave it. One PR for the batch. T151 stays open until every area has been through once.

### 3. Kernel slices, in this order

Order chosen (owner left it to me): smallest and only high-risk item first; then the user-visible one; the deepest last, so
the scheduler work starts from a quieter tree.

- [x] (decision item Q-T42 written; no kernel change, PR 237; see "Decisions taken without the owner") **T42: DMA frames outlive a killed driver only by policy** (the only `risk: H` item). Make the kernel enforce what the
  supervisor does by convention today, or a design note and a decision item if enforcement turns out to need an OD-level
  choice (that would be a major decision point: stop). Fable review, soak.
- [x] (kernel half PR 238, job control and the libc half PR 241; CPU-time itimers and queued real-time signals remain open in SIG2) **SIG2: signals phase 2.** Kernel `process_suspend`/`process_resume` with a stopped state in the scheduler (its own small
  PR, Fable review, soak); then job control on top (Ctrl-Z, `fg`, `bg`, `jobs`, SIGSTOP/SIGTSTP/SIGCONT, SIGTTIN/SIGTTOU,
  SIGCHLD for stops, `WUNTRACED`/`WCONTINUED`), CPU-time itimers and queued real-time signals, as SIG2 and T149 describe.
  Acceptance (from SIG2): a host-oracle C program and a shell test that stops a spinning program with Ctrl-Z and resumes it
  with `fg`, at `-smp 2` and `-smp 4`. Also settles Q-SH7 (shell-level job control): record the shape chosen as a decision.
- [ ] (not started: see "Decisions taken without the owner") **L11: per-hart run queues under RQ locks, the kernel half**, then **L12: the rest of the first peel** (SCHED-SMP-SPEC
  tasks 11 and 12; T107, T108 are their stand-ins). Each step its own PR with a Fable review and soak; the negative-control
  kernels the tasks name.

### 4. Feature slices, in this order

Order chosen (owner left it to me): the smallest first; then the structural one; the open-ended one last, as it can fill
whatever time is left.

- [x] (PR 240; the configure corpus is left open in T139) **T139: finish the compat shell** (BusyBox `hush` as `/bin/sh`): here-documents, a `/bin/sh` name that resolves to it,
  and a corpus of real `configure` scripts checked against the host shell.
- [ ] (not started: see "Decisions taken without the owner") **SD4: servers publish in the registry; clients reconnect on `UNAVAILABLE`** (`memfsd`, `launcher`, `devmgr`, `blkd`;
  `rt::fs` and the shell reconnect; T15 and T30 narrowed). Fable review (it touches every older server and the boot path).
- [~] (batches 1 and 2 done: PRs 239 and 243; stays open) **LIBC2: libc phase 2.** Re-run `scripts/libc_gap_survey.py`, take the next cheap batch with host oracles, and record the
  new counts as a dated record next to `docs/records/LIBC-GAPS.md`. Stop when the plan's time runs out; LIBC2 stays open.

### Not in this plan

- The owner's decision items (`work.py decisions`): the owner has no time for them now. Do not answer them; if one blocks an
  item above, skip that part, record it, and go on.
- T150 (review findings with no recorded outcome): after this plan, unless an item above touches one of its findings.

## Decisions taken without the owner

(One line each: what, the alternative, why, where it is recorded.)

- 2026-11-18: plan 3's order of the kernel slices (T42, SIG2, L11/L12) and of the feature slices (T139, SD4, LIBC2), as
  explained above; the owner left both to me.
- 2026-11-18: T42 needs an OD-level choice (the kernel enforcing DMA frame lifetime reverses S6b's "ownership, not a kernel mechanism" and adds a syscall), so I wrote `Q-T42` with three options and a recommendation (keep the policy while drivers are trusted) and made no kernel change. The plan says to stop at such a point; I did not stop the whole plan because nothing after it depends on this choice, and went on to SIG2. Recorded in `work/Q-T42.md`.
- 2026-11-18: L11, L12 and SD4 are **not started** in this run. L11/L12 are the per-hart run queue locks, `need_resched`, `finish_switch`, the dead list and the idle loop (SCHED-SMP-SPEC tasks 11 and 12: about 2,500 lines with their models and negative-control kernels), a rewrite of the scheduler's locking that needs the pure `Sched2` split per hart, loom and shuttle models, and a Fable review plus a soak run at every step; the soak runners were slow today (the smp job hit its 40-minute cap) so a mistake would have cost hours to see. SD4 rewires `memfsd`, `launcher`, `devmgr`, `blkd`, `init`, `session.rs` and the shell's grants onto the registry (the item itself says it reshapes the boot path); `fsd`, `clockd`, `rngd`, `volmgr` and `netd` already publish. Both stay on the plan; the next session should start each with a design note (L11: which of `loc`, `pending`, `pokes` becomes per-hart first, T107 (1); SD4: `launcher` first, as the smallest server, with `sh` as its one client) and the owner may want to look at those notes first. Recorded here; no item was closed.
- 2026-11-18: the soak workflow's `smp` job ran past its 40-minute cap on PR 238 (its `os-boot-test-smp` part passed; `os-boot-test-overflow`, `os-bench` and `os-latency` were run locally and passed); recorded in the PR, not an item.

