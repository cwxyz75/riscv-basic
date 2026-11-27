#!/usr/bin/env python3
"""The host-stall injector (T284): a deliberate pause of the emulator, so that a timing failure blamed on "a loaded host" can be
made to happen on purpose and told apart from a kernel delay.

    OS_HOST_STALL=<ms>[:<period ms>[:<seed>]]   (read by scripts/os_boot_test.py for every boot it starts)
    host_stall.py --self-test                    stop a child that reads its clock in a loop, and see the gap (no emulator)

While the boot runs, the injector stops the QEMU process (SIGSTOP) for <ms> milliseconds, then lets it go on (SIGCONT). The gap
between the end of one stall and the start of the next is drawn uniformly from [period / 2, 3 * period / 2] with a random generator
seeded by <seed> (default period 1000 ms; default seed 1), so a run can be repeated and two stalls are never closer than half a
period. Every vCPU thread stops at once: the guest's `time` counter follows the host clock (QEMU's virtual clock without -icount),
so after the stall it has jumped by at least <ms>, and a timer interrupt that fell due during it arrives that much late. That
lateness is what the guest's pause detector reads (`kernel/src/pause.rs`).

Each stall is written into the serial log as `##HOSTSTALL <t> <ms>` (t: seconds since QEMU started, ms: how long the process was
really stopped, both with three decimals), on the harness side, between the guest's lines. The harness also puts `hoststall=<ms>`
on the kernel line, so the guest's own self-test (`a_host_stall_is_seen_by_the_guest`) runs and shows the stall landed.
"""

import os
import random
import signal
import subprocess
import sys
import threading
import time


def parse(spec):
    """`<ms>[:<period>[:<seed>]]` -> (ms, period, seed); SystemExit on anything else."""
    parts = spec.split(":")
    try:
        if not 1 <= len(parts) <= 3 or any(p == "" for p in parts):
            raise ValueError
        ms = int(parts[0])
        period = int(parts[1]) if len(parts) > 1 else 1000
        seed = int(parts[2]) if len(parts) > 2 else 1
    except ValueError:
        raise SystemExit(f"OS_HOST_STALL={spec!r}: want <ms>[:<period ms>[:<seed>]], whole numbers")
    if ms <= 0 or period <= 0:
        raise SystemExit(f"OS_HOST_STALL={spec!r}: the stall and the period must be positive")
    if period < 2 * ms:
        # (the shortest gap is half a period: below 2 * ms the emulator would be stopped more than it runs)
        raise SystemExit(f"OS_HOST_STALL={spec!r}: the period must be at least twice the stall")
    return ms, period, seed


class HostStall:
    """Stops and continues `pid` from a thread of its own until `stop()`. `emit(line)` receives each `##HOSTSTALL` line."""

    def __init__(self, pid, spec, emit):
        self.pid = pid
        self.ms, self.period, self.seed = parse(spec)
        self.emit = emit
        self.rng = random.Random(self.seed)
        self.done = threading.Event()
        self.stalls = []  # (t, ms) of every stall made
        self.started = time.monotonic()
        self.thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def _run(self):
        while not self.done.wait(self.rng.uniform(self.period / 2, 3 * self.period / 2) / 1000):
            t0 = time.monotonic()
            try:
                os.kill(self.pid, signal.SIGSTOP)
            except ProcessLookupError:
                return
            try:
                # (not `done.wait`: a stop asked for during a stall still lets the stall end at its length, then continues the process)
                time.sleep(self.ms / 1000)
            finally:
                try:
                    os.kill(self.pid, signal.SIGCONT)
                except ProcessLookupError:
                    return
            t1 = time.monotonic()
            self.stalls.append((t0 - self.started, (t1 - t0) * 1000))
            self.emit(f"##HOSTSTALL {t0 - self.started:.3f} {(t1 - t0) * 1000:.3f}\n".encode())

    def stop(self):
        """End the injection; the process is left running (a stall under way finishes first)."""
        self.done.set()
        self.thread.join(timeout=5 + self.ms / 1000)
        try:
            os.kill(self.pid, signal.SIGCONT)
        except ProcessLookupError:
            pass

    def summary(self):
        if not self.stalls:
            return f"host stall: none made ({self.ms} ms every ~{self.period} ms, seed {self.seed})"
        longest = max(ms for _, ms in self.stalls)
        return (f"host stall: {len(self.stalls)} stalls of {self.ms} ms every ~{self.period} ms (seed {self.seed}), "
                f"the longest {longest:.1f} ms")


def from_env(proc, emit):
    """The injector `OS_HOST_STALL` asks for, started on `proc`, or None."""
    spec = os.environ.get("OS_HOST_STALL")
    return HostStall(proc.pid, spec, emit).start() if spec else None


def boot_arg():
    """The kernel-line word that tells the guest a stall is coming (its self-test runs), or ""."""
    spec = os.environ.get("OS_HOST_STALL")
    if not spec:
        return ""
    ms, period, _ = parse(spec)
    return f"hoststall={ms}:{period}"


# The child of the self-test: it reads the monotonic clock in a tight loop for `secs` and prints the largest gap between two reads.
CHILD = """
import sys, time
end = time.monotonic() + float(sys.argv[1])
last, worst = time.monotonic(), 0.0
while last < end:
    now = time.monotonic()
    worst = max(worst, now - last)
    last = now
print(f"{worst * 1000:.3f}", flush=True)
"""


def self_test():
    results = {}
    # the spec's rules
    results["a full spec is read"] = parse("20:200:7") == (20, 200, 7)
    results["the period and seed default"] = parse("5") == (5, 1000, 1)
    for bad in ("", "x", "20:", "0:100", "20:30", "1:2:3:4", "-5:100"):
        try:
            parse(bad)
            results[f"{bad!r} is refused"] = False
        except SystemExit:
            results[f"{bad!r} is refused"] = True
    # the same seed gives the same gaps
    a, b = random.Random(3), random.Random(3)
    results["a seed repeats its moments"] = [a.uniform(1, 2) for _ in range(5)] == [b.uniform(1, 2) for _ in range(5)]
    # the stall lands: a child that reads its clock sees a gap at least the stall's length, and each stall is reported
    child = subprocess.Popen([sys.executable, "-I", "-c", CHILD, "1.2"], stdout=subprocess.PIPE, text=True)
    lines = []
    inj = HostStall(child.pid, "150:400:5", lambda l: lines.append(l.decode())).start()
    out, _ = child.communicate(timeout=30)
    inj.stop()
    worst = float(out.strip() or 0)
    results[f"the child saw a gap of at least the stall ({worst:.1f} ms >= 150 ms)"] = worst >= 150
    results[f"every stall is reported ({len(lines)} lines, {len(inj.stalls)} stalls)"] = (
        len(inj.stalls) >= 1 and len(lines) == len(inj.stalls)
        and all(l.startswith("##HOSTSTALL ") and float(l.split()[2]) >= 150 for l in lines))
    # a control: without the injector the same child sees no such gap (the test can fail)
    plain = subprocess.run([sys.executable, "-I", "-c", CHILD, "0.4"], capture_output=True, text=True, timeout=30)
    gap = float(plain.stdout.strip() or 0)
    results[f"without the injector the child sees no such gap ({gap:.1f} ms < 150 ms)"] = gap < 150
    for what, good in results.items():
        print(("ok: " if good else "FAIL: ") + what)
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    sys.exit(__doc__)
