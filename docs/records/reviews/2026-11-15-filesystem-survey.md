# Survey: a disk format for `fsd` (2026-11-15)

Design survey for Q7 (owner's decisions: no ext2, our own format, BeOS-style attributes, indexes and `query`,
flash-like storage). Read: OS-ARCHITECTURE 6, 12, 17, 18; INVARIANTS; `memfs`; `proto::file`; `proto::blk`;
SD-SPEC; TIDY-UP T16, T29, T44. Facts about other systems are from memory, not re-verified online; the ones I am
unsure of are marked "(unsure)". Nothing was built or run.

Three things found in the repo that shape the answer: `memfs` has **no attributes, indexes or queries yet**
(section 12 says they are built there first); `proto::blk` has **no discard** and 512-byte sectors (so a 4 KiB
block can tear); and **flush cannot be verified on QEMU** (T44), so the host crash harness is the only real proof.

## 1. Borrow / avoid

| System | Borrow | Avoid |
|---|---|---|
| BFS (BeOS) | Typed attributes on any node, small ones inline in the inode; indexes as B+trees keyed by value; a query must name one indexed term; live queries; no hard links, inode knows its parent | Volume-wide query results (ambient authority); duplicate-key B+tree nodes (the classic bug nest); files written before `mkindex` never indexed; fixed-size metadata journal |
| Haiku | Proof the query model is usable daily; attribute-aware tools | Its compatibility with the BFS layout; (unsure what it changed on disk, I believe nothing substantial) |
| ZFS | Checksum in the **parent pointer** (Merkle tree: catches torn, lost and misdirected writes); uberblock ring; transaction groups; scrub; birth generation in every pointer; MAC stored in the pointer for encryption | Dedup, RAID-Z, ZIL, ARC, send/receive: all out |
| btrfs | One key shape `(object, type, offset)` for inodes, entries, attributes, extents | Extent back-references and qgroups (snapshot space accounting); ENOSPC-on-delete; RAID5/6 |
| bcachefs | "The filesystem is a database of keys in one B-tree type"; explicit space reservations before a transaction | Log-structured 256 KiB nodes plus a journal: too clever for us |
| APFS | Checkpoint ring; crypto-erase by key destruction | Metadata-only checksums (user data unprotected) |
| F2FS | Hot/cold separation of metadata and data; two alternating checkpoints | A cleaner under an FTL that already cleans (log-on-log); the indirection table that fixes the wandering tree |
| LFS / NILFS | Append-only writes are trivially crash-safe | The cleaner: unbounded complexity and write amplification |
| ReFS | "Integrity streams": checksummed data is always written copy-on-write | No offline checker (salvage only); opaque complexity |
| HAMMER2 | Free map is a **rebuildable hint**: reachability from the root is the truth; four rotated volume headers; wide block references | Clustering, its freemap tuning |
| WAFL | Consistency points: batch seconds of work into one atomic flip | Relies on NVRAM for fast `fsync`; we have none |
| XFS | Self-describing blocks (magic, own address, owner, generation) so the checker can cross-examine; extents | Logical journal and its recovery code (large); allocation groups (no parallelism in v1) |
| ext4 | Lessons: rename-over must be durable-or-old (2009), a failed write must not later report success ("fsyncgate") | Physical journal double-writes; in-place metadata |
| Fxfs / minfs / blobfs | Rust, userspace, files served as VMOs; per-volume keys; blobfs's verified-on-read immutable programs | LSM trees and compaction (Fxfs); (unsure of its test method) |
| seL4 / CAmkES | Nothing specific: no canonical filesystem that I know of (unsure) | - |
| KeyKOS / EROS / Coyotos | Whole-state checkpoint equals our commit; **space banks**: the right to consume storage is itself a capability; no identities on disk | Orthogonal persistence of the whole system |
| Plan 9 Fossil / Venti, 9P | A fid is a per-node capability (we have this); hash-addressed pointers | Fossil's fragility (reputed, unsure); content-addressed dedup |
| FSCQ / DFSCQ | A written crash specification: "after a crash the state is one of these" | Proof effort itself |
| Yggdrasil | Crash **refinement** against a simple model, layer by layer; design chosen for checkability (COW) | - |
| BilbyFS, SquirrelFS | Rust typestate so the wrong write order does not compile (a superblock write needs a "flushed" token) | Soft updates as the mechanism |
| CrashMonkey / ACE, ALICE | Exhaustive small workloads (1 to 3 operations) with a crash at every persistence point; most real bugs were in `fsync`+rename/link paths of clever filesystems; state the ordering guarantees applications may assume | - |

## 2. Recommended format (v1)

**Shape: one copy-on-write B+tree, committed by flipping a checksummed root.** Nothing reachable from a
committed root is ever overwritten, so every crash leaves a complete old tree; there is no replay code, and the
checker is "walk from the root". This is the smallest design with a strong oracle, and it makes attribute and
index updates atomic with the file change for free (BFS needed a journal for that).

- **Blocks:** 4 KiB (the VMO page, the flash page, half a `blkd` I/O). Tree nodes are one block.
- **Superblocks:** two rings of four slots (device start and end), written round-robin: magic, generation,
  root pointer, counters, checksum. Mount takes the newest slot whose root verifies. A torn slot is just invalid.
- **Pointer (32 bytes):** block address, birth generation, 16-byte check field. Every node also carries its own
  address, generation and magic (XFS-style), so a misdirected write is caught twice.
- **Keys:** `(object id, type, sub-key)`, all unique. Types: inode (size, times, kind, **parent id**, quota
  domain), directory entry (name), attribute (name; typed value inline up to about 1 KiB), attribute or file
  extent (offset to start, length, per-block checksums), symlink text, index entry
  `(index id, value, object id)` with an empty payload, index definition, quota counter, orphan record
  (unlinked but open, reaped at mount). The object id in the index key means no duplicate keys, ever.
- **Data:** files up to about 1 KiB inline; otherwise extents (capped near 1 MiB). Data is always written to
  fresh blocks, never in place: required for checksums to be honest, and it makes every write atomic per commit.
  Large attributes use the same extent items, so an attribute is a small named fork.
- **Indexes and queries:** an index is created explicitly for `(attribute name, type)` and **back-filled** in
  the creating transaction chain (fixing BFS's wart); name, size and modified time are built in. Indexed values
  are capped at 255 bytes. A query runs on a directory connection, needs `ENUMERATE`, must name one indexed
  term, and returns only nodes **under that directory** (checked by walking parent ids), as relative paths.
- **Allocation:** an in-memory free map **rebuilt at mount by walking the tree**; nothing on disk to get out of
  sync (HAMMER2's lesson, taken to its end). A persisted hint can come later. Allocation is a moving cursor
  (append-friendly, spreads wear), metadata and data from separate regions. A reserve guarantees that delete and
  commit always succeed on a full volume.
- **Commit:** write new blocks; `Flush`; write the next superblock slot; `Flush`. Every few seconds, on a dirty
  threshold, and on `sync`/`fsync` (no intent log in v1: `fsync` is a whole commit). Blocks freed in commit N
  are reusable, and discarded, only once no retained superblock slot can reach them.
- **Integrity:** every block read is verified against its parent's check field before use; data on every read
  from disk into a VMO. v1 algorithm CRC-32C, with an algorithm id in the superblock. Any failure is `IoError`
  and the mount goes read-only (never retry-and-succeed). Scrub is a tree walk; no self-healing with one device
  except the duplicate superblocks.
- **Security:** the disk stores **no users, modes or ACLs**: authority is only the connection, as today.
  Queries cannot see outside the caller's subtree. Space is charged to a persistent **quota domain** (a
  directory subtree, counter in the tree, updated in the same commit), which accounts (slice R) nest under.
  Stale data never leaks: a pointer is committed only after its block was fully written, and holes read as zero.
  Physical remanence on flash is not solved by overwriting; only encryption solves it. Encryption is not in v1,
  but the 16-byte check field is sized to hold an AEAD tag later (needs Q11, entropy, first).
- **SSD choices:** no journal, so no double write; batching bounds the root-to-leaf rewrite per commit; discard
  is batched after commit and never needed for correctness; assume a volatile cache and that writes since the
  last flush can be lost in any subset and torn per 512-byte sector.
- **Left out of v1:** snapshots and clones (birth generations keep the door open; their space accounting is what
  sank btrfs quotas), hard links (the parent id buys scoped queries, simple quotas and a simple checker),
  permissions, encryption, compression, multiple devices, a persisted free map, an intent log, live queries,
  multithreading.

**Oracles.** (a) `memfs`, extended with attributes and queries first, is the reference model: the format crate
must match it operation for operation. (b) Crash harness over a fault-injecting block device: cut at every
write, drop any subset of unflushed writes, tear sectors; the remounted state must equal the model at the last
or the in-flight commit, and everything synced must be there. ACE-style exhaustive short workloads. (c) A host
`fsck` written from the format spec by a separate context, run after every crash case.

## 3. Decisions wanted

1. **Query scope.** Recommend volume-wide indexes with results filtered to the caller's subtree. The
   alternative, indexes owned by a subtree, makes directory renames across scopes cost the whole subtree.
   Accept that filtering leaks a little by timing.
2. **No hard links.** Recommend yes. The POSIX layer returns "not supported" for `link`; few programs need it.
3. **Durability contract.** Recommend: `fsync` is a full commit, rename-over is atomic, data is always
   copy-on-write. Cost: slow `fsync`-heavy workloads and fragmentation under random overwrite. An intent log is
   the later fix.
4. **Who pays.** Recommend persistent quota domains on directories for disk space, moves across domains
   refused; cache pages charged to `fsd`'s own job in v1, with a cap. Charging cache to clients needs the pager
   work (Q4).

## 4. Biggest risks

- **The B+tree** (variable-length items, split, merge, COW) is where the bugs will be; it needs its own model
  tests before any filesystem logic sits on it.
- **Writeback of mapped files** needs dirty tracking, so pager-backed VMOs (Q4); until then `GetVmo` on a disk
  file can only be read-only or copied.
- **Flush is taken on trust** on QEMU (T44), and `proto::blk` must grow discard and should grow FUA.
- **Mount time and memory** grow with metadata because of the rebuild walk; fine for toy volumes, a known limit.
- **Write amplification** of the root-to-leaf rewrite if commits are too frequent (an `fsync` loop).
- **Full-volume behaviour** under COW: the reserve must be proven by test, not argued.
- **A shared misreading** between the implementation and the checker, both AI-written from one spec; the
  `memfs` differential is the guard, so its attribute semantics must be settled first.
- **Hostile indexing:** creating an index and back-filling it is expensive; it needs its own right and charge.
