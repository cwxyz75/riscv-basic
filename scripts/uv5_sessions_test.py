#!/usr/bin/env python3
"""UV5 acceptance check: sessions built from profiles, in QEMU.

Boots the `repl` kernel, types `sessions` at init's test prompt; init then runs a script in a session of each profile
(`alice`, `bob`), each on a terminal socket it holds (so the shell is in its non-interactive mode: no prompt, no echo), and
prints what came back, one line per line, prefixed with the profile. EXACT expectations, derived by hand from
USER-VIEW-SPEC section 10 and the profiles in `bootfs/etc/profiles/`:

- a session knows its identity (`USER`, `UID`, `HOME`, and the profile's own `EDITOR`) and starts in `/home`, which is the
  profile's own directory of the image (`homes/<name>`);
- another profile's home is not there: not by `/homes/...`, not by `/home`, and the profiles themselves are not readable;
- the root a session sees has no `homes` and no profiles;
- closing a session ends its job: a program that never ends is gone, and the session's job holds no processes.

Usage: uv5_sessions_test.py REPL_KERNEL_ELF [--break-expectation]
"""
import sys

sys.path.insert(0, __import__("os").path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"uv5 sessions test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


base.fail = fail


def main():
    s = base.Session(sys.argv[1])
    s.skip_to(base.BANNER, "the banner")
    s.step(b"sessions\n", [b"sessions ready"], "starting the sessions")
    expected = [
        b"alice| alice 1001 /home kilo",
        b"alice| /home",
        b"alice| alice's secret",
        b"alice| alice's secret",
        b'alice| 1 ["cat: /homes/bob/notes.txt: not found"]',
        b'alice| 1 ["cat: /etc/profiles/bob.json: not found"]',
        b"alice| 1",
        b"alice| bin/", b"alice| dev/", b"alice| etc/", b"alice| home/", b"alice| include/", b"alice| lib/",
        b"alice| mnt/", b"alice| share/", b"alice| svc/", b"alice| tmp/", b"alice| usr/",
        # /proc lists the session's own programs and nothing else: not the services, not bob's session
        b"alice| PID STATE COMMAND", b"alice| 8 R ps",
        b"alice status 0",
        b"bob| bob 1002",
        b"bob| bob's notes",
        b'bob| 1 ["cat: /homes/alice/secret.txt: not found"]',
        b"bob| 1",
        b"bob| PID STATE COMMAND", b"bob| 5 R ps",
        b"bob status 0",
        b"alice2| started",
        b"alice2| spinning",
        b"job processes while open: 5 or more",
        b"job processes after close: 0",
    ]
    for want in expected:
        got = s.next_line("a session's output")
        if got != want:
            fail(s, f"expected {want!r}, got {got!r}")
    for want in [b"##TEST interactive_init_exits_cleanly PASS",
                 b"##TEST interactive_no_input_was_dropped PASS",
                 b"##TEST interactive_nothing_survives_init PASS",
                 b"##TEST interactive_no_kernel_thread_survives PASS"]:
        got = s.next_line("the verdict lines")
        if got != want:
            fail(s, f"verdict: expected {want!r}, got {got!r}")
    done = s.next_line("##DONE")
    if not done.startswith(b"##DONE ") or not done.endswith(b" fail=0"):
        fail(s, f"no clean ##DONE line: {done!r}")
    try:
        rc = s.p.wait(timeout=30)
    except Exception:
        fail(s, "QEMU did not exit")
    if rc != 0:
        fail(s, f"QEMU exit status {rc}")
    print("uv5 sessions test passed")


if __name__ == "__main__":
    if "--break-expectation" in sys.argv:
        orig = base.Session.step

        def step(self, send, want, what):
            return orig(self, send, [b"sessions not ready"] if what == "starting the sessions" else want, what)

        base.Session.step = step
    main()
