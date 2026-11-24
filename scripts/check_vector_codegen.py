#!/usr/bin/env python3
"""PL3: the user C code is really built for RVA23U64. Disassembles the C archives the build made (Lua, the C tests) and fails if none holds a vector
instruction (`vsetvli`, `vle*`, `vse*`): a build whose `-march` quietly dropped the vector extension would pass every other test.

Usage: check_vector_codegen.py   (looks for the newest `liblua.a` and `libctests.a` under target/)
"""
import glob
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def objdump():
    for cand in (os.environ.get("LLVM_OBJDUMP"), shutil.which("llvm-objdump"), "/opt/homebrew/opt/llvm/bin/llvm-objdump",
                 "/usr/local/opt/llvm/bin/llvm-objdump", shutil.which("llvm-objdump-18"), shutil.which("llvm-objdump-19"),
                 shutil.which("llvm-objdump-20")):
        if cand and os.path.exists(cand):
            return cand
    return None


def newest(name):
    found = glob.glob(os.path.join(ROOT, "target", "**", name), recursive=True)
    return max(found, key=os.path.getmtime) if found else None


def main():
    archives = [a for a in (newest("liblua.a"), newest("libctests.a")) if a]
    if not archives:
        if os.environ.get("REQUIRE_C_ARCHIVES"):
            print("check_vector_codegen: FAILED: no C archive was built and REQUIRE_C_ARCHIVES is set")
            return 1
        print("check_vector_codegen: no C archive was built (no RISC-V clang?), skipped")
        return 0
    tool = objdump()
    if not tool:
        print("check_vector_codegen: FAILED: C archives exist but there is no llvm-objdump to read them (set LLVM_OBJDUMP)")
        return 1
    total = 0
    lua = 0
    for a in archives:
        out = subprocess.run([tool, "-d", "--mattr=+v", a], capture_output=True, text=True).stdout
        n = sum(1 for line in out.splitlines() if any(f"\t{m}" in line for m in ("vsetvli", "vsetivli", "vle", "vse", "vadd", "vmv")))
        print(f"check_vector_codegen: {os.path.relpath(a, ROOT)}: {n} vector instructions")
        total += n
        if os.path.basename(a) == "libctests.a":
            # (T188 item 4: the C test's `cbo.zero` is emitted as a raw encoding, 0x0045200f for rs1 = a0 and so on: opcode 0x0f, funct3 2, imm 4)
            zero = sum(1 for line in out.splitlines() if "cbo.zero" in line or (line.split("\t")[-2:-1] and "45200f" in line.replace(" ", "")[:40]))
            print(f"check_vector_codegen: {os.path.relpath(a, ROOT)}: {zero} cbo.zero")
            if zero == 0:
                print("check_vector_codegen: FAILED: no cbo.zero in libctests.a (the transcript's cache-block test did not assemble)")
                return 1
        if os.path.basename(a) == "liblua.a":
            lua = n
    if lua == 0 and any(os.path.basename(a) == "liblua.a" for a in archives):
        print("check_vector_codegen: FAILED: no vector instruction in liblua.a (compiler-generated code: the -march lost the V extension?)")
        return 1
    if total == 0:
        print("check_vector_codegen: FAILED: no vector instruction in the built C code (the -march lost the V extension?)")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
