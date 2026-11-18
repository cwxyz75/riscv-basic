# Review findings: what became of them (2026-11-18)

A record, append-only. For every review in `docs/records/reviews/`, where each finding went: **fixed** (in the PR named,
according to that PR's commit message), **tracked** (a work item in `work/` holds it), **design input** (the review was a
recommendation that a spec absorbed), or **not recorded** (no commit message, work item or record says what happened).
"Fixed" here is what the merge commits say; it was not re-checked against the code for this record. The not-recorded
findings are collected in work item T150, to be checked against the code and then closed, fixed or turned into items.

Built from the merge commits of the reviewed PRs, the old roadmap (`roadmap-2026-11-18.md`), the old Resolved table
(`done.md`, R-rows) and the work items as of 2026-11-18.

| Review | Fixed | Tracked | Design input / other | Not recorded |
|---|---|---|---|---|
| `2026-11-14-design-review.md` (D1-D18) | D1 (slice W), D3 (slice V), D5 (slice I), D6 (slice DM), D7, D8, D11, D12 (slice R), D9 (slice T), D10 (slice SD), D13 (slice A), D14 (T55, PR 94), D4 (S9) | D1 remainder and D17 (T60, T61; L21), D2 (SCHED-SMP-SPEC, `L` items), D9 remainder (T1, T13), D10 remainder (SD4), D11 remainder (T30, R4) | D15 (entropy: OD108; wall clock: Q-OS11) | D16 (NO_MEMORY overloaded, no cancel), D18 (VMO child, resize, page source; lifetime by scanning) |
| `2026-11-14-wake-path-design.md` | | L21 (what W left) | The basis of W-SPEC and OD80 | |
| `2026-11-14-security-review.md` (bugs 1-4, suspicions 5-10) | 1, 2, 4 (PR 108) | 3 (T60) | | suspicions 5 (object ids wrap), 6 (infallible kernel allocations on user paths), 7 (long BKL holds at teardown; partly T11, T76), 8 (`debug_stat` is ambient; partly T2), 9 (memfsd fairness; partly T30), 10 (frame overcommit) |
| `2026-11-14-smp-review.md` (1-6) | 1 (R21), 2 (R22), 3 (R26), 4 (T65, scheduler task 4) | 5 (T66), 6 (T40) | | |
| `2026-11-14-slice-i-aia.md` (1-6) | 1 to 5 (PR 106) | 6 (T59) | | |
| `2026-11-14-task8-priority-authority.md` (1-6) | 1, 2, 3 (PR 132) | the launcher policy and unrestricted jobs (T80) | | 4 (`thread_set_sched` on a fresh handle answers BAD_STATE), 5 (`max_prio` 0 against PR1), 6 (error order; the root's budget) |
| `2026-11-15-filesystem-survey.md` | | | The basis of FS-SPEC | |
| `2026-11-15-growable-heap.md` (1-10) | 1, 2, 3, 4, 7 (PR 133) | 3 remainder (T81), 8 (T82) | | 5 (lock-order checker coverage of `HEAPS`), 6 (idle harts take the BKL on every wakeup; partly L12), 9 (panic while `HEAPS` is held), 10 (first-fit latency) |
| `2026-11-15-s8-network.md` (1-15) | 1 to 4, 8, 9, 10 and parts of 12, 13 (PR 139) | 5, 6, 7, 11, the rest of 12 and 13, 14, 15 (T88) | | |
| `2026-11-15-s9-step1.md` (1-15) | 1 (P8), and the P1, P2, P5, P7 corrections (PR 144); 14 (T91, R28) | | | which of 2 to 13 and 15 the "tighter P1/P2/P5/P7, corrected result" covers is not itemised |
| `2026-11-15-s9-step4b.md` (1-13) | 1 to 4, 7 (PR 150) | 5, 6, 8 to 13 (T98) | | |
| `2026-11-15-s9-step5.md` (1-13) | 3, 4, 6, 8, 9 and the warnings of 1 and 7 (PR 152) | 1, 2, 5, 7, 10, 12, 13 (T99 item 7) | | 11 (spec and bookkeeping out of step: partly the S9-SPEC revision note) |
| `2026-11-15-task10-klock-park.md` (1-8) | 1, 2, 3, 4, 7, and the PK1 part of 5 (PR 136) | 8 and the stand-ins (T85) | | 5 (the controls accept any panic), 6 (`Held` looser than the table: partly T85 item 5) |
| `2026-11-15-thread-spawn.md` (1-9) | 1, 2, 4, 5, and the caller-stack part of 7 (PR 134) | 3, 6, 9 (T83) | | 7 remainder (error order against SYSCALLS), 8 (`find_gap` is linear) |
| `2026-11-16-doofs-slice.md` (1-15+) | 1 to 7, 9, 12 (PR 177) | the rest (T117; 15 partly T109, T115) | | |
| `2026-11-16-l11b-scheduler.md` (1-13) | the findings PR 167's message lists (class-aware pull and push, one mark per thread, ...) | the rest (T107) | | which of 5 to 13 were fixed is not itemised in one place |
| `2026-11-16-l12-step1.md` (1-9) | 1, 2 and the double-dispatch and stale-transit asserts (PR 168) | 6 (T108) | | 3 (models and suite do not cover the change), 4, 5, 7, 8, 9 |
| The independent review of 2026-11-17 night (no file; spawn, signal, terminal, attribute work) | its fixes (PR 229) | the rest (T148; policy item 5 is Q-SIGINT) | | |
