#!/usr/bin/env python3
"""Contract-document checks (T55): nobody reads the code, so the documents are the contract, and a
document that says something false is a bug. This cannot prove the documents right; it keeps the known-stale
statements from coming back and checks the decision log's bookkeeping.

Usage: check_docs.py               (run from anywhere; exits 1 with a list of problems)
       check_docs.py --self-test   (the rules for the soak and overnight workflows' dispatch inputs, on planted workflows)
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


GATE_BUDGET_MINUTES = 7  # TESTING.md: every job of ci.yml may take at most this long (the jobs run side by side)
# TESTING.md, "Budgets": the one exception, the tool-build job (CI's QEMU into the cache: seconds when warm, a build when cold). It runs no
# make target, starts nothing, and only calls the install action in `mode: fill`; every job that uses CI's QEMU needs it.
TOOL_BUILD_JOB, TOOL_BUILD_BUDGET_MINUTES = "qemu", 20
# Soak targets that CI runs split into smaller jobs: the soak workflow may name the parts instead.
SOAK_SPLIT = {"os-boot-test": ["os-boot-test-main", "os-boot-test-rr", "os-boot-test-release"]}
# Targets besides the smoke ones that ci.yml may run: parts of a smoke target that CI runs as their own jobs.
# (the smoke boot is two parallel jobs, one per shard of the suite, and the release smoke is a third: OD121)
GATE_EXTRA = {"os-boot-test-main", "os-boot-test-smoke-a", "os-boot-test-smoke-b", "os-ktest-small", "os-ktest-4", "os-ktest-big", "lint-target",
              "os-boot-test-fail-demo"}  # the harness check, when the harness itself changed
# What in a recipe means "this starts the emulator" (or a script that does).
BOOTS_RE = re.compile(r"qemu-system|\$\(QEMU\)|os_boot_test\.py|os_repeat\.py|chaos\.py|_test\.py")
SMOKE_IF = "needs.changes.outputs.smoke == 'true'"


def make_var(makefile, name):
    m = re.search(rf"^{name}\s*:?=\s*(.*)$", makefile, re.M)
    out = []
    for w in (m.group(1).split() if m else []):
        ref = re.fullmatch(r"\$\((\w+)\)", w)
        out += make_var(makefile, ref.group(1)) if ref else [w]
    return out


def make_rules(makefile):
    """target -> (prerequisites and `$(MAKE) x` calls, recipe text)."""
    rules, cur = {}, None
    for line in makefile.splitlines():
        m = re.match(r"^([A-Za-z0-9_-]+):(?!=)\s*(.*)$", line)
        if m:
            cur = m.group(1)
            rules[cur] = ([d for d in m.group(2).split() if re.fullmatch(r"[A-Za-z0-9_-]+", d)], "")
        elif line.startswith("\t") and cur:
            deps, recipe = rules[cur]
            deps += re.findall(r"\$\(MAKE\)\s+([a-z][A-Za-z0-9_-]*)", line)
            rules[cur] = (deps, recipe + line + "\n")
        elif line.strip() and not line.startswith("#"):
            cur = None
    return rules


def boots(rules, target, seen=None):
    """Does building this target start the emulator, itself or through anything it depends on?"""
    seen = set() if seen is None else seen
    if target in seen or target not in rules:
        return False
    seen.add(target)
    deps, recipe = rules[target]
    return bool(BOOTS_RE.search(recipe)) or any(boots(rules, d, seen) for d in deps)


def check_ci_qemu(wf, text, rules):
    """CI's QEMU (TEST-TOOLS.md): a job that boots gets QEMU only from the install action (never `qemu: 'false'`, never apt), and a job
    that uses CI's QEMU needs the tool-build job that fills the cache, so it cannot start before the entry exists."""
    out = []
    for job in re.split(r"^  (?=[a-z][\w-]*:\s*$)", text.split("\njobs:\n", 1)[-1], flags=re.M):
        name = job.split(":", 1)[0].strip()
        if name == TOOL_BUILD_JOB:
            continue
        targets = " ".join(re.findall(r"^\s*(?:- )?run: make (.*)$", job, re.M)).split()
        targets += re.findall(r"^\s+(?:[A-Z_]+=\S+ )*make ([a-z][\w-]*)", job, re.M)
        uses = "uses: ./.github/actions/install-qemu" in job and "qemu: 'false'" not in job
        if (any(boots(rules, t) for t in targets if "$" not in t) or BOOTS_RE.search(job)) and not uses:
            out.append(f"{wf}: job {name} boots but does not install CI's QEMU (the install action, without `qemu: 'false'`)")
        if uses:
            needs = re.search(r"^    needs:\s*(.*)$", job, re.M)
            if not needs or not re.search(rf"(?<![\w-]){TOOL_BUILD_JOB}(?![\w-])", needs.group(1)):
                out.append(f"{wf}: job {name} uses CI's QEMU but does not need the job `{TOOL_BUILD_JOB}` that fills the cache")
    return out


# T283 (STAB0): the soak and overnight workflows take dispatch inputs that run one target as a piece of at most two hours. The only
# trigger is a manual dispatch; the inputs are these, strings that default to empty; the `piece` job runs only when `target` is set,
# within PIECE_BUDGET_MINUTES, and only `make <tier>-piece`, which refuses a target outside the tier; every other job (but the tool-build
# job) runs only when it is not set, so a dispatch with no inputs is the whole tier as before.
PIECE_INPUTS = {"soak.yml": {"target"}, "overnight.yml": {"target", "chaos_seeds", "n", "pkg", "shard"}}
PIECE_TARGET = {"soak.yml": "soak-piece", "overnight.yml": "overnight-piece"}
PIECE_BUDGET_MINUTES = 120
PIECE_IF, WHOLE_IF = "inputs.target != ''", "inputs.target == ''"


def yaml_block(text, key, indent):
    """The lines under `key:` at `indent` spaces (up to the next line indented as much or less), or None when there is no such key."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if re.fullmatch(rf" {{{indent}}}{re.escape(key)}:\s*(#.*)?", line):
            out = []
            for l in lines[i + 1:]:
                if l.strip() and not l.lstrip().startswith("#") and len(l) - len(l.lstrip()) <= indent:
                    break
                out.append(l)
            return out
    return None


def keys_at(lines, indent):
    return [m.group(1) for l in lines if (m := re.fullmatch(rf" {{{indent}}}([\w-]+):.*", l))]


def check_dispatch(name, text):
    """The triggers and the piece job of soak.yml or overnight.yml (OD142, T283)."""
    out = []
    on = yaml_block(text, "on", 0)
    if on is None:
        return [f"{name}: no `on:` block"]
    if re.search(r"^on:[ \t]*[^\s#]", text, re.M):
        out.append(f"{name}: `on:` must be a block with `workflow_dispatch:` alone (OD142)")
    for t in keys_at(on, 2):
        if t != "workflow_dispatch":
            out.append(f"{name}: the trigger `{t}` is forbidden; the owner starts it by hand, `workflow_dispatch` only (OD142)")
    wd = yaml_block("\n".join(on), "workflow_dispatch", 2) or []
    for k in keys_at(wd, 4):
        if k != "inputs":
            out.append(f"{name}: `workflow_dispatch` takes `inputs:` only, not `{k}`")
    inputs = yaml_block("\n".join(wd), "inputs", 4) or []
    names = keys_at(inputs, 6)
    for n in names:
        if n not in PIECE_INPUTS.get(name, set()):
            out.append(f"{name}: the dispatch input `{n}` is not one of the piece inputs {sorted(PIECE_INPUTS.get(name, set()))} (T283)")
        props = yaml_block("\n".join(inputs), n, 6) or []
        for k in keys_at(props, 8):
            if k not in ("description", "required", "default", "type"):
                out.append(f"{name}: the dispatch input `{n}` has `{k}`; an input is a description, `required: false`, `default: ''` and `type: string`")
        body = "\n".join(props)
        if not re.search(r"^ {8}type:\s*string\s*$", body, re.M) or not re.search(r"^ {8}required:\s*false\s*$", body, re.M) \
                or not re.search(r"^ {8}default:\s*''\s*$", body, re.M):
            out.append(f"{name}: the dispatch input `{n}` must be `type: string`, `required: false`, `default: ''` (empty: the whole tier)")
    if not names:
        return out
    if "target" not in names:
        out.append(f"{name}: the piece inputs need `target`")
    jobs = re.split(r"^  (?=[a-z][\w-]*:\s*$)", text.split("\njobs:\n", 1)[-1], flags=re.M)
    seen_piece = False
    for job in jobs:
        jname = job.split(":", 1)[0].strip()
        if not re.fullmatch(r"[a-z][\w-]*", jname) or jname == TOOL_BUILD_JOB:
            continue
        cond = re.search(r"^    if:\s*(.*)$", job, re.M)
        cond = cond.group(1).strip() if cond else ""
        if jname == "piece":
            seen_piece = True
            if cond != PIECE_IF:
                out.append(f"{name}: the piece job runs only when `target` is set (`if: {PIECE_IF}`)")
            t = re.findall(r"timeout-minutes:\s*(\d+)", job)
            if not t or any(int(x) > PIECE_BUDGET_MINUTES for x in t):
                out.append(f"{name}: the piece job needs `timeout-minutes` of at most {PIECE_BUDGET_MINUTES} (a piece is two hours, STAB0)")
            makes = re.findall(r"(?<![\w-])make\s+(?:-\S+\s+)*([a-z][\w-]*)", "\n".join(l for l in job.splitlines() if not l.lstrip().startswith("#")))
            if PIECE_TARGET[name] not in makes:
                out.append(f"{name}: the piece job must run `make {PIECE_TARGET[name]}` (which refuses a target outside the tier)")
            for m in makes:
                if m not in (PIECE_TARGET[name], "fetch-third-party"):
                    out.append(f"{name}: the piece job runs `make {m}`; it runs only `make {PIECE_TARGET[name]}`")
            # (an input may name the job, key a cache or set an environment variable; anywhere else, a script would see its text)
            if any("${{ inputs." in l and not re.match(r"\s*(?:- )?(?:name|key|[A-Z][A-Z0-9_]*):", l) for l in job.splitlines()):
                out.append(f"{name}: the piece job puts an input into a script's text; pass it through `env:`")
        elif WHOLE_IF not in cond:
            out.append(f"{name}: job {jname} must not run in a piece (`if: {WHOLE_IF}`), so a dispatch with no inputs is the whole tier")
    if not seen_piece:
        out.append(f"{name}: the dispatch inputs need a `piece` job")
    return out


def check_pieces(makefile, rules):
    """`make soak-piece` and `make overnight-piece` validate the piece against the tier's own targets (T283)."""
    out = []
    soak, night = make_var(makefile, "SOAK_TARGETS"), make_var(makefile, "OVERNIGHT_TARGETS")
    allowed = {"soak-piece": set(soak) | {p for parts in SOAK_SPLIT.values() for p in parts} | {"os-miri"}, "overnight-piece": set(night)}
    lists = {"soak-piece": "SOAK_PIECES", "overnight-piece": "OVERNIGHT_TARGETS"}
    for target, var in lists.items():
        if target not in rules:
            out.append(f"Makefile: the target {target} is missing (T283)")
            continue
        recipe = rules[target][1]
        first = next((l for l in recipe.splitlines() if not l.strip().startswith("#")), "")
        if not re.search(rf'python3 scripts/tier\.py piece "\$\$PIECE" \$\({var}\)', first):
            out.append(f"Makefile: {target} must first check the piece with `python3 scripts/tier.py piece \"$$PIECE\" $({var})` (the shell's "
                       "variable: make's $(PIECE) would paste the input into the script before it is checked)")
        extra = set(make_var(makefile, var)) - allowed[target]
        if extra:
            out.append(f"Makefile: {var} names targets outside its tier: {sorted(extra)}")
    return out


def dispatch_self_test():
    """Planted workflows: the rules accept the piece inputs and refuse an extra trigger, a stray input, a piece job without its budget or
    its guard, and a whole-tier job that would also run in a piece."""
    good = """on:
  workflow_dispatch:
    inputs:
      target:
        description: "one target"
        required: false
        default: ''
        type: string
      chaos_seeds:
        description: "seeds"
        required: false
        default: ''
        type: string

jobs:
  qemu:
    runs-on: ubuntu-latest
  chaos:
    needs: qemu
    if: inputs.target == ''
    timeout-minutes: 300
    steps:
      - run: make os-chaos
  piece:
    needs: qemu
    if: inputs.target != ''
    timeout-minutes: 120
    env:
      PIECE: ${{ inputs.target }}
    steps:
      - run: make overnight-piece PIECE="$PIECE"
"""
    assert check_dispatch("overnight.yml", good) == [], check_dispatch("overnight.yml", good)
    plain = "on:\n  workflow_dispatch:\n\njobs:\n  kernel:\n    steps:\n      - run: make os-bench\n"
    assert check_dispatch("soak.yml", plain) == [], "no inputs: the tier as it was"
    bad = {
        "a schedule beside the dispatch": good.replace("  workflow_dispatch:\n", "  schedule:\n    - cron: '0 3 * * *'\n  workflow_dispatch:\n"),
        "a push trigger": good.replace("\njobs:", "  push:\n    branches: [main]\n\njobs:"),
        "a workflow_call trigger that takes the same inputs": good.replace("  workflow_dispatch:\n", "  workflow_call:\n    inputs:\n      target:\n        type: string\n  workflow_dispatch:\n"),
        "an input indented into a trigger": good.replace("      chaos_seeds:\n", "  repository_dispatch:\n      chaos_seeds:\n"),
        "an input that is not a piece input": good.replace("      chaos_seeds:", "      cron:"),
        "an input of another type": good.replace("        type: string\n      chaos_seeds", "        type: choice\n      chaos_seeds"),
        "an input that is required": good.replace("        required: false\n        default: ''\n        type: string\n\n", "        required: true\n        default: ''\n        type: string\n\n"),
        "an input with a default": good.replace("        default: ''\n        type: string\n\njobs", "        default: '1..200'\n        type: string\n\njobs"),
        "a piece over two hours": good.replace("timeout-minutes: 120", "timeout-minutes: 300"),
        "a piece with no timeout": good.replace("    timeout-minutes: 120\n", ""),
        "a piece that runs whatever it is given": good.replace("make overnight-piece PIECE=\"$PIECE\"", "make \"$PIECE\""),
        "a piece that runs a smoke target too": good.replace("      - run: make overnight-piece", "      - run: make os-ktest-4\n      - run: make overnight-piece"),
        "an input in the script's text": good.replace("make overnight-piece PIECE=\"$PIECE\"", "make overnight-piece PIECE=${{ inputs.target }}"),
        "an input in a block script's text": good.replace("      - run: make overnight-piece PIECE=\"$PIECE\"", "      - run: |\n          echo ${{ inputs.chaos_seeds }}\n          make overnight-piece PIECE=\"$PIECE\""),
        "a piece job that runs on every dispatch": good.replace("    if: inputs.target != ''\n", ""),
        "a whole-tier job that also runs in a piece": good.replace("    if: inputs.target == ''\n", ""),
        "inputs with no piece job": good.split("  piece:")[0],
    }
    for what, text in bad.items():
        found = check_dispatch("overnight.yml", text)
        assert found, f"not refused: {what}"
        if os.environ.get("CHECK_DOCS_VERBOSE"):
            print(f"{what}: {found}")
    assert check_dispatch("soak.yml", good), "soak.yml takes `target` only"
    # the workflows in the tree pass
    for wf in ("soak.yml", "overnight.yml"):
        found = check_dispatch(wf, read(f".github/workflows/{wf}"))
        assert found == [], found
    makefile = read("Makefile")
    assert check_pieces(makefile, make_rules(makefile)) == [], check_pieces(makefile, make_rules(makefile))
    planted = makefile.replace("SOAK_PIECES := $(SOAK_TARGETS)", "SOAK_PIECES := os-ktest-4 $(SOAK_TARGETS)")
    assert any("outside its tier" in p for p in check_pieces(planted, make_rules(planted))), "a smoke target among the soak pieces"
    unchecked = makefile.replace('\tpython3 scripts/tier.py piece "$$PIECE" $(OVERNIGHT_TARGETS)\n', "")
    assert unchecked != makefile and any("must first check" in p for p in check_pieces(unchecked, make_rules(unchecked))), "a piece target without its check"
    pasted = makefile.replace('\tpython3 scripts/tier.py piece "$$PIECE" $(OVERNIGHT_TARGETS)\n', '\tpython3 scripts/tier.py piece "$(PIECE)" $(OVERNIGHT_TARGETS)\n')
    assert pasted != makefile and any("must first check" in p for p in check_pieces(pasted, make_rules(pasted))), "make's text pasted into the check"
    print(f"check_docs self-test passed ({len(bad)} planted workflows refused, the piece inputs accepted)")
    return 0


def check_tiers():
    """The testing tiers (OD83, OD144, TESTING.md) are enforced mechanically, so they cannot drift back: the feature gate
    starts no emulator; the epic gate (`make check`, ci.yml) holds only tier 0 and 1 targets and stays inside its budget;
    every boot in ci.yml runs only for the epic gate; the commit hook runs no tests; every soak target is run by soak.yml."""
    out = []
    makefile = read("Makefile")
    feature = make_var(makefile, "FEATURE_TARGETS")
    smoke, soak = make_var(makefile, "SMOKE_TARGETS"), make_var(makefile, "SOAK_TARGETS")
    if not smoke or not soak or not feature:
        return ["Makefile: FEATURE_TARGETS, SMOKE_TARGETS or SOAK_TARGETS is missing"]
    rules = make_rules(makefile)
    for t in feature:
        if t not in smoke:
            out.append(f"Makefile: the feature target {t} is not in SMOKE_TARGETS (the epic gate includes the feature gate)")
        if boots(rules, t):
            out.append(f"Makefile: the feature target {t} starts QEMU; the feature gate is build and host tests only (TESTING.md)")
    if set(smoke) & set(soak):
        out.append(f"Makefile: targets in both tiers: {sorted(set(smoke) & set(soak))}")
    m = re.search(r"^check:\s*(.*)$", makefile, re.M)
    if not m or m.group(1).split() != ["smoke"]:
        out.append("Makefile: `check` must be exactly the smoke tier (`check: smoke`); everything else is `soak` / `check-all`")
    ci, soakyml = read(".github/workflows/ci.yml"), read(".github/workflows/soak.yml")
    allowed = set(smoke) | GATE_EXTRA
    # one job at a time: its make targets are smoke targets, and a job that boots runs only for the epic gate
    for job in re.split(r"^  (?=[a-z][\w-]*:\s*$)", ci.split("\njobs:\n", 1)[-1], flags=re.M):
        name = job.split(":", 1)[0].strip()
        targets = " ".join(re.findall(r"^\s*(?:- )?run: make (.*)$", job, re.M)).split()
        targets += re.findall(r"^\s+(?:[A-Z_]+=\S+ )*make ([a-z][\w-]*)", job, re.M)
        for t in targets:
            if t not in allowed and t not in rules:
                out.append(f"ci.yml: job {name} runs `make {t}`, which is not a target")
            elif t not in allowed and t not in ("os-interactive", "fetch-third-party"):
                out.append(f"ci.yml (the merge gate) runs `make {t}`, which is not a smoke target (TESTING.md)")
        starts = any(boots(rules, t) for t in targets) or bool(BOOTS_RE.search(job))
        cond = re.search(r"^    if:\s*(.*)$", job, re.M)
        if starts and not (cond and SMOKE_IF in cond.group(1)):
            out.append(f"ci.yml: job {name} starts QEMU but is not limited to the epic gate (its `if:` needs `{SMOKE_IF}`)")
        tool = name == TOOL_BUILD_JOB
        if tool and (targets or starts or "mode: fill" not in job or re.search(r"^\s*(?:- )?run:", job, re.M)):
            out.append(f"ci.yml: job {name} is the tool-build job: it may only call the install action in `mode: fill` (no run step, no make, no boot)")
        if tool and not (cond and SMOKE_IF in cond.group(1)):
            out.append(f"ci.yml: the tool-build job {name} runs only for the epic gate (its `if:` needs `{SMOKE_IF}`)")
        budget = TOOL_BUILD_BUDGET_MINUTES if tool else GATE_BUDGET_MINUTES
        for n in re.findall(r"timeout-minutes:\s*(\d+)", job):
            if int(n) > budget:
                out.append(f"ci.yml: job {name}'s timeout is {n} minutes; its budget is {budget} (TESTING.md, \"Budgets\")")
    for wf in ("ci.yml", "soak.yml", "overnight.yml"):
        out += check_ci_qemu(wf, read(f".github/workflows/{wf}"), rules)
    if "matrix:" in ci:
        out.append("ci.yml: the merge gate has a build matrix; that belongs in soak.yml")
    # the commit hook: format, docs check, clippy; never a test or a boot
    try:
        hook = read("scripts/hooks/pre-commit")
    except FileNotFoundError:
        hook = None
        out.append("scripts/hooks/pre-commit is missing (TESTING.md: the commit hook)")
    if hook is not None:
        code = "\n".join(l for l in hook.splitlines() if not l.lstrip().startswith("#"))
        if re.search(r"cargo\s+(?:\S+\s+)*test|nextest|miri", code) or BOOTS_RE.search(code):
            out.append("scripts/hooks/pre-commit runs tests; the commit hook is format, docs check and clippy only (TESTING.md)")
        for t in re.findall(r"make (?:-\w+ )*([a-z][\w-]*)", code):
            if t not in ("fmt-check", "docs-check", "lint", "fmt"):
                out.append(f"scripts/hooks/pre-commit runs `make {t}`; the commit hook is format, docs check and clippy only (TESTING.md)")
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
            out += check_dispatch(name, text)
        out += check_pieces(makefile, rules)
    if not os.path.exists(os.path.join(ROOT, "docs", "reference", "TESTING.md")):
        out.append("docs/reference/TESTING.md is missing")
    return out


LIVING = ["CLAUDE.md", "README.md", "docs/README.md", "docs/specs", "docs/reference", "work", "home"]
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
    "kcore/src/handle.rs": (6, "two process-wide counters of the handle tables: slot growth events and clamped tables (MM5), read by the kernel so a syscall that grew nothing skips the charging pass; and the next table identity, the counter a port binding names the table it was made through by (L21c)"),
    "kcore/src/aspace.rs": (8, "`DRAIN_PENDING` (T207 PR 3c: set when a VMO is dying, read after every call and fault so the common case costs one load; the list itself is under the VMO lock), `NEXT_OWNER` (T207: the counter of address-space owner ids, one fetch_add per space made, never reused), `PAGER_NEED_PENDING` (MM9: set when a pager page was missing, read after every call so the common case takes no lock), `COPY_PENDING`: set when the VMO store logged a page that makes translations stale (MM8), read by the kernel after every call and fault so the common case costs one load; the log itself is under the VMO lock"),
    "kcore/src/jobsched.rs": (9, "per-job CPU and real-time accounting (SCHED-SMP-SPEC 4.10, L8): each hart charges the running thread's job with no lock, one atomic for the CPU total and one that packs the real-time window with its used time so a boundary is one compare-and-swap; tested under real threads in `kcore/tests/jobsched.rs`"),
    "kcore/src/asid.rs": (3, "the allocator's current generation, published for a return to user mode that holds no big lock (L19): written under the big lock when the generation turns, read with acquire"),
    "kcore/src/park.rs": (4, "the park word of the block/wake protocol (PK1), modelled under loom and shuttle"),
    "kcore/src/msiid.rs": (3, "`IdSet`, the identities a hart claimed from its IMSIC file and has not yet delivered: kept in the hart's own record (not on the trap stack, KT13), written by that hart's trap and emptied by its delivery under the lock; atomic only so a shared `PerHart` can hold it, never touched by another hart"),
    "kcore/src/latch.rs": (4, "the one-way latch, modelled under loom and shuttle"),
    "kcore/src/pause.rs": (6, "T284 pause detector ring: one writer (the hart, interrupts off), any reader, one word per sample so no reader sees half of one"),
    "kcore/src/sched2.rs": (2, "`Home`, the thread's hart: written under run-queue locks, read lock-free (L11b, shuttle model)"),
    "kcore/src/sched_smp.rs": (40, "the scheduler's shared state: marks, pokes, hints, transit slots, counters (L11b, shuttle model `rq_concurrency`; the hint's `key` is read and written only under the hart's run-queue lock), and the three global masks, hints published at the release of a run-queue lock (SC2), and the placement policy, one byte read at each wake (PM4)"),
    "kernel/src/arch/aarch64/mod.rs": (4, "the AArch64 stub (AR1): the two statistics the riscv64 layer has (T206), never run"),
    "kernel/src/arch/riscv64/irq.rs": (2, "the count of windows made inside syscalls (T206, a statistic read by `debug_stat`)"),
    "kernel/src/percpu.rs": (95, "per-hart state, touched by its own hart or read by others: the audited per-hart layer (including the FP and vector claims, T166, the process and address-space lock of the running thread for the lock-free calls, L14 and L17, and the per-class magazine lengths of MM4, and the frame cache length mirror of L16, and the hart state of PM1, and the library text range of the running process, AB8, and the depth inside heap::nofail of KT9, and the syscall-window flag of T206: set and read by its own hart only)"),
    "klock/src/aslock.rs": (3, "the installed machine hooks of the address-space lock: one pointer, stored once at boot and read with acquire (L17)"),
    "kernel/src/bkl.rs": (13, "the big kernel lock's owner and checker state"),
    "kernel/src/chaos.rs": (10, "the chaos feature's per-hart generator states, enable flag, point mask and delay counter (KT7: test-build diagnostics, no code without the feature)"),
    "kernel/src/latency.rs": (23, "latency trace counters (diagnostics only)"),
    "kernel/src/pause.rs": (3, "T284 pause detector: each hart's armed deadline (written by that hart only) and its sample ring (diagnostics only)"),
    "kernel/src/user/lifecycle.rs": (4, "the number of threads with suspend state (EX1): a copy of the table's size, read with no lock by `thread::block` so that a kernel thread that blocks holding a lock the big lock must not be taken under pays one load; the table itself is under the big lock"),
    "kernel/src/smp.rs": (12, "hart bring-up and IPI flags, and the two counters of instruction-cache synchronisations (PL7)"),
    "kernel/src/arch/riscv64/iommu.rs": (22, "the IOMMU's on flag (set once at boot after the directory denies every device, read by `build_windows`) and its counters (IOMMU-SPEC 7.3: faults, dropped lines, overflows, the last fault, fence timeouts; the commands are the store's, under the lock; statistics read by `debug_stat` 150 to 157); `Direct` reads and writes the directory and the tables as `AtomicU64` words because the IOMMU reads them while the kernel writes them; the queues and the directory themselves are under the IOMMU lock (class Iommu); `UNFINISHED` (IOMMU6), a destruction begun and not ended as last seen by the store: written only under the IOMMU lock (by a last-handle close, and by `drain` after it settled the destructions), read with no lock by `drain`'s fast path; a stale true costs one settle, and a close's own write of true cannot be overtaken by a stale false because both writes are made under the lock"),
    "kernel/src/arch/riscv64/aia.rs": (34, "interrupt controller statistics (taken, misrouted, no route, the top identity, moves, and IOMMU7's vector notices taken and fired), IOMMU7's flag that a vector has work for the next lock-free drain (a charge due or an entry to rewrite), the per-file work flags and the file-to-hart and hart-to-file maps sized at boot (IOMMU4: written once by `init_hart`, read on the claim path without the lock), and the debug flag that catches a nested open of the route table; the route table itself is under the object lock"),
    "kernel/src/thread.rs": (38, "scheduler ready flags, wake statistics, the watchdog's per-hart sample times and the stack-budget record (KT13: the high water, the first offender's use, its id and its name as two words, the exempt thread); and the count of donations the graph refused (L22); and the flag that lets one `hart_set_online` call run at a time (IOMMU4)"),
    "kernel/src/arch/riscv64/timer.rs": (6, "the clock's zero (the counter value at the first tick), and the counter's frequency and fixed-point multiplier (PL4b), set once at boot and read by `now_ns`"),
    "kernel/src/user/wake.rs": (8, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the deferred-reap flag, the user-mode counters and the exit-cycle record: the part of the old user.rs that wakes and cleans up)"),
    "kernel/src/user/mod.rs": (7, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the user-state slot); `STATE_OPEN` (debug builds: the open-closure flag of T239); `USER_READY`: the user state exists, read by the commit-on-drop of `irq::without` before it touches the state (L20)"),
    "kernel/src/user/trap.rs": (8, "diagnostic counters, the deferred-reap flag and the attention epoch (L12b: kill and stop bump it, a user return reads it) (the attention epoch); the pointer to the allocator's published generation (L19), set once at boot; the count of `clock_get` calls the kernel answers (AB7: a test reads it to see that the library's clock does not enter the kernel)"),
    "kernel/src/arch/riscv64/trap.rs": (14, "the expected-fault test hook and its log words, and the per-hart stale-page retry word (T241), and the count of interrupts taken inside a syscall window (T206, a statistic)"),
    "kernel/src/kstack.rs": (2, "the count of kernel stacks in use, which sizes the allocator's reserve for the allocations that cannot fail (KT9)"),
    "kernel/src/heap.rs": (30, "heap statistics, the growth guard, the frame pool's readiness, size mirror, starvation flag and refused-free counter, and the fault-alloc test hook (armed thread, fail-at, count, fired, exempt thread, persistent mode: test builds only), and the reserve's switch (RESERVE_ON, KT9)"),
    "kernel/src/kmem.rs": (6, "the active-table bookkeeping counters, and the PI futex's lock word read and compare-and-swapped through the direct map (L23); the kernel page table's root, set once at boot (T224)"),
    "kernel/src/user/waits.rs": (2, "the flag that says a PI futex has had a waiter, so a thread's exit looks for owned PI futexes only then (L23)"),
    "kernel/src/dtb.rs": (5, "the device tree's address and length, set once at boot (PL4a), and the boot-argument and device-tree flags"),
    "kernel/src/arch/riscv64/fp.rs": (2, "the vector register length, read once on the boot hart (PL2)"),
    "kernel/src/entropy.rs": (8, "whether the pool was keyed from a real seed, its first output for the boot tests (HD1), and the test-build flag `aslr=off` (HD2)"),
    "kernel/src/arch/riscv64/sbi.rs": (4, "whether the firmware has the debug console extension, probed once; the count of calls to the IPI extension (stays 0, PL6)"),
    "kernel/src/console.rs": (12, "the console's panic flag and line state, the lock-broken flag that stops other harts' output after a panic (KT14), and the UART's address, the firmware-console switch and its two counters (PL5b)"),
    "kernel/src/arch/riscv64/finisher.rs": (3, "the test device's address from the device tree, set once at boot (PL5b)"),
    "kernel/src/interactive.rs": (4, "the interactive build's input flags"),
    "kernel/src/sched_tests.rs": (57, "self-test flags and counters (the deep-recursion test of KT13 adds the use it measured; L1 adds the probe's 99th percentile; T284 the probe's re-sampling record)"),
    "kernel/src/smp_tests.rs": (18, "self-test flags and counters (PM1: the offline test's tick counter)"),
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


# KT9 (T177, T246): class N, the allocations that cannot be refused because nobody could be told, run in `heap::nofail(..)`. The class cannot grow
# silently: a new use is a new row here, with the reason.
NOFAIL = {
    "kernel/src/thread.rs": (2, "the scheduler's spawn tail and the kernel thread registration"),
    "kernel/src/user/lifecycle.rs": (1, "a thread's exit from its PI futexes"),
    "kernel/src/user/trap.rs": (1, "the calls that end or deliver something for a process that exists (kill, exception resume, thread resume, the pager calls): their own tables are prepaid, what they reach is the scheduler's and teardown's (class N)"),
    "kernel/src/user/calls.rs": (2, "the kill of a process and of each process of a job (the caller's own exit, which does not return, is outside)"),
    "kernel/src/user/kobjects.rs": (1, "a kernel thread's admission to the wait table"),
    "kernel/src/user/waits.rs": (1, "the wake of the threads a pager step ended (a wake cannot be refused); the pager tables themselves are prepaid per thread (T248)"),
    "kernel/src/user/wake.rs": (5, "the epilogue's wakes, kills and teardown arms: a wake or a teardown cannot be refused"),
}


def check_nofail():
    """T177: the number of `heap::nofail(` sites per file is the audited one."""
    out = []
    seen = {}
    for d, _, files in os.walk(os.path.join(ROOT, "kernel", "src")):
        for f in files:
            if not f.endswith(".rs"):
                continue
            rel = os.path.relpath(os.path.join(d, f), ROOT)
            n = sum(l.split("//")[0].count("heap::nofail(") for l in open(os.path.join(d, f)) if not l.lstrip().startswith("//"))
            if n:
                seen[rel] = n
    for rel, n in sorted(seen.items()):
        want = NOFAIL.get(rel)
        if want is None or want[0] != n:
            out.append(f"T177: {rel} has {n} heap::nofail site(s), the audited count is {want[0] if want else 0} (scripts/check_docs.py NOFAIL): a new class-N allocation needs a row and a reason")
    for rel in NOFAIL:
        if rel not in seen:
            out.append(f"T177: {rel} is in NOFAIL but has no heap::nofail site")
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


def check_workflows():
    """A plain (unquoted) YAML scalar cannot contain ': '. overnight.yml had one (`run: echo "...: ..."`): GitHub then cannot read the
    whole file, every push shows a red run with no jobs, and `gh workflow run` would fail. No YAML parser is needed for this case."""
    out = []
    wf = os.path.join(ROOT, ".github", "workflows")
    for f in sorted(os.listdir(wf)) if os.path.isdir(wf) else []:
        if not f.endswith(".yml"):
            continue
        for n, line in enumerate(open(os.path.join(wf, f)), 1):
            m = re.match(r"\s*(?:- )?(?:run|name): (.*)$", line)
            if m:
                v = m.group(1).strip()
                if v and v[0] not in "|>'\"[{&*!%@`" and ": " in v.split(" #")[0]:
                    out.append(f"workflows/{f}:{n}: an unquoted value contains ': ' (invalid YAML: GitHub cannot read the file); quote it or use a block scalar")
    return out


# PL4 (KERNEL-V1-SPEC 2.3): nothing in the kernel assumes where RAM is, how big it is, or where the image was loaded; all of it comes from the
# device tree and from the early boot code. The literals below may appear only in test code (the kernel's self-tests and `*_tests.rs`).
RAM_LITERAL_RE = re.compile(r"\bRAM_SIZE\b|\bRAM_BASE\b|\bKERNEL_PHYS\b|\bKERNEL_OFFSET\b|0x8000_0000|0x8000_00|0x80000000|0x8020_0000|0x80200000")


def check_ram_literals():
    out = []
    for top in ["kernel/src", "kcore/src", "abi/src", "ksys/src"]:
        for dp, _, fs in os.walk(os.path.join(ROOT, top)):
            for f in fs:
                if not f.endswith(".rs") or f == "selftest.rs" or f.endswith("_tests.rs"):
                    continue
                path = os.path.join(dp, f)
                for n, line in enumerate(open(path), 1):
                    if RAM_LITERAL_RE.search(line.split("//", 1)[0]):
                        out.append(f"PL4: {os.path.relpath(path, ROOT)}:{n} assumes where RAM or the image is (RAM_SIZE, 0x8000_0000, KERNEL_PHYS, ...): it comes from the device tree and the boot code")
    return out


def check_unsafe_inventory():
    """KT14: every `unsafe` in kernel/src is in the audit list with its current hash."""
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import unsafe_inventory
    return unsafe_inventory.problems()


def check_kernel_allows():
    """KT3 (KERNEL-V1-SPEC 7.4; T279): every allow of a panic-finding lint in the kernel crates carries its `// INVARIANT:` reason."""
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import kernel_allows
    return ["KT3: " + p for p in kernel_allows.problems()]


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
    allowed = {"README.md", "CLAUDE.md"}
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
                    # a closed work item's file is gone by design (`work.py done`); an old spec amendment may still name it, and a spec is not edited
                    m = re.match(r"^work/([A-Za-z0-9-]+)\.md$", ref)
                    if m and re.search(rf"(?m)^\|[^|]*\|\s*{re.escape(m.group(1))}\s*\|", read("docs/records/done.md")):
                        continue
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
                 "work/README.md"]:
        if not os.path.exists(os.path.join(ROOT, name)):
            problems.append(f"{name} is missing")

    problems += check_layout() + check_links() + check_specs() + check_records_append_only()

    problems += check_tiers() + check_atomics() + check_nofail() + check_syscall_tests() + check_unsafe_inventory() + check_kernel_allows() + check_arch() + check_workflows() + check_ram_literals()
    if problems:
        print("docs check FAILED:")
        for p in problems:
            print("  " + p)
        return 1
    print(f"docs check passed ({len(STALE)} stale phrases absent, {len(nums)} decisions, {len(items)} work items)")
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(dispatch_self_test())
    sys.exit(main())
