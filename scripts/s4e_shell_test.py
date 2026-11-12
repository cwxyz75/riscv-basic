#!/usr/bin/env python3
"""S4e acceptance check: the shell session (kernel feature `interactive`: init supervises
memfsd, launcher, consoled and sh), driven over the serial port like a user.

Expectations are derived by hand from S4-SPEC.md section 8 (not from a run). The console echoes
what is typed after the prompt, so a command shows up as `$ <command>` on one line; a prompt
with nothing typed yet is unterminated and joins the next thing printed (`$ ^C`).

Usage: s4e_shell_test.py INTERACTIVE_KERNEL_ELF [--break-expectation]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"s4e shell test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


base.fail = fail

BANNER = b"riscv-os S4 ready"
BIN = [b"cat", b"consoled", b"crash", b"echo", b"hello", b"init", b"launcher", b"ls",
       b"memfsd", b"probe", b"sh", b"spin"]


def main():
    s = base.Session(sys.argv[1])
    s.skip_to(BANNER, "the banner")
    step = s.step

    # builtins
    step(b"help\n", [b"$ help", b"builtins: cd pwd exit help halt",
                     b"programs: ls cat echo probe spin"], "help")
    step(b"pwd\n", [b"$ pwd", b"/home"], "pwd at start")
    step(b"\n", [b"$ "], "an empty line")
    step(b"# only a comment\n", [b"$ # only a comment"], "a comment")

    # programs on the fixtures
    step(b"ls\n", [b"$ ls", b"hello.txt", b"lines.txt", b"sub/"], "ls of the working directory")
    step(b"ls sub\n", [b"$ ls sub", b"inner.txt"], "ls of a subdirectory")
    step(b"ls /\n", [b"$ ls /", b"bin/", b"etc/", b"home/", b"tmp/"], "ls of the root (trusted)")
    step(b"ls /bin\n", [b"$ ls /bin"] + BIN, "ls of /bin: sorted bytewise")
    step(b"cat hello.txt\n", [b"$ cat hello.txt", b"hello, world"], "cat")
    step(b"cat lines.txt sub/inner.txt\n",
         [b"$ cat lines.txt sub/inner.txt", b"one", b"two", b"three", b"inner"], "cat of two files")
    step(b"cat /etc/trusted\n", [b"$ cat /etc/trusted", b"ls", b"cat"], "the trust list")
    step(b"echo a   'b  c' \"d\"\n", [b"$ echo a   'b  c' \"d\"", b"a b  c d"], "echo and quoting")
    step(b"echo\n", [b"$ echo", b""], "echo with no arguments")

    # errors of programs, with their exit codes
    step(b"cat nope\n", [b"$ cat nope", b"cat: nope: not found", b"[exit 1]"], "cat: missing file")
    step(b"cat sub\n", [b"$ cat sub", b"cat: sub: not a file", b"[exit 1]"], "cat: a directory")
    step(b"cat ../x\n", [b"$ cat ../x", b"cat: ../x: bad path", b"[exit 1]"], "cat: a path with ..")
    step(b"ls nope\n", [b"$ ls nope", b"ls: nope: not found", b"[exit 1]"], "ls: missing directory")
    step(b"ls hello.txt\n", [b"$ ls hello.txt", b"ls: hello.txt: not a directory", b"[exit 1]"],
         "ls: a file")

    # pipelines
    step(b"echo hi | cat\n", [b"$ echo hi | cat", b"hi"], "a two-stage pipeline")
    step(b"echo one|cat|cat\n", [b"$ echo one|cat|cat", b"one"], "a three-stage pipeline")
    step(b"ls sub | cat | cat\n", [b"$ ls sub | cat | cat", b"inner.txt"], "ls into cat")
    step(b"cat nope | cat\n", [b"$ cat nope | cat", b"cat: nope: not found", b"[exit 1]"],
         "a failing first stage is reported")

    # cd and pwd are lexical
    step(b"cd sub\n", [b"$ cd sub"], "cd")
    step(b"pwd\n", [b"$ pwd", b"/home/sub"], "pwd after cd")
    step(b"ls\n", [b"$ ls", b"inner.txt"], "ls in the new directory")
    step(b"cat inner.txt\n", [b"$ cat inner.txt", b"inner"], "cat in the new directory")
    step(b"cd ../..\n", [b"$ cd ../.."], "cd to the root")
    step(b"pwd\n", [b"$ pwd", b"/"], "pwd at the root")
    step(b"cd ..\n", [b"$ cd ..", b"cd: ..: not found"], "cd above the root")
    step(b"cd nope\n", [b"$ cd nope", b"cd: nope: not found"], "cd to a missing directory")
    step(b"cd home/hello.txt\n", [b"$ cd home/hello.txt", b"cd: home/hello.txt: not a directory"],
         "cd to a file")
    step(b"cd /home/sub\n", [b"$ cd /home/sub"], "cd with an absolute path")
    step(b"pwd\n", [b"$ pwd", b"/home/sub"], "pwd after an absolute cd")
    step(b"cd\n", [b"$ cd", b"cd: usage: cd <dir>"], "cd without an argument")
    step(b"cd /home\n", [b"$ cd /home"], "back to /home")

    # shell errors
    step(b"frob\n", [b"$ frob", b"sh: frob: not found"], "an unknown command")
    step(b"bin/ls\n", [b"$ bin/ls", b"sh: bin/ls: bad command name"], "a command name with a slash")
    step(b"echo 'x\n", [b"$ echo 'x", b"sh: unterminated quote"], "an unterminated quote")
    step(b"| ls\n", [b"$ | ls", b"sh: empty command in pipeline"], "an empty stage")
    step(b"ls | cd sub\n", [b"$ ls | cd sub", b"sh: cd: a builtin cannot be piped"], "a piped builtin")
    step(b"echo ok | frob\n", [b"$ echo ok | frob", b"sh: frob: not found"], "an unknown stage")
    step(b"echo still alive\n", [b"$ echo still alive", b"still alive"], "the shell after errors")

    # confinement: `probe` has only a read-only working directory
    step(b"probe\n", [b"$ probe", b"open /etc/trusted: -108", b"open ../x: -108",
                      b"write ./f: -3", b"clone: ok", b"clone all: -3", b"spawn: -1"],
         "probe: nothing outside the working directory")

    # Ctrl-C
    step(b"spin\n", [b"$ spin", b"spinning"], "a running job")
    step(b"\x03", [b"^C", b"[exit -130]"], "Ctrl-C kills the job")
    step(b"echo after\n", [b"$ echo after", b"after"], "one prompt after the kill (no extra prompt)")
    step(b"\x03", [b"$ ^C"], "Ctrl-C at an idle prompt")
    step(b"echo idle\n", [b"$ echo idle", b"idle"], "the shell after an idle Ctrl-C")
    step(b"spin | cat\n", [b"$ spin | cat", b"spinning"], "a running pipeline")
    step(b"\x03", [b"^C", b"[exit -130]", b"[exit -130]"], "Ctrl-C kills every stage")
    step(b"echo again\n", [b"$ echo again", b"again"], "the shell after a pipeline kill")

    # end of input and exit: the shell ends and a fresh session starts
    step(b"\x04", [b"$ " + BANNER], "Ctrl-D ends the shell; init starts a new one")
    step(b"pwd\n", [b"$ pwd", b"/home"], "a fresh session starts in /home")
    step(b"exit\n", [b"$ exit"], "exit")
    line = s.next_line("the banner after exit")
    if line != BANNER:  # (the echoed `exit` ended the prompt's line)
        fail(s, f"after exit: expected a new banner, got {line!r}")
    step(b"echo fresh\n", [b"$ echo fresh", b"fresh"], "the new session works")

    # halt: init ends the session and the kernel checks for leaks
    step(b"halt\n", [b"$ halt"], "halt")
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
    print("s4e shell test passed")


if __name__ == "__main__":
    if "--break-expectation" in sys.argv:
        orig = base.Session.step

        def step(self, send, want, what):
            if what == "pwd after cd":
                want = [want[0], b"/home/sob"]
            return orig(self, send, want, what)

        base.Session.step = step
    main()
