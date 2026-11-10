#!/usr/bin/env python3
"""Console boot test (milestone M1, driven through BASIC since M4): type
lines at the serial prompt and check the echo and what the interpreter sees.

Usage: console_test.py KERNEL_ELF

The BASIC prompt `READY.` is the sync point. Each step types a line whose
effect on the line editor shows up in what BASIC prints back.
"""
import sys

from boot_test import boot, check

BS_SP_BS = "\x08 \x08"  # how a terminal is told to rub out one character

# (bytes typed, text that must then appear in the output)
STEPS = [
    # Backspace removes the previous character, and the terminal is told to erase it.
    ('PRINT "AB\x7fC"\r', 'PRINT "AB' + BS_SP_BS + 'C"\nAC\nREADY.'),
    # Ctrl-U erases the whole line, one erase sequence per character.
    ("PRINT 1\x15PRINT 2\r", "PRINT 1" + BS_SP_BS * 7 + "PRINT 2\n 2 \nREADY."),
    # Ctrl-C abandons the line: echoes ^C and returns to the prompt.
    ("PRINT 9\x03", "PRINT 9^C\nREADY."),
    # An arrow-key escape sequence (ESC [ A) is swallowed, not typed.
    ("\x1b[APRINT 3\r", "READY.\nPRINT 3\n 3 \nREADY."),
    # The line holds at most 255 characters ("PRINT " + quote is 7): 248 a's get through.
    ('PRINT "' + "a" * 300 + "\r", "a" * 248 + "\nREADY."),
    # An empty line is just a new prompt.
    ("\r", "READY.\n\nREADY."),
    # A CR LF pair is one Enter, not two.
    ("PRINT 4\r\nPRINT 5\r", "PRINT 4\n 4 \nREADY.\nPRINT 5\n 5 \nREADY."),
    # Backspace on an empty line does nothing (and must not echo an erase).
    ("\x7f\x7fPRINT 6\r", "READY.\nPRINT 6\n 6 \nREADY."),
    # Characters outside ASCII are ignored.
    ("PRINT 7\xe9\r", "PRINT 7\n 7 \nREADY."),
]


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    typed = "".join(step[0] for step in STEPS)
    expects = ["hello from S-mode", "RISCV BASIC V0.1\n32768 BYTES FREE\n\nREADY."]
    expects += [step[1] for step in STEPS]
    text, timed_out, returncode = boot(
        sys.argv[1],
        typed.encode("latin-1"),
        wait_for="READY.",
        stop_when=[STEPS[-1][1]],
    )
    # The 255-character limit: a 249th character must not have been accepted.
    return check(text, expects, timed_out, returncode, forbidden=["a" * 249])


if __name__ == "__main__":
    sys.exit(main())
