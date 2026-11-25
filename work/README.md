# work/: the work queue

Every piece of open work is one file here: a short header and free text. Work lists are kept apart from the documentation
(`docs/`): a spec says what the system is meant to be, a work item says what is left to do. Closed items are deleted; their
line goes to `docs/records/done.md` and their text stays in git history.

`WORK-PLAN.md` at the root is the near-term plan: the order of the next few items and why, in prose, by id.

## Using it

`scripts/work.py` reads the queue (it needs nothing but Python 3):

```
scripts/work.py ready [AREA]       slices and tidy items with no open blocker
scripts/work.py decisions          questions waiting on the owner, and what each blocks
scripts/work.py blocked            items waiting on another item
scripts/work.py show T42 L21       items in full, with what they block
scripts/work.py list --area net    filter by --kind, --area, --horizon, --fixed-by
scripts/work.py areas              counts per area
scripts/work.py graph              blocked-by and fixed-by edges as a Mermaid graph
scripts/work.py next T             the next free number for a prefix
scripts/work.py done T42 "what was done" 245     close an item (deletes the file, appends to docs/records/done.md)
scripts/work.py check              validate the queue (part of `make docs-check`)
```

Read one item or one query at a time; never the whole directory.

## An item

```
---
id: T42
kind: tidy
title: DMA frames outlive a killed driver only by policy
area: drivers
horizon: long
risk: H
blocked_by: [Q-OS10]
fixed_by: [L20]
refs: [OS-ARCHITECTURE, OD14]
---
Free text. For a tidy item: the shortcut, why it was acceptable, the risk, the proper fix.
```

The file is named `<id>.md`. Fields:

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Unique, never reused. A capital letter first, then letters, digits, hyphens. |
| `kind` | yes | `slice` (a planned piece of work with an acceptance check), `tidy` (a shortcut, stand-in or deferred finding: the old TIDY-UP register), `decision` (a question only the owner can answer), `topic` (a direction noted but not committed; needs a spec, an acceptance check and an OD before it becomes a slice). |
| `title` | yes | One line. |
| `area` | yes | One of the areas in `scripts/work.py` (`work.py areas` lists them). |
| `horizon` | yes | `near` (ordered in `WORK-PLAN.md`; at most 15), `long` (planned, not next), `someday` (watch items, ideas, things to do only if they bite). |
| `risk` | no | For tidy items: `H` can hurt the system or its security once untrusted code runs; `M` weakens a design property or will cost real rework later; `L` convenience or polish. |
| `complexity` | no | How tricky the fix is and how much work or planning it needs: `L` a small, local change or a test that calls a function; `M` a contained change across a few files, or real design; `H` concurrency or lock-order logic, an on-disk or wire format, or a change that needs planning first. Set on tidy items. |
| `change_risk` | no | How likely the fix is to introduce subtle bugs (not how bad the shortcut is: that is `risk`): `L` mechanical or covered by existing tests; `M` touches live paths the suite covers well; `H` concurrency, memory ownership, translations or other things the suite cannot easily catch. Set on tidy items. |
| `blocked_by` | no | Open items that must be done (or decided) first. |
| `fixed_by` | no | Open items expected to resolve this one as a side effect (a debt that a planned slice removes). When that item closes, re-check this one. |
| `refs` | no | Specs, decisions or records it relates to. |

`work.py check` refuses unknown fields and values, a blocker or fixer that is not an open item (so closing an item makes
you update what pointed at it), cycles in `blocked_by`, and more than 15 near-term items.

## Ids

- `T<n>`: tidy items (the numbers of the old tidy-up register are kept; "TIDY-UP T42" in older text means `work/T42.md`, or
  `docs/records/done.md` if it is closed). New ones take `work.py next T`.
- `L<n>`: the scheduler and SMP tasks of `SCHED-SMP-SPEC.md` (task n).
- Other slices by name: `SD4`, `R4`, `SIG2`, `S10-UNION`, ...
- `Q-<source><n>` or `Q-<NAME>`: decisions. `Q-OS4` is question Q4 of OS-ARCHITECTURE section 18 as it stood; `Q-UV` and
  `Q-SH` are USER-VIEW-SPEC and SHELL-SPEC questions.
- `X-<NAME>`: topics.

## Rules

- A shortcut, stand-in, piece of ambient authority or deferred review finding becomes a tidy item **in the same PR** that
  takes it (what, why acceptable, risk, proper fix and when).
- Close an item only when its fix is verified (for kernel work: booted in QEMU; for SMP work at `-smp 2` and `-smp 4`).
- An item lives in one place. Other items, specs and code refer to it by id; they do not copy its text.
- Design belongs in a spec (or an amendment to one) and decisions in `docs/records/decisions.md`, not in an item. An item
  may say "see FS-SPEC 8" instead of restating it.
