#!/usr/bin/env python3
"""Milestone M2 boot test: FPU, timer ticks, and trap reporting.

Usage: trap_test.py KERNEL_ELF

Two boots:
  1. The FPU works (the banner prints 1.5 * 2); `ticks` reports a tick count
     and a millisecond clock that both advance in real time (two readings half
     a second apart); `crash` executes an illegal instruction and the trap
     handler reports scause and sepc, then powers the machine off cleanly.
  2. `badload` reads an unmapped address; the handler must report a load access
     fault whose stval is exactly that address.

Why two boots: a fatal trap powers the machine off.
"""
import re
import sys

from boot_test import boot, check

PAUSE = 0.5  # seconds between the two `ticks` readings


def fail(problems, text) -> int:
    print("trap test FAILED:", *problems, sep="\n  ", file=sys.stderr)
    print("--- captured output ---", file=sys.stderr)
    print(text, file=sys.stderr)
    return 1


def first_boot(kernel: str) -> int:
    steps = [(b"ticks\r", PAUSE), (b"ticks\r", 0.2), (b"crash\r", 1.0)]
    text, timed_out, returncode = boot(kernel, steps, wait_for="console ready")
    status = check(
        text,
        ["fpu: 1.5 * 2 = 3", "TRAP: scause=0x2 (illegal instruction)", "stval=0x"],
        timed_out,
        returncode,
    )
    if status:
        return status

    problems = []
    readings = [(int(t), int(m)) for t, m in re.findall(r"ticks=(\d+) ms=(\d+)", text)]
    if len(readings) != 2:
        problems.append(f"expected two `ticks=N ms=M` readings, got {readings}")
    else:
        (t1, m1), (t2, m2) = readings
        if not 300 <= m2 - m1 <= 2000:
            problems.append(f"ms clock advanced by {m2 - m1}, expected about {int(PAUSE * 1000)}")
        # Nominally 50 ticks; QEMU delivers them somewhat late, so only check the order of magnitude.
        if not 10 <= t2 - t1 <= 200:
            problems.append(f"tick counter advanced by {t2 - t1}, expected roughly {int(PAUSE * 100)}")
    if not re.search(r"sepc=0x8020[0-9a-f]{4}\b", text):
        problems.append("sepc should be an address inside the kernel image")
    if timed_out or returncode != 0:
        problems.append(f"expected a clean shutdown (timed_out={timed_out}, status={returncode})")
    return fail(problems, text) if problems else 0


def second_boot(kernel: str) -> int:
    text, timed_out, returncode = boot(kernel, [(b"badload\r", 1.0)], wait_for="console ready")
    problems = []
    if "scause=0x5 (load access fault)" not in text:
        problems.append("expected scause=0x5 (load access fault)")
    if "stval=0x12345678" not in text:
        problems.append("stval should be the faulting address 0x12345678")
    if timed_out or returncode != 0:
        problems.append(f"expected a clean shutdown (timed_out={timed_out}, status={returncode})")
    return fail(problems, text) if problems else 0


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    if first_boot(sys.argv[1]) or second_boot(sys.argv[1]):
        return 1
    print("trap test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
