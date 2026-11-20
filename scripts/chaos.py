#!/usr/bin/env python3
"""Schedule perturbation runs (KT7): boot the kernel-only shard of a `chaos` kernel once per (seed, hart count) and report the failures.
Usage: chaos.py ELF SEEDS [--smp 2,4,8] [--jobs N] [--expect-fail]
  SEEDS is `A..B` (inclusive) or `A,B,C`. Each boot is `OS_SHARD=k OS_SMP=<n> OS_CHAOS_SEED=<seed> os_boot_test.py ELF`. A failing seed
  prints `FAIL seed=<s> smp=<n>` and keeps its log under target/chaos/. Without --expect-fail the exit status is 1 if anything failed.
  With --expect-fail (a negative-control kernel) the run stops at the first failing boot, prints how many seeds it took, and exits 0 only
  if one failed. Overnight tier (OD142): the owner starts `make os-chaos`; the controls are run by hand once, for the record."""
import argparse, concurrent.futures, os, subprocess, sys

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.join(here, "..")
ap = argparse.ArgumentParser()
ap.add_argument("elf")
ap.add_argument("seeds")
ap.add_argument("--smp", default="2,4,8")
ap.add_argument("--jobs", type=int, default=int(os.environ.get("SOAK_JOBS", "3")))
ap.add_argument("--expect-fail", action="store_true")
a = ap.parse_args()


def parse_seeds(text):
    if ".." in text:
        lo, hi = text.split("..", 1)
        return list(range(int(lo), int(hi) + 1))
    return [int(x) for x in text.split(",")]


logdir = os.path.join(root, "target", "chaos")
os.makedirs(logdir, exist_ok=True)


def boot(seed, smp):
    env = {"OS_BOOT_STALL": "400", **os.environ, "OS_SHARD": "k", "OS_SMP": str(smp), "OS_CHAOS_SEED": str(seed)}
    r = subprocess.run([sys.executable, os.path.join(here, "os_boot_test.py"), a.elf], capture_output=True, text=True, env=env)
    if r.returncode != 0:
        with open(os.path.join(logdir, f"seed{seed}-smp{smp}.log"), "w") as f:
            f.write(r.stdout + "\n--- stderr ---\n" + r.stderr)
    return seed, smp, r.returncode == 0


seeds, smps = parse_seeds(a.seeds), [int(x) for x in a.smp.split(",")]
jobs = 1 if a.expect_fail else a.jobs
failed, done = [], 0
with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
    work = [(s, n) for s in seeds for n in smps]
    for seed, smp, ok in pool.map(lambda t: boot(*t), work):
        done += 1
        if ok:
            print(f"ok   seed={seed} smp={smp}", flush=True)
        else:
            failed.append((seed, smp))
            print(f"FAIL seed={seed} smp={smp} (log target/chaos/seed{seed}-smp{smp}.log)", flush=True)
            if a.expect_fail:
                print(f"the negative control failed at its boot number {done} (seed {seed}, smp {smp})")
                sys.exit(0)
print(f"{done - len(failed)}/{done} boots passed")
if a.expect_fail:
    print("the negative control never failed: the perturbation did not expose it")
    sys.exit(1)
sys.exit(1 if failed else 0)
