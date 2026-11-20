#!/usr/bin/env python3
"""Mutation testing of `kcore` and `klock` (KT10, OD140): run cargo-mutants with the crate's host tests as the oracle and fail on any
surviving mutant that is not in `mutants-accepted.txt`.

Usage: mutants.py PACKAGE [--shard K/N] [--file PATH]... [--list]
  PACKAGE   kcore or klock
  --shard   run one slice of the mutants (cargo-mutants' own sharding), so a CI matrix fits the job budget
  --file    only the mutants of these files (the acceptance check and local use)
Output: target/mutants-<package>[-<k>of<n>]/ (cargo-mutants' own), a summary on stdout and, in CI, in the job summary.

`mutants-accepted.txt`: one line per accepted survivor, `path: replace X with Y in fn  # reason` (no line and column: they drift). A
reason is required: an equivalent mutant (the change cannot be observed) or diagnostic text only. Everything else is killed by a test.
Exit status: 0 if every survivor is accepted, 1 otherwise (and 2 if a line of the accepted list has no reason).
This is a long run: the owner starts it (OD142), through overnight.yml or `make os-mutants`."""
import argparse, os, re, subprocess, sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
ap = argparse.ArgumentParser()
ap.add_argument("package", choices=["kcore", "klock"])
ap.add_argument("--shard")
ap.add_argument("--file", action="append", default=[])
a = ap.parse_args()

tag = a.package + (f"-{a.shard.replace('/', 'of')}" if a.shard else "")
out = os.path.join(root, "target", f"mutants-{tag}")
cmd = ["cargo", "mutants", "-p", a.package, "--output", out, "--no-times"]
if a.shard:
    cmd += ["--shard", a.shard]
for f in a.file:
    cmd += ["--file", f]
subprocess.run(cmd, cwd=root)  # (a non-zero status means survivors or timeouts: read the lists below, not the status)


def norm(line):
    return re.sub(r"^([^:]+):\d+:\d+:", r"\1:", line.strip())


accepted, bad = {}, []
path = os.path.join(root, "mutants-accepted.txt")
if os.path.exists(path):
    for n, line in enumerate(open(path), 1):
        line = line.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if " # " not in line or not line.split(" # ", 1)[1].strip():
            bad.append(f"mutants-accepted.txt:{n}: no reason")
            continue
        accepted[norm(line.split(" # ", 1)[0])] = line.split(" # ", 1)[1]
if bad:
    print("\n".join(bad))
    sys.exit(2)


def read(name):
    p = os.path.join(out, "mutants.out", name)
    return [l for l in open(p).read().splitlines() if l.strip()] if os.path.exists(p) else []


missed, timeout = read("missed.txt"), read("timeout.txt")
new = [m for m in missed + timeout if norm(m) not in accepted]
summary = [f"## mutants ({tag}): {len(missed)} missed, {len(timeout)} timed out, {len(new)} not accepted"] + [f"- {m}" for m in new]
print("\n".join(summary))
if os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
        f.write("\n".join(summary) + "\n")
sys.exit(1 if new else 0)
