#!/usr/bin/env python3
"""KT13, T289: a static bound on the kernel's stack depth, from the built kernel's own machine code (a by-hand analysis, not a gate).

The boot-time check (`kernel_stack_budget`) measures the paths a boot takes; this bounds the ones it does not. It reads the linked
kernel (any build: the frames are the compiler's real ones, with its inlining), takes every function's frame from its prologue (the
`addi sp, sp, -N` and `sub sp, sp, reg` adjustments before the first release) and its direct calls (`jal`, `auipc` + `jalr`, and tail
calls into another function), and prints the deepest call chain under each root: the frame of a function plus the deepest of its
callees. A tail call is counted as a call (an over-estimate: the caller's frame is gone by then). Indirect calls (`jalr` through a
register: trait objects, function pointers) are not followed; the chain says where it meets one. A cycle (recursion) is cut at the
back edge and listed, because its depth is bounded by the code's logic, not by the graph (`docs/records/reviews/2026-11-19-kt13-stack-audit.md`).

Usage:
  stack_depth.py ELF [--root NAME]... [--top N] [--budget BYTES] [--self-test]

A root is the function whose demangled name is NAME, or else every function whose name contains it. With `--budget`, the exit status is 1 when a root's
chain is over it. Without roots, the roots are the kernel's thread and trap entries (`ROOTS` below). `--self-test` checks the parser
on a planted disassembly (a frame, a large frame, a call, a deeper chain, a cycle)."""
import bisect
import os
import re
import subprocess
import sys
import tempfile

# The kernel's entries: what a thread starts in, the user trap's dispatch, and the kernel trap body (an interrupt lands on top of any
# of the others, so the trap body's chain is added to each in the summary).
ROOTS = ["thread_bootstrap", "kernel::user::lifecycle::user_thread_main", "trap_handler"]
TRAP = "trap_handler"


def tool(name):
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    for root, _dirs, files in os.walk(os.path.join(sysroot, "lib", "rustlib")):
        if name in files:
            return os.path.join(root, name)
    sys.exit(f"stack_depth: no {name} (rustup component add llvm-tools)")


def disassemble(elf):
    """The kernel's text as objdump prints it, and its function symbols as (address, size, demangled name). The relocation and dynamic
    sections are dropped first: the static-PIE's `.rela.dyn` stops llvm-objdump."""
    with tempfile.TemporaryDirectory() as d:
        copy = os.path.join(d, "k.elf")
        subprocess.run([tool("llvm-objcopy"), "-R", ".rela.dyn", "-R", ".dynamic", elf, copy], check=True)
        dis = subprocess.run([tool("llvm-objdump"), "-d", "--no-show-raw-insn", copy], capture_output=True, text=True, check=True).stdout
        nm = subprocess.run([tool("llvm-nm"), "-S", "-n", "-C", "--defined-only", copy], capture_output=True, text=True, check=True).stdout
    syms = []
    for line in nm.splitlines():
        m = re.match(r"([0-9a-f]+) ([0-9a-f]+) [tTwW] (.+)$", line)
        if m and int(m.group(2), 16) > 0 and not m.group(3).startswith(".L"):  # (not the assembler's local labels)
            syms.append((int(m.group(1), 16), int(m.group(2), 16), clean(m.group(3))))
    return dis, syms


def clean(name):
    """A demangled name without its hash suffix."""
    return re.sub(r"::h[0-9a-f]{16}$", "", name)


INSN = re.compile(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$")


def upper(s):
    """A `lui`/`auipc` immediate (objdump prints the 20 bits unsigned), shifted and sign-extended."""
    v = int(s.strip(), 0) & 0xFFFFF
    return (v - 0x100000 if v & 0x80000 else v) << 12


def imm(s):
    s = s.strip()
    neg = s.startswith("-")
    v = int(s.lstrip("-"), 0)
    return -v if neg else v


def analyse(dis, syms):
    """Per function: its frame in bytes, its direct callees, whether it makes an indirect call."""
    starts = [a for a, _, _ in syms]
    funcs = {}
    for a, size, name in syms:
        funcs.setdefault(a, {"name": name, "size": size, "frame": 0, "released": False, "calls": set(), "indirect": False})
    regs = {}  # a register's known constant (lui/addi/li) or auipc base, reset per function
    cur = None
    for line in dis.splitlines():
        m = INSN.match(line)
        if not m:
            continue
        addr, op, args = int(m.group(1), 16), m.group(2), m.group(3)
        i = bisect.bisect_right(starts, addr) - 1
        if i < 0:
            continue
        start = starts[i]
        f = funcs[start]
        if addr >= start + max(f["size"], 1):
            continue
        if start != cur:
            cur, regs = start, {}
        a = [x.strip() for x in args.split("<")[0].split(",")]
        if op in ("addi", "c.addi16sp", "c.addi") and len(a) == 3 and a[0] == "sp" and a[1] == "sp":
            v = imm(a[2])
            if v < 0 and not f["released"]:
                f["frame"] += -v
            elif v > 0:
                f["released"] = True
            continue
        if op == "sub" and len(a) == 3 and a[0] == "sp" and a[1] == "sp" and isinstance(regs.get(a[2]), int) and not f["released"]:
            f["frame"] += regs[a[2]]
            continue
        if op == "add" and len(a) == 3 and a[0] == "sp" and a[1] == "sp":
            v = regs.get(a[2])
            if isinstance(v, int) and v < 0 and not f["released"]:
                f["frame"] += -v
            else:
                f["released"] = True
            continue
        if op == "lui" and len(a) == 2:
            regs[a[0]] = upper(a[1])
            continue
        if op in ("addi", "addiw") and len(a) == 3 and a[1] in regs and a[0] != "sp":
            b = regs[a[1]]
            regs[a[0]] = ("pc", b[1] + imm(a[2])) if isinstance(b, tuple) else b + imm(a[2])
            continue
        if op == "li" and len(a) == 2:
            regs[a[0]] = imm(a[1])
            continue
        if op == "auipc" and len(a) == 2:
            regs[a[0]] = ("pc", (addr + upper(a[1])) & (2**64 - 1))
            continue
        target = None
        if op in ("jal", "call", "tail", "j") and a:
            t = re.match(r"(0x)?([0-9a-f]+)$", a[-1])
            if t:
                target = int(t.group(2), 16)
                if op == "j" and start <= target < start + f["size"]:
                    target = None  # a branch inside the function
        elif op in ("jalr", "jr"):
            mm = re.match(r"(-?(?:0x)?[0-9a-f]+)?\((\w+)\)$", a[-1]) if a else None
            base = mm.group(2) if mm else (a[-1] if a else None)
            off = imm(mm.group(1)) if mm and mm.group(1) else 0
            rv = regs.get(base)
            if isinstance(rv, tuple):
                target = (rv[1] + off) & (2**64 - 1)
            elif op == "jalr" and base != "ra" or op == "jr" and base not in ("ra",):
                f["indirect"] = True
        if target is not None:
            j = bisect.bisect_right(starts, target) - 1
            if j >= 0 and starts[j] == target and target != start:
                f["calls"].add(target)
        # any other write to a tracked register forgets it
        if a and a[0] in regs and op not in ("sd", "sw", "sh", "sb", "c.sd", "beq", "bne", "blt", "bge", "bltu", "bgeu"):
            regs.pop(a[0], None)
    return funcs


def deepest(funcs, root, memo, stack, cycles):
    """The deepest chain from `root`: (bytes, [function addresses], met an indirect call)."""
    if root in memo:
        return memo[root]
    if root in stack:
        cycles.add(tuple(stack[stack.index(root):]) + (root,))
        return (0, [], False)
    stack.append(root)
    f = funcs[root]
    best = (0, [], False)
    for c in f["calls"]:
        d = deepest(funcs, c, memo, stack, cycles)
        if d[0] > best[0]:
            best = d
    stack.pop()
    r = (f["frame"] + best[0], [root] + best[1], f["indirect"] or best[2])
    memo[root] = r
    return r


def report(funcs, roots, top, budget):
    memo, cycles = {}, set()
    by_name = {}
    for a, f in funcs.items():
        by_name.setdefault(f["name"], a)
    print(f"{len(funcs)} functions; the {top} largest frames:")
    for a, f in sorted(funcs.items(), key=lambda kv: -kv[1]["frame"])[:top]:
        print(f"  {f['frame']:6}  {f['name']}")
    over = False
    trap = next((a for n, a in by_name.items() if n == TRAP), None)
    trap_depth = deepest(funcs, trap, memo, [], cycles)[0] if trap is not None else 0
    for r in roots:
        matches = [by_name[r]] if r in by_name else [a for n, a in by_name.items() if r in n]
        if not matches:
            print(f"\nroot {r}: no such function")
            over = True
            continue
        for a in matches:
            d, path, ind = deepest(funcs, a, memo, [], cycles)
            print(f"\n{funcs[a]['name']}: {d} bytes deep (+ {trap_depth} for a kernel trap on top = {d + trap_depth})"
                  + ("; the chain meets an indirect call" if ind else ""))
            for p in path:
                print(f"  {funcs[p]['frame']:6}  {funcs[p]['name']}" + ("  [indirect call]" if funcs[p]["indirect"] else ""))
            if budget is not None and d > budget:
                print(f"  OVER the budget of {budget} bytes")
                over = True
    if cycles:
        print("\nrecursion (cut at the back edge; bounded by the code, see the KT13 audit record):")
        for c in sorted({tuple(sorted(set(c))) for c in cycles}):
            print("  " + " -> ".join(funcs[a]["name"] for a in c))
    return 1 if over else 0


PLANTED = """
0000000000001000 <a>:
    1000:      	addi	sp, sp, -0x20
    1004:      	jal	0x2000 <b>
    1008:      	auipc	ra, 0xfffff
    100c:      	jalr	0x1000(ra)
    1010:      	addi	sp, sp, 0x20
    1014:      	ret
0000000000002000 <b>:
    2000:      	addi	sp, sp, -0x7f0
    2004:      	lui	a0, 0x1
    2008:      	addiw	a0, a0, -0x7f0
    200c:      	sub	sp, sp, a0
    2010:      	jalr	a5
    2014:      	jal	0x1000 <a>
    2018:      	ret
0000000000003000 <c>:
    3000:      	addi	sp, sp, -0x30
    3004:      	auipc	t1, 0x0
    3008:      	jr	-0x2004(t1)
"""


def self_test():
    syms = [(0x1000, 0x18, "a"), (0x2000, 0x1C, "b"), (0x3000, 0xC, "c")]
    f = analyse(PLANTED, syms)
    assert f[0x1000]["frame"] == 0x20, f[0x1000]
    assert f[0x2000]["frame"] == 0x7F0 + 0x1000 - 0x7F0, f[0x2000]  # 4096: the two-step large frame
    assert f[0x1000]["calls"] == {0x2000}, f[0x1000]["calls"]  # jal, and the auipc+jalr to itself is not an edge
    assert f[0x2000]["indirect"] and f[0x2000]["calls"] == {0x1000}
    assert f[0x3000]["calls"] == {0x1000}  # a tail call counts
    memo, cycles = {}, set()
    d, path, ind = deepest(f, 0x3000, memo, [], cycles)
    assert d == 0x30 + 0x20 + 4096 and path == [0x3000, 0x1000, 0x2000] and ind, (d, path)
    assert cycles, "the a -> b -> a cycle is reported"
    print("stack_depth self-test: ok")


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        self_test()
        return 0
    roots, top, budget, elf = [], 25, None, None
    while args:
        x = args.pop(0)
        if x == "--root":
            roots.append(args.pop(0))
        elif x == "--top":
            top = int(args.pop(0))
        elif x == "--budget":
            budget = int(args.pop(0))
        else:
            elf = x
    if not elf:
        sys.exit(__doc__)
    dis, syms = disassemble(elf)
    return report(analyse(dis, syms), roots or ROOTS, top, budget)


if __name__ == "__main__":
    sys.exit(main())
