# Review: the doofs disk filesystem slice (FS steps 1 to 6: `fsfmt`, `doofs`, `fsd`, `rt::blk`, the crash and corruption tests)

Date: 2026-11-16. Reviewer: Fable (fresh context, independent of the author). Scope: `fsfmt/` (all modules), `doofs/`
(`lib.rs`, `attrs.rs`, `fsck.rs`), `init/src/bin/fsd.rs` against `memfsd.rs` and `proto/src/file.rs`, `rt/src/blk.rs`,
`fsd_keeps_files_across_a_restart` in `suite.rs`, and the tests `doofs/tests/{crash,corruption,differential,space,fsck}.rs`,
`fsfmt/tests/{tree,volume}.rs`. Read against FS-SPEC sections 3, 8, 9.1, 10, 11, 12, INVARIANTS FI1 to FI7, TIDY-UP T109 to
T115 and `notes/2026-11-16-overnight.md`. Commit reviewed: `0d693d5` (the clock slice, `1c98ac4`, landed in the worktree
during the review; it touches none of the files in scope).

Method: reading, plus host tests written for this review (`fsfmt/tests/zz_review_tree.rs`, `doofs/tests/zz_review_fs.rs`,
both deleted again; the outputs are quoted in the findings), and a mutation run on a copy of `fsfmt`, `doofs`, `memfs` and
`proto` outside the repository (section "Test quality"). **Nothing was booted in QEMU.** Every `fsd` finding is from the code;
every `fsfmt` and `doofs` finding marked confirmed was reproduced on the host. No tracked file was changed.

Summary: the central mechanism is sound. The pool's retire, reclaim and transaction logic keeps every block of the last
committed root out of reuse in every ordering I could construct, including across a failed commit, a rollback and the
automatic commit-and-retry; the crash harness is a real test of it. The problems are at the edges of that core:

* the B+tree can **write a node larger than a block** when it merges two internal nodes, and a client reaches it with
  ordinary attribute operations: `fsd` panics (finding 1);
* **mount treats a transient read error like a torn slot** and silently goes back one commit, which loses fsynced data
  (finding 2);
* **a commit that fails is retried into the next slot**, so eight failures in a row (a few milliseconds in `fsd`, which
  retries in a hot loop) overwrite every slot, the last durable one included (finding 3);
* `symlink` is two transactions and the hidden commit can publish the first alone (finding 4);
* a **checksum-valid hostile image** panics or hangs mount, `Fs::open`, reads and the checker in many places; no test
  builds one, because a flipped bit never gets past the CRC (findings 5 and 6).

Severity counts: high 3, medium 5, low 9. Confirmed: 1 to 8. Plausible (from the code): 9 to 17.

---

## Confirmed findings (reproduced on the host)

### 1. Merging two internal nodes can produce a node larger than a block: a client panics `fsd` (high)

`fsfmt/src/tree.rs:434-476` (`merge_small`; the size test at 453, the bound replaced at 463).

The test `na.size() + nb.size() > CAP` is made **before** `y[0].0 = kids[b].0.clone()`. The right node's own first bound
and the parent's bound for it are the same key only until the right node loses its first child (`Del::Empty`,
`kids.remove(0)` at 414): after that its stored first bound is its old second bound, which may be much shorter than the
parent's. The merge then grows the node by up to `MAX_KEY - 1` bytes after the fit was checked. `Node::encode` hits
`debug_assert!(self.size() <= CAP)` (`node.rs:115`); without debug assertions the slice writes run past the 4096-byte
array, which is also a panic.

Reached through the public API (scratch `zz_index_churn_panics_the_tree`): one `MakeIndex("t", Bytes)`, 3000 files,
`attr_set` with values of 255 bytes that are mostly zero (each zero is two bytes in the index key, so the key is 522
bytes) on six files of seven and a short value on the seventh, then `attr_remove` in runs of 40 neighbours. With
526-byte entries a leaf holds seven, so a leaf that is down to one entry does not fit its full neighbour, is not merged,
and empties instead: exactly the case above. Output:

```
panicked at fsfmt/src/node.rs:115:9: a node must fit its block
  fsfmt::tree::Tree::merge_small ← Tree::del ← Tree::delete ← doofs::Fs::del ← Fs::attr_remove
```

The same at the tree level with seed 1 of a directed walk (`zz_merge_overflow_directed`, and with 522-byte keys and empty
values). The repository's random walk cannot see it: its keys are at most about 256 bytes and it deletes at random, so
leaves merge long before they empty (150 seeds of an undirected walk with 600-byte keys did not hit it either).

Consequence: any client with `WRITE` on any directory can stop the volume's server (`ATTR_SET`, `ATTR_REMOVE`,
`MAKE_INDEX` are all it needs; long file names in the built-in name index are a second route). The tree shape is on
disk, so the same delete panics again after the restart; if that delete is part of an unlink the file cannot be removed,
and if it is part of reaping an orphan (`Fs::open`) the volume no longer mounts. I did not build the last two cases.

Fix: compute the merged node (with the replaced bound) first and test **its** `size()`; or test
`na.size() + nb.size() + kids[b].0.len() - y[0].0.len()`. Make `Pool::write_node` return an error for an oversized node
instead of relying on a debug assertion. Add a directed walk to `fsfmt/tests/tree.rs` (two entries to a leaf, range
deletes, long and short keys mixed).

### 2. One transient read error at mount silently rolls the volume back a commit (high; FI4)

`fsfmt/src/volume.rs:232-266` (the `_ => dev = pool.dev` arm at 264).

`mount` goes to the next older slot on **any** error from `verify_all`, a device error included. Under FI2 a slot with
a valid checksum always has a whole tree after a crash, so the only crash-legitimate reason to pass over a slot is that
the slot itself is torn (and then it does not decode). A read that fails once (`rt::blk::BlkDevice` turns a 10-second
reply timeout, a driver restart or any driver error into `DevError::Io`) makes mount take generation G-1. That succeeds
whenever G-1's blocks have not been reused yet, which is the normal case right after an orderly exit (`fsd` commits when
its last client leaves and stops). The next commit then takes generation G again and overwrites slot G.

Scratch `zz_one_transient_read_error_at_mount_rolls_back_a_commit` (two commits; the device fails the first read of the
newest root once):

```
committed generation 3; mounted generation 2; key b: None
after one more commit and a clean remount: generation 3, key b: None
```

The key was committed (a returned `Sync`) and is gone for good, with no error anywhere. The same path makes real media
damage in the newest tree a silent rollback instead of a failed mount; INVARIANTS FI1 was reworded to allow that ("or
mount takes the next older slot"), FS-SPEC section 10 says "or mount fails closed".

Fix: fall back only for a slot that does not decode. A `Dev` error fails the mount (`fsd` exits, the supervisor
retries). A tree that fails its checks under a valid slot is damage: fail closed (or mount read-only and say so), never
continue writing from an older root. If the fallback is kept as a policy, it must be the owner's decision in the spec,
and the mount must not reuse the generation number of the slot it skipped.

### 3. A failed commit is retried into the next slot; eight failures overwrite every slot, and `fsd` retries in a hot loop (high)

`fsfmt/src/volume.rs:288-315` (the generation is consumed at 293 and not given back at 307-310),
`init/src/bin/fsd.rs:123-133` and `145-153`, `rt/src/service.rs:462-464`.

`Tree::commit` advances `open_gen` before the slot write; when the write or the flush fails the snapshot is released
but the generation stays consumed. The retry therefore writes slot `(G+1) % 8`, the next `(G+2) % 8`, and the eighth
retry writes the slot of the last durable commit. A write that fails may leave the block torn (the crash model already
assumes tearing). Scratch `zz_failed_commits_walk_over_every_slot` (20 good commits, then a device whose slot writes
fail and leave half the new block, then eight `commit()` calls):

```
tear=false: 8 failed commits, then remount: Ok(Some("committed"))
tear=true:  8 failed commits, then remount: Err(NoValidSuperblock)
```

In `fsd` nobody has to ask for those retries: after a failed `sync()` `dirty_since` stays set, `next_deadline()` is in
the past, `port_wait` returns at once and `tick()` commits again. That is a busy loop of commits (flush, slot write) for
as long as the disk fails, eight slots in a few milliseconds. It is also the "retry until it works" that FS-SPEC
section 3 forbids: after a failed flush a device may have dropped the writes it could not make durable, and a later
commit whose flush succeeds then records a root whose blocks were never written (FI1). TIDY-UP T114 (6) records "no
read-only mode yet" but not either of these effects.

Fix: (a) after any failed flush or slot write the volume is read-only until remount, as the spec says: `Volume`
refuses further commits, `Fs` refuses transactions with `Io`, `fsd` clears `dirty_since` and answers `IO_ERROR`; that
alone ends the walk over the slots; (b) as a second guard, a commit never writes the slot that holds `last` (skip a
generation when `gen % SLOTS == last.gen % SLOTS`); (c) a host test with a failing device (there is none in the
repository: no test returns `DevError::Io`).

### 4. `symlink` is two transactions; the automatic commit-and-retry commits the first without the second (medium; FI3)

`doofs/src/lib.rs:591-608` (the comment at 595) with `txn_with` at 410-417.

The link's inode, name and entry are one transaction, the target record and size a second. If the second meets
`NoSpace`, `txn_with` rolls it back, **commits** (the state with a link that has no target), and tries again. The
retry usually succeeds, so the caller sees `Ok`, but the committed state on disk is the half-made link until the next
commit. Scratch `zz_symlink_half_state_is_committed` (a 260-block volume filled to within a few blocks of the reserve,
one `symlink`, then a power cut):

```
free 69: symlink returned Ok(9); after a crash: link Ok(Err(Corrupt)), fsck ["symlink 9 has no matching target record"]
```

at six of the forty fill levels tried. The remounted state is neither the model before the operation nor after it, and
`READLINK` on it is `IO_ERROR`. The second path to the same state: the target transaction fails with `Io` and the
clean-up `unlink` (its result is dropped at 604) fails too.

Fix: one transaction (`insert`'s body and the target in the same closure; the quota check is already before it). More
generally the hidden commit is safe only if every public operation is exactly one transaction: say so at `txn_with` and
keep it true.

### 5. A checksum-valid hostile image panics mount, `Fs::open`, directory reads and the checker (medium)

Values read from disk go into unchecked arithmetic. The build keeps overflow checks on (workspace `Cargo.toml`), so
each is a panic; with them off each wraps into wrong behaviour. Reproduced (scratch `zz_superblock_generation_max`,
`zz_hostile_records_panic`; the images are made with `Tree::insert` on a mounted volume, so every checksum is right):

| Where | What | Result |
|---|---|---|
| `fsfmt/src/volume.rs:236` (mount), `:176` (`check`) | a slot with `generation = u64::MAX` (`Superblock::decode` accepts any non-zero value) | `attempt to add with overflow` in mount and in the checker; with wrapping, `open_gen` is 0 and every later commit is invisible to the next mount |
| `doofs/src/lib.rs:618` (also 627, 628) | an orphan record for a file whose size is above the byte counter | panic inside `Fs::open` (the reap): `fsd` cannot start |
| `doofs/src/lib.rs:506`, `attrs.rs:267` (also `lib.rs:903`, `attrs.rs:275`) | a directory with id `u64::MAX` (`id + 1` for the end of a key range) | `read_dir` and `attr_list` panic: `READDIR` on it kills `fsd` |
| `doofs/src/lib.rs:664` (also 738, 745) | a directory whose entry count is 0 but has an entry | `unlink` panics |
| `doofs/src/fsck.rs:157` | an extent at block `2^60` | `check_fs` panics (`block * BLOCK`) |

Not run but the same kind: `next_id` (`lib.rs:529`, the counter at `u64::MAX`), `Tree::commit` (`tree.rs:120`, a slot
with generation `u64::MAX - 1` mounts and the first commit overflows), `counters.meta + cost` in `insert` (551),
`n.attr_cost - old_cost as u32` (`attrs.rs:223`, 257), `d.size += 1`.

FS-SPEC check 12 and section 6 ask that a hostile device is "refused without a panic". Fix: checked arithmetic on
everything decoded from disk, mapped to `FsError::Corrupt`; `Superblock::decode` refuses a generation above, say,
`u64::MAX / 2`; key-range ends built with a helper that handles the last id (`k_dirent(dir, b"")` to the next tag when
`dir == u64::MAX`); and a host test that builds such images (see "Test quality").

### 6. Parent chains are walked without a bound, and `fsck` does not see a detached cycle (medium)

`doofs/src/attrs.rs:337-356` (`path_below`), `doofs/src/lib.rs:475-486` (`depth_below`), `762-773` (`is_within`),
`doofs/src/fsck.rs` (no reachability check).

All three loops end only at the root (or the directory asked for). Two directories that name each other (A in B, B in
A, parents to match, reachable from nowhere) pass every check `check_fs` makes: each is named once, has one link, the
right entry count, a name record and its index entries. A query on the built-in `name` index from **any** directory
then reaches one of them and `path_below` never returns, pushing a name per step (unbounded memory). Scratch
`zz_detached_parent_cycle`:

```
fsck on the detached cycle: []
query of the name index from the root: Err(Timeout)        (5 s; the thread is still running)
```

`depth_below` (every `SYMLINK`) and `is_within` (every directory rename) hang the same way on a directory whose parent
chain does not reach the root; `fsck` reports that one only when the directory is itself named somewhere else.

Fix: bound each walk (a counter up to the node count, or `MAX_COMPONENTS`-style depth limit, `Corrupt` beyond it), and
give `check_fs` a reachability pass from the root over the entries (every inode is reached exactly once; this also
finds detached trees that are not cycles, which leak quota today).

### 7. A leaf split can leave the left half larger than a block (low today; `fsfmt` only)

`fsfmt/src/tree.rs:76-86` (`cut`), `319-337`.

`cut` stops at the first entry that takes the running total to half, so the left half can be up to half the total plus
one entry. With the tree's own limits (`MAX_KEY` 600, `MAX_VALUE` 1100, an entry up to 1704 bytes) that exceeds `CAP`.
Scratch `zz_leaf_split_with_big_entries`: entries of 1200, 1200 and 1600 bytes, then one of 1704 between the second and
third: `panicked at fsfmt/src/node.rs:115: a node must fit its block`.

`doofs` cannot reach it now: its largest entry is an attribute record of 1293 bytes, and the overflow needs an entry
above about 1360. It becomes reachable the day a larger record exists (FS-SPEC's inline data "up to about 1 KiB" plus a
long key would do). Fix: choose the cut so that both halves fit (walk back one entry when the left half is over `CAP`;
with entries under `CAP / 2` a fitting cut always exists), and assert `MAX_KEY + MAX_VALUE + 4 <= CAP / 2` or lower the
limits.

### 8. A rename moves a symlink above the root it was checked against (low; FI7 as worded)

`doofs/src/lib.rs:672-759`; `memfs` has the same behaviour, so the differential cannot see it.

The climb rule is applied when the link is made (`symlink`, 592-593) and never again. Scratch
`zz_rename_moves_a_link_above_its_root`: `a/b/l -> ../../x` is accepted (depth 2), `rename(a/b/l, /l)` is `Ok` in both
implementations, and the link in the root now fails `check_link_target(target, 0)` (`Err(Dots)`), the very target
`symlink(ROOT, "l2", "../../x")` refuses. Moving a directory that holds links does the same. FI7's enforcement column
says the check in `memfs` and `doofs` is what keeps a target inside its connection; in fact only the client-side walk
(T104) can, also because a link made through a connection on `/a` is read through one on `/a/b`. Either re-check on
rename (a link, or cheaper: refuse to move a link to a shallower depth than its `..` count) or reword FI7 so that
nobody relies on the server.

---

## Plausible findings (from the code; `fsd` was not run)

### 9. `fsd` stamps `mtime` with the time of the last open, not of the change (medium)

`init/src/bin/fsd.rs:176` is the only `set_time` call, inside `add_conn`. A write, truncate, mkdir, unlink or rename
uses whatever the clock was when **any** connection was last added. A file opened once and written for an hour has the
`mtime` of the open (or of someone else's later open). FS-SPEC 9.1: "a node's `mtime` is the time of its last content
change". The built-in `mtime` index and its queries inherit the error. No test reads `mtime` through `fsd`. Fix: set
the time at the top of `handle_request`.

### 10. `MakeIndex`, truncate, unlink and mount read unbounded ranges into memory (medium for `MakeIndex`)

`doofs/src/attrs.rs:303-308`: `make_index` collects **every attribute record of the volume** (name and value, up to
1 KiB each; the metadata quota in `fsd` is 64 MiB) before it filters by name. One request from any writer on a volume
with many attributes can take `fsd` past its heap and abort it. `truncate_blocks` (`lib.rs:901-912`) collects every
extent of the file (about 100 bytes of heap per 4 KiB block: 25 MiB for a 1 GiB file) and deletes them in one
transaction; `verify_all` (`fsfmt/src/volume.rs:25`) does the same for all extents at mount. T110 (3) records that
`range` has no cursor but not that these three callers are unbounded. Fix: scan in chunks with `range_limited`, as
`query` already does.

### 11. A query whose next hit does not fit one reply never advances (low)

`init/src/bin/fsd.rs:577-579`. `encode_hits` stops before a hit that would pass `MAX_IO`; if that is the first hit,
the reply has no hits and `more = true`, and the client's next `after` is the same as before. A path longer than
32 KiB needs about 129 nested directories with 255-byte names, which a writer in a shared directory can make (each
`OPEN` takes 32 components, connections nest). Everyone who queries a directory above it then loops. Fix: answer
`TooBig` (or skip the hit and say so) when a single hit cannot fit; cap the depth of a directory tree.

### 12. An operation the server does not implement leaks the handle that came with it (low)

`init/src/bin/fsd.rs:632` (and the same line in `memfsd.rs`). `op::handles` says `PUBLISH` and `VIEW` carry one handle
and `is_directory_op` lets them through on a directory connection; the `_` arm answers `BAD_REQUEST` without
`close_ep(endpoint)`. The runtime gives the handle to the service ("the callee owns them", `rt/src/service.rs:29`). A
client can fill the server's handle table one request at a time.

### 13. Untested paths: the counter rollback, commit failure, and leaked blocks (low)

Eight of the fourteen mutations tried (table below) survive the whole suite. M18 matters most: if `doofs` stops retiring a block it
no longer uses, nothing notices, because the only "space comes back" test is at the tree level
(`overwriting_forever_does_not_run_out_of_space`) and the differential checks that no reachable block is free but not
the converse. A check that every block of the data region is either free or reachable from the live tree or the last
commit, after every sample of the differential, would pin it. Beyond finding 3, a failed commit leaves the client with no way to know which state is
on disk: the slot may have been written before the flush failed, so a crash after an `IO_ERROR` from `Sync` can come
back with the "failed" commit. That is within FI3 (a commit in flight) but worth a sentence in FS-SPEC section 9.

### 14. `fsck` gaps beyond finding 6 (low)

`doofs/src/fsck.rs`: (a) the next-id record is not compared with the ids in use (`K_NEXTID` at 63 is accepted
unseen): a value at or below a live id makes the next create overwrite that inode; (b) a name record is not compared
with the directory entry that names the node, so a query can return a path that does not exist; (c) `links` above 1 is
only caught for named nodes by the "named but has N links" message, an unnamed node with `links = 2` is neither linked
nor orphan and passes; (d) it loads the whole tree into memory and looks attributes up with a linear `find` per
attribute (quadratic). T113 (1) already says the independent checker does not exist; these are what it should catch.

### 15. `MakeIndex` and `Query` leak a little across subtrees (low; partly recorded in T109 (3), T115 (4), (6))

`make_index` answers `TooBig` when **any node on the volume** has a value over 255 bytes under that name, so a client
confined to one directory learns that such an attribute exists elsewhere; a query's running time depends on matches
outside the caller's directory; sixteen `MakeIndex` calls from any writer use up the volume's indexes for good.
Acceptable for now only because the `INDEX` right is still to come; it should gate all three.

### 16. Small things in `fsd` (low)

* `changed()` runs before the rights check in `MKDIR`, `UNLINK`, `RENAME`, `SETSIZE`, `WRITE` and in every `open`, so a
  read-only client arms the commit timer (the commit is then a no-op: cost only a wake-up).
* `SYMLINK` walks the parent path before any `READ` check and reports `NOT_FOUND` against `NOT_DIRECTORY`: a
  `CREATE`-only connection can probe names. `MKDIR` asks for `WRITE`, `SYMLINK` and `OPEN|CREATE` for `CREATE`.
* `BlkDevice::read_only` is never looked at; a read-only disk fails at the first commit in `Fs::open` (exit 6), which
  is at least closed.
* `Fs::read` reserves `min(len, size)` bytes up front; only `fsd`'s `MAX_IO` check keeps a hostile inode size from
  being an allocation of that size.

### 17. The tree's operations are not atomic on error without a transaction (low; a trap for the next caller)

`fsfmt/src/tree.rs:290-307`, `402-428`: `ins` and `del` retire the old node of each level as they return, and the
root is replaced only at the end. If a later write fails, the old root is still the tree but some of its blocks are on
the retired list and the next `reclaim` frees them. `doofs` is safe because every change runs between `Pool::begin`
and `rollback`; `fsfmt`'s own `Volume` API does not say that a caller must do the same.

---

## Test quality

**Mutation run** (a copy of the four crates, `cargo test -p fsfmt -p doofs`, one mutation at a time):

| # | Mutation | Result |
|---|---|---|
| M1 | `Pool::end` frees every block retired in the transaction, whatever its birth | caught |
| M2 | `Pool::rollback` keeps the retirements | caught |
| M3 | mount does not count data blocks as reachable | caught |
| M4 | a failed commit leaves its snapshot pinned (`Tree::release` removed at `volume.rs:308`) | **survived** |
| M5 | `txn_with` does not restore the counters on rollback (`lib.rs:409`) | **survived** |
| M7 | no in-transaction free in `Pool::retire` | caught |
| M9 | a shrink does not zero the tail of the last block | caught |
| M13 | `verify_all` does not refuse a data block born after the snapshot (`volume.rs:30-32`) | **survived** |
| M18 | `write_block` never retires the block it replaced (`lib.rs:894-896`): a leak until the next mount | **survived** |
| M20 | mount does not filter the candidate slots by the newest one's uuid (`volume.rs:234`) | **survived** |
| M21 | the result of the flush after the slot write is ignored (`volume.rs:306`) | **survived** (no device in the tests ever fails) |
| M22 | `symlink` does not remove the half-made link when its second transaction fails (`lib.rs:604`) | **survived** |
| M23 | `rename` credits the target's cost even when the target is open and stays charged (`lib.rs:716-718`) | **survived** |

Six of the eight survivors are failure or corner paths no test enters (M4, M21: a failing device; M5, M22: a
transaction that fails after it changed something; M13, M20: a built hostile image); M18 and M23 are ordinary
behaviour: a block leak, and a quota check that lets metadata go over the limit when a rename replaces an open file
(the differential never renames over an open target at the quota edge).

**What the existing tests would miss of the findings above:** all of them. Specifically:

* **No test has a device that fails.** `DevError::Io` is never returned in `fsfmt/tests` or `doofs/tests`, so the
  commit-failure path, the rollback after an I/O error, `give_back`, and mount's handling of a read error (findings 2,
  3, 13) are unexercised.
* **The corruption test cannot reach decode logic.** A flipped bit fails the CRC of the block or of its parent's
  pointer, so nothing after the check (`Inode::decode`, counters, parent chains, key ranges, the checker's arithmetic)
  ever sees a bad value. It also accepts "mount refused" for every flip, so a mount that refused everything would
  pass; only `fsfmt/tests/volume.rs` pins the fallback. A hostile-image test needs images **built** with valid
  checksums (ten lines with `Tree::insert`, as the scratch tests did).
* **The crash harness** is the strongest test here, with these holes: (a) a workload without `Fsync` never commits, so
  most of the "every two-operation workload" set checks only that uncommitted work leaves the initial state alone; a
  commit that follows two operations, and writes after a commit that reuse reclaimed blocks, are covered only by the 60
  sampled triples and the nine attribute sets; (b) no symlink operation, no open-and-unlinked node (orphans across a
  crash are tested once, in `space.rs`, without a crash point sweep), no full volume, so the hidden commit of
  `txn_with` never runs under it (finding 4); (c) writes are dropped or torn only at the crash, never failed while
  running; (d) it does not assert FI2 directly (no check that a write after commit N lands outside N's reachable set),
  only through the remount. T113 records (b) in part and the sampling.
* **The tree's random walk** uses keys up to about 256 bytes and values up to 900, far from `MAX_KEY` and `MAX_VALUE`,
  and random deletes (findings 1 and 7).
* **The differential** compares with `memfs`, which shares finding 8 by construction, and has no clock that moves
  between an open and a write as `fsd`'s does (finding 9).
* **`fsd_keeps_files_across_a_restart`** sets attributes and an index before the `Sync` but never reads an attribute or
  runs a query after the restart, so FS-SPEC check 7's "write files and attributes ... read everything back" is half
  done; and its "never committed" half depends on the kill arriving within the 2-second commit interval of the first
  unsynced change (a slow `-smp 4` run under load could commit `late` and fail the test for no fault of the code).

---

## Checked and found sound

* **FI2 in the pool.** For a block born in generation `b` and retired while `r` was open, `reclaim` frees it only when
  no pinned generation is in `b..r`; `Volume::commit` unpins the old `last` only after the slot write and the second
  flush returned, and reclaims after that. `Pool::end` frees only blocks born in the open generation, which no
  committed root reaches, **also after a failed commit**: the failed generation is no longer the open one, so its
  blocks stay retired until the next successful commit even if the "failed" slot did reach the disk. `retire`'s
  in-transaction free matches only addresses taken since `begin`; `rollback` truncates the retirements and returns
  exactly the taken blocks (a block freed and retaken in the transaction is counted once). I found no ordering of
  begin, write, retire, end, rollback, hidden commit and failed commit that frees a block the last committed root or
  the restored root reaches.
* **The automatic commit in `txn_with`** runs after the rollback, on the root as it was between operations; apart from
  finding 4 every public operation is one transaction, the closures can run twice (they capture copies and change
  nothing outside `fs`), and there is no nested `begin`.
* **Commit order**: flush, slot, flush; the slot written is never the newest valid one in a single commit; a torn slot
  fails its CRC; `dirty()` cannot be false after a change (every insert rewrites the root).
* **Mount's free map** counts tree nodes and extent targets, refuses a block reached twice, a pointer outside the data
  region and a data block born after the snapshot; all extent keys start with the tag the volume knows.
* **`verify` on a hostile tree terminates**: levels strictly decrease, a shared subtree fails its bounds at the first
  leaf of the second visit, so the walk is linear in the device; node decode checks every length against the block
  and rejects trailing bytes.
* **The reserve (FI6).** Ordinary transactions cannot go below 64 free blocks; `rollback` and the end of every
  transaction clear `use_reserve`; the deleting paths use at most a few blocks net per transaction because a path
  copied twice in one generation is freed at `end`. Filling a 400-block volume with 128 files, data and 900-byte
  attributes until refusal and then unlinking everything without a commit, with and without the files held open,
  succeeded every time and left `fsck` clean (scratch `zz_fill_then_delete_everything`).
* **Rename**: onto itself, a directory into its own subtree, over a non-empty directory, file over directory and the
  reverse, from or into a directory unlinked while open, over an open target (it becomes an orphan and is not
  credited) all behave as FS-SPEC 9.1 and `memfs` say; the name record, the three built-in index entries and the user
  index entries follow the node by id through rename, unlink-while-open, reap and an attribute's change of type.
* **Query bounds**: the escape is order-preserving and prefix-free; `Eq`, `lo` inclusive, `hi` exclusive and `after`
  map to the right tree ranges; nodes unlinked while open, the directory itself and the root are filtered.
* **`fsd` request handling** matches `memfsd` operation for operation (the diff is the status mapping, `changed()`,
  the new operations and the exemptions for `SYNC` and `ATTR_*`); every client-reachable `expect`/`unwrap` is guarded
  (`split_path` never returns an empty list, the handle count is checked before `endpoint.unwrap()`, the `RENAME` and
  `SYMLINK` split lengths are checked against the payload); paths cannot contain `.` or `..`, and neither `walk` nor
  `lookup` follows a link, so a connection cannot leave its directory by path; rights on `OPEN`, `CLONE` and `ACCOUNT`
  only shrink; a reply over `MAX_IO` cannot be built.
* **`rt::blk`**: block numbers are range-checked before the multiply, a reply of the wrong length is an error, a late
  reply to a timed-out request is skipped by its txid and cannot be taken for the next answer.

## Resolution (2026-11-16, review-fixes PR)

Fixed: 1 (the merge size is checked on the built node), 2 (a device read error at mount is returned, not a rollback), 3 (a failed commit rewinds the generation so a retry reuses the same slot; `fsd` retries after its interval), 4 (a symlink is one transaction), 5 (hostile numbers: generation bound, checked subtractions, saturating key bounds), 6 (bounded parent walks; fsck checks every chain reaches the root), 7 (the split cut minimises the larger half), 9 (`fsd` stamps at each request), 12 (the stray handle is closed). Tests: `fsfmt/tests/faults.rs` (a faulty device, long keys), `doofs/tests/review.rs`. Open: see TIDY-UP T117.
