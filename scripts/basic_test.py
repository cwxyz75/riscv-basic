#!/usr/bin/env python3
"""Milestone M4 boot test: the interpreter running on the real machine.

Usage: basic_test.py KERNEL_ELF

Pipes a BASIC program into the serial port and checks the output: the banner,
a stored and listed program, a FOR loop, floating-point maths (so the FPU,
libm and number formatting all work on target), PEEK/POKE on real RAM, and the
clock advancing between two readings.
"""
import re
import sys

from boot_test import boot, check

# 0x87F00000, near the top of QEMU's default 128 MiB of RAM, belongs to nobody.
SCRATCH = 0x87F00000

LINES = [
    b"10 FOR I=1 TO 3\r",
    b'20 PRINT "HELLO";I\r',
    b"30 NEXT I\r",
    b"RUN\r",
    b"LIST\r",
    b"PRINT 2^0.5\r",
    b"PRINT 1/3\r",
    b"PRINT ATN(1)*4\r",
    b"X=RND(-5): A=RND(1): X=RND(-5): PRINT A=RND(1)\r",
    f"POKE {SCRATCH},123\r".encode(),
    f"PRINT PEEK({SCRATCH})\r".encode(),
    b"PRINT TIMER\r",
]
PAUSE = 0.5  # between the two clock readings
TIMER_READING = re.compile(r"PRINT TIMER\n (\d+(?:\.\d+)?) \nREADY\.")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    steps = [(line, 0.05) for line in LINES]
    steps[-1] = (LINES[-1], PAUSE)
    steps.append((b"PRINT TIMER\r", 0.1))
    text, timed_out, returncode = boot(
        sys.argv[1],
        steps,
        wait_for="READY.",
        timeout=20,
        stop_when=[lambda t: len(TIMER_READING.findall(t)) >= 2],
    )
    status = check(
        text,
        [
            "RISCV BASIC V0.1\n32768 BYTES FREE\n\nREADY.",
            "RUN\nHELLO 1 \nHELLO 2 \nHELLO 3 \nREADY.",
            'LIST\n10 FOR I=1 TO 3\n20 PRINT "HELLO";I\n30 NEXT I\nREADY.',
            "PRINT 2^0.5\n 1.41421356 \nREADY.",
            "PRINT 1/3\n 0.333333333 \nREADY.",
            "PRINT ATN(1)*4\n 3.14159265 \nREADY.",
            "PRINT A=RND(1)\n-1 \nREADY.",
            f"PRINT PEEK({SCRATCH})\n 123 \nREADY.",
        ],
        timed_out,
        returncode,
    )
    if status:
        return status
    first, second = (float(x) for x in TIMER_READING.findall(text)[:2])
    if not PAUSE * 0.6 <= second - first <= 5.0:
        print(f"clock readings {first} then {second}: expected about {PAUSE} s apart", file=sys.stderr)
        print(text, file=sys.stderr)
        return 1
    print("basic test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
