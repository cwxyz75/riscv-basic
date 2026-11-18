#!/usr/bin/env python3
"""Contract-document checks (T55): nobody reads the code, so the documents are the contract, and a
document that says something false is a bug. This cannot prove the documents right; it keeps the known-stale
statements from coming back and checks the decision log's bookkeeping.

Usage: check_docs.py   (run from anywhere; exits 1 with a list of problems)
"""
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")


def read(name):
    with open(os.path.join(ROOT, name), encoding="utf-8") as f:
        return f.read()


# Phrases that were true once and are false now: (file, phrase, why)
STALE = [
    ("OS-ARCHITECTURE.md", "Nothing here is implemented", "S0-S7 are built"),
    ("OS-ARCHITECTURE.md", "# OS Architecture (proposal)", "the title should not call a built system a proposal"),
    ("SYSCALLS.md", "The kernel takes no lock", "syscalls run under the big kernel lock (OD60)"),
    ("SYSCALLS.md", "which do not exist yet", "per-job quotas exist (S4.5)"),
    ("S5-SPEC.md", "Nothing here is built yet", "S5 is built"),
    ("INVARIANTS.md", "(single hart)", "the scheduler state is under the big kernel lock (C6)"),
    ("CLAUDE.md", "S7 (libc, futexes, BASIC and Lua as userland programs) is specified", "S7 is done"),
    ("CLAUDE.md", "both\n  normal and icount modes", "icount runs only one hart (T21)"),
]


GATE_BUDGET_MINUTES = 7  # TESTING.md: every job of the merge gate (ci.yml) may take at most this long
# Soak targets that CI runs split into smaller jobs: the soak workflow may name the parts instead.
SOAK_SPLIT = {"os-boot-test": ["os-boot-test-main", "os-boot-test-rr", "os-boot-test-release"]}
# Targets besides the smoke ones that the gate may run, conditionally (see ci.yml): the harness check.
# (the smoke boot is two parallel jobs, one per shard of the suite, and the release smoke is a third: OD121)
GATE_EXTRA = {"os-boot-test-main", "os-boot-test-smoke-a", "os-boot-test-smoke-b"}


def make_var(makefile, name):
    m = re.search(rf"^{name}\s*:?=\s*(.*)$", makefile, re.M)
    return m.group(1).split() if m else []


def check_tiers():
    """The testing tiers (OD83, TESTING.md) are enforced mechanically, so they cannot drift back: the gate
    (`make check`, ci.yml) holds only tier 0 and 1 targets and stays inside its budget; every soak target
    is run by soak.yml."""
    out = []
    makefile = read("Makefile")
    smoke, soak = make_var(makefile, "SMOKE_TARGETS"), make_var(makefile, "SOAK_TARGETS")
    if not smoke or not soak:
        return ["Makefile: SMOKE_TARGETS or SOAK_TARGETS is missing"]
    if set(smoke) & set(soak):
        out.append(f"Makefile: targets in both tiers: {sorted(set(smoke) & set(soak))}")
    m = re.search(r"^check:\s*(.*)$", makefile, re.M)
    if not m or m.group(1).split() != ["smoke"]:
        out.append("Makefile: `check` must be exactly the smoke tier (`check: smoke`); everything else is `soak` / `check-all`")
    ci, soakyml = read(".github/workflows/ci.yml"), read(".github/workflows/soak.yml")
    allowed = set(smoke) | GATE_EXTRA
    for line in re.findall(r"^\s*(?:- )?run: make (.*)$", ci, re.M):
        for t in line.split():
            if t not in allowed:
                out.append(f"ci.yml (the merge gate) runs `make {t}`, which is not a smoke target (TESTING.md)")
    for n in re.findall(r"timeout-minutes:\s*(\d+)", ci):
        if int(n) > GATE_BUDGET_MINUTES:
            out.append(f"ci.yml: a gate job's timeout is {n} minutes; the budget is {GATE_BUDGET_MINUTES} (TESTING.md)")
    if "matrix:" in ci:
        out.append("ci.yml: the merge gate has a build matrix; that belongs in soak.yml")
    for t in soak:
        names = SOAK_SPLIT.get(t, []) + [t]
        if not any(re.search(rf"(?<![\w-]){re.escape(n)}(?![\w-])", soakyml) for n in names):
            out.append(f"soak.yml does not run the soak target {t}")
    if "TESTING.md" not in os.listdir(ROOT):
        out.append("TESTING.md is missing")
    return out


def main():
    problems = []
    for name, phrase, why in STALE:
        if phrase in read(name):
            problems.append(f"{name}: still says {phrase!r} ({why})")

    # Decision log: OD numbers are unique and consecutive.
    log = read("OS-ARCHITECTURE.md")
    nums = [int(m.group(1)) for m in re.finditer(r"^\| OD(\d+) \|", log, re.M)]
    seen = set()
    for n in nums:
        if n in seen:
            problems.append(f"OS-ARCHITECTURE.md: OD{n} appears twice")
        seen.add(n)
    if nums:
        missing = [n for n in range(min(nums), max(nums) + 1) if n not in seen]
        if missing:
            problems.append("OS-ARCHITECTURE.md: decision log has gaps: " + ", ".join(f"OD{n}" for n in missing))

    # The work queue (work/, scripts/work.py): ids, fields, blockers, cycles, the near-term cap.
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import work
    items, errors = work.load()
    problems += errors + work.check(items)

    # Every document the README and CLAUDE.md point at exists.
    for name in ["OS-ARCHITECTURE.md", "SYSCALLS.md", "INVARIANTS.md", "WORK-PLAN.md", "work/README.md",
                 "docs/records/done.md", "S4-SPEC.md", "S5-SPEC.md", "S6-SPEC.md", "S7-SPEC.md"]:
        if not os.path.exists(os.path.join(ROOT, name)):
            problems.append(f"{name} is missing")

    problems += check_tiers()
    if problems:
        print("docs check FAILED:")
        for p in problems:
            print("  " + p)
        return 1
    print(f"docs check passed ({len(STALE)} stale phrases absent, {len(nums)} decisions, {len(items)} work items)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
