#!/usr/bin/env python3
"""KT13 (T289): the kernel's large-frame allows. Clippy's `large_stack_frames` is denied at 1 KiB (`clippy.toml`, `stack-size-threshold`)
in every crate whose code runs on a kernel stack: `kernel`, `ksys`, `kcore`. An allow of it is accepted only for code that runs where nothing
else is under it, and says so:

  // KT13: boot or test only, on a stack nothing else is under   (boot code on the boot stack, a self-test, a test-only report)
  // KT13: a const fn that builds a static at compile time       (no run-time frame at all)

and a boot-or-test allow only in a test file (`*_tests.rs`, `selftest.rs`) or in a function `BOOT` lists, so that a frame on a path every
thread can take cannot be relabelled to pass. An allow with no note is refused. `KT13: audited` (the debt marker of T172) is refused in the
kernel crate: T289 shrank every such frame there. In `kcore` and `ksys`, whose frames the lint first measured in T289, the frames T289 did not
shrink carry `KT13: audited, ... left by T289`: their number is pinned at `AUDITED_LEFT`, so a new one fails and one shrunk lowers the pin
(work/T289.md lists them). The check also refuses a crate without its `clippy.toml` threshold or its crate-level `deny`.

Usage: stack_allows.py [--list] [--self-test]"""
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
CRATES = {"kernel": "src/main.rs", "ksys": "src/lib.rs", "kcore": "src/lib.rs"}
THRESHOLD = 1024
BOOT_NOTE = "KT13: boot or test only, on a stack nothing else is under"
CONST_NOTE = "KT13: a const fn that builds a static at compile time"
TEST_FILE = re.compile(r"(_tests|selftest)\.rs$")
LEFT_NOTE = "KT13: audited, clippy's estimate over 1 KiB, left by T289"
# The `kcore` and `ksys` frames over the estimate that T289 left (work/T289.md): may only fall.
AUDITED_LEFT = 48
# Boot-only and test-only functions outside the test files: (file, function). A new entry is a claim a reviewer checks: the function runs once
# at boot on the boot stack (or only in a test build, or only on a test's failure), never on a path a user thread or an interrupt takes.
BOOT = {
    ("kernel/src/main.rs", "kmain"),
    ("kernel/src/kmem.rs", "init"),
    ("kernel/src/thread.rs", "init"),
    ("kernel/src/thread.rs", "timer_report"),
    ("kernel/src/thread.rs", "report_threads"),
    ("kernel/src/smp.rs", "start_secondaries"),
    ("kernel/src/entropy.rs", "init"),
    ("kernel/src/dtb.rs", "ff_report"),
    ("kernel/src/dtb.rs", "find_chosen"),
    ("kernel/src/arch/riscv64/aia.rs", "discover"),
    ("kernel/src/arch/riscv64/iommu.rs", "init"),
    ("kernel/src/arch/riscv64/iommu.rs", "selftest"),
    ("kernel/src/user/mod.rs", "fresh_state"),
    ("kernel/src/user/mod.rs", "report_processes"),
    ("kernel/src/user/dying.rs", "report"),
    ("kernel/src/user/loader.rs", "start_locked"),
    ("kernel/src/user/syslib.rs", "build_lib"),
    # the user state: built once at boot by `kernel::user::fresh_state` (and by the host tests)
    ("ksys/src/state.rs", "with_irq_slots"),
    ("ksys/src/state.rs", "new"),
    ("ksys/src/state.rs", "default"),
    # a process record put in the table by the boot loader (`kernel::user::loader::start_locked`) and by the host tests
    ("ksys/src/proc.rs", "put_process"),
    # kcore: the device tree, the ELF images and the IOMMU tables are read by boot code only; the audits are the tests' invariant checks
    ("kcore/src/aiatree.rs", "discover"),
    ("kcore/src/iommutree.rs", "pci_memory_windows"),
    ("kcore/src/iommutree.rs", "discover"),
    ("kcore/src/iommutree.rs", "highest_address"),
    ("kcore/src/fdt.rs", "scan"),
    ("kcore/src/fdt.rs", "test_finisher_base"),
    ("kcore/src/fdt.rs", "walk_props"),
    ("kcore/src/fdt.rs", "parse"),
    ("kcore/src/isa.rs", "extensions_of"),
    ("kcore/src/devwin.rs", "device_windows"),
    ("kcore/src/frame.rs", "with_ranges"),
    ("kcore/src/elf.rs", "parse"),
    ("kcore/src/elf.rs", "check_relocations"),
    ("kcore/src/iodomain.rs", "new"),
    ("kcore/src/aspace.rs", "audit_maps"),
    ("kcore/src/buddy.rs", "audit"),
    ("kcore/src/donate.rs", "audit"),
    ("kcore/src/pifutex.rs", "audit"),
    ("kcore/src/sched.rs", "audit"),
    ("kcore/src/sched2.rs", "audit"),
    ("kcore/src/sched_smp.rs", "audit"),
    ("kcore/src/slab.rs", "audit"),
    ("kcore/src/wait.rs", "audit"),
}
ALLOW = re.compile(r"#\[allow\(([^)]*)\)\]")
# Any attribute that can silence the lint: an outer or inner `allow` or `expect`, or one inside `cfg_attr`.
ATTR = re.compile(r"#!?\[\s*(allow|expect|cfg_attr)\s*\(")
FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s+(\w+)")


def allows_in(rel, text):
    """(line, function, note) for each allow of `large_stack_frames` in a file's text; the note is the `// KT13:` text on the allow's line or on the
    comment line after it (rustfmt moves a trailing comment there when more attributes follow), or ''."""
    out = []
    lines = text.split("\n")
    for i, first in enumerate(lines):
        if not first.lstrip().startswith(("#[allow(", "#![allow(")) and not ALLOW.search(first):
            continue
        # (a multi-line allow runs to its `)]`: it is read as one line, and its note is on its last line or the comment after it)
        end, line = i, first
        while ")]" not in line and end + 1 < len(lines):
            end += 1
            line += " " + lines[end].strip()
        m = ALLOW.search(line)
        if not m or "clippy::large_stack_frames" not in m.group(1):
            continue
        note = ""
        for cand in (line, lines[end + 1] if end + 1 < len(lines) else ""):
            k = cand.find("// KT13:")
            if k >= 0 and (cand is line or cand.strip().startswith("//")):
                note = cand[k + 3:].strip()
                break
        func = "?"
        for j in range(i, min(i + 12, len(lines))):
            f = FN.match(lines[j])
            if f:
                func = f.group(1)
                break
        out.append((i + 1, func, note))
    return out


def file_problems(rel, text):
    bad = []
    kernel = rel.startswith("kernel/")
    for n, line in enumerate(text.split("\n"), 1):
        if "KT13: audited" in line and (kernel or LEFT_NOTE not in line):
            bad.append(f"{rel}:{n}: `KT13: audited` (the debt marker of T172) is gone: shrink the frame (T289)")
    lines = text.split("\n")
    for i, first in enumerate(lines):
        if not ATTR.search(first):
            continue
        end, attr = i, first
        while ")]" not in attr and end + 1 < len(lines):
            end += 1
            attr += " " + lines[end].strip()
        code = attr.split("//", 1)[0]
        if "large_stack_frames" not in code and "clippy::nursery" not in code:
            continue
        if "clippy::nursery" in code:
            bad.append(f"{rel}:{i + 1}: an allow of the clippy::nursery group silences large_stack_frames (KT13): allow the lint you mean")
        elif not ALLOW.search(code) or code.lstrip().startswith("#!"):
            bad.append(f"{rel}:{i + 1}: clippy::large_stack_frames silenced by an inner, `expect` or `cfg_attr` attribute (KT13): a plain `#[allow]` on the function, with its note")
        else:
            nxt = next((l.strip() for l in lines[end + 1 :] if l.strip() and not l.strip().startswith(("//", "#["))), "")
            if re.match(r"(pub(\([^)]*\))?\s+)?(unsafe\s+)?(impl|mod|trait)\b", nxt):
                bad.append(f"{rel}:{i + 1}: an allow of clippy::large_stack_frames on an `impl`, `mod` or `trait` (KT13): put it on the function")
    for n, func, note in allows_in(rel, text):
        if note.startswith(CONST_NOTE) or (not kernel and note.startswith(LEFT_NOTE)):
            continue
        if note.startswith(BOOT_NOTE):
            if not TEST_FILE.search(rel) and (rel, func) not in BOOT:
                bad.append(f"{rel}:{n}: `{func}` is marked boot or test only but is not a test file's or in stack_allows.py's BOOT list")
            continue
        if "KT13: audited" not in note:
            bad.append(f"{rel}:{n}: an allow of clippy::large_stack_frames in `{func}` with no KT13 note (boot or test only, or a const fn)")
    return bad


def crate_problems(crate, root=ROOT):
    bad = []
    toml = os.path.join(root, crate, "clippy.toml")
    if not os.path.exists(toml) or not re.search(rf"^stack-size-threshold\s*=\s*{THRESHOLD}\s*$", open(toml).read(), re.M):
        bad.append(f"{crate}/clippy.toml: no `stack-size-threshold = {THRESHOLD}` (KT13: the crate's frames run on a kernel stack)")
    top = os.path.join(root, crate, CRATES[crate])
    if "#![deny(clippy::large_stack_frames)]" not in open(top).read():
        bad.append(f"{crate}/{CRATES[crate]}: no `#![deny(clippy::large_stack_frames)]` (KT13)")
    return bad


def sources(root=ROOT):
    for crate in CRATES:
        for d, _, files in os.walk(os.path.join(root, crate, "src")):
            for f in sorted(files):
                if f.endswith(".rs"):
                    path = os.path.join(d, f)
                    yield os.path.relpath(path, root), open(path, encoding="utf-8").read()


def left_count(root=ROOT):
    """The `kcore` and `ksys` allows T289 left."""
    return sum(
        1 for rel, text in sources(root) for _, _, note in allows_in(rel, text) if not rel.startswith("kernel/") and note.startswith(LEFT_NOTE)
    )


def problems(root=ROOT):
    bad = []
    for crate in CRATES:
        bad += crate_problems(crate, root)
    for rel, text in sources(root):
        bad += file_problems(rel, text)
    n = left_count(root)
    if n > AUDITED_LEFT:
        bad.append(f"kcore/ksys: {n} large frames left over clippy's estimate, more than the {AUDITED_LEFT} T289 left: shrink the new one (KT13)")
    elif n < AUDITED_LEFT:
        bad.append(f"kcore/ksys: {n} large frames left, fewer than the pin: lower `AUDITED_LEFT` in scripts/stack_allows.py to {n} (and the list in work/T289.md)")
    return bad


def self_test():
    ok = 'fn main() {}\n'
    planted = {
        "an audited allow": ("kernel/src/user/calls.rs", "#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB\nfn process_call() {}\n"),
        "an allow with no note": ("ksys/src/proc.rs", "#[allow(clippy::large_stack_frames)]\npub fn proc_call() {}\n"),
        "a production frame relabelled": ("kernel/src/user/trap.rs", f"#[allow(clippy::large_stack_frames)] // {BOOT_NOTE}\nfn syscall() {{}}\n"),
        "an invariant comment instead of a KT13 note": ("kernel/src/user/calls.rs", "#[allow(clippy::large_stack_frames)]\n// INVARIANT: one frame\nfn pager_supply_call() {}\n"),
        "a multi-line allow with no note": ("kernel/src/user/syslib.rs", "#[allow(\n    clippy::expect_used,\n    clippy::large_stack_frames\n)]\n// INVARIANT: once\nfn ensure() {}\n"),
    }
    planted["an inner allow"] = ("kernel/src/user/trap.rs", "#![allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under\nfn syscall() {}\n")
    planted["an expect"] = ("kernel/src/user/trap.rs", "#[expect(clippy::large_stack_frames)]\nfn syscall() {}\n")
    planted["a cfg_attr allow"] = ("kernel/src/user/trap.rs", "#[cfg_attr(not(test), allow(clippy::large_stack_frames))]\nfn syscall() {}\n")
    planted["the nursery group"] = ("kcore/src/aspace.rs", "#[allow(clippy::nursery)]\nfn page_for() {}\n")
    planted["an allow on an impl"] = ("kernel/src/kmem.rs", f"#[allow(clippy::large_stack_frames)] // {BOOT_NOTE}\nimpl X {{\n    pub fn init() {{}}\n    fn other() {{}}\n}}\n")
    planted["the left marker in the kernel"] = ("kernel/src/user/trap.rs", f"#[allow(clippy::large_stack_frames)] // {LEFT_NOTE}\nfn syscall() {{}}\n")
    for what, (rel, text) in planted.items():
        assert file_problems(rel, text), f"stack_allows self-test: {what} was not refused"
    accepted = {
        "a test file": ("kernel/src/smp_tests.rs", f"#[allow(clippy::large_stack_frames)] // {BOOT_NOTE} (the samples)\nfn harts_come_online() {{}}\n"),
        "a listed boot function": ("kernel/src/main.rs", f"#[allow(clippy::large_stack_frames)]\n// {BOOT_NOTE}\n#[no_mangle]\npub extern \"C\" fn kmain() {{}}\n"),
        "a const fn": ("kernel/src/percpu.rs", f"    #[allow(clippy::large_stack_frames)] // {CONST_NOTE}; no run-time frame\n    const fn new() -> PerHart {{}}\n"),
        "no allow": ("kcore/src/aspace.rs", ok),
        "a frame T289 left in kcore": ("kcore/src/aspace.rs", f"    #[allow(clippy::large_stack_frames)] // {LEFT_NOTE} (see work/T289.md)\n    fn page_for() {{}}\n"),
    }
    for what, (rel, text) in accepted.items():
        assert not file_problems(rel, text), f"stack_allows self-test: {what} was refused: {file_problems(rel, text)}"
    print("stack_allows self-test: ok")
    return 0


def main():
    if "--self-test" in sys.argv:
        return self_test()
    if "--list" in sys.argv:
        for rel, text in sources():
            for n, func, note in allows_in(rel, text):
                print(f"{rel}:{n}: {func}: {note or '(no note)'}")
    bad = problems()
    for b in bad:
        print(b)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
