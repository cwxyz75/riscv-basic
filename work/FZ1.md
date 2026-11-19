---
id: FZ1
kind: slice
title: The freeze: the last review, seven green nights, the declaration
area: kernel
horizon: long
blocked_by: [AB9, KT14, KT7, KT8, KT9, KT10, KT12, KT13, L20, L25, MM9, EX2, PM3, PM4, HD3, HD4, HD5, PL8, SC2]
refs: [KERNEL-V1-SPEC, OD126]
---

**How.** KERNEL-V1-SPEC section 9. (1) Rewrite `docs/reference/INVARIANTS.md` as the invariants in force (the "planned" tables fold into the main ones; retired rules go) and check `SYSCALLS.md` against `abi-conformance`. (2) A fresh-context Fable review of the whole kernel against `KERNEL-V1-SPEC.md`, `INVARIANTS.md` and `SYSCALLS.md`, recorded in `docs/records/reviews/`; every finding is fixed or becomes a work item with a risk. (3) No open item of risk H in areas `kernel`, `sched`, `security`. (4) Tier 3 green on seven consecutive nights on the commit range that ends at the freeze (a failing night restarts the count after the fix). (5) Record the benchmark and scaling numbers of that commit in `TESTING.md` as the v1 baseline. (6) Append the decision "the kernel is v1 and frozen" to `docs/records/decisions.md` with the commit, and a release note in `docs/records/notes/`. After it, `WORK-PLAN.md` starts again with the follow-on work.

**Acceptance.** The six conditions, each with its evidence linked in the decision row.

**Review.** This item is the review. **Stop here and report to the owner** before starting the follow-on work.
