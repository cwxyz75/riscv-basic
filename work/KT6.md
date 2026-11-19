---
id: KT6
kind: slice
title: Tier 3: the overnight workflow
area: testing
horizon: long
blocked_by: [KT1]
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** A third tier: hours, nightly on `main`, never required to merge. Long-running tests go here and stay out of the gate and out of the on-demand soak.

**How.** `Makefile`: `OVERNIGHT_TARGETS` (empty at first; each KT item below adds its target). `.github/workflows/overnight.yml`: cron nightly after the soak run, `workflow_dispatch`, one job per target, `timeout-minutes` up to 300, failing logs uploaded as artifacts, a summary step that writes pass or fail per target to the job summary. `scripts/check_docs.py`: the three target lists are disjoint; every overnight target is run by `overnight.yml`; `ci.yml` runs none. `TESTING.md`: the tier table gains the row, and a rule: a red overnight run means "a work item today", per OD140. A scheduled-run failure is noticed by the session that next opens the repository: `WORK-PLAN.md`'s standing instructions say to look at the last overnight run first.

**Acceptance, written first.** The docs check fails when an overnight target is missing from the workflow or appears in the gate (planted, by hand). A first target (`os-repeat` of `os-boot-test-smp`, N=20, from KT2) runs green from the workflow.

**Review.** Default.
