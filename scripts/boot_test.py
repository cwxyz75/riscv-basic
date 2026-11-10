#!/usr/bin/env python3
"""Boot the kernel in QEMU non-interactively and check the serial output.

Usage:
    boot_test.py KERNEL_ELF --expect "text" [--expect "more"] [--stdin FILE] [--timeout 10]

Exit status 0 if every --expect string appears in the output, 1 otherwise.
QEMU is killed after --timeout seconds if it has not exited on its own (the
kernel shuts down via SBI when it has nothing left to do, but an interactive
BASIC session never will, so a timeout is a normal way for a test to end).
"""
import argparse
import subprocess
import sys


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

    cmd = [
        args.qemu,
        "-machine", "virt",
        "-bios", "default",
        "-display", "none",
        "-serial", "stdio",
        "-monitor", "none",
        "-kernel", args.kernel,
    ]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    timed_out = False
    try:
        out, _ = proc.communicate(input=stdin_data, timeout=args.timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        proc.kill()
        out, _ = proc.communicate()

    text = out.decode("utf-8", errors="replace").replace("\r\n", "\n")
    missing = [e for e in args.expect if e not in text]
    if missing:
        print("boot test FAILED; missing:", missing, file=sys.stderr)
        print("timed out:", timed_out, "| exit status:", proc.returncode, file=sys.stderr)
        print("--- captured output ---", file=sys.stderr)
        print(text, file=sys.stderr)
        return 1
    print("boot test passed" + (" (QEMU killed at timeout)" if timed_out else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
