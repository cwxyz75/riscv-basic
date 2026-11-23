# Reviews

Independent reviews of the design and code, kept so their findings and reasoning are not lost with the
session that asked for them. Each is the report of a reviewer (a Fable-class model with no part in
writing the code); they are advice, not decisions. Decisions go in `OS-ARCHITECTURE.md` section 17,
debts in `TIDY-UP.md`, plans in `ROADMAP.md`.

| File | What | Date |
|---|---|---|
| `2026-11-14-design-review.md` | Design review of the whole OS (findings D1-D18) against the directions in `ROADMAP.md`, ahead of S8 | 2026-11-14 |
| `2026-11-14-wake-path-design.md` | Recommended design for finding D1: targeted wake, ports as registrations, exclusive wakes, retiring the global wait queue. Basis of the roadmap slice "W" | 2026-11-14 |
| `2026-11-14-slice-i-aia.md` | Fable review of slice I (AIA interrupts) | 2026-11-14 |
| `2026-11-14-security-review.md` | Fable security pass over the syscall surface (4 confirmed bugs, 6 suspicions; fixes in the sec-fixes PR) | 2026-11-14 |
| `2026-11-14-smp-review.md` | Fable SMP and locking review (no memory-safety bug; scheduling-order and latent findings, T62-T66) | 2026-11-14 |
| `2026-11-14-task8-priority-authority.md` | Fable review of scheduler task 8, priority authority (PR 132) | 2026-11-14 |
| `2026-11-14-wake-path-design.md` | (listed above) | 2026-11-14 |
| `2026-11-15-filesystem-survey.md` | Fable survey of disk formats for `fsd`; the basis of FS-SPEC | 2026-11-15 |
| `2026-11-15-growable-heap.md` | Fable review of the growable kernel heap (slice T step 2, PR 133) | 2026-11-15 |
| `2026-11-15-s8-network.md` | Fable review of S8 steps 3 to 5, the network driver and stack (PR 139) | 2026-11-15 |
| `2026-11-15-s9-step1.md` | Fable review of S9 step 1, the stream data-path prototype (PR 144) | 2026-11-15 |
| `2026-11-15-s9-step4b.md` | Fable review of S9 step 4b, TCP streams in `netd` (PR 150) | 2026-11-15 |
| `2026-11-15-s9-step5.md` | Fable review of S9 step 5, the random service (PR 152) | 2026-11-15 |
| `2026-11-15-task10-klock-park.md` | Fable review of scheduler task 10, `klock` and the park protocol (PR 136) | 2026-11-15 |
| `2026-11-15-thread-spawn.md` | Fable review of scheduler task 9, `thread_spawn` (PR 134) | 2026-11-15 |
| `2026-11-16-doofs-slice.md` | Fable review of the doofs slice (FS steps 1 to 6; fixes in PR 177) | 2026-11-16 |
| `2026-11-16-l11b-scheduler.md` | Fable review of L11b-1, per-hart run queues (PR 167) | 2026-11-16 |
| `2026-11-16-l12-step1.md` | Fable review of L12 step one, the scheduler behind its own lock (PR 168) | 2026-11-16 |
| `2026-11-23-kt14-unsafe-audit.md` | KT14: Fable audit of every `unsafe` in the kernel binary (162 blocks; one isolation bug fixed, T239 to T242); the rows are `kt14-unsafe-audit.tsv` | 2026-11-23 |
| `../review-dispositions-2026-11-18.md` | What became of every finding above: fixed (where), tracked (which work item), or not recorded | 2026-11-18 |
