#!/usr/bin/env python3
"""HD2 build check: every user program in the given directory is a static-PIE (`e_type` ET_DYN, no PT_INTERP). Usage: check_pie.py DIR [--self-test]."""
import os
import struct
import sys

PROGRAMS_SKIP = {"build", "deps", "examples", "incremental"}


def kind(path):
    with open(path, "rb") as f:
        head = f.read(64)
        if head[:4] != b"\x7fELF":
            return None
        etype = struct.unpack_from("<H", head, 16)[0]
        phoff, phentsize, phnum = struct.unpack_from("<QxxxxxxxxHH", head, 32)[0], *struct.unpack_from("<HH", head, 54)
        interp = False
        f.seek(phoff)
        for _ in range(phnum):
            ph = f.read(phentsize)
            if struct.unpack_from("<I", ph, 0)[0] == 3:
                interp = True
        return etype, interp


def problems(d):
    bad, n = [], 0
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        if name in PROGRAMS_SKIP or name.endswith(".d") or not os.path.isfile(p):
            continue
        k = kind(p)
        if k is None:
            continue
        n += 1
        if k != (3, False):
            bad.append(f"{name}: e_type {k[0]}{' with PT_INTERP' if k[1] else ''}, not a static-PIE")
    return n, bad


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        import tempfile
        with tempfile.TemporaryDirectory() as t:
            good = bytearray(64)
            good[:4] = b"\x7fELF"
            struct.pack_into("<H", good, 16, 3)
            struct.pack_into("<Q", good, 32, 64)
            struct.pack_into("<HH", good, 54, 56, 0)
            open(os.path.join(t, "good"), "wb").write(good)
            struct.pack_into("<H", good, 16, 2)
            open(os.path.join(t, "fixed"), "wb").write(good)
            n, bad = problems(t)
            assert n == 2 and len(bad) == 1 and bad[0].startswith("fixed"), (n, bad)
        print("check_pie self-test passed")
        sys.exit(0)
    n, bad = problems(sys.argv[1])
    if n == 0:
        sys.exit(f"no programs found in {sys.argv[1]}")
    if bad:
        sys.exit("\n".join(bad))
    print(f"check_pie: {n} programs, every one a static-PIE")
