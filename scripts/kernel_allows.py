#!/usr/bin/env python3
"""List the `#[allow(clippy::...)] // INVARIANT: ...` lines of the kernel crates (KT3): every site where a panic-finding lint
(unwrap, expect, indexing, arithmetic, panic, unreachable) was allowed, with the reason. Prints the count per crate.
Fails (exit 1) when an allow of those lints has no `INVARIANT:` reason on the same line.
Usage: kernel_allows.py [--list]"""
import os, re, sys
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
CRATES = ["kcore", "klock", "abi", "kernel"]
LINT = re.compile(r"clippy::(unwrap_used|expect_used|indexing_slicing|arithmetic_side_effects|panic|unreachable)")
bad, counts = [], {}
for crate in CRATES:
    n = 0
    for d, _, files in os.walk(os.path.join(ROOT, crate, "src")):
        for f in files:
            if not f.endswith(".rs"):
                continue
            path = os.path.join(d, f)
            lines = open(path).read().split("\n")
            i = 0
            while i < len(lines):
                s = lines[i].strip()
                start = i
                if s.startswith("#[allow(") or s.startswith("#![allow("):
                    while ")]" not in lines[i] and i + 1 < len(lines):  # a multi-line allow runs to its `)]`
                        i += 1
                        s += " " + lines[i].strip()
                    if LINT.search(s):
                        n += 1
                        rel = os.path.relpath(path, ROOT)
                        if "INVARIANT:" not in s:
                            bad.append(f"{rel}:{start + 1}: an allow of a panic-finding lint with no `// INVARIANT:` reason")
                        elif "--list" in sys.argv:
                            print(f"{rel}:{start + 1}: {s.split('INVARIANT:', 1)[1].strip()}")
                i += 1
    counts[crate] = n
print("allowed sites per crate:", counts, "total", sum(counts.values()))
for b in bad:
    print(b)
sys.exit(1 if bad else 0)
