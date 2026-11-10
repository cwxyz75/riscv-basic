#!/usr/bin/env python3
"""Milestone M8 boot test: the demo programs, typed in over the serial port
and run on the real machine.

Usage: demo_test.py KERNEL_ELF

Each demo is the program from `examples/` typed line by line, followed by a
scripted game. The random parts are pinned by typing a replacement line after
the program (the same trick the host golden tests use).
"""
import pathlib
import sys

from boot_test import boot, check

EXAMPLES = pathlib.Path(__file__).resolve().parent.parent / "examples"


def program(name: str) -> list:
    return [line.encode() + b"\r" for line in (EXAMPLES / f"{name}.bas").read_text().splitlines()]


def lines(*texts: str) -> list:
    return [t.encode() + b"\r" for t in texts]


def run(kernel: str, label: str, typed: list, expects: list) -> int:
    steps = [(b"".join(typed), 0.0)]
    text, timed_out, returncode = boot(
        kernel, steps, wait_for="READY.", timeout=60, stop_when=[expects[-1]]
    )
    status = check(text, expects, timed_out, returncode)
    print(f"demo {label}: {'ok' if status == 0 else 'FAILED'}")
    return status


def b(k: int) -> str:
    if k == 0:
        return "NO MORE BOTTLES"
    return f"{k} BOTTLE" + ("S" if k > 1 else "")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    kernel = sys.argv[1]
    failures = 0

    failures += run(
        kernel,
        "99 bottles",
        program("bottles") + lines("RUN"),
        [
            f"{b(99)} OF BEER ON THE WALL, {b(99)} OF BEER.\n"
            f"TAKE ONE DOWN AND PASS IT AROUND, {b(98)} OF BEER ON THE WALL.\n\n",
            f"{b(1)} OF BEER ON THE WALL, {b(1)} OF BEER.\n"
            f"TAKE ONE DOWN AND PASS IT AROUND, {b(0)} OF BEER ON THE WALL.\n\nREADY.",
        ],
    )

    failures += run(
        kernel,
        "guess the number",
        program("guess")
        + lines("30 N=42", "RUN", "50", "25", "37", "43", "40", "41", "42"),
        ["TOO HIGH", "TOO LOW", "YOU GOT IT IN 7 GUESSES!\nNICE WORK.\nREADY."],
    )

    failures += run(
        kernel,
        "hunt the wumpus",
        program("wumpus")
        + lines("985 P=1: W=18: X1=7: X2=12: B1=3: B2=16", "RUN", "M", "8", "M", "9", "S", "18"),
        [
            "HUNT THE WUMPUS\nYOU ARE IN ROOM 1 \n",
            "I FEEL A DRAFT.",
            "I SMELL A WUMPUS!",
            "AHA! YOU GOT THE WUMPUS!\nHEE HEE HEE - THE WUMPUS'LL GETCHA NEXT TIME!!\nREADY.",
        ],
    )

    failures += run(
        kernel,
        "wumpus placement under the real RNG",
        program("wumpus")
        + lines(
            "2000 FOR T=1 TO 100",
            "2010 GOSUB 900",
            '2020 FOR I=1 TO 6: FOR J=I+1 TO 6: IF L(I)=L(J) THEN PRINT "DUP": END',
            "2030 NEXT J: NEXT I",
            "2060 NEXT T",
            '2070 PRINT "OK"',
            "RUN 2000",
        ),
        ["RUN 2000\nOK\nREADY."],
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
