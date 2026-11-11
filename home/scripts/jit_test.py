#!/usr/bin/env python3
"""The JIT on the real machine (milestone L4): generated code runs, and when it
faults the trap handler shows it.

Usage: jit_test.py KERNEL_ELF

Two boots, driven from the boot menu's debugging keys:
  3  runs the JIT self-tests: the kernel assembles small functions with the
     `rv64` crate, loads them into RAM, executes `fence.i`, and calls them
     (`li a0,42; ret`, double-precision arithmetic, loops with labels and
     branches, signed and unsigned compares, 64-bit constants).
  4  emits code that loads from an unmapped address. The fatal trap report must
     then include a dump of the generated code with the faulting instruction
     marked.
"""
import sys

from boot_test import boot, check


def self_test(kernel: str) -> int:
    text, timed_out, returncode = boot(
        kernel, [], menu_choice=b"3", timeout=30, stop_when=["jit self-test: 5/5 ok"]
    )
    return check(
        text,
        [
            "jit: li a0,42; ret -> 0x2a ok",
            "jit: 1.5 * 2 + 0.25 (FPU) -> 0x400a000000000000 ok",
            "jit: sum 1..10 (loop) -> 0x37 ok",
            "jit: signed/unsigned branches -> 0x7 ok",
            "jit: 64-bit tag arithmetic -> 0xfffa000000000007 ok",
            "jit self-test: 5/5 ok",
        ],
        timed_out,
        returncode,
        forbidden=["FAILED"],
    )


def crash_demo(kernel: str) -> int:
    text, timed_out, returncode = boot(kernel, [], menu_choice=b"4", timeout=30)
    status = check(
        text,
        [
            "jit: faulting on purpose",
            "TRAP: scause=0x5 (load access fault)",
            "stval=0x12345678",
            "JIT code at 0x",
            "(16 bytes), fault at +0x8:",
            # lui t0,0x12345 / addiw t0,t0,0x678 / ld a0,0(t0) / ret
            "+0x00: 123452b7\n",
            "+0x04: 6782829b\n",
            "+0x08: 0002b503 <-- fault\n",
            "+0x0c: 00008067\n",
        ],
        timed_out,
        returncode,
    )
    if status == 0 and (timed_out or returncode != 0):
        print("expected a clean shutdown after the fatal trap", file=sys.stderr)
        return 1
    return status


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    if self_test(sys.argv[1]) or crash_demo(sys.argv[1]):
        return 1
    print("jit test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
