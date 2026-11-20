#!/usr/bin/env python3
"""Contract-document checks (T55): nobody reads the code, so the documents are the contract, and a
document that says something false is a bug. This cannot prove the documents right; it keeps the known-stale
statements from coming back and checks the decision log's bookkeeping.

Usage: check_docs.py   (run from anywhere; exits 1 with a list of problems)
"""
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")


def read(name):
    with open(os.path.join(ROOT, name), encoding="utf-8") as f:
        return f.read()


# Phrases that were true once and are false now: (file, phrase, why)
STALE = [
    ("docs/reference/SYSCALLS.md", "The kernel takes no lock", "syscalls run under the big kernel lock (OD60)"),
    ("docs/reference/SYSCALLS.md", "which do not exist yet", "per-job quotas exist (S4.5)"),
    ("docs/reference/INVARIANTS.md", "(single hart)", "the scheduler state is under the big kernel lock (C6)"),
    ("CLAUDE.md", "S7 (libc, futexes, BASIC and Lua as userland programs) is specified", "S7 is done"),
    ("CLAUDE.md", "both\n  normal and icount modes", "icount runs only one hart (T21)"),
]


GATE_BUDGET_MINUTES = 7  # TESTING.md: every job of the merge gate (ci.yml) may take at most this long
# Soak targets that CI runs split into smaller jobs: the soak workflow may name the parts instead.
SOAK_SPLIT = {"os-boot-test": ["os-boot-test-main", "os-boot-test-rr", "os-boot-test-release"]}
# Targets besides the smoke ones that the gate may run, conditionally (see ci.yml): the harness check.
# (the smoke boot is two parallel jobs, one per shard of the suite, and the release smoke is a third: OD121)
GATE_EXTRA = {"os-boot-test-main", "os-boot-test-smoke-a", "os-boot-test-smoke-b", "os-ktest-small", "os-ktest-4", "lint-kernel"}


def make_var(makefile, name):
    m = re.search(rf"^{name}\s*:?=\s*(.*)$", makefile, re.M)
    return m.group(1).split() if m else []


def check_tiers():
    """The testing tiers (OD83, TESTING.md) are enforced mechanically, so they cannot drift back: the gate
    (`make check`, ci.yml) holds only tier 0 and 1 targets and stays inside its budget; every soak target
    is run by soak.yml."""
    out = []
    makefile = read("Makefile")
    smoke, soak = make_var(makefile, "SMOKE_TARGETS"), make_var(makefile, "SOAK_TARGETS")
    if not smoke or not soak:
        return ["Makefile: SMOKE_TARGETS or SOAK_TARGETS is missing"]
    if set(smoke) & set(soak):
        out.append(f"Makefile: targets in both tiers: {sorted(set(smoke) & set(soak))}")
    m = re.search(r"^check:\s*(.*)$", makefile, re.M)
    if not m or m.group(1).split() != ["smoke"]:
        out.append("Makefile: `check` must be exactly the smoke tier (`check: smoke`); everything else is `soak` / `check-all`")
    ci, soakyml = read(".github/workflows/ci.yml"), read(".github/workflows/soak.yml")
    allowed = set(smoke) | GATE_EXTRA
    for line in re.findall(r"^\s*(?:- )?run: make (.*)$", ci, re.M):
        for t in line.split():
            if t not in allowed:
                out.append(f"ci.yml (the merge gate) runs `make {t}`, which is not a smoke target (TESTING.md)")
    for n in re.findall(r"timeout-minutes:\s*(\d+)", ci):
        if int(n) > GATE_BUDGET_MINUTES:
            out.append(f"ci.yml: a gate job's timeout is {n} minutes; the budget is {GATE_BUDGET_MINUTES} (TESTING.md)")
    if "matrix:" in ci:
        out.append("ci.yml: the merge gate has a build matrix; that belongs in soak.yml")
    for t in soak:
        names = SOAK_SPLIT.get(t, []) + [t]
        if not any(re.search(rf"(?<![\w-]){re.escape(n)}(?![\w-])", soakyml) for n in names):
            out.append(f"soak.yml does not run the soak target {t}")
    # tier 3 (KT6): the overnight targets are their own tier, run by overnight.yml, never by the gate or by soak
    night = make_var(makefile, "OVERNIGHT_TARGETS")
    if not os.path.exists(os.path.join(ROOT, ".github", "workflows", "overnight.yml")):
        out.append(".github/workflows/overnight.yml is missing (KT6)")
    else:
        nightyml = read(".github/workflows/overnight.yml")
        run_lines = " ".join(re.findall(r"^\s*(?:- )?run: make (.*)$", nightyml, re.M)).split()
        for t in night:
            if t not in run_lines:
                out.append(f"overnight.yml does not run the overnight target {t}")
            if t in smoke or t in soak:
                out.append(f"Makefile: the overnight target {t} is also in a lower tier")
            if re.search(rf"(?<![\w-]){re.escape(t)}(?![\w-])", ci) or re.search(rf"(?<![\w-]){re.escape(t)}(?![\w-])", soakyml):
                out.append(f"{t} is an overnight target: ci.yml and soak.yml must not run it")
        for name in ("soak.yml", "overnight.yml"):
            text = read(f".github/workflows/{name}")
            if re.search(r"^\s*schedule\s*:", text, re.M) or re.search(r"^\s*pull_request(_target)?\s*:", text, re.M) or re.search(r"^\s*push\s*:", text, re.M):
                out.append(f"{name}: a schedule, push or pull_request trigger is forbidden; the owner starts it by hand (OD142)")
    if not os.path.exists(os.path.join(ROOT, "docs", "reference", "TESTING.md")):
        out.append("docs/reference/TESTING.md is missing")
    return out


LIVING = ["CLAUDE.md", "README.md", "WORK-PLAN.md", "docs/README.md", "docs/specs", "docs/reference", "work", "home"]
RETIRED = {"ROADMAP.md": "work/ (open items) or docs/records/roadmap-2026-11-18.md (history)",
           "TIDY-UP.md": "work/ (tidy items) or docs/records/done.md (closed ones)"}
# Words that say how far the build has got. A spec describes the end state (owner, 2026-11-18): progress belongs in work/.
PROGRESS = re.compile(r"(?i)\b(nothing (here )?is built|nothing in it is built|not built yet|not yet built|is not built|are not built|"
                      r"not started|built on 20\d\d|built in step|steps? \d+(?: (?:to|and) \d+)? (?:is|are) (?:built|done)|"
                      r"\bas built\b|^status:)", re.M)


# C5 (INVARIANTS.md): atomics live only in `kcore::sync` and a small audited layer. The kernel crates are scanned for atomic types; a
# file may mention them at most as often as recorded here, with the reason (the layer the file belongs to). A new atomic in a
# file that is not listed, or one more than recorded, fails the docs check: add it to the audited layer, or raise the count here
# with a reason a reviewer can judge.
ATOMICS = {
    "kcore/src/sync.rs": (15, "the re-export of the atomics (core's, loom's or shuttle's): the layer itself"),
    "kcore/src/ticket.rs": (13, "the ticket lock protocol, modelled under loom and shuttle"),
    "kcore/src/park.rs": (4, "the park word of the block/wake protocol (PK1), modelled under loom and shuttle"),
    "kcore/src/latch.rs": (4, "the one-way latch, modelled under loom and shuttle"),
    "kcore/src/sched2.rs": (2, "`Home`, the thread's hart: written under run-queue locks, read lock-free (L11b, shuttle model)"),
    "kcore/src/sched_smp.rs": (29, "the scheduler's shared state: marks, pokes, hints, transit slots, counters (L11b, shuttle model `rq_concurrency`)"),
    "kernel/src/percpu.rs": (62, "per-hart state, touched by its own hart or read by others: the audited per-hart layer"),
    "kernel/src/bkl.rs": (11, "the big kernel lock's owner and checker state"),
    "kernel/src/chaos.rs": (10, "the chaos feature's per-hart generator states, enable flag, point mask and delay counter (KT7: test-build diagnostics, no code without the feature)"),
    "kernel/src/latency.rs": (23, "latency trace counters (diagnostics only)"),
    "kernel/src/smp.rs": (7, "hart bring-up and IPI flags"),
    "kernel/src/arch/riscv64/aia.rs": (12, "interrupt controller statistics and masks"),
    "kernel/src/thread.rs": (30, "scheduler ready flags, wake statistics, the watchdog's per-hart sample times and the stack-budget record (KT13: the high water, the first offender as one word and its name as two, the exempt thread)"),
    "kernel/src/arch/riscv64/timer.rs": (2, "the clock's zero (the counter value at the first tick), read by `now_ns`"),
    "kernel/src/user/wake.rs": (8, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the deferred-reap flag, the user-mode counters and the exit-cycle record: the part of the old user.rs that wakes and cleans up)"),
    "kernel/src/user/mod.rs": (2, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the user-state slot)"),
    "kernel/src/user/trap.rs": (2, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the attention epoch)"),
    "kernel/src/arch/riscv64/trap.rs": (10, "the expected-fault test hook and its log words"),
    "kernel/src/heap.rs": (6, "heap statistics and the growth guard"),
    "kernel/src/kmem.rs": (2, "the active-table bookkeeping counters"),
    "kernel/src/dtb.rs": (3, "boot-argument and device-tree flags set once at boot"),
    "kernel/src/console.rs": (2, "the console's panic flag and line state"),
    "kernel/src/interactive.rs": (4, "the interactive build's input flags"),
    "kernel/src/sched_tests.rs": (47, "self-test flags and counters (the deep-recursion test of KT13 adds the use it measured)"),
    "kernel/src/smp_tests.rs": (16, "self-test flags and counters"),
}
ATOMIC_RE = re.compile(r"\bAtomic(?:U8|U16|U32|U64|Usize|Bool|I8|I16|I32|I64|Isize|Ptr)\b")


def check_atomics():
    """C5: no new atomics outside the audited layer (KT4)."""
    out = []
    for crate in ("kernel", "kcore", "klock", "abi"):
        for d, _, files in os.walk(os.path.join(ROOT, crate, "src")):
            for f in files:
                if not f.endswith(".rs"):
                    continue
                rel = os.path.relpath(os.path.join(d, f), ROOT)
                n = 0
                for line in open(os.path.join(d, f)):
                    if not line.lstrip().startswith("//"):
                        n += len(ATOMIC_RE.findall(line.split("//")[0]))
                if n == 0:
                    continue
                allowed = ATOMICS.get(rel)
                if allowed is None:
                    out.append(f"C5: {rel} uses {n} atomic type(s) and is not in the audited allow-list (scripts/check_docs.py ATOMICS)")
                elif n > allowed[0]:
                    out.append(f"C5: {rel} uses {n} atomic types, the audited count is {allowed[0]} ({allowed[1]}): add them to the audited layer or raise the count with a reason")
    return out


# AR1 (OD143, KERNEL-V1-SPEC 2.6): the architecture boundary. No inline assembly, control-register access, TLB or
# firmware instruction, or RISC-V register name in the kernel outside kernel/src/arch/, nor in ksys, nor in kcore (kcore's
# page-table format is the one exception, AR1c). ARCH_PENDING lists files not yet clean (empty since AR1b; it only shrinks).
ARCH_RE = re.compile(
    r"\b(?:asm|global_asm)!|\bcsr[rwsc]i?\b|\bsfence\b|\becall\b|\bwfi\b|\bsret\b"
    r"|\b(?:satp|sstatus|scause|stvec|sepc|stval|sscratch|stimecmp|sie|sip)\b")
ARCH_PENDING = {
}
ARCH_EXEMPT = {"kcore/src/pagetable.rs"}


def check_arch():
    out = []
    for top in ["kernel/src", "ksys/src", "kcore/src"]:
        for dp, _, fs in os.walk(os.path.join(ROOT, top)):
            for f in fs:
                if not f.endswith(".rs"):
                    continue
                path = os.path.join(dp, f)
                rel = os.path.relpath(path, ROOT)
                if rel.startswith("kernel/src/arch/") or rel in ARCH_EXEMPT:
                    continue
                n = 0
                for line in open(path):
                    code = re.sub(r'feature = "[^"]*"', "", line.split("//", 1)[0])
                    n += len(ARCH_RE.findall(code))
                if n and rel not in ARCH_PENDING:
                    out.append(f"AR1: {rel} names the architecture ({n} places): it belongs under kernel/src/arch/")
                if not n and rel in ARCH_PENDING:
                    out.append(f"AR1: {rel} is clean now: remove it from ARCH_PENDING in scripts/check_docs.py")
    return out


def check_syscall_tests():
    """Every error SYSCALLS.md lists for a call has a row in docs/reference/syscall-tests.tsv naming a test that covers it (KT4)."""
    import io, contextlib
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import syscall_tests
    buf = io.StringIO()
    saved = sys.argv
    sys.argv = ["syscall_tests.py", "--quiet"]
    try:
        with contextlib.redirect_stdout(buf):
            syscall_tests.main()
    finally:
        sys.argv = saved
    return [l for l in buf.getvalue().splitlines() if l.strip()]


def md_files(paths):
    for p in paths:
        full = os.path.join(ROOT, p)
        if os.path.isfile(full):
            yield p
        for d, dirs, files in os.walk(full):
            dirs[:] = [x for x in dirs if x not in ("target", "third_party", "node_modules")]
            for f in files:
                if f.endswith(".md"):
                    yield os.path.relpath(os.path.join(d, f), ROOT)


def check_layout():
    """Only the work lists and the two entry points live at the root (owner, 2026-11-18)."""
    allowed = {"README.md", "CLAUDE.md", "WORK-PLAN.md"}
    return [f"{f}: documents live under docs/ (specs, reference, records); only {sorted(allowed)} stay at the root"
            for f in os.listdir(ROOT) if f.endswith(".md") and f not in allowed]


def check_links():
    """A document named in living text exists: a bare `NAME.md` by its file name anywhere in the tree, a path by its path."""
    names = set()
    for d, dirs, files in os.walk(ROOT):
        dirs[:] = [x for x in dirs if x not in ("target", ".git", "node_modules")]
        names.update(f for f in files if f.endswith(".md"))
    out = []
    for f in md_files(LIVING):
        text = read(f)
        for m in re.finditer(r"(?<![\w/.-])((?:[\w.-]+/)*[A-Za-z0-9_-]+\.md)\b", text):
            ref = m.group(1)
            line = text.count("\n", 0, m.start()) + 1
            base = os.path.basename(ref)
            if base in RETIRED:
                out.append(f"{f}:{line}: names {base}, which is gone; use {RETIRED[base]}")
            elif "/" in ref:
                here = os.path.normpath(os.path.join(os.path.dirname(f), ref))
                if not (os.path.exists(os.path.join(ROOT, ref)) or os.path.exists(os.path.join(ROOT, here))):
                    out.append(f"{f}:{line}: {ref} does not exist")
            elif base not in names and not re.match(r"^[a-z]", base):
                out.append(f"{f}:{line}: {ref} does not exist anywhere in the tree")
    return out


def check_specs():
    out = []
    for f in md_files(["docs/specs"]):
        if not re.search(r"^Version: ", read(f), re.M):
            out.append(f"{f}: no `Version:` line under the title")
        for i, line in enumerate(read(f).splitlines(), 1):
            m = PROGRESS.search(line)
            if m:
                out.append(f"{f}:{i}: says {m.group(0)!r}: build progress belongs in work/, not in a spec")
    return out


def check_records_append_only():
    """docs/records/ only grows: against the merge base with origin/main (when there is one), no line may be removed."""
    import subprocess
    try:
        base = subprocess.run(["git", "merge-base", "HEAD", "origin/main"], cwd=ROOT, capture_output=True, text=True,
                              check=True).stdout.strip()
        diff = subprocess.run(["git", "diff", "--no-renames", "--diff-filter=MD", "-U0", base, "--", "docs/records"],
                              cwd=ROOT, capture_output=True, text=True, check=True).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return []  # no git or no origin/main (a shallow CI checkout): nothing to compare with
    out, cur = [], None
    for line in diff.splitlines():
        if line.startswith("--- a/"):
            cur = line[6:]
        elif line.startswith("-") and not line.startswith("---"):
            out.append(f"{cur}: a line was changed or removed; records are append-only: {line[1:60]!r}")
    return out


def main():
    problems = []
    for name, phrase, why in STALE:
        if phrase in read(name):
            problems.append(f"{name}: still says {phrase!r} ({why})")

    # Decision log: OD numbers are unique and consecutive.
    log = read("docs/records/decisions.md")
    nums = [int(m.group(1)) for m in re.finditer(r"^\| OD(\d+) \|", log, re.M)]
    seen = set()
    for n in nums:
        if n in seen:
            problems.append(f"decisions.md: OD{n} appears twice")
        seen.add(n)
    if nums:
        missing = [n for n in range(min(nums), max(nums) + 1) if n not in seen]
        if missing:
            problems.append("decisions.md: the decision log has gaps: " + ", ".join(f"OD{n}" for n in missing))

    # The work queue (work/, scripts/work.py): ids, fields, blockers, cycles, the near-term cap.
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import work
    items, errors = work.load()
    problems += errors + work.check(items)

    # Every document the README and CLAUDE.md point at exists.
    for name in ["docs/README.md", "docs/specs/OS-ARCHITECTURE.md", "docs/reference/SYSCALLS.md",
                 "docs/reference/INVARIANTS.md", "docs/records/decisions.md", "docs/records/done.md",
                 "WORK-PLAN.md", "work/README.md"]:
        if not os.path.exists(os.path.join(ROOT, name)):
            problems.append(f"{name} is missing")

    problems += check_layout() + check_links() + check_specs() + check_records_append_only()

    problems += check_tiers() + check_atomics() + check_syscall_tests() + check_arch()
    if problems:
        print("docs check FAILED:")
        for p in problems:
            print("  " + p)
        return 1
    print(f"docs check passed ({len(STALE)} stale phrases absent, {len(nums)} decisions, {len(items)} work items)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
