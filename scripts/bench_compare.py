#!/usr/bin/env python3
"""Compare a benchmark run with the checked-in baseline (SCHED-SMP-SPEC 6.1, slice B).

    bench_compare.py RUN.tsv [BASELINE.tsv] [--max-ratio R]   exit 1 if any benchmark is more than R times slower
    bench_compare.py --self-test                               check that a 3x slowdown is caught

A TSV row is `label <TAB> name <TAB> value <TAB> unit` (the label is the hart count, `smp1`, `smp4`). Every
benchmark reports a *cost* (ns per operation): smaller is better, so a ratio above R is a slowdown. A name in
the run but not in the baseline is reported and not an error (a new benchmark); one in the baseline but not in
the run is an error (a benchmark that stopped running). The threshold is generous (QEMU on a shared host is
noisy, SCHED-SMP-SPEC T75): it catches a 3x slowdown, not a 10% one; compare the printed ratios for those.
"""
import sys


def load(path):
    rows = {}
    for line in open(path):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 4 or not parts[2].isdigit():
            raise SystemExit(f"{path}: malformed row {line!r}")
        rows[(parts[0], parts[1])] = (int(parts[2]), parts[3])
    return rows


def compare(run, base, max_ratio):
    problems = []
    for key, (bv, unit) in sorted(base.items()):
        if key not in run:
            problems.append(f"{key[0]} {key[1]}: in the baseline but not in this run")
            continue
        v, u = run[key]
        if u != unit:
            problems.append(f"{key[0]} {key[1]}: unit {u} (baseline {unit})")
            continue
        ratio = v / bv if bv else float("inf")
        flag = "  <-- SLOWER" if ratio > max_ratio else ""
        print(f"{key[0]:6} {key[1]:28} {v:>12} {unit:8} baseline {bv:>12}  x{ratio:5.2f}{flag}")
        if ratio > max_ratio:
            problems.append(f"{key[0]} {key[1]}: {v} {unit} is x{ratio:.2f} the baseline {bv}")
    for key in sorted(set(run) - set(base)):
        print(f"{key[0]:6} {key[1]:28} {run[key][0]:>12} {run[key][1]:8} (new: not in the baseline)")
    return problems


def self_test():
    base = {("smp1", "thread_create_join"): (1000, "ns"), ("smp4", "wake_latency_event"): (500, "ns")}
    same = dict(base)
    ok = compare(same, base, 3.0)
    slow = {k: (v * 3 + 1, u) for k, (v, u) in base.items()}
    bad = compare(slow, base, 3.0)
    missing = compare({}, base, 3.0)
    assert not ok, ok
    assert len(bad) == 2, bad
    assert len(missing) == 2, missing
    print("bench_compare self-test passed")


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test() or 0
    args = [a for a in argv[1:] if not a.startswith("--")]
    max_ratio = 3.0
    if "--max-ratio" in argv:
        max_ratio = float(argv[argv.index("--max-ratio") + 1])
        args = [a for a in args if a != argv[argv.index("--max-ratio") + 1]]
    if not 1 <= len(args) <= 2:
        print(__doc__, file=sys.stderr)
        return 2
    run = load(args[0])
    base = load(args[1] if len(args) == 2 else "bench/baseline.tsv")
    problems = compare(run, base, max_ratio)
    if problems:
        print("\nbenchmark problems:", *problems, sep="\n  ", file=sys.stderr)
        return 1
    print("benchmarks within the baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
