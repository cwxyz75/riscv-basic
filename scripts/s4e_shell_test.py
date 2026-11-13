#!/usr/bin/env python3
"""S4e acceptance check: the shell session (kernel feature `interactive`: init supervises
memfsd, launcher, consoled and sh), driven over the serial port like a user.

Expectations are derived by hand from S4-SPEC.md section 8 (not from a run). The console echoes
what is typed after the prompt, so a command shows up as `$ <command>` on one line; a prompt
with nothing typed yet is unterminated and joins the next thing printed (`$ ^C`).

Usage: s4e_shell_test.py INTERACTIVE_KERNEL_ELF [--break-expectation]
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"s4e shell test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


base.fail = fail


class Session(base.Session):
    """A session that can tell when the shell is waiting at its prompt. The prompt is an
    unterminated line (`$ `), so the line reader cannot see it: keep the partial line visible and
    wait for it before typing. (Typing early is legal, but then the echo of the first characters
    can come before the prompt, and the expected lines would depend on timing.)"""

    def __init__(self, kernel):
        self.partial = b""
        self.wait_for_prompt = True
        super().__init__(kernel)

    def _pump(self):
        buf = b""
        while True:
            c = self.p.stdout.read(1)
            if not c:
                break
            if c == b"\r":
                continue
            if c == b"\n":
                self.lines.put(buf)
                buf = b""
            else:
                buf += c
            self.partial = buf
        if buf:
            self.lines.put(buf)
        self.lines.put(None)

    def prompt(self):
        deadline = time.time() + base.TIMEOUT
        while self.partial != b"$ ":
            if time.time() > deadline:
                fail(self, "timed out waiting for the prompt")
            time.sleep(0.002)

    def step(self, send, want, what):
        if self.wait_for_prompt:
            self.prompt()
        return super().step(send, want, what)

BANNER = b"riscv-os S4 ready"
BIN = [b"basic", b"blkd", b"bomb", b"cat", b"consoled", b"crash", b"ctest", b"devmgr", b"echo", b"hello", b"hog", b"init", b"ipchog", b"launcher",
       b"ls", b"lua", b"memfsd", b"probe", b"sh", b"spin"]


def main():
    s = Session(sys.argv[1])
    s.skip_to(BANNER, "the banner")
    step = s.step

    # builtins
    step(b"help\n", [b"$ help", b"builtins: cd pwd exit help halt",
                     b"programs: ls cat echo probe spin hog bomb ipchog"], "help")
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

    # BASIC (S7a): the home computer's interpreter as an ordinary program on the terminal
    step(b"basic\n", [b"$ basic", b"READY."], "basic starts and prompts")
    s.wait_for_prompt = False  # basic has no "$ " prompt: the steps below talk to BASIC
    step(b"PRINT 6*7\n", [b"PRINT 6*7", b" 42 ", b"READY."], "basic evaluates an immediate statement")
    step(b'10 PRINT "FROM A PROGRAM"\n', [b'10 PRINT "FROM A PROGRAM"', b"READY."], "basic stores a program line")
    step(b"RUN\n", [b"RUN", b"FROM A PROGRAM", b"READY."], "basic runs it")
    step(b"PRINT 2+2\n", [b"PRINT 2+2", b" 4 ", b"READY."], "basic stays alive")
    step(b"\x04", [], "Ctrl-D at an empty line ends basic")
    s.prompt()  # (exit 0 prints nothing: the shell is simply back at its prompt)
    step(b"basic\n", [b"$ basic", b"READY."], "basic again")
    s.wait_for_prompt = False
    step(b"BYE\n", [b"BYE"], "BYE ends basic")
    s.prompt()
    s.wait_for_prompt = True
    step(b"echo back at the shell\n", [b"$ echo back at the shell", b"back at the shell"],
         "the shell after basic")

    # Lua (S7d): the upstream interpreter's read-eval-print loop on the terminal
    s.wait_for_prompt = True
    step(b"lua\n", [b"$ lua", b"Lua 5.4.8  Copyright (C) 1994-2025 Lua.org, PUC-Rio"], "lua starts")
    s.wait_for_prompt = False  # lua's own prompt is "> "
    step(b"print(6*7)\n", [b"> print(6*7)", b"42"], "lua evaluates a statement")
    step(b"x = 5\n", [b"> x = 5"], "lua assigns")
    step(b"x * 2\n", [b"> x * 2", b"10"], "lua prints the value of an expression")
    step(b"for i=1,3 do io.write(i, ' ') end print()\n",
         [b"> for i=1,3 do io.write(i, ' ') end print()", b"1 2 3 "], "lua runs a loop")
    step(b"error('oops')\n", [b"> error('oops')", b"stdin:1: oops", b"stack traceback:",
                              b"\t[C]: in function 'error'", b"\tstdin:1: in main chunk", b"\t[C]: in ?"],
         "lua reports an error and carries on")
    step(b"print(x)\n", [b"> print(x)", b"5"], "lua kept its state")
    step(b"\x04", [b"> "], "Ctrl-D at an empty line ends lua")
    s.prompt()
    s.wait_for_prompt = True
    step(b"echo back from lua\n", [b"$ echo back from lua", b"back from lua"], "the shell after lua")

    # confinement: `probe` has only a read-only working directory
    step(b"probe\n", [b"$ probe", b"open /etc/trusted: -108", b"open ../x: -108",
                      b"write ./f: -3", b"clone: ok", b"clone all: -3", b"root: absent",
                      b"spawn: -5"],
         "probe: nothing outside the working directory")

    # limits (S4.5): a program is the only process of its job, runs at most four threads, and
    # has a bounded amount of memory, whatever it tries
    step(b"bomb\n", [b"$ bomb", b"bomb: process_create: -5", b"bomb: threads: 3 then -5"],
         "a process and thread bomb is refused")
    counts = []
    for attempt in (1, 2):
        step(b"hog\n", [b"$ hog"], f"a memory hog, run {attempt}")
        mib = 0
        while True:
            line = s.next_line("the hog's output")
            if line == f"hog: refused at {mib} MiB".encode():
                break
            mib += 1
            if line != f"hog: {mib} MiB".encode():
                fail(s, f"hog output: expected 'hog: {mib} MiB', got {line!r}")
        line = s.next_line("the hog's exit")
        if line != b"[exit 7]":
            fail(s, f"the hog ended with {line!r}, wanted [exit 7]")
        counts.append(mib)
    # the 4096-page job holds the program's image and stack too: about 15 MiB, never more than 16
    if not 8 <= counts[0] <= 16:
        fail(s, f"the hog got {counts[0]} MiB of a 16 MiB job")
    if counts[0] != counts[1]:
        fail(s, f"the second hog got {counts[1]} MiB, the first {counts[0]}: memory was not given back")
    step(b"echo survived\n", [b"$ echo survived", b"survived"], "the shell after the hog")

    # quotas on kernel objects and queued data (S4.5c): the hog is stopped by its own job, twice
    # with identical results, and the pipes of the shell keep working around it
    results = []
    for attempt in (1, 2):
        step(b"ipchog\n", [b"$ ipchog", b"ipchog: queued 192 KiB"], f"an ipc and object hog, run {attempt}")
        line = s.next_line("the hog's event count")
        m = re.fullmatch(rb"ipchog: made (\d+) events", line)
        if not m:
            fail(s, f"ipchog: expected the event count, got {line!r}")
        results.append(int(m.group(1)))
        step(b"echo through | cat | cat\n", [b"$ echo through | cat | cat", b"through"],
             "pipes work right after the hog")
    if not 180 <= results[0] <= 250:
        fail(s, f"the hog made {results[0]} events of a 256-object job")
    if results[0] != results[1]:
        fail(s, f"second run {results[1]} events, first {results[0]}: objects were not given back")

    # Ctrl-C
    step(b"spin\n", [b"$ spin", b"spinning"], "a running job")
    s.wait_for_prompt = False  # a job is running: there is no prompt until it is gone
    step(b"\x03", [b"^C", b"[exit -130]"], "Ctrl-C kills the job")
    s.wait_for_prompt = True
    step(b"echo after\n", [b"$ echo after", b"after"], "one prompt after the kill (no extra prompt)")
    step(b"\x03", [b"$ ^C"], "Ctrl-C at an idle prompt")
    step(b"echo idle\n", [b"$ echo idle", b"idle"], "the shell after an idle Ctrl-C")
    # a Ctrl-C typed together with the command, before the job is even registered, still ends it
    # (the job may or may not have printed its first line yet)
    step(b"spin\n\x03", [b"$ spin"], "a command and Ctrl-C in one write")
    s.wait_for_prompt = False
    while True:
        line = s.next_line("the early Ctrl-C")
        if line == b"spinning":
            continue
        if line != b"^C":
            fail(s, f"early Ctrl-C: expected ^C, got {line!r}")
        break
    line = s.next_line("the early Ctrl-C report")
    if line == b"spinning":  # (printed between the echo and the report)
        line = s.next_line("the early Ctrl-C report")
    if line != b"[exit -130]":
        fail(s, f"early Ctrl-C: expected [exit -130], got {line!r}")
    s.wait_for_prompt = True
    step(b"echo calm\n", [b"$ echo calm", b"calm"], "one prompt after an early Ctrl-C")
    step(b"spin | cat\n", [b"$ spin | cat", b"spinning"], "a running pipeline")
    s.wait_for_prompt = False
    step(b"\x03", [b"^C", b"[exit -130]", b"[exit -130]"], "Ctrl-C kills every stage")
    s.wait_for_prompt = True
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
