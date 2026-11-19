# Work plan

The plan: what comes next and why, by work-item id (`work/`, `scripts/work.py`). The items being worked now have
`horizon: near` (at most 15); as a phase finishes, the next phase's items are moved to `near`. When the plan is finished or
replaced, the old text goes to `docs/records/` as a dated record and this file starts again.

Earlier plans: `docs/records/work-plan-2026-11-17.md` (plans 1 and 2, finished) and `docs/records/work-plan-2026-11-18.md`
(plan 3, replaced by this one).

## Plan 4: the v1 kernel (agreed with the owner, 2026-11-18; OD126 to OD143)

**Goal.** Finish the kernel so it can be left alone: `docs/specs/KERNEL-V1-SPEC.md` is the end state, its section 9 says what
"frozen" means, and item FZ1 is the last step. The plan covers the kernel plus what must change in `rt`, the C library and
the servers to keep the system green. It is long (twelve phases, about seventy items); every phase leaves a better, bootable
system, so stopping between phases loses nothing.

**Start.** The plan starts when the owner says so. From then on every item in it, the lock-peel tasks included, has the
owner's go-ahead: the sentences in older `L` items about waiting for one are superseded (each carries a note).

### Standing instructions from the owner (read these first, every session, and again after any compaction)

- **The owner is not to be asked.** Every question that came up in the review of 2026-11-18 was settled then and is recorded
  (OD126 to OD143, the spec, the items). Work through the plan in order until it is done or the owner interrupts.
- **Each item says what to build, how, its acceptance check and its review.** Write the acceptance check first and watch it
  fail. Where an item leaves a detail open, decide, record it under "Decisions taken without the owner" below, and carry on.
- **Stop only at a truly major decision point**: something that contradicts a decision OD126 to OD143 or the spec's section
  1, that changes the security model, or that needs something only the owner can give (an account, a download the
  environment refuses, money). Then write a `kind: decision` item, add a line below, and go on with whatever does not depend
  on it. **FZ1 ends with a report to the owner**; the follow-on work does not start without one.
- **Never start a soak run or a long run** (OD142). Not `make soak`, not one of its targets, not `soak.yml` or
  `overnight.yml`, not a schedule, not a label, however kernel-heavy the change. Only the owner starts them. Where an item
  says "soak" or names an overnight target, that means: wire the check in, say in the PR that the run is owed, and add a
  line to "Runs owed to the owner" below. The item still merges on the green gate and is closed on its acceptance check.
- **First thing each session:** if the owner has started a soak or long run since the last session (`gh run list --workflow
  soak.yml --limit 1`, and `overnight.yml` once KT6 exists), read its result. A red one is a work item today (OD140), ahead
  of the next plan item; a green one clears the matching lines below.
- **An intermittent kernel failure is a defect** (`docs/reference/TESTING.md` rule 8): never "re-run once and note". A job
  that fails for infrastructure reasons (a package mirror, a cancelled runner) is re-run.
- **Reviews.** An item marked Fable gets a fresh-context Fable review in its own small PR before it merges; "default" gets
  the default reviewer. Findings are fixed in the PR or become tidy items in it.
- **Testing.** While iterating: `make kquick` once KT1 exists (before it: `make quick` and the one targeted test). The gate
  is `make smoke`; merge on a green gate. Verification at one, two and four harts comes from the gate's kernel-only boots
  (KT1, the first item, for that reason); before KT1 lands, and for wider hart counts, the run is owed, not started. One
  mutation check per new test. Say plainly what was and was not run.
- **Architecture code goes behind the boundary** once AR1 has landed: in `kernel/src/arch/riscv64/`, with its interface item
  added to `arch/mod.rs` and the AArch64 stub in the same PR (KERNEL-V1-SPEC 2.6).
- **Track CI yourself; merge green PRs** (squash, delete the branch). Never force-push and never `git reset --hard`; merge
  `main` into a branch instead. Do not stack PRs.
- **One item (or one named part of an item) per PR.** `main` always builds and boots. A flag-day item (AB1, AB2, AB3) is one
  PR by nature; everything else is cut as small as its acceptance check allows.
- The rules of `CLAUDE.md` apply throughout: shortcuts as tidy items in the same PR, decisions as OD rows, specs changed
  only by dated amendments, reference documents changed with the code, items closed with `work.py done` only when verified
  in QEMU.
- **Keep this file current:** tick the boxes with the PR number as items land, and when a phase is done move the next
  phase's items to `horizon: near` (and finished phases' leftovers to `long`).

### Phase 1: make kernel work cheap to test

Why first: the owner's complaint is that editing the kernel is slow on testing, and every later phase is a kernel edit.

- [x] **KT1** the kernel-only boot (`make kquick`, `os-ktest` at 1, 2, 4 harts and the release kernel, in the gate) (PR 252)
- [x] **MM1** single-page flush after a fault, fault-around (small, and a real cost on every fault) (PR 253)
- [x] **KT3** panic lints, overflow checks, `unsafe` documentation as errors (by file, `kcore` first) (PRs 254, 255)
- [x] **KT4** the C5 atomics gate and the syscall-error-to-test table (PR 256)
- [x] **KT2** record and replay capture, the repeat target (PR 257)
- [x] **T157** (PR 258), **T155** (PR 259), **T158** (PR 261), and T162 (PR 260, a real scheduler defect the model found meanwhile): root-cause the three intermittent failures with KT2's tools (T157 first: it may
  be a scheduler race, and L11c builds on that code)

### Phase 2: finish the first peel; time without a tick

Phase 1 is complete (2026-11-19). L11c part 1 (the dead-list reap) is PR 262.

- [x] **L11** (L11c: `need_resched`, `finish_switch`, the dead list; L11d: the idle loop, the watchdog, the negative controls) (PRs 262, 263, 264, and T162 in 260)
- [x] **L12** the scheduler leaves the big kernel lock (PRs 266, 267, 268; pure sleeps still take it)
- [x] **AB2** nanosecond time (flag day) (PR 270; leftovers T168)
- [x] **L13** no tick: timer queues, `stimecmp`, tickless idle, slack (closes **L28** with it) (PRs 272, 273; leftovers T169, T170)
- [x] **PM2** long idles suspend through firmware (PR 275)

### Phase 3: the syscall layer on the host; the long-run tier

Why here: the scheduler glue is stable after phase 2, and everything after this phase changes the syscall layer, so it should
be host-testable, fuzzable and covered before that starts.

- [ ] **TS3** (a, b, c) `kernel/src/user.rs` becomes the host-testable crate `ksys`
- [x] **KT6** (PR 281) tier 3, the long-run workflow (manual start only)
- [ ] **KT7** schedule perturbation
- [ ] **KT10** automated mutation testing
- [ ] **KT12** the checked release build
- [x] **KT13** (PR 283) the kernel stack budget
- [ ] **KT5** instruction-count checks
- [ ] **KT8** syscall fuzzing (host and guest)
- [ ] **KT9** the allocation-failure sweep
- [ ] **KT11** coverage report

### Phase 4: the architecture boundary, then the platform as it really is

AR1 goes first so that everything after it is written on the right side of the boundary.

- [ ] **AR1** (a, b, c, d) all RISC-V code behind `kernel/src/arch/`, page tables generic over a format, the AArch64 stub
- [ ] **PL1** RVA23 in the gate, the boot-time extension check
- [ ] **MM2** the buddy frame allocator
- [ ] **MM3** large pages in the direct map
- [ ] **PL4** discovery: memory, load address, device tree pointer, timer frequency
- [ ] **PL5** discovery: console, interrupt controller, device windows, reset
- [ ] **PL6** AIA only; inter-hart interrupts through the IMSIC
- [ ] **SC1** sixty-four harts
- [ ] **HD1** the kernel entropy pool
- [ ] **PL2** vector state
- [ ] **PL3** user code built for RVA23U64

### Phase 5: the ABI flag days

Each rebuilds all of user space; they are grouped so the churn happens once.

- [ ] **AB1** 64-bit handles
- [ ] **AB3** 64-bit identifiers, no counter wraps, the fast-forward tests
- [ ] **AB5** remove `thread_create`
- [ ] **AB4** `port_wait_many`

### Phase 6: kernel memory and the remaining lock peels

- [ ] **MM4** the slab allocator
- [ ] **L15** mappings hold a VMO reference
- [ ] **L16** `JOBS`, per-family `VMO`, `FRAME`; page reservations
- [ ] **MM5** all kernel memory charged to jobs
- [ ] **L17** per-process address-space locks, with the fault path shaped for pagers
- [ ] **L18** the ASID allocator with generations; batched shootdown
- [ ] **PL7** instruction-cache flush on executable mappings
- [ ] **L14** futex bucket locks
- [ ] **L19** `PROCS` and `TT`; teardown out of the big lock
- [ ] **MM10** the sweep in slices
- [ ] **L21** the W remainder (exact ready set, wake-one)
- [ ] **L20** the end of the big kernel lock; the `&'static mut` accessors deleted
- [ ] **PL8** limits derived from RAM; per-process ceilings as quotas

The order of L14 to L19 is the default of SCHED-SMP-SPEC 7.2. If the lock profile after L12 says another class of syscalls
holds the big lock longest, reorder on that evidence and record it below.

### Phase 7: the memory model programs need

- [ ] **MM6** VMOs by size with no cap, `vmo_set_size`, `vmo_decommit`
- [ ] **MM7** range unmap, `process_protect`, ANYWHERE, `process_map_info`
- [ ] **MM8** copy-on-write children, `process_clone_region`

### Phase 8: threads, exceptions, pagers

- [ ] **EX1** thread suspend, resume and state
- [ ] **EX2** exception channels
- [ ] **MM9** pager-backed VMOs, blocking faults

### Phase 9: the scheduler's remaining tasks

- [ ] **L1** the remaining benchmarks (`pair_scaling`, `rt_latency`) and absolute targets
- [ ] **L8** RT budget accounting and the priority policy of the launcher and the device manager
- [ ] **L22** donation through `channel_call`
- [ ] **L23** PI futex
- [ ] **L24** direct hand-off
- [ ] **L25** RT budgets enforced
- [ ] **AB6** CPU time for threads, processes and jobs

### Phase 10: scale and power

- [ ] **SC2** placement by masks; scaling at 16 and 32 harts; the `OBJ` trigger evaluated
- [ ] **PM1** harts offline and online
- [ ] **PM4** the placement policy (`spread`, `pack`)
- [ ] **PM3** `system_suspend`

### Phase 11: hardening

- [ ] **HD2** position-independent programs, address randomisation
- [ ] **HD3** kernel address randomisation
- [ ] **HD4** the debug resource; a release kernel with no test code
- [ ] **HD5** reserved ABI space (and the X-SPECULATION record)

### Phase 12: the boundary and the freeze

- [ ] **AB7** the system library
- [ ] **AB8** entry enforcement, shuffled numbers
- [ ] **AB9** the conformance program, `SYSCALLS.md` rewritten, the versioning rule
- [ ] **KT14** the audit of every `unsafe` block
- [ ] **FZ1** the freeze: the last review, three green soak and long runs **that the owner starts**, the declaration, **and a
  report to the owner**

### Not in this plan (after the freeze, unordered)

- User-space consequences of the new kernel: **DL1** (the dynamic loader and a dynamically linked C library), **S11-FORK**,
  **S11-MMAP**, **SIG3** (asynchronous signals over exception channels), **UV10** (the Rust `std`), **L27** (`rt::looper`).
- Plan 3's user-space leftovers: **SD4**, **LIBC2**, and the open parts of **SIG2**, **T139**, **T151**, **T152**.
- Recorded and not designed: heterogeneous harts and frequency scaling (X-POWER), speculation (X-SPECULATION), shadow stacks
  and pointer masking beyond their reserved places (X-HARDENING), a physical board, an IOMMU (T42), Sv48, large pages for
  user mappings, splitting the object lock (T70, unless SC2's measurement triggers it), a fair scheduling class (T72).
- The owner's other decision items (`work.py decisions`): none blocks this plan.

## Runs owed to the owner

Soak and long runs that an item wanted and only the owner may start (OD142). One line each: the item, the PR, which target
or tier, and what a red result would mean. The owner starts a run when they choose (`gh workflow run soak.yml`, or Actions >
soak > Run workflow); one run of the tier clears every line above it.

- **T157** (PR 258): the round-robin kernel boot (`os-boot-test-rr`, soak) with the stall-based timeout and the widened clock window passed 3 of 3 under full CPU load on the development machine; one soak run confirms it on CI's runners. A red `os-boot-test-rr` would mean a stall (the log names the last line) or a different cause.
- **L11c part 1** (PR 262): the dead-list reap in `finish_switch` wants the soak run of the smp targets at `-smp 1`, `2` and `4` (and the round-robin and polling kernels). A red one would mean a leaked or double-freed kernel stack.
- **L11c part 2** (PR to follow): the deferred switch wants the same soak run as part 1 (smp targets at `-smp 1`, `2`, `4`, round-robin and polling kernels): a red one would mean a lost switch or a wrong thread blocked.
- **L11d** (PR to follow): `make os-boot-test-sched-neg` (the three scheduler negative controls) is in the soak tier and was run by hand once per control only. A red one means a control no longer detects its fault.
- **AB2, L12b, L12c** (PRs 267, 268, 270): the soak tier at `-smp 1`, `2`, `4` also covers the quiet-trap paths (kill of a spinner) and the real-time deadlines of the suite (about 2.6 times tighter under load than when the clock was a tick count): a red one is a flaky bound or a missed kill.
- **L13** (PRs 272, 273): the soak tier shows the real-timer margins (the latency bounds were widened for the emulator's interrupt jitter) and the starvation watchdog under a loaded host; a red `starved thread` is a lost wake or pull, not noise.
- **KT6** (PR 281): the first overnight run, `gh workflow run overnight.yml` (20 consecutive boots of the smp suite, up to 300 minutes). A red one is a work item the same day (OD140): the log of the failing boot is uploaded as an artifact.
- **TS3a** (PR 276): `make os-miri` now includes `cargo miri test -p ksys` (the host tests of the syscall layer, over the real address-space code). A red one would be undefined behaviour or a leak in code the kernel runs, found on the host.

## Decisions taken without the owner

(One line each: what, the alternative, why, and where it is recorded.)

- 2026-11-18, while writing the plan (all inside the owner's answers of that day, each recorded in the spec or an item):
  - The order of the twelve phases: testing first (the owner's stated pain), then the first peel, then the host syscall
    layer before anything else touches it; the ABI flag days grouped; the system library last so it is cut once from the
    finished call set. Alternative: the ABI first. Recorded here.
  - Two kinds of copy-on-write child, with SNAPSHOT restricted to one job so a page never changes payer; the alternative
    (one kind with charges that move between jobs) breaks J1. KERNEL-V1-SPEC 4.3, MM8.
  - Pager requests and exceptions are delivered on kernel-written channels, not as port packets with payloads: it reuses
    the bootstrap channel's mechanism and needs no new wait path. KERNEL-V1-SPEC 3.6, 4.4.
  - Dirty-page tracking for pagers is reserved, not built: the owner's "the filesystem side comes later". MM9.
  - The reachability sweep becomes incremental by slices with an epoch, on the calling thread (SCHED-SMP-SPEC left three
    options open). KERNEL-V1-SPEC 4.6, MM10.
  - The syscall layer moves to a new crate `ksys`; the name and the three-PR cut are mine. TS3.
  - Syscall numbers are shuffled only in a long-run build, not every build, so logs stay readable. AB8.
  - The architecture boundary is a static facade (`arch::*` re-exports), not a trait object, and its completeness is
    checked by compiling an AArch64 stub; unhandled-fault exit codes change from the RISC-V cause to the neutral
    exception kind. KERNEL-V1-SPEC 2.6, AR1.
  - Kernel address randomisation is built although Sv39 leaves it about 9 and 7 bits; the owner chose the option that
    includes it, and the entropy is stated. HD3.
  - Thresholds: 5% for instruction counts, 8 KiB for the stack budget, 200 perturbation seeds, 1 ms for the idle-suspend
    threshold, 64 pages per reservation, 15 pages of fault-around. Each is one constant, in its item.
