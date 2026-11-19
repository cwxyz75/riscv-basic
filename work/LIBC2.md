---
id: LIBC2
kind: slice
title: A complete libc, phase 2
area: libc
horizon: long
---

- **A complete libc** (noted 2026-11-17, owner). The C standard library as `scripts/libc_gap_survey.py` can measure it, then the POSIX base interfaces (locale and wide characters, `regex`, `iconv`, `*at` calls, `mmap`/`fork` shims), with the host's libc as the oracle wherever behaviour is defined. Phase 1 is item 3 of `WORK-PLAN.md` (plan 2). Done when every fetched program in the survey reports nothing undeclared and the portable corpus passes. It unlocks GNU tar and coreutils (TIDY-UP T145).

Phase 1 is done (PR 229; `LIBC-GAPS.md`, T147).

## Note (2026-11-18, plan 4: the v1 kernel)

Plan 4 (OD141) parks this until the kernel is frozen (FZ1): it is user-space work that plan 3 left open, and it is not ordered now. Nothing in plan 4 depends on it.
