#!/usr/bin/env python3
"""Boot the kernel in QEMU non-interactively and check the serial output.

Command line:
    boot_test.py KERNEL_ELF --expect "text" [--expect "more"] [--stdin FILE] [--timeout 10]

Exit status 0 if every --expect string appears in the output, 1 otherwise.
QEMU is killed after --timeout seconds if it has not exited on its own (the
kernel shuts down via SBI when it has nothing left to do, but an interactive
BASIC session never will, so a timeout is a normal way for a test to end).

As a module (see console_test.py), `boot()` runs QEMU with bytes on the serial
port and `check()` reports missing strings. Output is compared with CRLF
normalised to LF.
"""
import argparse
import subprocess
import sys
import threading
import time


def boot(
    kernel: str,
    stdin_data=b"",
    timeout: float = 10.0,
    qemu: str = "qemu-system-riscv64",
    wait_for: str = "",
    stop_when=(),
):
    """Boot `kernel`, send `stdin_data` to the serial port, return (text, timed_out, returncode).

    `stdin_data` is either bytes, or a list of (bytes, pause_seconds) steps sent
    one after another with a pause after each (to let time pass between inputs).

    If `wait_for` is given, the input is sent only once that text has appeared in
    the output (so it is not typed into a machine that is still booting, where
    the firmware may swallow it); otherwise it is sent immediately.

    If `stop_when` is given (strings, or predicates taking the output so far),
    QEMU is killed as soon as every one holds (for an interactive kernel that
    never exits on its own); this is not reported as a timeout.
    """
    cmd = [
        qemu,
        "-machine", "virt",
        "-bios", "default",
        "-display", "none",
        "-serial", "stdio",
        "-monitor", "none",
        "-kernel", kernel,
    ]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    captured = bytearray()

    def reader():
        while True:
            chunk = proc.stdout.read1(4096)
            if not chunk:
                return
            captured.extend(chunk)

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()

    deadline = time.monotonic() + timeout
    marker = wait_for.encode()
    while marker and marker not in captured and proc.poll() is None and time.monotonic() < deadline:
        time.sleep(0.02)
    steps = [(stdin_data, 0.0)] if isinstance(stdin_data, (bytes, bytearray)) else list(stdin_data)
    for chunk, pause in steps:
        if proc.poll() is not None:
            break
        try:
            proc.stdin.write(chunk)
            proc.stdin.flush()
        except BrokenPipeError:
            break
        end = time.monotonic() + pause
        while time.monotonic() < end and proc.poll() is None:
            time.sleep(0.02)
    timed_out = False
    while proc.poll() is None:
        if stop_when:
            seen = bytes(captured).decode("utf-8", errors="replace").replace("\r\n", "\n")
            if all(w(seen) if callable(w) else w in seen for w in stop_when):
                proc.kill()
                break
        if time.monotonic() >= deadline:
            timed_out = True
            proc.kill()
            break
        time.sleep(0.02)
    proc.wait()
    thread.join(timeout=2)
    text = bytes(captured).decode("utf-8", errors="replace").replace("\r\n", "\n")
    return text, timed_out, proc.returncode


def check(text: str, expects: list, timed_out: bool, returncode, forbidden=()) -> int:
    """Print a verdict; return 0 if every expected string is in `text` and no
    forbidden string is, else 1."""
    missing = [e for e in expects if e not in text]
    present = [f for f in forbidden if f in text]
    if missing or present:
        print("boot test FAILED; missing:", missing, "| unexpectedly present:", present, file=sys.stderr)
        print("timed out:", timed_out, "| exit status:", returncode, file=sys.stderr)
        print("--- captured output ---", file=sys.stderr)
        print(text, file=sys.stderr)
        return 1
    print("boot test passed" + (" (QEMU killed at timeout)" if timed_out else ""))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("kernel")
    ap.add_argument("--expect", action="append", default=[], help="substring that must appear")
    ap.add_argument("--stdin", help="file whose contents are sent to the serial port")
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--qemu", default="qemu-system-riscv64")
    args = ap.parse_args()

    stdin_data = b""
    if args.stdin:
        with open(args.stdin, "rb") as f:
            stdin_data = f.read()

    text, timed_out, returncode = boot(args.kernel, stdin_data, args.timeout, args.qemu)
    return check(text, args.expect, timed_out, returncode)


if __name__ == "__main__":
    sys.exit(main())
