# FS-SPEC: the disk filesystem (specification, draft v0, 2026-11-15)

Status: **draft v0; the four decisions in section 2 are the owner's (2026-11-15); everything else is a proposal.** Nothing is
built. This is the written acceptance contract for slice F (CLAUDE.md: spec, acceptance checks, code, independent review). Basis:
`reviews/2026-11-15-filesystem-survey.md` (Fable's survey, facts from memory, not re-verified). It resolves Q7 (OS-ARCHITECTURE
section 18): **our own format, no ext2.** OD numbers are assigned when the slice merges (S9 has claimed OD102 onward for its own
proposals).

## 1. Goal and non-goals

**Goal.** A persistent filesystem served by a userspace `fsd` over `proto::blk`, speaking `proto::file` (plus the attribute and
query operations of section 7), with typed attributes and indexed queries on any node, data integrity checked on every read, and
crash consistency proven by exhaustive power-cut testing. Target storage is flash (SSD/NVMe-like), not rotating media.

**Non-goals (v1).** Snapshots and clones, hard links, users/modes/ACLs on disk, encryption, compression, multiple devices, a
persisted free map, an intent log, live queries, multithreaded `fsd`, writable shared mappings of disk files (needs Q4).

## 2. Decisions

| # | Decision | Owner |
|---|---|---|
| F1 | **Own format**: one copy-on-write B+tree committed by flipping a checksummed root; no journal, no replay | 2026-11-15 |
| F2 | **Queries** use volume-wide indexes; results are filtered to the subtree of the directory connection the caller holds (needs `ENUMERATE`) | 2026-11-15 |
| F3 | **No hard links** in v1; every node has exactly one directory entry (symlinks stay, never followed by the server) | 2026-11-15 |
| F4 | **Durability**: `fsync`/`sync` is a full commit; data is always written copy-on-write; rename-over is atomic. A btrfs-style tree log may come later | 2026-11-15 |
| F5 | **Who pays**: persistent quota domains on directory subtrees for disk space; the block cache is charged to `fsd`'s own job with a cap until pager-backed VMOs (Q4) allow per-client charging | 2026-11-15 |

F3, F4 and F5 are provisional: they are recorded in `TIDY-UP.md` as T94, T93 and T95 to be reconsidered, and before the
format is frozen we decide whether it reserves room for each (a log-root field, a link count behind the parent id).

## 3. Format (proposal)

* **Blocks** are 4 KiB. Tree nodes are one block.
* **Superblocks:** two rings of four slots (start and end of the device), written round robin. A slot holds magic, format version,
  checksum algorithm id, generation, root pointer, counters and its own checksum. Mount takes the newest slot whose checksum and
  root verify; a torn slot is simply invalid.
* **Pointer** (32 bytes): block address, birth generation, 16-byte check field (CRC-32C in v1, sized for an AEAD tag later).
  Every node also carries its own address, generation, owner and magic, so a misdirected write is caught twice.
* **Keys** `(object id, type, sub-key)`, all unique. Types: inode (kind, size, times, **parent id**, quota domain), directory
  entry, attribute (name; typed value, inline up to about 1 KiB), extent (offset, length, per-block checksums; used by files and
  by large attributes), symlink text, index definition, index entry `(index id, value, object id)` with an empty payload, quota
  counter, orphan record (unlinked but open; reaped at mount).
* **Data:** up to about 1 KiB inline, otherwise extents (capped near 1 MiB). Always written to fresh blocks.
* **Allocation:** the free map is **in memory and rebuilt at mount** by walking the tree (reachability is the truth). Moving
  cursor; metadata and data from separate regions. A reserve guarantees that delete and commit succeed on a full volume.
* **Commit:** write new blocks, `Flush`, write the next superblock slot, `Flush`. Triggered every few seconds, at a dirty
  threshold, and by `sync`/`fsync`. Blocks freed in commit N are reused, and discarded, only when no retained superblock slot can
  reach them.
* **Integrity:** every block is verified against its parent's check field before use, file data on every read from disk. Any
  mismatch is `IoError` and the mount turns read-only (never retry-until-it-works). Scrub is a tree walk.
* **Security:** the disk holds no identities; authority is the connection and its rights (rights only shrink). A pointer is
  committed only after its block was written, and holes read as zero, so freed blocks never leak through the filesystem.
  Remanence on flash is not addressed without encryption (later; needs Q11).

## 4. Indexes and queries (F2)

An index is created explicitly for `(attribute name, type)` through a right of its own (`INDEX`) and is **back-filled** for
existing nodes; name, size and modified time are built in. Indexed values are at most 255 bytes. A query names one indexed term
(equality or range) and is sent on a directory connection holding `ENUMERATE`; the result lists only nodes under that directory
(a walk of parent ids), as relative paths, bounded in count and charged to the caller's account. Creating and back-filling an
index is expensive and is charged accordingly (hostile-indexing risk, section 12).

## 5. Layers

1. `fsfmt` (pure, `no_std`, `forbid(unsafe_code)`): the B+tree, allocator, commit logic and checker over a block-device trait.
   Built and model-tested first, before any filesystem logic sits on it.
2. `fsd` (server on `rt::service`): block cache, front end for `proto::file`, the single `blkd` client, quota enforcement,
   mount-time free-map rebuild.
3. `proto::blk` grows **discard** and (proposed) a FUA flag; its sectors stay 512 bytes and the crash model assumes tearing per
   sector.
4. `memfs` gains attributes, indexes and queries first (section 7), as the reference model.

## 6. Steps

0. **Model first:** attributes, indexes and queries in `memfs` and `proto::file`, with golden tests (this is S10's first
   bullet, moved ahead; it fixes the semantics both implementations must share).
1. The B+tree in `fsfmt` with a random walk against a `BTreeMap` model; mutation-checked; no filesystem yet.
2. The format: superblock ring, pointers and checksums, commit, mount, the in-memory free map; host `fsck`.
3. Files, directories, symlinks, rename, quotas, orphans: differential against `memfs`.
4. The fault-injecting block device and the crash harness; the exhaustive short-workload suite.
5. `fsd` in QEMU on the existing disk image; `proto::blk` discard; the shell grants a writable `/data` as a powerbox entry.
6. Attributes, indexes and queries on disk; then the independent review of the whole slice.

## 7. Wire additions (to be derived into `proto` goldens)

Attribute `Get`, `Set`, `Remove`, `List` (typed values: a type code plus bytes), `MakeIndex`, `Query`. A server that does not
support them answers NOT_SUPPORTED, as with `Account` (OD84). Byte layouts are normative in `proto`'s goldens, as for S4 (T27).

## 8. Invariants (to add to INVARIANTS.md when built)

* **FI1** The newest valid superblock slot always names a tree in which every block verifies, or mount fails closed.
* **FI2** A block reachable from any retained superblock slot is never overwritten or reused.
* **FI3** After a crash at any point, the remounted state equals the model state at the last commit or at the one in flight.
* **FI4** Everything made durable by a returned `fsync` is present after any later crash.
* **FI5** Rights on a connection only shrink; a query returns only nodes under its directory; space is charged to exactly one
  quota domain and counters equal the sum of their nodes (checked by `fsck`).
* **FI6** Delete and commit succeed on a full volume (the reserve).

## 9. Acceptance checks (written before the code)

1. **Tree model:** a random operation stream (insert, delete, range scan, variable-length values) matches a `BTreeMap`;
   every intermediate tree verifies; three seeded mutations (a split off by one, a missed merge, a stale pointer) are caught.
2. **Crash exhaustive:** for every workload of one to three operations from the ACE-style set (create, write, truncate, rename,
   rename-over, unlink, mkdir, set/remove attribute, `fsync`, `sync`), with a crash after every device write, any subset of
   unflushed writes dropped and sector-level tearing: remount succeeds, `fsck` passes, FI3 and FI4 hold.
3. **Differential:** a random stream of file-protocol operations gives identical results and listings on `memfs` and on
   `fsfmt`, including quotas, errors and attribute queries.
4. **Corruption:** flipping any single bit of a committed image never panics the mount; either mount fails closed, or the read
   returns `IoError` and the mount is read-only. A bit flip inside any file's data is never returned as data.
5. **Independent checker:** a host `fsck` written from this spec by a separate context accepts every crash outcome of check 2
   and rejects hand-made broken images (each FI broken in turn).
6. **Full volume:** filling the volume, then deleting, then committing, always works (FI6); a quota-limited domain refuses
   writes with the right error and does not affect another domain.
7. **QEMU:** in the normal boot test at `-smp 2` and `-smp 4`: write files and attributes, `sync`, restart `fsd` and then the
   whole machine on the same disk image, read everything back; the host checks the image after exit with the independent `fsck`.
8. **Confinement:** a program holding a read-only connection to `/data/a` cannot read, query or enumerate `/data/b`; a query
   from `/data/a` never returns paths outside it.

## 10. Limits (proposal; expressed per volume, not as bare constants, T79)

Name 255 bytes, attribute name 255, inline attribute 1 KiB, attribute total per node bounded by its quota, index value 255 bytes,
query result 1000 entries per reply, open connections per account as in OD84.

## 11. Shortcuts to record in `TIDY-UP.md` when built

Flush trusted on QEMU (T44); mount time grows with metadata (free-map rebuild); `GetVmo` on a disk file read-only or copied until
Q4; cache charged to `fsd` not clients; no self-healing (single device); single-threaded `fsd`.

## 12. Risks

The B+tree (own model tests first); writeback of mapped files (Q4); flush taken on trust; mount time and memory; write
amplification under an `fsync` loop; full-volume behaviour; a shared misreading by the implementation and the checker (the
`memfs` differential is the guard, hence step 0); hostile index creation (needs `INDEX` right and a charge).

## 13. Open points

Whether the commit interval is time or dirty-size driven first; whether `proto::blk` FUA is worth having if flush is trusted
anyway; the exact `INDEX` right name and where it is granted; whether step 0 should be its own PR (recommended: yes).
