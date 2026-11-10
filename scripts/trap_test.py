#!/usr/bin/env python3
"""Trap, timer and FPU boot test (milestone M2, driven through BASIC since M4).

Usage: trap_test.py KERNEL_ELF

Two boots:
  1. The FPU works (the boot banner prints 1.5 * 2). `PRINT TIMER` twice, half
     a second apart, shows the clock advancing. Then `PRINT PEEK(...)` of an
     unmapped address must raise a load access fault; the trap handler reports
     scause, sepc and stval (the faulting address), the tick counter and clock
     (which must show the 10 ms timer interrupt has been running), and powers
     the machine off cleanly.
  2. `POKE` to the same unmapped address must raise a store access fault.

Why two boots: a fatal trap powers the machine off.
"""
import re
import sys

from boot_test import boot, check

PAUSE = 0.5  # seconds between the two TIMER readings
BAD_ADDRESS = 0x12345678  # nothing is mapped here on QEMU's virt machine
TIMER_READING = re.compile(r"PRINT TIMER\n (\d+(?:\.\d+)?) \nREADY\.")


def fail(problems, text) -> int:
    print("trap test FAILED:", *problems, sep="\n  ", file=sys.stderr)
    print("--- captured output ---", file=sys.stderr)
    print(text, file=sys.stderr)
    return 1


def first_boot(kernel: str) -> int:
    steps = [
        (b"PRINT TIMER\r", PAUSE),
        (b"PRINT TIMER\r", 0.1),
        (f"PRINT PEEK({BAD_ADDRESS})\r".encode(), 1.0),
    ]
    text, timed_out, returncode = boot(kernel, steps, wait_for="READY.")
    status = check(
        text,
        [
            "fpu: 1.5 * 2 = 3",
            "TRAP: scause=0x5 (load access fault)",
            f"stval={BAD_ADDRESS:#x}",
        ],
        timed_out,
        returncode,
    )
    if status:
        return status

    problems = []
    readings = [float(x) for x in TIMER_READING.findall(text)]
    if len(readings) != 2 or not PAUSE * 0.6 <= readings[1] - readings[0] <= 5.0:
        problems.append(f"TIMER readings {readings}: expected two, about {PAUSE} s apart")
    sepc = re.search(r"sepc=0x([0-9a-f]+)", text)
    if not sepc or not 0x8020_0000 <= int(sepc.group(1), 16) < 0x8040_0000:
        problems.append("sepc should be an address inside the kernel image (0x80200000..0x80400000)")
    report = re.search(r"ticks=(\d+) ms=(\d+)", text)
    if not report:
        problems.append("trap report should include ticks and ms")
    else:
        ticks, ms = int(report.group(1)), int(report.group(2))
        # Nominally ms / 10 ticks; QEMU delivers them somewhat late, so only
        # check that the tick counter has been advancing at all.
        if ticks < 10 or ms < 500:
            problems.append(f"ticks={ticks} ms={ms}: the timer should have been running for over 0.5 s")
    if timed_out or returncode != 0:
        problems.append(f"expected a clean shutdown (timed_out={timed_out}, status={returncode})")
    return fail(problems, text) if problems else 0


def second_boot(kernel: str) -> int:
    text, timed_out, returncode = boot(kernel, [(f"POKE {BAD_ADDRESS},1\r".encode(), 1.0)], wait_for="READY.")
    problems = []
    if "scause=0x7 (store access fault)" not in text:
        problems.append("expected scause=0x7 (store access fault)")
    if f"stval={BAD_ADDRESS:#x}" not in text:
        problems.append(f"stval should be the faulting address {BAD_ADDRESS:#x}")
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
