#!/usr/bin/env python3
"""Milestone M1 boot test: type a scripted session at the serial console and
check what the kernel echoes back and what it receives.

Usage: console_test.py KERNEL_ELF

The kernel is expected to show a `> ` prompt, echo what is typed (with the
classic `BS SP BS` erase sequence for rubbed-out characters), and answer each
finished line with `you typed: [LINE] (N chars)`. It shuts down on `exit`.
"""
import sys

from boot_test import boot, check

BS_SP_BS = "\x08 \x08"

# (bytes typed, text that must then appear in the output)
STEPS = [
    # The banner is printed through the ANSI helpers: green text, then reset.
    ("", "\x1b[32mconsole ready; type a line, or `exit` to power off\x1b[0m\n"),
    # Backspace removes the previous character, and the terminal is told to erase it.
    ("AB\x7fC\r", "> AB" + BS_SP_BS + "C\nyou typed: [AC] (2 chars)"),
    # Ctrl-U erases the whole line, one erase sequence per character.
    ("xyz\x15Q\r", "xyz" + BS_SP_BS * 3 + "Q\nyou typed: [Q] (1 chars)"),
    # Ctrl-C abandons the line: echoes ^C and reports a break.
    ("abc\x03", "abc^C\n(break)"),
    # An arrow-key escape sequence (ESC [ A) is swallowed, not typed.
    ("\x1b[Aok\r", "you typed: [ok] (2 chars)"),
    # The line holds at most 255 characters; the rest is ignored.
    ("a" * 300 + "\r", "(255 chars)"),
    # An empty line is a line.
    ("\r", "you typed: [] (0 chars)"),
    # A CR LF pair is one Enter, not two.
    ("x\r\ny\r", "you typed: [x] (1 chars)\n> y\nyou typed: [y] (1 chars)"),
    # Backspace on an empty line does nothing (and must not echo an erase).
    ("\x7f\x7fz\r", "> z\nyou typed: [z]"),
    # Characters outside ASCII are ignored.
    ("a\xe9b\r", "you typed: [ab] (2 chars)"),
    # Shut down cleanly.
    ("exit\r", "bye"),
]


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    typed = "".join(step[0] for step in STEPS)
    text, timed_out, returncode = boot(sys.argv[1], typed.encode("latin-1"), wait_for="console ready")
    expects = ["hello from S-mode"] + [step[1] for step in STEPS]
    return check(text, expects, timed_out, returncode)


if __name__ == "__main__":
    sys.exit(main())
