#!/usr/bin/env python3
"""AB7 build check: no user program makes a system call of its own. Every call goes through the system library (`libsys.so`), so the only `ecall`
instructions in a program's code are the ones the test programs plant on purpose: `crash` (it misbehaves deliberately: a call from outside the library must end the process, AB8). Usage: check_ecall.py DIR [--self-test]; DIR holds the built programs."""
import os
import re
import subprocess
import sys

SKIP = {"build", "deps", "examples", "incremental"}
# programs that contain `ecall` on purpose
ALLOWED = {"crash"}


def objdump():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    for root, _dirs, files in os.walk(os.path.join(sysroot, "lib", "rustlib")):
        if "llvm-objdump" in files:
            return os.path.join(root, "llvm-objdump")
    for c in ("/opt/homebrew/opt/llvm/bin/llvm-objdump", "llvm-objdump"):
        try:
            subprocess.run([c, "--version"], capture_output=True, check=True)
            return c
        except (OSError, subprocess.CalledProcessError):
            pass
    sys.exit("check_ecall: no llvm-objdump (rustup component add llvm-tools)")


def count_ecalls(tool, path):
    out = subprocess.run([tool, "-d", "--no-show-raw-insn", path], capture_output=True, text=True, check=False).stdout
    return len(re.findall(r"^\s*[0-9a-f]+:\s+ecall\b", out, re.M))


def programs():
    """The user programs the kernel embeds: the names in `kernel/build.rs`'s list."""
    src = open(os.path.join(os.path.dirname(__file__), "..", "kernel", "build.rs")).read()
    block = src[src.index("for name in ["):]
    block = block[: block.index("] {")]
    return re.findall(r'"([a-z0-9_]+)"', block)


def problems(d, tool):
    bad, n = [], 0
    for name in sorted(programs()):
        p = os.path.join(d, name)
        if name in SKIP or not os.path.isfile(p):
            continue
        with open(p, "rb") as f:
            if f.read(4) != b"\x7fELF":
                continue
        n += 1
        if name in ALLOWED:
            continue
        k = count_ecalls(tool, p)
        if k:
            bad.append(f"{name}: {k} ecall instruction(s)")
    return n, bad


def main():
    if "--self-test" in sys.argv:
        tool = objdump()
        # a program with an ecall is found: the test programs have them
        here = os.path.join("target", "riscv64gc-unknown-none-elf")
        found = False
        for prof in ("debug", "release"):
            p = os.path.join(here, prof, "crash")
            if os.path.isfile(p):
                assert count_ecalls(tool, p) > 0, "crash has ecall instructions: the counter is broken"
                found = True
        print("ok: the checker sees an ecall in crash" if found else "ok: (no crash binary to test against)")
        return
    d = sys.argv[1]
    n, bad = problems(d, objdump())
    if bad:
        print("check_ecall: programs with a system call of their own:")
        for b in bad:
            print("  " + b)
        sys.exit(1)
    print(f"check_ecall: {n} programs, none has an ecall of its own (the test programs {sorted(ALLOWED)} are exempt)")


main()
