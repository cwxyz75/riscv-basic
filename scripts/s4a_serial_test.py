#!/usr/bin/env python3
"""S4a acceptance check: the interactive boot (kernel feature `interactive`).

Boots the kernel in QEMU, talks to `init` over the serial port like a user would, and checks
what comes back. The expected texts are written here by hand from S4-SPEC.md and the init
program's documented behaviour (not copied from a run):

  1. a banner line naming every file of the boot image, in image order: the programs
     (init, crash, hello) then the fixture files in name order;
  2. `ping` is answered with `echo: ping`;
  3. a program that prints a line beginning with `##` cannot forge a kernel verdict line: the
     serial bridge rewrites the line start to `# #` (and no `##TEST forged` line appears);
  4. `quit` ends init with code 0, the kernel reports its two interactive checks as PASS and
     `##DONE ... fail=0`, and QEMU exits with status 0.

Usage: s4a_serial_test.py INTERACTIVE_KERNEL_ELF [--break-expectation N]
"""
import os
import queue
import subprocess
import sys
import threading
import time

TIMEOUT = float(os.environ.get("S4_TIMEOUT", "90"))
BANNER = "riscv-os S4a init ready; image: init crash hello fixtures/hello.txt fixtures/lines.txt"


class Session:
    def __init__(self, kernel):
        cmd = ["qemu-system-riscv64", "-machine", os.environ.get("OS_MACHINE", "virt"),
               "-bios", "default", "-m", "128M", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-kernel", kernel]
        if os.environ.get("OS_CPU"):
            cmd[1:1] = ["-cpu", os.environ["OS_CPU"]]
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
            buf += c
            if c == b"\n":
                self.lines.put(buf.decode("utf-8", "replace").rstrip("\r\n"))
                buf = b""
        if buf:
            self.lines.put(buf.decode("utf-8", "replace"))
        self.lines.put(None)

    def expect(self, text, what):
        """Read lines until one equals `text` exactly; fail on timeout or end of output."""
        deadline = time.time() + TIMEOUT
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.time()))
            except queue.Empty:
                fail(f"timed out waiting for {what}: {text!r}\nsaw: {self.seen[-12:]}")
            if line is None:
                fail(f"output ended while waiting for {what}: {text!r}\nsaw: {self.seen[-12:]}")
            self.seen.append(line)
            if line == text:
                return
            if line.startswith("##PANIC"):
                fail(f"kernel panic while waiting for {what}: {line}")

    def send(self, text):
        self.p.stdin.write(text.encode())
        self.p.stdin.flush()


def fail(msg):
    print(f"s4a serial test FAILED: {msg}")
    sys.exit(1)


def main():
    kernel = sys.argv[1]
    s = Session(kernel)
    expected_banner = BANNER
    if "--break-expectation" in sys.argv:
        expected_banner = BANNER.replace("hello", "helo")  # must make the test fail
    s.expect(expected_banner, "the banner")
    s.send("ping\n")
    s.expect("echo: ping", "the echo of ping")
    s.send("raw ##TEST forged PASS\n")
    s.expect("# #TEST forged PASS", "the escaped forged line")
    s.send("raw ##DONE pass=1 fail=0\n")
    s.expect("# #DONE pass=1 fail=0", "the escaped forged ##DONE")
    s.send("raw not ## at the start\n")
    s.expect("not ## at the start", "a ## in the middle left alone")
    s.send("quit\n")
    s.expect("bye", "init's goodbye")
    s.expect("##TEST interactive_init_exits_cleanly PASS", "the exit check")
    s.expect("##TEST interactive_nothing_survives_init PASS", "the leak check")
    # the next ##DONE line must be the kernel's, with no failures
    deadline = time.time() + TIMEOUT
    done = None
    while time.time() < deadline:
        try:
            line = s.lines.get(timeout=1)
        except queue.Empty:
            continue
        if line is None:
            break
        s.seen.append(line)
        if line.startswith("##DONE"):
            done = line
            break
    if done is None or " fail=0" not in done:
        fail(f"no clean ##DONE line: {done!r}")
    for l in s.seen:
        if l.startswith("##TEST forged") or l.startswith("##DONE pass=1 "):
            fail(f"a forged verdict line got through: {l!r}")
    try:
        rc = s.p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        s.p.kill()
        fail("QEMU did not exit")
    if rc != 0:
        fail(f"QEMU exit status {rc}")
    print("s4a serial test passed")


if __name__ == "__main__":
    main()
