#!/usr/bin/env python3
"""HD4: the release kernel contains no test-only code. Reads the symbol table of an ELF64 kernel and fails if a name from the deny-list is present
(KERNEL-V1-SPEC 3.8): the self-tests, the user-mode test driver, the SMP and scheduler tests, the expected-fault hook, the negative controls.

  release_symbols.py KERNEL_ELF          check (exit 1 and the names on a violation)
  release_symbols.py --self-test         prove the check sees a planted name

The deny-list is matched against the demangled-enough names (Rust legacy mangling keeps the path components readable)."""
import struct, sys

# ("fault_" is not on it: the negative controls are `cfg!` branches of the features, not functions named so, and the word is inside `page_fault_quiet` and
# `default_ignorable_code_point`)
DENY = ["selftest", "user_tests", "smp_tests", "sched_tests", "expect_refused", "chaos", "ktest_", "run_all_tests"]


def symbols(data):
    if data[:4] != b"\x7fELF" or data[4] != 2:
        raise SystemExit("not an ELF64 file")
    shoff = struct.unpack_from("<Q", data, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    secs = []
    for i in range(shnum):
        o = shoff + i * shentsize
        name, typ, flags, addr, off, size, link, info, align, entsize = struct.unpack_from("<IIQQQQIIQQ", data, o)
        secs.append((typ, off, size, link, entsize))
    out = []
    for typ, off, size, link, entsize in secs:
        if typ != 2:  # SHT_SYMTAB
            continue
        stroff, strsize = secs[link][1], secs[link][2]
        strtab = data[stroff:stroff + strsize]
        for j in range(size // entsize):
            st_name = struct.unpack_from("<I", data, off + j * entsize)[0]
            end = strtab.index(b"\0", st_name)
            out.append(strtab[st_name:end].decode("latin-1"))
    return out


def violations(names):
    return sorted({n for n in names for d in DENY if d in n})


def main(argv):
    if argv == ["--self-test"]:
        assert violations(["kernel::main", "kernel::heap::alloc"]) == []
        assert violations(["kernel::user::trap::expect_refused", "kernel::selftest::TESTS"]) == [
            "kernel::selftest::TESTS", "kernel::user::trap::expect_refused"]
        print("release_symbols self-test passed")
        return 0
    if len(argv) != 1:
        print(__doc__)
        return 2
    names = symbols(open(argv[0], "rb").read())
    bad = violations(names)
    if bad:
        print(f"the release kernel contains test-only symbols ({len(bad)}):")
        for n in bad[:40]:
            print("  " + n)
        return 1
    print(f"release kernel: no test-only symbols among {len(names)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
