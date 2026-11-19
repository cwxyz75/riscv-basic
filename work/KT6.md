---
id: KT6
kind: slice
title: Tier 3: the long-run workflow, started only by the owner
area: testing
horizon: long
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** A third tier: hours, never required to merge, and, like the soak tier, **started only when the owner asks** (OD142). Long-running tests go here and stay out of the gate.

**How.** `Makefile`: `OVERNIGHT_TARGETS` (empty at first; each KT item below adds its target). `.github/workflows/overnight.yml`: `workflow_dispatch` as its **only** trigger (no schedule, no pull-request trigger), one job per target, `timeout-minutes` up to 300, failing logs uploaded as artifacts, a summary step that writes pass or fail per target to the job summary. `scripts/check_docs.py`: the three target lists are disjoint; every overnight target is run by `overnight.yml`; `ci.yml` runs none. `TESTING.md`: the tier table gains the row, and the rule that a red run means "a work item today" (OD140). The docs check also fails if `soak.yml` or `overnight.yml` has a `schedule:` or `pull_request:` trigger, so the rule cannot drift back.

**Acceptance, written first.** The docs check fails when an overnight target is missing from the workflow or appears in the gate (planted, by hand). A first target (`os-repeat` of `os-boot-test-smp`, N=20, from KT2) is wired in; **its first run is owed to the owner** (listed in `WORK-PLAN.md`), not started by the builder. The docs check fails on a planted `schedule:` in either workflow.

**Review.** Default.
