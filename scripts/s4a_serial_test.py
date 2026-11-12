#!/usr/bin/env python3
"""S4a acceptance check: the interactive boot (kernel feature `interactive`).

Boots the kernel in QEMU, talks to `init` over the serial port like a user would, and compares
EXACT output (bytes, line by line) with expectations written here by hand from S4-SPEC.md and
the documented behaviour of `init`'s test commands (not copied from a run). After the banner,
every line the system prints must be one a step expects: extra or garbled output fails.

The steps cover: the banner naming the boot image; echo; the forgery guard of the serial bridge
(whole lines, `##` split across writes, after an unterminated line and a kernel-side line,
across the bridge's 256-byte chunk boundary, binary and UTF-8 bytes passing through
unchanged); a write larger than one socket buffer; the read-only boot image rights; a program
that leaves an unterminated line in front of the kernel's own verdict; and a clean finish.

Usage: s4a_serial_test.py INTERACTIVE_KERNEL_ELF [--break-expectation]
The second form must FAIL (it checks that this harness can fail).
"""
import os
import queue
import subprocess
import sys
import threading
import time

TIMEOUT = float(os.environ.get("S4_TIMEOUT", "90"))
BANNER = (b"riscv-os S4a init ready; image: bin/init bin/crash bin/hello bin/launcher bin/memfsd bin/consoled bin/spin bin/echo bin/cat bin/ls bin/probe bin/sh bin/hog bin/bomb bin/ipchog etc/trusted "
          b"home/hello.txt home/lines.txt home/sub/inner.txt")


class Session:
    def __init__(self, kernel):
        cmd = ["qemu-system-riscv64", "-machine", os.environ.get("OS_MACHINE", "virt"),
               "-bios", "default", "-m", "128M", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-kernel", kernel]
        if os.environ.get("OS_CPU"):
            cmd[1:1] = ["-cpu", os.environ["OS_CPU"]]
        if os.environ.get("OS_SMP"):
            cmd[1:1] = ["-smp", os.environ["OS_SMP"]]
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT)
        self.lines = queue.Queue()
        self.seen = []
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        buf = b""
        while True:
            c = self.p.stdout.read(1)
            if not c:
                break
            if c == b"\r":
                continue  # the terminal's CR of CRLF; a bare CR never reaches a golden line
            if c == b"\n":
                self.lines.put(buf)
                buf = b""
            else:
                buf += c
        if buf:
            self.lines.put(buf)
        self.lines.put(None)

    def next_line(self, what):
        try:
            line = self.lines.get(timeout=TIMEOUT)
        except queue.Empty:
            fail(self, f"timed out waiting for {what}")
        if line is None:
            fail(self, f"output ended while waiting for {what}")
        self.seen.append(line)
        return line

    def skip_to(self, text, what):
        """Before the banner the kernel prints its self-test lines: skip them."""
        while True:
            line = self.next_line(what)
            if line == text:
                return
            if line.startswith(b"##PANIC"):
                fail(self, f"kernel panic while waiting for {what}: {line!r}")

    def step(self, send, expect_lines, what):
        """Send bytes; the next lines must be exactly `expect_lines`."""
        self.p.stdin.write(send)
        self.p.stdin.flush()
        for want in expect_lines:
            got = self.next_line(what)
            if got != want:
                fail(self, f"{what}: expected {want[:120]!r} ({len(want)} bytes), "
                           f"got {got[:120]!r} ({len(got)} bytes)")


def fail(session, msg):
    print(f"s4a serial test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


def main():
    s = Session(sys.argv[1])
    banner = BANNER.replace(b"hello", b"helo") if "--break-expectation" in sys.argv else BANNER
    s.skip_to(banner, "the banner")
    s.step(b"ping\n", [b"echo: ping"], "echo")
    s.step(b"raw ##TEST forged PASS\n", [b"# #TEST forged PASS"], "a forged TEST line")
    s.step(b"raw ##DONE pass=1 fail=0\n", [b"# #DONE pass=1 fail=0"], "a forged DONE line")
    s.step(b"raw not ## at the start\n", [b"not ## at the start"], "## inside a line")
    s.step(b"raw #\n", [b"#"], "a single #")
    # an unterminated program line, then a kernel-side line, then a forged verdict
    s.step(b"desync\n", [b"xkernel-side line", b"# #TEST forged PASS"], "desync")
    # a line that crosses the bridge's 256-byte chunk boundary, then ## right after it
    s.step(b"raw " + b"c" * 254 + b"\n", [b"c" * 254], "a 254-byte line")
    s.step(b"raw ##TEST chunk PASS\n", [b"# #TEST chunk PASS"], "## after a long line")
    # bytes pass through unchanged: UTF-8 split across a chunk, and binary
    utf = b"a" * 255 + "é".encode()
    s.step(b"raw " + utf + b"\n", [utf], "UTF-8 across a chunk boundary")
    s.step(b"raw \xff\xfe|\n", [b"\xff\xfe|"], "binary bytes")
    # a write larger than one socket buffer, then the line after it
    s.step(b"big\n", [b"b" * 70000, b"big: Ok(())"], "a 70000-byte write")
    # S4b: programs run through the launcher service. `hello` prints its arguments and copies its
    # (empty) stdin; its exit code is the argument count (here 4: hello a b c).
    s.step(b"run hello a b c\n", [b"args=hello a b c", b"[exit 4]"], "a program run by the launcher")
    s.step(b"run hello\n", [b"args=hello", b"[exit 1]"], "a second run")
    s.step(b"run nosuch\n", [b"run: nosuch: not found"], "an unknown program")
    s.step(b"run home/hello.txt\n", [b"run: home/hello.txt: spawn failed (-107)"], "a text file is not a program")
    s.step(b"run hello after-errors\n", [b"args=hello after-errors", b"[exit 2]"], "the launcher still works")
    # S4c: the filesystem server, through init's `ls`, `cat` and `write` test commands
    s.step(b"ls .\n", [b"bin/", b"etc/", b"home/", b"tmp/"], "the top level")
    s.step(b"ls bin\n", [b"bomb", b"cat", b"consoled", b"crash", b"echo", b"hello", b"hog", b"init", b"ipchog", b"launcher", b"ls", b"memfsd", b"probe", b"sh", b"spin"], "the programs")
    s.step(b"ls home\n", [b"hello.txt", b"lines.txt", b"sub/"], "the home directory")
    s.step(b"cat home/hello.txt\n", [b"hello, world"], "a fixture")
    s.step(b"cat home/sub/inner.txt\n", [b"inner"], "a nested fixture")
    s.step(b"cat etc/trusted\n", [b"ls", b"cat"], "the trust list")
    s.step(b"write tmp/note some text here\n", [b"ok"], "a write to /tmp")
    s.step(b"cat tmp/note\n", [b"some text here"], "reading it back")
    s.step(b"write bin/x hi\n", [b"write: bin/x: access denied"], "writing into /bin")
    s.step(b"write home/hello.txt hi\n", [b"write: home/hello.txt: access denied"], "overwriting a fixture")
    s.step(b"cat nope\n", [b"cat: nope: not found"], "a missing file")
    s.step(b"cat ../etc/trusted\n", [b"cat: ../etc/trusted: bad path"], "a path with ..")
    s.step(b"ls tmp\n", [b"note"], "the writable directory")
    # the boot image handle is read-only
    s.step(b"rights\n", [b"rights: write=-3 mapw=-3 mapr=0 mapx=-3 dup_all=-3 dup_same=0 "
                         b"write_dup=-3"], "the boot image rights")
    # an unterminated program line must not swallow the kernel's verdict
    s.step(b"quitx\n", [b"bye-no-newline",
                        b"##TEST interactive_init_exits_cleanly PASS",
                        b"##TEST interactive_no_input_was_dropped PASS",
                        b"##TEST interactive_nothing_survives_init PASS",
                        b"##TEST interactive_no_kernel_thread_survives PASS"], "the verdict lines")
    done = s.next_line("##DONE")
    if not done.startswith(b"##DONE ") or not done.endswith(b" fail=0"):
        fail(s, f"no clean ##DONE line: {done!r}")
    for l in s.seen:
        if l.startswith(b"##TEST forged") or l.startswith(b"##DONE pass=1 "):
            fail(s, f"a forged verdict line got through: {l!r}")
    try:
        rc = s.p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        fail(s, "QEMU did not exit")
    if rc != 0:
        fail(s, f"QEMU exit status {rc}")
    print("s4a serial test passed")


if __name__ == "__main__":
    main()
