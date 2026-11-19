#!/usr/bin/env python3
"""Run a make target N times and report the pass rate (KT2, OD140: an intermittent failure is a defect, so measure it).
Usage: os_repeat.py TARGET N   (also `make os-repeat TARGET=<target> N=<n>`)
Each run is `make TARGET` with SOAK_JOBS=1; the serial log of every failing run is saved under target/repeat/. Prints `pass/N`.
Running this over a soak target is a soak run: only the owner starts those (OD142); the acceptance check uses the fail-demo target."""
import os, subprocess, sys, time
target, n = sys.argv[1], int(sys.argv[2])
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
logdir = os.path.join(root, "target", "repeat")
os.makedirs(logdir, exist_ok=True)
passed = 0
for i in range(1, n + 1):
    r = subprocess.run(["make", "-C", root, target], capture_output=True, text=True, env={**os.environ, "SOAK_JOBS": "1"})
    if r.returncode == 0:
        passed += 1
        print(f"run {i}/{n}: pass", flush=True)
    else:
        path = os.path.join(logdir, f"{target}-{i}-{int(time.time())}.log")
        with open(path, "w") as f:
            f.write(r.stdout + "\n--- stderr ---\n" + r.stderr)
        print(f"run {i}/{n}: FAIL (log {os.path.relpath(path, root)})", flush=True)
print(f"{target}: {passed}/{n}")
sys.exit(0)
