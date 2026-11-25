#!/usr/bin/env python3
"""The work queue: one file per item in `work/`, a short header and free text (see `work/README.md`).

Usage:
  work.py ready [AREA]          open slices and tidy items with no open blocker (not someday), near first
  work.py next PREFIX           the next unused number for a prefix (T, L, ...), counting closed items too
  work.py blocked              items waiting on another item, with what they wait on
  work.py decisions             decisions waiting on the owner (kind: decision)
  work.py list [--kind K] [--area A] [--horizon H] [--fixed-by ID] [--complexity L|M|H] [--change-risk L|M|H] [--epic NAME]
  work.py show ID [ID...]       print items in full
  work.py areas                 item counts per area
  work.py graph                 the blocked-by / fixed-by graph as Mermaid
  work.py check                 validate every item (run by `make docs-check`)
  work.py done ID "what" [PR]   close an item: delete its file, append a line to docs/records/done.md

Nothing here needs a package: the header is a deliberately tiny subset of YAML (`key: value`, lists as `[a, b]`).
"""
import datetime
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
WORK = os.path.join(ROOT, "work")
DONE = os.path.join(ROOT, "docs", "records", "done.md")

KINDS = ["slice", "tidy", "decision", "topic"]
HORIZONS = ["near", "long", "someday"]
RISKS = ["H", "M", "L"]
AREAS = {
    "security": "authority, rights, confinement, isolation",
    "kernel": "kernel memory, objects, handles, limits",
    "sched": "scheduler, SMP, kernel locking (SCHED-SMP-SPEC)",
    "ipc": "channels, ports, wake paths, the service runtime",
    "drivers": "device manager, DMA, interrupts, virtio drivers",
    "net": "network stack, sockets, DNS, HTTP",
    "fs": "file protocol, memfs, doofs, fsd, volmgr, names, clock",
    "shell": "ish, the compat shell, the console",
    "session": "sessions, login, /proc, user view",
    "libc": "the C library and rt",
    "ports": "ported third-party programs",
    "signals": "signals and job control",
    "testing": "test harness, CI, flaky tests, oracles",
    "platform": "boot, build, hardware assumptions",
}
FIELDS = ["id", "kind", "title", "area", "horizon", "risk", "complexity", "change_risk", "epic", "blocked_by", "fixed_by", "refs"]
REQUIRED = ["id", "kind", "title", "area", "horizon"]
LIST_FIELDS = {"blocked_by", "fixed_by", "refs"}
NEAR_CAP = 15  # the near horizon is what is worked on next; keep it short enough to read every session
ID_RE = re.compile(r"^[A-Z][A-Za-z0-9-]*$")


class Item:
    def __init__(self, path, meta, body):
        self.path, self.meta, self.body = path, meta, body

    def __getattr__(self, k):
        if k in FIELDS:
            return self.meta.get(k, [] if k in LIST_FIELDS else None)
        raise AttributeError(k)

    def line(self):
        risk = f" [{self.risk}]" if self.risk else ""
        return f"{self.id:<10} {self.kind:<8} {self.area:<9} {self.horizon:<7}{risk} {self.title}"


def parse(path):
    text = open(path, encoding="utf-8").read()
    m = re.match(r"^---\n(.*?)\n---\n?(.*)$", text, re.S)
    if not m:
        raise ValueError(f"{path}: no header (a block between two `---` lines)")
    meta = {}
    for raw in m.group(1).splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        k, sep, v = raw.partition(":")
        if not sep:
            raise ValueError(f"{path}: header line without a colon: {raw!r}")
        k, v = k.strip(), v.strip()
        if k in LIST_FIELDS:
            if not (v.startswith("[") and v.endswith("]")):
                raise ValueError(f"{path}: {k} must be a list like [A, B]")
            v = [x.strip() for x in v[1:-1].split(",") if x.strip()]
        meta[k] = v
    return Item(path, meta, m.group(2))


def load():
    items, errors = {}, []
    if not os.path.isdir(WORK):
        return items, [f"{WORK} is missing"]
    for name in sorted(os.listdir(WORK)):
        if not name.endswith(".md") or name == "README.md":
            continue
        try:
            it = parse(os.path.join(WORK, name))
        except ValueError as e:
            errors.append(str(e))
            continue
        if it.id in items:
            errors.append(f"{name}: id {it.id} is used twice")
        items[it.id] = it
    return items, errors


def check(items):
    out = []
    for it in items.values():
        name = os.path.basename(it.path)
        for f in REQUIRED:
            if not it.meta.get(f):
                out.append(f"{name}: missing {f}")
        for f in it.meta:
            if f not in FIELDS:
                out.append(f"{name}: unknown field {f}")
        if it.id and name != f"{it.id}.md":
            out.append(f"{name}: file name must be {it.id}.md")
        if it.id and not ID_RE.match(it.id):
            out.append(f"{name}: bad id {it.id!r}")
        if it.kind and it.kind not in KINDS:
            out.append(f"{name}: kind {it.kind!r} is not one of {KINDS}")
        if it.horizon and it.horizon not in HORIZONS:
            out.append(f"{name}: horizon {it.horizon!r} is not one of {HORIZONS}")
        if it.area and it.area not in AREAS:
            out.append(f"{name}: area {it.area!r} is not one of {sorted(AREAS)}")
        if it.risk and it.risk not in RISKS:
            out.append(f"{name}: risk {it.risk!r} is not one of {RISKS}")
        if it.epic and not re.match(r"^[a-z0-9][a-z0-9-]*$", it.epic):
            out.append(f"{name}: epic {it.epic!r} must be a branch name part like iommu-kernel (the branch is epic/<name>)")
        for f in ("complexity", "change_risk"):
            if getattr(it, f) and getattr(it, f) not in RISKS:
                out.append(f"{name}: {f} {getattr(it, f)!r} is not one of {RISKS}")
        for f in ("blocked_by", "fixed_by"):
            for ref in getattr(it, f):
                if ref == it.id:
                    out.append(f"{name}: {f} names itself")
                elif ref not in items:
                    out.append(f"{name}: {f} names {ref}, which is not an open item (done? then remove it)")
        if not it.body.strip():
            out.append(f"{name}: no text below the header")
    out += cycles(items)
    near = [i.id for i in items.values() if i.horizon == "near"]
    if len(near) > NEAR_CAP:
        out.append(f"{len(near)} items are near-term; the cap is {NEAR_CAP} (move some to long)")
    return out


def cycles(items):
    out, state = [], {}

    def visit(i, stack):
        state[i] = 1
        for j in items[i].blocked_by:
            if j not in items:
                continue
            if state.get(j) == 1:
                out.append("blocked_by cycle: " + " -> ".join(stack[stack.index(j):] + [j]))
            elif not state.get(j):
                visit(j, stack + [j])
        state[i] = 2

    for i in items:
        if not state.get(i):
            visit(i, [i])
    return out


def open_blockers(items, it):
    return [b for b in it.blocked_by if b in items]


def order(it):
    return (HORIZONS.index(it.horizon) if it.horizon in HORIZONS else 9, it.area or "", natural(it.id))


def natural(s):
    return [int(p) if p.isdigit() else p for p in re.split(r"(\d+)", s)]


def main(argv):
    items, errors = load()
    cmd = argv[1] if len(argv) > 1 else "ready"
    args = argv[2:]
    if cmd == "check":
        problems = errors + check(items)
        if problems:
            print("work check FAILED:")
            for p in problems:
                print("  " + p)
            return 1
        kinds = {k: sum(1 for i in items.values() if i.kind == k) for k in KINDS}
        print(f"work check passed ({len(items)} items: " + ", ".join(f"{n} {k}" for k, n in kinds.items()) + ")")
        return 0
    if errors:
        print("\n".join(errors), file=sys.stderr)
    vals = sorted(items.values(), key=order)
    if cmd == "ready":
        for it in vals:
            if (not open_blockers(items, it) and it.kind in ("slice", "tidy") and it.horizon != "someday"
                    and (not args or it.area == args[0])):
                print(it.line())
    elif cmd == "next":
        if len(args) != 1 or not re.match(r"^[A-Z]+$", args[0]):
            print("usage: work.py next PREFIX   (letters only, e.g. T)", file=sys.stderr)
            return 2
        done = open(DONE, encoding="utf-8").read() if os.path.exists(DONE) else ""
        used = [int(n) for n in re.findall(rf"\b{args[0]}(\d+)\b", " ".join(items) + " " + done)]
        print(f"{args[0]}{max(used, default=0) + 1}")
    elif cmd == "blocked":
        for it in vals:
            b = open_blockers(items, it)
            if b:
                print(f"{it.line()}\n{'':10} waits on: {', '.join(b)}")
    elif cmd == "decisions":
        for it in vals:
            if it.kind == "decision":
                waiting = [i.id for i in vals if it.id in i.blocked_by]
                print(it.line() + (f"\n{'':10} blocks: {', '.join(waiting)}" if waiting else ""))
    elif cmd == "list":
        filt = dict(zip(args[0::2], args[1::2]))
        for it in vals:
            if all((v in getattr(it, k.lstrip("-").replace("-", "_"))) if k == "--fixed-by"
                   else getattr(it, k.lstrip("-").replace("-", "_")) == v for k, v in filt.items()):
                print(it.line())
    elif cmd == "show":
        for i in args:
            it = items.get(i)
            if not it:
                print(f"{i}: no open item (closed items are in docs/records/done.md)")
                continue
            print(open(it.path, encoding="utf-8").read().rstrip())
            needs = [j.id for j in vals if i in j.blocked_by]
            fixes = [j.id for j in vals if i in j.fixed_by]
            if needs:
                print(f"\n(blocks: {', '.join(needs)})")
            if fixes:
                print(f"(fixes when done: {', '.join(fixes)})")
            print()
    elif cmd == "areas":
        for a, desc in AREAS.items():
            n = [i for i in vals if i.area == a]
            print(f"{a:<9} {len(n):>3}  {desc}")
    elif cmd == "graph":
        print("graph LR")
        for it in vals:
            for b in it.blocked_by:
                print(f"  {b} --> {it.id}")
            for f in it.fixed_by:
                print(f"  {f} -. fixes .-> {it.id}")
    elif cmd == "done":
        if len(args) < 2:
            print("usage: work.py done ID \"what was done\" [PR]", file=sys.stderr)
            return 2
        it = items.get(args[0])
        if not it:
            print(f"{args[0]}: no open item", file=sys.stderr)
            return 1
        pr = f" (PR {args[2]})" if len(args) > 2 else ""
        line = f"| {datetime.date.today()} | {it.id} | {it.title} | {args[1]}{pr} |\n"
        with open(DONE, "a", encoding="utf-8") as f:
            f.write(line)
        os.remove(it.path)
        blocked = [j.id for j in vals if it.id in j.blocked_by or it.id in j.fixed_by]
        print(f"closed {it.id}; recorded in docs/records/done.md")
        if blocked:
            print(f"now update the references in: {', '.join(blocked)}")
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
