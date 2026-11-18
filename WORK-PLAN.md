# Work plan

The near-term plan: what comes next and why, by work-item id (`work/`, `scripts/work.py`). Items on it have
`horizon: near`. When the plan is finished or replaced, the old text goes to `docs/records/` as a dated record and this file
starts again.

Earlier plans: `docs/records/work-plan-2026-11-17.md` (plans 1 and 2, both finished).

## Now (2026-11-18)

1. **Documentation and work-queue reorganisation** (agreed with the owner 2026-11-18). Done in PRs 232 (the work queue),
   233 (`docs/`: specs, reference, records; progress out of the specs) and 234 (review dispositions). Left:
   - `T151`: re-check the migrated tidy items against the code, one area at a time.
   - `T150`: the review findings with no recorded outcome.

## Next: the owner's choice

Plan 2 ended with the next slice left to the owner. The candidates (`work.py ready` lists all):

- `SD4`: servers publish in the registry, clients reconnect.
- `SH6` (blocked on `Q-SH2`), `T139` (the rest of the compat shell).
- `SIG2`: signals phase 2 (a kernel slice: Fable review and the soak label).
- `LIBC2`, then `PORT-GNU`.
- The scheduler tasks (`L11` onwards): do not start the kernel locking tasks without the owner's go-ahead.

Decisions waiting on the owner: `work.py decisions`.
