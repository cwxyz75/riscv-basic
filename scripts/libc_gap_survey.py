#!/usr/bin/env python3
"""libc gap survey: what do real programs need that our libc headers do not declare? (UV0, UV0-GAP-SURVEY.md)

Each source file of the chosen programs is compiled SYNTAX-ONLY with the project's riscv64 clang against
`libc/include` (our headers first, the program's own include directories next, a directory of empty stand-in
headers last, created on demand when a header is missing so the compile can continue). Nothing is linked, run,
configured or generated. clang's diagnostics list what is undeclared: functions, constants and variables, types,
and whole system headers. Names that are not system interfaces (a program's own functions, undeclared only
because their generated headers are stand-ins) are dropped by keeping only names the host system headers declare.

Usage:
  make fetch-third-party && python3 -I scripts/libc_gap_survey.py            # survey all programs
  python3 -I scripts/libc_gap_survey.py kilo tcc sqlite --check              # exit 1 if anything is undeclared
  python3 -I scripts/libc_gap_survey.py --list                               # the known programs

Output (default `target/libc-gap/`, ignored by git): `report.txt`, `summary.json`, and per-program raw results.

Limits (see UV0-GAP-SURVEY.md section 10): configuration macros (`HAVE_*`, `ENABLE_*`) are absent, so the counts are
lower bounds; gnulib programs would get some names from gnulib; "declared" is not "implemented".
"""
import argparse
import collections
import concurrent.futures as cf
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import threading

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
FETCHED = os.path.join(REPO, "third_party", "fetched")
LOCK = threading.Lock()

# name -> (directory under third_party/fetched, source root under <dir>/src, file globs, include dirs, defines)
PROGRAMS = {
    "kilo": ("kilo", "", ["kilo.c"], [""], []),
    "sqlite": ("sqlite", "sqlite-amalgamation-3450100", ["sqlite3.c", "shell.c"], [""],
               ["-DHAVE_READLINE=0", "-DSQLITE_OMIT_LOAD_EXTENSION=1"]),
    "tcc": ("tcc", "", ["tcc.c", "libtcc.c", "tccpp.c", "tccgen.c", "tccelf.c", "tccasm.c", "tccrun.c"], [""],
            ['-DTCC_VERSION="0.9.27"', "-DONE_SOURCE=0"]),
    "dash": ("dash", "src", ["*.c"], ["", "../"], []),
    "busybox": ("busybox", "",
                ["libbb/*.c", "shell/ash.c", "coreutils/*.c", "findutils/*.c", "archival/tar.c", "archival/gzip.c",
                 "archival/bbunzip.c", "editors/sed.c", "editors/awk.c", "editors/vi.c", "editors/diff.c",
                 "miscutils/less.c", "procps/ps.c"],
                ["include", "libbb"], ['-DBB_VER="1.36.1"', '-DKBUILD_BASENAME="x"']),
    "coreutils": ("coreutils", "",
                  ["src/ls.c", "src/cp.c", "src/mv.c", "src/rm.c", "src/cat.c", "src/head.c", "src/tail.c",
                   "src/wc.c", "src/tr.c", "src/cut.c", "src/sort.c", "src/env.c", "src/chmod.c", "src/mkdir.c",
                   "src/date.c", "src/du.c", "src/ln.c", "src/touch.c", "src/install.c", "src/expr.c", "src/test.c",
                   "src/tee.c", "src/uname.c", "src/sleep.c", "src/stat.c", "src/id.c", "src/pwd.c",
                   "src/readlink.c", "src/mktemp.c", "src/copy.c"],
                  ["lib", "src"], ["-DHAVE_CONFIG_H"]),
    "gnutar": ("gnutar", "", ["src/*.c"], ["lib", "src"], ["-DHAVE_CONFIG_H"]),
    "gzip": ("gzip", "", ["*.c"], ["lib", "."], ["-DHAVE_CONFIG_H"]),
}

RE_FATAL = re.compile(r"fatal error: '([^']+)' file not found")
RE_FUNC = re.compile(r"(?:call to undeclared (?:library )?function|implicit declaration of function) '([^']+)'")
RE_IDENT = re.compile(r"use of undeclared identifier '([^']+)'")
RE_TYPE = re.compile(r"unknown type name '([^']+)'")
RE_INC = re.compile(r"(?:incomplete type|incomplete definition of type) '(?:struct|union) ([^']+)'")
RE_WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
RE_CALL = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(")
# names that are platform plumbing or helper libraries, not part of the portable surface we are measuring
NOISE = re.compile(r"^(_|__|OSSwap|OSAtomic|os_|dispatch|CF|NS|xpc|mach|task_|vm_|kern_|IO|MAC|sandbox|malloc_zone|"
                   r"libkern|el_|history|fts_|EditLine|History|major_t|minor_t)")


def find_clang():
    cands = []
    if os.environ.get("CC_RISCV"):
        cands.append(os.environ["CC_RISCV"])
    cands += ["/opt/homebrew/opt/llvm/bin/clang", "/usr/local/opt/llvm/bin/clang", "clang"]
    for c in cands:
        try:
            out = subprocess.run([c, "--print-targets"], capture_output=True, text=True).stdout
        except OSError:
            continue
        if "riscv64" in out:
            return c
    sys.exit("libc_gap_survey: no clang with a riscv64 target (set CC_RISCV, as init/build.rs does)")


def sys_include_dir():
    """The host's system headers, used only as the list of names that count as 'system interfaces'."""
    if sys.platform == "darwin":
        try:
            sdk = subprocess.run(["xcrun", "--show-sdk-path"], capture_output=True, text=True).stdout.strip()
            if sdk:
                return os.path.join(sdk, "usr", "include")
        except OSError:
            pass
    return "/usr/include"


def scan_headers(root, skip=()):
    """(function-like names, all words, header paths relative to root) over every .h under root."""
    funcs, words, hdrs = set(), set(), set()
    for dp, _dn, fn in os.walk(root):
        if any(s in dp for s in skip):
            continue
        for f in fn:
            if f.endswith(".h"):
                p = os.path.join(dp, f)
                hdrs.add(os.path.relpath(p, root))
                try:
                    t = open(p, errors="ignore").read()
                except OSError:
                    continue
                words.update(RE_WORD.findall(t))
                funcs.update(RE_CALL.findall(t))
    return funcs, words, hdrs


def compile_one(clang, path, incs, defs, stubs):
    base = ["--target=riscv64-unknown-elf", "-march=rv64gc", "-mabi=lp64d", "-ffreestanding", "-nostdlibinc",
            "-fsyntax-only", "-ferror-limit=0", "-w", "-fno-color-diagnostics", "-std=gnu11",
            "-fgnuc-version=4.2.1"]
    cmd = [clang] + base + [f"-I{REPO}/libc/include"] + [f"-I{i}" for i in incs] + [f"-I{stubs}"] + defs + [path]
    found = {k: set() for k in ("funcs", "idents", "types", "structs", "headers")}
    for _ in range(80):
        err = subprocess.run(cmd, capture_output=True, text=True, timeout=600).stderr
        m = RE_FATAL.search(err)
        if m:
            h = m.group(1)
            found["headers"].add(h)
            with LOCK:
                p = os.path.join(stubs, h)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                if not os.path.exists(p):
                    open(p, "w").close()
            continue
        for rx, key in ((RE_FUNC, "funcs"), (RE_IDENT, "idents"), (RE_TYPE, "types"), (RE_INC, "structs")):
            found[key].update(rx.findall(err))
        return found, 0
    return found, 1


def survey(clang, name, outdir):
    d, root, globs, incdirs, defs = PROGRAMS[name]
    top = os.path.join(FETCHED, d, "src", root)
    if not os.path.isdir(top):
        sys.exit(f"libc_gap_survey: {name}: {top} is missing; run `make fetch-third-party`")
    stubs = os.path.join(outdir, "stubs", name)
    shutil.rmtree(stubs, ignore_errors=True)  # stale stand-ins would hide headers we have since added
    os.makedirs(stubs)
    files = sorted({f for g in globs for f in glob.glob(os.path.join(top, g))})
    incs = [os.path.normpath(os.path.join(top, i)) for i in incdirs]
    agg = {k: set() for k in ("funcs", "idents", "types", "structs", "headers")}
    gave_up = 0
    with cf.ThreadPoolExecutor(max_workers=os.cpu_count() or 4) as ex:
        for found, bad in ex.map(lambda f: compile_one(clang, f, incs, defs, stubs), files):
            gave_up += bad
            for k in agg:
                agg[k] |= found[k]
    out = {k: sorted(v) for k, v in agg.items()}
    out["files"] = len(files)
    out["gave_up"] = gave_up
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("programs", nargs="*", help="programs to survey (default: all)")
    ap.add_argument("--out", default=os.path.join(REPO, "target", "libc-gap"), help="output directory")
    ap.add_argument("--check", action="store_true", help="exit 1 if any system name is undeclared by our headers")
    ap.add_argument("--list", action="store_true", help="list the known programs and exit")
    a = ap.parse_args()
    if a.list:
        print("\n".join(sorted(PROGRAMS)))
        return 0
    names = a.programs or list(PROGRAMS)
    for n in names:
        if n not in PROGRAMS:
            sys.exit(f"libc_gap_survey: unknown program {n!r} (try --list)")
    os.makedirs(a.out, exist_ok=True)
    clang = find_clang()

    ours_funcs, ours_words, _ = scan_headers(os.path.join(REPO, "libc", "include"))
    sysinc = sys_include_dir()
    sys_funcs, sys_words, sys_hdrs = scan_headers(sysinc, skip=("/c++", "/objc", "/mach", "/os", "/xpc",
                                                                 "/dispatch", "/Block", "/libkern"))
    raw = {}
    for n in names:
        raw[n] = survey(clang, n, a.out)
        with open(os.path.join(a.out, f"raw_{n}.json"), "w") as fh:
            json.dump(raw[n], fh, indent=1)

    F = {n: sorted(f for f in raw[n]["funcs"] if f in sys_funcs and f not in ours_funcs and not NOISE.match(f))
         for n in names}
    I = {n: sorted(i for i in raw[n]["idents"] if i in sys_words and i not in ours_words and not NOISE.match(i))
         for n in names}
    T = {n: sorted(t for t in raw[n]["types"] if t in sys_words and t not in ours_words and not NOISE.match(t))
         for n in names}
    H = {n: sorted(h for h in raw[n]["headers"] if h in sys_hdrs) for n in names}

    def tally(m):
        c = collections.Counter()
        for n in names:
            for x in m[n]:
                c[x] += 1
        return c

    cf_, ci_, ct_, ch_ = tally(F), tally(I), tally(T), tally(H)
    lines = [f"libc gap survey: {len(names)} programs, system headers from {sysinc}",
             "program      files  functions  constants/vars  types  headers"]
    for n in names:
        lines.append(f"  {n:10s} {raw[n]['files']:5d} {len(F[n]):10d} {len(I[n]):15d} {len(T[n]):6d} {len(H[n]):8d}"
                     + (f"   (gave up on {raw[n]['gave_up']} files)" if raw[n]["gave_up"] else ""))
    lines.append(f"distinct: functions {len(cf_)}, constants/variables {len(ci_)}, types {len(ct_)}, headers {len(ch_)}")
    ordered = lambda c, k=1: " ".join(f"{x}({v})" for x, v in sorted(c.items(), key=lambda t: (-t[1], t[0])) if v >= k)
    lines.append("\nmissing system headers (programs): " + ordered(ch_))
    lines.append("\nfunctions needed by >= 2 programs: " + ordered(cf_, 2))
    lines.append("\nconstants/variables needed by >= 3 programs: " + ordered(ci_, 3))
    lines.append("\ntypes: " + ordered(ct_))
    for n in names:
        lines.append(f"\n[{n}] functions: " + " ".join(F[n]))
        lines.append(f"[{n}] headers: " + " ".join(H[n]) + " | types: " + " ".join(T[n]))
    report = "\n".join(lines)
    with open(os.path.join(a.out, "report.txt"), "w") as fh:
        fh.write(report + "\n")
    with open(os.path.join(a.out, "summary.json"), "w") as fh:
        json.dump({"functions": F, "constants": I, "types": T, "headers": H}, fh, indent=1)
    print(report)
    print(f"\n(report: {os.path.join(a.out, 'report.txt')})")
    total = sum(len(F[n]) + len(I[n]) + len(T[n]) + len(H[n]) for n in names)
    if a.check:
        print("check: OK, nothing undeclared" if total == 0 else f"check: FAILED, {total} undeclared system names")
        return 0 if total == 0 else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
