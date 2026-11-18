# Work plan

The near-term plan: what comes next and why, by work-item id (`work/`, `scripts/work.py`). Items on it have
`horizon: near`. When the plan is finished or replaced, the old text goes to `docs/records/` as a dated record and this file
starts again.

Earlier plans: `docs/records/work-plan-2026-11-17.md` (plans 1 and 2, both finished).

## Now (2026-11-18)

1. **Documentation and work-queue reorganisation** (agreed with the owner 2026-11-18). Work lists out of the documents:
   `work/` replaces `ROADMAP.md`, `TIDY-UP.md` and the old plan; documentation moves to `docs/specs`, `docs/reference` and
   `docs/records`; specs say what the system is, not how far it is built; changes to a spec are dated amendments appended
   to it; records are append-only. Then every review finding that still needs action is checked to have an item, and the
   migrated items are checked against the code (stale ones closed).

## Next: the owner's choice

Plan 2 ended with the next slice left to the owner. The candidates (`work.py ready` lists all):

- `SD4`: servers publish in the registry, clients reconnect.
- `SH6` (blocked on `Q-SH2`), `T139` (the rest of the compat shell).
- `SIG2`: signals phase 2 (a kernel slice: Fable review and the soak label).
- `LIBC2`, then `PORT-GNU`.
- The scheduler tasks (`L11` onwards): do not start the kernel locking tasks without the owner's go-ahead.

Decisions waiting on the owner: `work.py decisions`.
