#!/usr/bin/env python3
"""Resolve the `[stack 0x... 0x...]` addresses of a kernel `##PANIC` line (T233) to the nearest symbols.

Usage: resolve_trace.py <kernel ELF> <log file or - for stdin>    (or: resolve_trace.py <ELF> 0xADDR ...)

The kernel's panic handler lists the words on the panicking thread's stack that point into its code: return addresses of the calls that led
to the panic, innermost first, among a few stale ones. Needs `nm` (GNU or LLVM); a line and file need `llvm-addr2line` and are not resolved here.
"""
import bisect
import re
import subprocess
import sys


def demangle(name):
    """Legacy Rust mangling (`_ZN4core5slice5index...17h<hash>E`): the path without the hash."""
    if name.startswith("_R"):
        # v0 mangling: drop the crate hashes and back references, then read the length-prefixed identifiers in order (generics are left out)
        t = re.sub(r"\.llvm\.\d+$", "", name[2:])
        t = re.sub(r"Cs[0-9A-Za-z]+_", "C", t)
        t = re.sub(r"B[0-9a-z]*_", "", t)
        words, i = [], 0
        while i < len(t):
            if t[i].isdigit():
                j = i
                while j < len(t) and t[j].isdigit():
                    j += 1
                n = int(t[i:j])
                w = t[j:j + n]
                if w[:1].isalpha() or w[:1] == "_":
                    words.append(w)
                i = j + n
            else:
                i += 1
        return "::".join(words) or name
    if not name.startswith("_ZN"):
        return name
    parts, i, s = [], 3, name
    while i < len(s) and s[i].isdigit():
        j = i
        while s[j].isdigit():
            j += 1
        n = int(s[i:j])
        parts.append(s[j:j + n])
        i = j + n
    parts = [re.sub(r"^\$.*", "?", p) if p.startswith("$") else p for p in parts]
    if parts and re.fullmatch(r"h[0-9a-f]{16}", parts[-1]):
        parts.pop()
    return "::".join(parts) or name


def symbols(elf):
    out = subprocess.run(["nm", "-n", elf], capture_output=True, text=True, check=True).stdout
    addrs, names = [], []
    for line in out.splitlines():
        f = line.split()
        if len(f) == 3 and f[1] in "tTwW" and not f[2].startswith((".L", "$")):
            addrs.append(int(f[0], 16))
            names.append(f[2])
    return addrs, names


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    elf, rest = sys.argv[1], sys.argv[2:]
    if rest[0].startswith("0x"):
        text = " ".join(rest)
    else:
        text = sys.stdin.read() if rest[0] == "-" else open(rest[0]).read()
        m = re.search(r"\[stack([^\]]*)\]", text)
        if not m:
            sys.exit("no [stack ...] in the input")
        text = m.group(1)
    addrs, names = symbols(elf)
    for a in re.findall(r"0x[0-9a-f]+", text):
        v = int(a, 16)
        k = bisect.bisect_right(addrs, v) - 1
        if k >= 0:
            print(f"{a}  {demangle(names[k])} +{v - addrs[k]:#x}")
        else:
            print(f"{a}  ?")


main()
