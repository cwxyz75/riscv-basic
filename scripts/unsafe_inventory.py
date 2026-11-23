#!/usr/bin/env python3
"""KT14: the inventory of every `unsafe` block, function, impl and extern block in the kernel binary (`kernel/src`), each with a hash of its text and the
`// SAFETY:` comment above it. `docs/records/reviews/kt14-unsafe-audit.tsv` lists the blocks that were audited (file, hash, verdict, one line on the
condition). The docs check fails when a block's hash is not in that list: a block that is new or was changed has to be audited again.

  unsafe_inventory.py --list         every block: file, line, hash, kind, first line
  unsafe_inventory.py --check        the check (also run by `make docs-check`)
  unsafe_inventory.py --self-test    plants a new block and a changed one in a copy and sees both refused
  unsafe_inventory.py --unaudited    the blocks not in the audit list, with their text (what a reviewer is given)
"""
import hashlib
import os
import re
import shutil
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(ROOT, "docs/records/reviews/kt14-unsafe-audit.tsv")
START = re.compile(r"\bunsafe\b\s*(\{|fn\b|impl\b|extern\b|trait\b)")


def code_part(line):
    """The line without a trailing `//` comment (strings with `//` in them are rare enough here; a wrong cut only makes the hash differ)."""
    i = line.find("//")
    return line if i < 0 else line[:i]


def blocks_in(path, rel):
    lines = open(path, encoding="utf-8").read().split("\n")
    out = []
    i = 0
    while i < len(lines):
        raw = lines[i]
        stripped = raw.lstrip()
        if stripped.startswith("//"):
            i += 1
            continue
        m = START.search(code_part(raw))
        if not m:
            i += 1
            continue
        kind = {"{": "block", "fn": "fn", "impl": "impl", "extern": "extern", "trait": "trait"}[m.group(1)]
        # the comment lines directly above the statement
        c = i - 1
        comment = []
        while c >= 0 and lines[c].lstrip().startswith("//"):
            comment.append(lines[c].strip())
            c -= 1
        comment.reverse()
        # the text: from the keyword to the end of its braces (or its `;`)
        text = []
        depth = 0
        seen_open = False
        j = i
        done = False
        while j < len(lines) and not done:
            part = code_part(lines[j])
            if j == i:
                part = part[m.start():]
            text.append(part.strip())
            for ch in part:
                if ch == "{":
                    depth += 1
                    seen_open = True
                elif ch == "}":
                    depth -= 1
                    if seen_open and depth == 0:
                        done = True
                        break
                elif ch == ";" and not seen_open and kind in ("fn", "extern", "impl", "trait"):
                    done = True
                    break
            j += 1
        body = " ".join(t for t in text if t)
        norm = re.sub(r"\s+", " ", " ".join(comment) + " | " + body)
        h = hashlib.sha256(norm.encode()).hexdigest()[:12]
        out.append((rel, i + 1, h, kind, body[:80]))
        # an `unsafe fn`'s body is one block with the signature; nested blocks inside it are listed too
        i += 1
    return out


def inventory(root=ROOT):
    out = []
    base = os.path.join(root, "kernel", "src")
    for d, _, files in sorted(os.walk(base)):
        for f in sorted(files):
            if f.endswith(".rs"):
                p = os.path.join(d, f)
                out.extend(blocks_in(p, os.path.relpath(p, root)))
    return out


def audited(path=AUDIT):
    rows = {}
    if os.path.exists(path):
        for line in open(path, encoding="utf-8"):
            if line.startswith("#") or not line.strip():
                continue
            cells = line.rstrip("\n").split("\t")
            if len(cells) >= 3:
                rows[(cells[0], cells[1])] = cells[2:]
    return rows


def problems(root=ROOT, path=AUDIT):
    have = audited(path)
    out = []
    seen = set()
    for rel, line, h, kind, first in inventory(root):
        seen.add((rel, h))
        if (rel, h) not in have:
            out.append(f"KT14: {rel}:{line} ({kind}, {h}) is not in the audit list (docs/records/reviews/kt14-unsafe-audit.tsv): audit it, then add its row: {first}")
    for key in sorted(set(have) - seen):
        out.append(f"KT14: the audit list has {key[0]} {key[1]} and no such block exists any more: remove the row")
    return out


def main():
    if "--list" in sys.argv:
        for r in inventory():
            print("\t".join(str(x) for x in r))
        return 0
    if "--unaudited" in sys.argv:
        have = audited()
        for rel, line, h, kind, first in inventory():
            if (rel, h) not in have:
                print(f"{rel}:{line}\t{h}\t{kind}\t{first}")
        return 0
    if "--self-test" in sys.argv:
        tmp = tempfile.mkdtemp()
        try:
            os.makedirs(os.path.join(tmp, "kernel", "src"))
            f = os.path.join(tmp, "kernel", "src", "x.rs")
            open(f, "w").write("fn f() {\n    // SAFETY: fine.\n    unsafe { core::ptr::read(&1u8) };\n}\n")
            a = os.path.join(tmp, "audit.tsv")
            rows = inventory(tmp)
            assert len(rows) == 1, rows
            open(a, "w").write("\t".join(["kernel/src/x.rs", rows[0][2], "ok", "reads a local"]) + "\n")
            assert problems(tmp, a) == [], problems(tmp, a)
            open(f, "a").write("fn g() {\n    unsafe { core::ptr::read(&2u8) };\n}\n")
            assert len(problems(tmp, a)) == 1, "a new block is refused"
            open(f, "w").write("fn f() {\n    // SAFETY: fine.\n    unsafe { core::ptr::read(&3u8) };\n}\n")
            assert len(problems(tmp, a)) == 2, "a changed block is refused (and its old row is stale)"
        finally:
            shutil.rmtree(tmp)
        print("unsafe_inventory self-test passed")
        return 0
    p = problems()
    if p:
        print("\n".join(p))
        print(f"{len(p)} problem(s)")
        return 1
    print(f"unsafe inventory: {len(inventory())} blocks, all audited")
    return 0


if __name__ == "__main__":
    sys.exit(main())
