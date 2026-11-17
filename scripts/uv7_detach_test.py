#!/usr/bin/env python3
"""UV7 acceptance check: detach and reattach, in QEMU (USER-VIEW-SPEC section 9 item 8).

Boots the `repl` kernel and types `detach` at init's test prompt. Init then acts as a terminal backend provider for a session
(`consoled` serving a byte socket it holds): a job runs, the backend closes, a new backend attaches, a hangup ends the session.
Expectations come from the UV7 acceptance text, not from a run:

  * a long job keeps running when the backend closes (and so do the shell and the console server);
  * a new backend attaches and sees the screen state, and its input works (Ctrl-C reaches the job);
  * hangup still ends the job: the session's processes are gone afterwards.

Usage: uv7_detach_test.py REPL_ELF [--break-expectation]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"uv7 detach test FAILED: {msg}")
    print(f"last lines: {[l[:90] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


base.fail = fail


def main():
    s = base.Session(sys.argv[1])
    s.skip_to(base.BANNER, "the banner")
    s.p.stdin.write(b"detach\n")
    s.p.stdin.flush()
    expected = [
        b"detach| attached: a prompt, and a job running",
        b"detach| backend closed: the shell runs, the console server runs, the job runs",
        b"detach| reattached: the screen state is shown again",
        b"detach| the new backend's Ctrl-C reached the job",
        b"detach| the shell answers the new backend",
        b"detach| hangup: acknowledged, the shell ended",
        b"detach| job processes after the hangup: 0",
    ]
    if "--break-expectation" in sys.argv:
        expected[1] = b"detach| backend closed: the shell ended, the console server runs, the job runs"
    for want in expected:
        got = s.next_line("the test's output")
        if got != want:
            fail(s, f"expected {want!r}, got {got!r}")
    s.p.kill()
    print("uv7 detach test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
