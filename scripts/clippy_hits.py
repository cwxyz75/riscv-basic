#!/usr/bin/env python3
"""List the panic-finding lint hits (KT3) of a crate with the source line: clippy_hits.py CRATE_DIR [FILE_SUBSTRING]."""
import json, subprocess, sys, os
crate = sys.argv[1]
only = next((a for a in sys.argv[2:] if not a.startswith("--")), "")
LINTS = ["clippy::unwrap_used", "clippy::expect_used", "clippy::indexing_slicing", "clippy::arithmetic_side_effects",
         "clippy::panic", "clippy::unreachable"]
cmd = ["cargo", "clippy"] + ([] if os.path.basename(os.path.abspath(crate)) == "kernel" else ["--lib"]) + ["--message-format=json", "--"] + [f"-W{l}" for l in LINTS]
out = subprocess.run(cmd, cwd=crate, capture_output=True, text=True).stdout
root = os.path.abspath(os.path.join(crate, ".."))
hits = []
for l in out.splitlines():
    try:
        m = json.loads(l)
    except ValueError:
        continue
    if m.get("reason") != "compiler-message":
        continue
    msg = m["message"]
    code = (msg.get("code") or {}).get("code", "")
    if code not in LINTS or not msg["spans"]:
        continue
    sp = msg["spans"][0]
    if only and only not in sp["file_name"]:
        continue
    hits.append((sp["file_name"], sp["line_start"], code.split("::")[1], sp["text"][0]["text"].strip() if sp["text"] else ""))
def enclosing_fn(path, line):
    """The name and starting line of the `fn` that contains `line` (the nearest `fn` above at a lower or equal indent)."""
    with open(os.path.join(root, path)) as f:
        src = f.read().split("\n")
    import re
    i = line - 1
    indent = len(src[i]) - len(src[i].lstrip())
    while i >= 0:
        m = re.match(r"^(\s*)(?:pub(?:\([a-z]+\))? )?(?:const )?(?:unsafe )?(?:extern \"C\" )?fn ([a-zA-Z0-9_]+)", src[i])
        if m and len(m.group(1)) <= indent:
            return m.group(2), i + 1
        i -= 1
    return "?", 0


if "--by-fn" in sys.argv:
    import collections
    g = collections.defaultdict(list)
    for f, n, c, t in sorted(set(hits)):
        name, start = enclosing_fn(f, n)
        g[(f, name, start)].append((n, c, t))
    for (f, name, start), hs in sorted(g.items()):
        kinds = sorted({c for _, c, _ in hs})
        print(f"{f}:{start} fn {name}: {len(hs)} hits {kinds}")
else:
    for f, n, c, t in sorted(set(hits)):
        print(f"{f}:{n} {c}: {t[:110]}")
