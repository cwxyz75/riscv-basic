#!/usr/bin/env python3
"""Compare a benchmark run with the checked-in baseline (SCHED-SMP-SPEC 6.1, slice B).

    bench_compare.py RUN.tsv [BASELINE.tsv] [--max-ratio R]   exit 1 if any benchmark is more than R times slower
    bench_compare.py --self-test                               check that a 3x slowdown is caught
    ... --scaling-floor FILE                                   also fail if a pair_scaling ratio is below its floor in FILE (a TSV of
                                                               `label <TAB> series <TAB> N <TAB> floor`, e.g. `smp4 pair_scaling_futex 4 0.7`)
    ... --prefix count_                                        only the rows whose name starts so (the instruction counts of KT5)
    bench_compare.py --provenance                              print the `#@` lines naming this machine and its QEMU (a run file starts with them)

A TSV row is `label <TAB> name <TAB> value <TAB> unit` (the label is the hart count, `smp1`, `smp4`). Every
benchmark reports a *cost* (ns per operation): smaller is better, so a ratio above R is a slowdown. A name in
the run but not in the baseline is reported and not an error (a new benchmark); one in the baseline but not in
the run is an error (a benchmark that stopped running). The threshold is generous (QEMU on a shared host is
noisy, SCHED-SMP-SPEC T75): it catches a 3x slowdown, not a 10% one; compare the printed ratios for those.

The numbers belong to a machine and an emulator (T195: a laptop's wall times against another laptop's baseline were 1.2 to 3.0 times
off, and one build's instruction counts under another QEMU 70% above `bench/counts.tsv`). So a file names where it was measured, in
`#@ machine: ...` and `#@ qemu: ...` lines (`--provenance` prints them; the make targets start every run file with them, and a baseline
recorded from a run file keeps them), and the comparison says so when the run and the baseline differ, or when the baseline names
neither: the ratios then compare two machines, not two builds.
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


def provenance():
    """{"machine": ..., "qemu": ...} of this host: the OS, the architecture and the CPU model, and the first line of the QEMU on the PATH."""
    import platform
    import subprocess
    cpu, arch = "", platform.machine()
    try:
        if platform.system() == "Darwin":
            cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
            # (a Python running under Rosetta reports x86_64 on an Apple processor: the machine's own architecture is what counts)
            if subprocess.run(["sysctl", "-n", "hw.optional.arm64"], capture_output=True, text=True).stdout.strip() == "1":
                arch = "arm64"
        else:
            for line in open("/proc/cpuinfo"):
                if line.lower().startswith(("model name", "uarch", "isa")):
                    cpu = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    try:
        qemu = subprocess.run(["qemu-system-riscv64", "--version"], capture_output=True, text=True).stdout.splitlines()[0]
        qemu = qemu.replace("QEMU emulator version ", "", 1).strip()
    except (OSError, IndexError):
        qemu = "none on the PATH"
    return {"machine": " ".join(x for x in (platform.system(), arch, cpu) if x), "qemu": qemu}


def load_provenance(path):
    """The `#@ key: value` lines of a TSV, as a dict ({} when it names none)."""
    out = {}
    for line in open(path):
        if line.startswith("#@ ") and ":" in line:
            k, v = line[3:].split(":", 1)
            out[k.strip()] = v.strip()
    return out


def provenance_note(run_prov, base_prov):
    """What to say about where the run and the baseline were measured: None when they agree."""
    if not base_prov.get("machine") or not base_prov.get("qemu"):
        return ("note: the baseline does not say which machine and QEMU it was measured on, so its numbers may be another machine's: "
                f"this run is {run_prov.get('machine', '?')}, QEMU {run_prov.get('qemu', '?')}")
    differ = [k for k in ("machine", "qemu") if run_prov.get(k) != base_prov.get(k)]
    if not differ:
        return None
    return ("note: the run and the baseline were measured on different "
            + " and ".join("machines" if k == "machine" else "QEMU versions" for k in differ)
            + f" (baseline: {base_prov['machine']}, QEMU {base_prov['qemu']}; this run: {run_prov.get('machine', '?')}, QEMU "
            + f"{run_prov.get('qemu', '?')}): the ratios compare two machines, not two builds")


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


def check_floors(ratios, path):
    """T252: each recorded floor is a tripwire for the scaling collapsing, not the design's goal (3.0 futex, 2.5 channel_call, not gates:
    SCHED-SMP-SPEC amendment of 2026-11-24). A floor whose series was not run is an error."""
    problems = []
    for line in open(path):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            raise SystemExit(f"{path}: malformed row {line!r}")
        label, kind, n, floor = parts[0], parts[1], int(parts[2]), float(parts[3])
        got = ratios.get((label, kind, n))
        if got is None:
            problems.append(f"{label} {kind} n{n}: a scaling floor is recorded but the series did not run")
        elif got < floor:
            problems.append(f"{label} {kind} n{n}: scaling x{got:.2f} is below the floor x{floor:.2f}")
        else:
            print(f"{label:6} {kind:28} n{n} scaling x{got:5.2f} (floor x{floor:.2f})")
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
    r = scaling({("smp4", "pair_scaling_futex_n1"): (900, "ns"), ("smp4", "pair_scaling_futex_n4"): (300, "ns")})
    assert r == {("smp4", "pair_scaling_futex", 1): 1.0, ("smp4", "pair_scaling_futex", 4): 3.0}, r
    import os
    import tempfile

    with tempfile.NamedTemporaryFile("w", suffix=".tsv", delete=False) as f:
        f.write("# floors\nsmp4\tpair_scaling_futex\t4\t2.0\nsmp4\tpair_scaling_futex\t2\t0.5\n")
    try:
        assert check_floors(r, f.name) == ["smp4 pair_scaling_futex n2: a scaling floor is recorded but the series did not run"], "a missing series"
        low = {("smp4", "pair_scaling_futex", 4): 1.5}
        assert len(check_floors(low, f.name)) == 2, "a scaling below its floor"
        assert check_floors({("smp4", "pair_scaling_futex", 4): 2.0, ("smp4", "pair_scaling_futex", 2): 0.5}, f.name) == []
    finally:
        os.unlink(f.name)
    # T195: where a run and its baseline were measured
    here = {"machine": "Darwin arm64 Apple M2", "qemu": "11.1.3"}
    assert provenance_note(here, dict(here)) is None, "the same machine and QEMU say nothing"
    other = provenance_note(here, {"machine": "Linux x86_64 AMD EPYC 7763", "qemu": "10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.3)"})
    assert other and "different machines and QEMU versions" in other and "AMD EPYC 7763" in other and "Apple M2" in other, other
    q = provenance_note(here, {"machine": "Darwin arm64 Apple M2", "qemu": "10.2.1"})
    assert q and "different QEMU versions" in q and "machines" not in q.split("(")[0], q
    m = provenance_note(here, {"machine": "Linux x86_64 AMD EPYC 7763", "qemu": "11.1.3"})
    assert m and "different machines" in m and "QEMU versions" not in m.split("(")[0], m
    none = provenance_note(here, {})
    assert none and "does not say which machine" in none, none
    with tempfile.NamedTemporaryFile("w", suffix=".tsv", delete=False) as f:
        f.write("#@ machine: Linux x86_64 AMD EPYC 7763\n#@ qemu: 11.1.3\n# a comment: not provenance\nsmp1\tthread_create_join\t5\tns\n")
    try:
        assert load_provenance(f.name) == {"machine": "Linux x86_64 AMD EPYC 7763", "qemu": "11.1.3"}, load_provenance(f.name)
        assert load(f.name) == {("smp1", "thread_create_join"): (5, "ns")}, "the provenance lines are not rows"
    finally:
        os.unlink(f.name)
    p = provenance()
    assert p["machine"] and p["qemu"], p
    print("bench_compare self-test passed")


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test() or 0
    if argv[1:] == ["--provenance"]:
        for k, v in provenance().items():
            print(f"#@ {k}: {v}")
        return 0
    args = [a for a in argv[1:] if not a.startswith("--")]
    prefix = ""
    if "--prefix" in argv:
        prefix = argv[argv.index("--prefix") + 1]
        args = [a for a in args if a != prefix]
    max_ratio = 3.0
    if "--max-ratio" in argv:
        max_ratio = float(argv[argv.index("--max-ratio") + 1])
        args = [a for a in args if a != argv[argv.index("--max-ratio") + 1]]
    floors = None
    if "--scaling-floor" in argv:
        floors = argv[argv.index("--scaling-floor") + 1]
        args = [a for a in args if a != floors]
    if not 1 <= len(args) <= 2:
        print(__doc__, file=sys.stderr)
        return 2
    run = load(args[0], prefix)
    base_path = args[1] if len(args) == 2 else "bench/baseline.tsv"
    base = load(base_path, prefix)
    note = provenance_note(load_provenance(args[0]) or provenance(), load_provenance(base_path))
    if note:
        print(note)
    problems = compare(run, base, max_ratio)
    ratios = scaling(run)
    if floors:
        problems += check_floors(ratios, floors)
    if problems:
        print("\nbenchmark problems:", *problems, sep="\n  ", file=sys.stderr)
        if note:
            print(note, file=sys.stderr)
        return 1
    print("benchmarks within the baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
