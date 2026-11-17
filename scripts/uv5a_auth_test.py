#!/usr/bin/env python3
"""UV5a acceptance check: logging in through `authd`, in QEMU, over the serial line (USER-VIEW-SPEC section 10.1).

Three kernels, all `interactive`, differing only in the boot image (the `OS_BOOTFS_OVERLAY` test overlays under tests/uv5a/):

  empty  no profile on the volume: only the setup session starts; it creates one profile and one credential, then it is gone.
  seed   alice and bob with seed credentials (made by Python's hashlib.scrypt, scripts/uv5a_make_creds.py); alice is marked
         `autologin` but the manifest does not enable it: a login is required. Wrong passwords yield no session, right ones a
         session for that profile only. Run twice: an ordinary boot refuses the console's recovery command, and a boot with
         `recovery` on the kernel command line honours it.

Expectations are written from USER-VIEW-SPEC 10.1 and the UV5a acceptance text, not taken from a run.

Usage: uv5a_auth_test.py EMPTY_ELF SEED_ELF [--break-expectation]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402
import s4e_shell_test as s4e  # noqa: E402

SHELL_BANNER = b"riscv-os S4 ready"


def fail(session, msg):
    print(f"uv5a auth test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    print(f"partial line: {getattr(session, 'partial', None)!r}")
    session.p.kill()
    sys.exit(1)


base.fail = fail
s4e.fail = fail


class Session(s4e.Session):
    def __init__(self, kernel, extra=()):
        base.Session.EXTRA_ARGS = list(extra)
        super().__init__(kernel)
        self.wait_for_prompt = False  # prompts here are not the shell's

    def at(self, prompt, what):
        """Wait for an unterminated prompt (the line is not complete until the user answers)."""
        deadline = time.time() + base.TIMEOUT
        while not self.partial.endswith(prompt):
            if time.time() > deadline:
                fail(self, f"timed out waiting for the prompt {prompt!r} ({what})")
            time.sleep(0.002)

    def drain(self):
        """Drop what came before the first prompt (the kernel's self-test lines)."""
        while True:
            try:
                self.lines.get_nowait()
            except Exception:
                return

    def say(self, text):
        self.p.stdin.write(text)
        self.p.stdin.flush()

    def lines_are(self, want, what):
        for w in want:
            got = self.next_line(what)
            if got != w:
                fail(self, f"{what}: expected {w!r}, got {got!r}")

    def shell_ready(self, what):
        self.skip_to(SHELL_BANNER, what)
        self.prompt()

    def command(self, text, want, what):
        self.prompt()
        self.say(text + b"\n")
        self.lines_are([b"$ " + text] + want, what)

    def logout(self, what):
        self.command(b"exit", [], what)


def login(s, name, password, what):
    s.at(b"login: ", what)
    s.say(name + b"\n")
    s.lines_are([b"login: " + name], what)
    s.at(b"password: ", what)
    s.say(password + b"\n")
    s.lines_are([b"password: "], what)  # (the password is not echoed)


def refused_login(s, name, password, what):
    login(s, name, password, what)
    s.lines_are([b"login incorrect"], what)


def run_empty(elf):
    s = Session(elf)
    s.skip_to(b"setup: this system has no users yet", "the setup session")
    s.at(b"new user name: ", "the setup prompt")
    s.say(b"Bad Name\n")
    s.lines_are([b"new user name: Bad Name", b"that name will not do (lower case letters, digits, - and _)"], "a bad name")
    s.at(b"new user name: ", "the setup prompt again")
    s.say(b"carol\n")
    s.lines_are([b"new user name: carol"], "a good name")
    s.at(b"password: ", "the password")
    s.say(b"carol-pw\n")
    s.lines_are([b"password: ", b"setup: done"], "the credential")
    s.shell_ready("carol's session")
    s.command(b"print $env.USER", [b"carol"], "the first profile is carol")
    s.logout("log out of the setup profile")
    # the setup session is gone: the next prompt is a login, and the credential works
    s.at(b"login: ", "the login after setup")
    s.say(b"!reset\n")
    s.lines_are([b"login: !reset", b"recovery refused: it needs the console's recovery request at boot"], "no recovery on an ordinary boot")
    refused_login(s, b"carol", b"wrong", "a wrong password")
    refused_login(s, b"mallory", b"carol-pw", "an unknown user")
    login(s, b"carol", b"carol-pw", "the right password")
    s.shell_ready("carol again")
    s.command(b"print $env.USER", [b"carol"], "a session for carol")
    s.p.kill()


def run_seed(elf, recovery):
    s = Session(elf, ["-append", "recovery"] if recovery else [])
    # alice is marked autologin, but this manifest does not enable it: a login is required
    s.at(b"login: ", "a login prompt, not an autologin")
    time.sleep(0.2)
    s.drain()
    s.say(b"\n")
    s.lines_are([b"login: "], "an empty name")
    refused_login(s, b"alice", b"bob-pass", "alice with bob's password")
    refused_login(s, b"alice", b"", "alice with no password")
    refused_login(s, b"carol", b"alice-pass", "an unknown user")
    login(s, b"bob", b"bob-pass", "bob")
    s.shell_ready("bob's session")
    s.command(b"print $env.USER $env.UID", [b"bob 1002"], "a session for bob only")
    s.command(b"cat /homes/alice/secret.txt", [b"cat: /homes/alice/secret.txt: not found", b"[exit 1]"], "bob cannot see alice's home")
    s.logout("log out of bob's session")
    if not recovery:
        s.at(b"login: ", "the login")
        s.say(b"!reset\n")
        s.lines_are([b"login: !reset", b"recovery refused: it needs the console's recovery request at boot"], "no recovery on an ordinary boot")
        login(s, b"alice", b"alice-pass", "alice")
        s.shell_ready("alice's session")
        s.command(b"print $env.USER $env.UID", [b"alice 1001"], "a session for alice only")
    else:
        s.at(b"login: ", "the login")
        s.say(b"!reset\n")
        s.at(b"reset which user: ", "the recovery prompt")
        s.say(b"alice\n")
        s.lines_are([b"login: !reset", b"reset which user: alice"], "recovery")
        s.at(b"new password: ", "the new password")
        s.say(b"fresh-pass\n")
        s.lines_are([b"new password: ", b"credential reset"], "the reset")
        refused_login(s, b"alice", b"alice-pass", "the old password no longer works")
        login(s, b"alice", b"fresh-pass", "the new password")
        s.shell_ready("alice's session")
        s.command(b"print $env.USER $env.UID", [b"alice 1001"], "a session for alice after recovery")
    s.p.kill()


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    empty, seed = sys.argv[1], sys.argv[2]
    if "--break-expectation" in sys.argv:
        s = Session(seed)
        s.at(b"login: ", "a login prompt")
        s.say(b"alice\n")
        s.lines_are([b"login: alicia"], "a broken expectation")  # must fail
        return 0
    run_empty(empty)
    run_seed(seed, recovery=False)
    run_seed(seed, recovery=True)
    print("uv5a auth test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
