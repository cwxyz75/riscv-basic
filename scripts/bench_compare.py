#!/usr/bin/env python3
"""Compare a benchmark run with the checked-in baseline (SCHED-SMP-SPEC 6.1, slice B).

    bench_compare.py RUN.tsv [BASELINE.tsv] [--max-ratio R]   exit 1 if any benchmark is more than R times slower
    bench_compare.py --self-test                               check that a 3x slowdown is caught
    ... --prefix count_                                        only the rows whose name starts so (the instruction counts of KT5)

A TSV row is `label <TAB> name <TAB> value <TAB> unit` (the label is the hart count, `smp1`, `smp4`). Every
benchmark reports a *cost* (ns per operation): smaller is better, so a ratio above R is a slowdown. A name in
the run but not in the baseline is reported and not an error (a new benchmark); one in the baseline but not in
the run is an error (a benchmark that stopped running). The threshold is generous (QEMU on a shared host is
noisy, SCHED-SMP-SPEC T75): it catches a 3x slowdown, not a 10% one; compare the printed ratios for those.
"""
import sys


def load(path, prefix=""):
    rows = {}
    for line in open(path):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 4 or not parts[2].isdigit():
            raise SystemExit(f"{path}: malformed row {line!r}")
        if parts[1].startswith(prefix):
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
        flag = "  <-- SLOWER" if ratio > max_ratio else ("  <-- FASTER: record the new value in the baseline" if ratio < 1 / max_ratio else "")
        print(f"{key[0]:6} {key[1]:28} {v:>12} {unit:8} baseline {bv:>12}  x{ratio:5.2f}{flag}")
        if ratio > max_ratio:
            problems.append(f"{key[0]} {key[1]}: {v} {unit} is x{ratio:.2f} the baseline {bv}")
    for key in sorted(set(run) - set(base)):
        print(f"{key[0]:6} {key[1]:28} {run[key][0]:>12} {run[key][1]:8} (new: not in the baseline)")
    return problems


def scaling(run):
    """Print the scaling of every `pair_scaling_<kind>_n<N>` series (SCHED-SMP-SPEC 5.2): the cost at N = 1 over the cost at N,
    which is how many times more round trips a second N pairs complete. Returns {(label, kind, N): ratio}."""
    ratios = {}
    for (label, name), (v, _) in sorted(run.items()):
        kind, _, n = name.rpartition("_n")
        if not kind.startswith("pair_scaling_") or not n.isdigit():
            continue
        one = run.get((label, f"{kind}_n1"))
        if one and v:
            ratios[(label, kind, int(n))] = one[0] / v
            print(f"{label:6} {name:28} scaling x{one[0] / v:5.2f} over one pair")
    return ratios


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
    r = scaling({("smp4", "pair_scaling_futex_n1"): (900, "ns"), ("smp4", "pair_scaling_futex_n4"): (300, "ns")})
    assert r == {("smp4", "pair_scaling_futex", 1): 1.0, ("smp4", "pair_scaling_futex", 4): 3.0}, r
    print("bench_compare self-test passed")


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test() or 0
    args = [a for a in argv[1:] if not a.startswith("--")]
    prefix = ""
    if "--prefix" in argv:
        prefix = argv[argv.index("--prefix") + 1]
        args = [a for a in args if a != prefix]
    max_ratio = 3.0
    if "--max-ratio" in argv:
        max_ratio = float(argv[argv.index("--max-ratio") + 1])
        args = [a for a in args if a != argv[argv.index("--max-ratio") + 1]]
    if not 1 <= len(args) <= 2:
        print(__doc__, file=sys.stderr)
        return 2
    run = load(args[0], prefix)
    base = load(args[1] if len(args) == 2 else "bench/baseline.tsv", prefix)
    problems = compare(run, base, max_ratio)
    scaling(run)
    if problems:
        print("\nbenchmark problems:", *problems, sep="\n  ", file=sys.stderr)
        return 1
    print("benchmarks within the baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
