#!/usr/bin/env python3
"""Release smoke test (T48): the kernel built with the `release` feature must boot straight to the
shell with no self-test output, run a BASIC session, and halt cleanly with a one-line verdict.

Usage: release_smoke_test.py RELEASE_KERNEL_ELF [--break-expectation]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import s4e_shell_test as shell  # noqa: E402


def main():
    s = shell.Session(sys.argv[1])
    # nothing before the banner may be a `##` line: the release build runs no boot tests
    while True:
        line = s.next_line("the banner")
        if line == shell.BANNER:
            break
        if line.startswith(b"##"):
            shell.fail(s, f"a test line in the release build: {line!r}")
    step = s.step
    step(b"echo release\n", [b"$ echo release", b"release"], "the shell works")
    step(b"basic\n", [b"$ basic", b"READY."], "basic starts")
    s.wait_for_prompt = False
    expect = b" 99 " if "--break-expectation" in sys.argv else b" 42 "
    step(b"PRINT 6*7\n", [b"PRINT 6*7", expect, b"READY."], "basic evaluates")
    step(b'10 PRINT "RELEASE"\n', [b'10 PRINT "RELEASE"', b"READY."], "a program line")
    step(b"RUN\n", [b"RUN", b"RELEASE", b"READY."], "the program runs")
    step(b"BYE\n", [b"BYE"], "BYE ends basic")
    s.wait_for_prompt = True
    step(b"halt\n", [b"$ halt"], "halt")
    line = s.next_line("the verdict")
    if line != b"system halted":
        shell.fail(s, f"expected 'system halted', got {line!r}")
    try:
        rc = s.p.wait(timeout=30)
    except Exception:
        shell.fail(s, "QEMU did not exit")
    if rc != 0:
        shell.fail(s, f"QEMU exit status {rc}")
    print("release smoke test passed")


if __name__ == "__main__":
    main()
