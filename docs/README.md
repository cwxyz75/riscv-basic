# Documentation (OS track)

Three kinds of document live here. Work lists do not: what is left to do is in `work/` (one file per item,
`work/README.md`), and the near-term plan is `WORK-PLAN.md` at the root. The home computer's documents stay in `home/`.

## specs/: what the system is meant to be

A spec describes the end state. It has no opinion about how far the build has got: what is done or left belongs in
`work/` (the docs check refuses progress words in a spec). A spec is versioned; a change to it is a **dated amendment
appended at the end** ("## Amendment 2026-12-01: ..."), saying what it replaces, so the history of the design stays
readable. The body is not rewritten. (Specs written before 2026-11-18 carry inline "Revision" notes where the
contract was settled differently while it was built; amendments from now on go at the end.)

| Spec | Subject |
|---|---|
| `specs/OS-ARCHITECTURE.md` | The whole OS: object model, memory, IPC, namespaces, drivers, the milestone ladder. Start here. |
| `specs/KERNEL-V1-SPEC.md` | The kernel that is frozen: targets, the system-library contract, memory, harts and power, hardening, how it is tested. Read after OS-ARCHITECTURE. |
| `specs/S4-SPEC.md` | S4: the first interactive system (boot image, launcher, memfs, console, shell). |
| `specs/S5-SPEC.md` | S5: SMP. |
| `specs/S6-SPEC.md` | S6: storage drivers (interrupt objects, virtio-blk, device tree, device manager). |
| `specs/S7-SPEC.md` | S7: the C library, threads on futexes, ported software. |
| `specs/W-SPEC.md` | W: targeted wake and ports as registrations. |
| `specs/T-SPEC.md` | T: kernel memory and cheap threads. |
| `specs/R-SPEC.md` | R: the service runtime. |
| `specs/SD-SPEC.md` | SD: the service directory and namespaces. |
| `specs/SCHED-SMP-SPEC.md` | The scheduler and the SMP execution model (the `L` work items are its tasks). |
| `specs/NET-DESIGN.md` | The network stack's architecture (S8 and S9). |
| `specs/NET-SPEC.md` | S8: the network stack's lower layers. |
| `specs/S9-SPEC.md` | S9: TCP, sockets in the C library, HTTP and DNS. |
| `specs/FS-SPEC.md` | doofs, the disk filesystem, and the wall clock. |
| `specs/USER-VIEW-SPEC.md` | The command-line view: namespace layout, POSIX surface, terminal, sessions, login, `/proc`. |
| `specs/SHELL-SPEC.md` | `ish`, the native shell. |

## reference/: the system as it is

Indices and policies that change in place with the code, in the same PR.

| Document | Subject |
|---|---|
| `reference/SYSCALLS.md` | Every syscall: arguments, preconditions, rights, errors. |
| `reference/INVARIANTS.md` | The invariants every change keeps: handles, rights, lock order, memory ownership. |
| `reference/TESTING.md` | The testing tiers, their budgets and where a new test goes. |

## records/: what happened

Append-only: lines are added, never changed or removed (the docs check compares against `origin/main`). A record is
dated; a later record may correct an earlier one, which stays as written.

| Record | What |
|---|---|
| `records/decisions.md` | The OS decision log (OD numbers). |
| `records/done.md` | Every closed work item, one line each (`scripts/work.py done` appends). |
| `records/reviews/` | Independent reviews and their findings (`records/reviews/README.md`). |
| `records/notes/` | Notes left by unattended runs. |
| `records/roadmap-2026-11-18.md` | The old roadmap, frozen when the work queue replaced it: how each slice was built. |
| `records/work-plan-2026-11-17.md` | The work plans of 2026-11-17. |
| `records/UV0-GAP-SURVEY.md`, `records/LIBC-GAPS.md` | The libc gap measurements of 2026-11-15 and 2026-11-17. |
| `records/S9-STEP1-RESULT.md` | The S9 data-path prototype's result. |
| `records/S8-FOLLOWUP-SPEC.md`, `records/S9-STEP0-SPEC.md` | Briefs for single slices, as given. |
| `records/SHELL-CORPUS.md` | The design probe that shaped the shell's syntax. |
| `records/review-dispositions-2026-11-18.md` | What became of every review finding (fixed, tracked, or not recorded: T150). |
