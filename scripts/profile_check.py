#!/usr/bin/env python3
"""Time every phase of `make check` and every QEMU boot in it, to see where the time goes (slice TS
step 0; the result is kept in TESTING.md).

    python3 scripts/profile_check.py [OUT_DIR]

Runs the targets of `make check` one after another (a cold build for the first that needs one, warm
after: the order is the order of the Makefile's `check` target), writes OUT_DIR/phases.tsv (target,
wall seconds, exit status) and OUT_DIR/boots.tsv (every boot's per-test host seconds and guest ticks,
from `OS_PROFILE`), then prints a summary. It does not stop at a failing phase.
"""
import os
import re
import subprocess
import sys
import time
from collections import defaultdict

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/profile"
os.makedirs(out, exist_ok=True)
makefile = open("Makefile").read()
targets = re.search(r"^check:\s*(.*)$", makefile, re.M).group(1).split()
boots = os.path.join(out, "boots.tsv")
open(boots, "w").close()
env = dict(os.environ, OS_PROFILE=os.path.abspath(boots))
rows = []
for t in targets:
    start = time.monotonic()
    r = subprocess.run(["make", t], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rows.append((t, time.monotonic() - start, r.returncode))
    print(f"{t:28} {rows[-1][1]:8.1f} s  exit {r.returncode}", flush=True)
with open(os.path.join(out, "phases.tsv"), "w") as f:
    for t, secs, rc in rows:
        f.write(f"{t}\t{secs:.1f}\t{rc}\n")
total = sum(s for _, s, _ in rows)
print(f"{'total':28} {total:8.1f} s")
# boots: total per boot, and the slowest tests across all boots
boot_total, tests = {}, defaultdict(float)
for line in open(boots):
    p = line.rstrip("\n").split("\t")
    if p[0] == "#boot":
        boot_total[p[1]] = boot_total.get(p[1], 0) + float(p[2])
    elif len(p) >= 3:
        tests[p[1]] += float(p[2])
print(f"\n{len(boot_total)} distinct boot configurations, {sum(boot_total.values()):.0f} s of boots in all")
for label, secs in sorted(boot_total.items(), key=lambda x: -x[1]):
    print(f"  {secs:7.1f} s  {label}")
print("\nTests by total host time over all boots:")
for name, secs in sorted(tests.items(), key=lambda x: -x[1])[:25]:
    print(f"  {secs:7.1f} s  {name}")
