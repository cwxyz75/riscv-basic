#!/usr/bin/env python3
"""The soak and overnight tiers as runs of their targets (T195, T283). Only the owner starts either tier (OD142).

    tier.py run JOBS TARGET...       run each make target (JOBS at a time), keep going after a failure, and end with one line per
                                     target: PASS or FAIL, its wall time, and the log of a failing one (`make soak` is this)
    tier.py piece PIECE ALLOWED...   check that PIECE is one of ALLOWED and that the piece variables in the environment are well formed
                                     (`make soak-piece`, `make overnight-piece`: the two-hour pieces of STAB0 that the workflows'
                                     `piece` jobs run); exits 2 otherwise
    tier.py --self-test              check both without running make

JOBS is SOAK_JOBS, 1 by default: the boots follow the host's clock and the host-side network tests share its loopback, so several boots
at once on one machine report failures that are not bugs (the first full soak, 2026-11-21: eight targets failed at three at a time and
passed alone). The piece variables are CHAOS_SEEDS (`A..B` or `A,B,C`), N (a repeat count), PKG and SHARD (`k/n`) for os-mutants.
"""
import os
import re
import subprocess
import sys
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")

# the variables a piece may set, and the form each must have (the workflows pass them from dispatch inputs)
PIECE_VARS = {
    "CHAOS_SEEDS": re.compile(r"[0-9]+\.\.[0-9]+|[0-9]+(,[0-9]+)*"),
    "N": re.compile(r"[1-9][0-9]{0,3}"),
    "PKG": re.compile(r"[a-z][a-z0-9_-]*"),
    "SHARD": re.compile(r"[0-9]+/[1-9][0-9]*"),
}


def piece_problems(piece, allowed, env):
    out = []
    if piece not in allowed:
        out.append(f"{piece!r} is not a target of this tier (one of: {' '.join(sorted(allowed))})")
    for name, form in PIECE_VARS.items():
        v = env.get(name, "")
        if v and not form.fullmatch(v):
            out.append(f"{name}={v!r} is not of the form {form.pattern}")
    shard = env.get("SHARD", "")
    if shard and form_ok(shard):
        k, n = map(int, shard.split("/"))
        if k >= n:
            out.append(f"SHARD={shard}: k must be below n")
    if (env.get("SHARD") or env.get("PKG")) and piece != "os-mutants":
        out.append("SHARD and PKG are os-mutants' variables")
    if env.get("CHAOS_SEEDS") and piece != "os-chaos":
        out.append("CHAOS_SEEDS is os-chaos's variable")
    if env.get("N") and piece != "overnight-repeat-smp":
        out.append("N is overnight-repeat-smp's variable")
    return out


def form_ok(shard):
    return bool(PIECE_VARS["SHARD"].fullmatch(shard))


def fmt(secs):
    m, s = divmod(int(secs), 60)
    h, m = divmod(m, 60)
    return f"{h}h{m:02d}m{s:02d}s" if h else f"{m}m{s:02d}s"


def run(jobs, targets):
    import concurrent.futures
    logdir = os.path.join(ROOT, "target", "tier")
    os.makedirs(logdir, exist_ok=True)

    def one(t):
        start = time.monotonic()
        log = os.path.join(logdir, f"{t}.log")
        with open(log, "w") as f:
            if jobs == 1:
                # one at a time: the output streams as it would from make, and goes to the log too
                p = subprocess.Popen(["make", "-C", ROOT, t], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                for line in p.stdout:
                    sys.stdout.write(line)
                    f.write(line)
                code = p.wait()
            else:
                code = subprocess.run(["make", "-C", ROOT, t], stdout=f, stderr=subprocess.STDOUT).returncode
        took = time.monotonic() - start
        print(f"tier: {t} {'PASS' if code == 0 else 'FAIL'} in {fmt(took)}", flush=True)
        return t, code == 0, took, log

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        results = list(ex.map(one, targets))
    print("\n== summary, one line per target ==")
    for t, ok, took, log in results:
        print(f"{'PASS' if ok else 'FAIL'}  {t:28} {fmt(took):>10}" + ("" if ok else f"  log {os.path.relpath(log, ROOT)}"))
    failed = [t for t, ok, _, _ in results if not ok]
    print(f"{len(results) - len(failed)}/{len(results)} targets passed" + (f"; failed: {' '.join(failed)}" if failed else ""))
    return 1 if failed else 0


def self_test():
    allowed = {"os-chaos", "os-mutants", "overnight-repeat-smp", "os-boot-test-smp"}
    assert piece_problems("os-boot-test-smp", allowed, {}) == []
    assert piece_problems("os-chaos", allowed, {"CHAOS_SEEDS": "1..50"}) == []
    assert piece_problems("os-chaos", allowed, {"CHAOS_SEEDS": "3,9,27"}) == []
    assert piece_problems("os-mutants", allowed, {"PKG": "kcore", "SHARD": "3/8"}) == []
    assert piece_problems("overnight-repeat-smp", allowed, {"N": "20"}) == []
    assert piece_problems("os-ktest-4", allowed, {}), "a target of another tier is refused"
    assert piece_problems("os-chaos; rm -rf /", allowed, {}), "a command is not a target"
    assert piece_problems("os-chaos", allowed, {"CHAOS_SEEDS": "1..50\" && true"}), "a malformed seed range"
    assert piece_problems("os-mutants", allowed, {"SHARD": "8/8"}), "k must be below n"
    assert piece_problems("os-mutants", allowed, {"SHARD": "x/8"}), "a malformed shard"
    assert piece_problems("overnight-repeat-smp", allowed, {"N": "0"}), "N of 0"
    assert piece_problems("os-boot-test-smp", allowed, {"N": "5"}), "a variable of another target"
    assert piece_problems("os-boot-test-smp", allowed, {"CHAOS_SEEDS": "1..2"}), "a variable of another target"
    assert fmt(3725) == "1h02m05s" and fmt(65) == "1m05s"
    print("tier self-test passed")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if len(argv) >= 3 and argv[1] == "run":
        return run(int(argv[2] or "1"), argv[3:])
    if len(argv) >= 3 and argv[1] == "piece":
        found = piece_problems(argv[2], set(argv[3:]), os.environ)
        for p in found:
            print(f"tier piece: {p}", file=sys.stderr)
        return 2 if found else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
