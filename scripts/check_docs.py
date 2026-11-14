#!/usr/bin/env python3
"""Contract-document checks (TIDY-UP T55): nobody reads the code, so the documents are the contract, and a
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
    ("ROADMAP.md", "S3 onwards is not", "S0-S7 are built"),
    ("ROADMAP.md", "`abi`, `kcore`, `os-kernel`; more arrive", "crate names are fixed"),
    ("S5-SPEC.md", "Nothing here is built yet", "S5 is built"),
    ("INVARIANTS.md", "(single hart)", "the scheduler state is under the big kernel lock (C6)"),
    ("CLAUDE.md", "S7 (libc, futexes, BASIC and Lua as userland programs) is specified", "S7 is done"),
    ("CLAUDE.md", "both\n  normal and icount modes", "icount runs only one hart (T21)"),
]


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

    # TIDY-UP: no T-number is both open and resolved, and ids are unique.
    tidy = read("TIDY-UP.md")
    open_ids = re.findall(r"^\| (T\d+) \|", tidy, re.M)
    if len(open_ids) != len(set(open_ids)):
        problems.append("TIDY-UP.md: a T-number appears twice")
    resolved = re.findall(r"^\| R\d+ \| (T\d+)[: ]", tidy, re.M)
    for t in resolved:
        if t in open_ids:
            problems.append(f"TIDY-UP.md: {t} is in the open list and also resolved")

    # Every document the README and CLAUDE.md point at exists.
    for name in ["OS-ARCHITECTURE.md", "SYSCALLS.md", "INVARIANTS.md", "TIDY-UP.md", "ROADMAP.md",
                 "S4-SPEC.md", "S5-SPEC.md", "S6-SPEC.md", "S7-SPEC.md"]:
        if not os.path.exists(os.path.join(ROOT, name)):
            problems.append(f"{name} is missing")

    if problems:
        print("docs check FAILED:")
        for p in problems:
            print("  " + p)
        return 1
    print(f"docs check passed ({len(STALE)} stale phrases absent, {len(nums)} decisions, {len(open_ids)} open debts)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
