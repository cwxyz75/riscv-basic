# FS-SPEC: doofs, the disk filesystem (specification, draft v0, 2026-11-15)

**Name:** doofs (a deliberately silly working name, chosen by the owner 2026-11-15). **If we ever want a serious name, the proposal is Cambium** (the growing layer of a tree: new wood added, old rings untouched, which is copy-on-write and generations); switching is a rename in docs, crates and tool names. Tools follow the Unix convention: `mkfs.doofs`, `fsck.doofs`, `dump.doofs`; crates `doofs` (pure format) and `doofsd` (server) are proposed, nothing claims the bare `fsck`.

Status: **draft v0; the four decisions in section 2 are the owner's (2026-11-15); everything else is a proposal.** Nothing is
built. This is the written acceptance contract for slice F (CLAUDE.md: spec, acceptance checks, code, independent review). Basis:
`reviews/2026-11-15-filesystem-survey.md` (Fable's survey, facts from memory, not re-verified). It resolves Q7 (OS-ARCHITECTURE
section 18): **our own format, no ext2.** OD numbers are assigned when the slice merges (S9 has claimed OD102 onward for its own
proposals). Slice C (the wall clock) is split out of it in the roadmap.

## 1. Goal and non-goals

**Goal.** A persistent filesystem served by a userspace `fsd` over `proto::blk`, speaking `proto::file` (plus the attribute and
query operations of section 9), with typed attributes and indexed queries on any node, data integrity checked on every read, and
crash consistency proven by exhaustive power-cut testing. Target storage is flash (SSD/NVMe-like), not rotating media.

**Non-goals (v1).** Snapshots and clones, hard links, users/modes/ACLs on disk, encryption, compression, multiple devices, a
persisted free map, an intent log, live queries, multithreaded `fsd`, writable shared mappings of disk files (needs Q4).

## 2. Decisions

| # | Decision | Owner |
|---|---|---|
| F1 | **Own format**: one copy-on-write B+tree committed by flipping a checksummed root; no journal, no replay | 2026-11-15 |
| F2 | **Queries** use volume-wide indexes; results are filtered to the subtree of the directory connection the caller holds (needs `ENUMERATE`) | 2026-11-15 |
| F3 | **No hard links** in v1; every node has exactly one directory entry (symlinks stay: relative-only, never followed by the server, resolved by the client against the connection they were found through; OD17 as amended, `OS-ARCHITECTURE.md` section 12) | 2026-11-15 |
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
index is expensive and is charged accordingly (hostile-indexing risk, section 14).

## 5. Layers

1. `fsfmt` (pure, `no_std`, `forbid(unsafe_code)`): the B+tree, allocator, commit logic and checker over a block-device trait.
   Built and model-tested first, before any filesystem logic sits on it.
2. `fsd` (server on `rt::service`): block cache, front end for `proto::file`, the single `blkd` client, quota enforcement,
   mount-time free-map rebuild.
3. `proto::blk` grows **discard** and (proposed) a FUA flag; its sectors stay 512 bytes and the crash model assumes tearing per
   sector.
4. `memfs` gains attributes, indexes and queries first (section 9), as the reference model.
5. `volmgr` (server, section 6): finds block devices, recognises doofs volumes, starts one supervised `fsd` per volume and
   publishes each in the service directory. The **mount table** is data (`/etc/mounts`, parsed by the pure `mounts` crate) that
   the shell and launcher use to grant a volume to a program at a prefix. Nothing about a disk is compiled in.
6. `clockd` (server, section 7): the wall clock, `/svc/clock`; `fsd` stamps inodes with it.
7. Tools: `mkfs.doofs` (blank image, or populated from a directory like `mke2fs -d`), `fsck.doofs`, `dump.doofs` (host,
   written by separate contexts where the oracle rule says so), and `volls` (an OS program listing `/svc/vol`).

## 6. Volumes and mounting (proposal; the owner asked for a mount table or equivalent, 2026-11-15: no hardwired images)

There is no global mount table in the kernel or in any shared namespace: every process has its own view (OS-ARCHITECTURE
section 6, SD-SPEC), so "mounting" is **granting a connection at a prefix** in a program's startup namespace, as in Plan 9's
`bind`. What is needed is (a) discovery, (b) a name for each volume, and (c) a table saying who gets which volume where.

* **Discovery: `volmgr`.** A user program on `rt::service` (OD84). It asks `devmgr` for every block device (the table-driven
  `devmgr` already serves several devices by index, DM steps 1 and 2), reads the first sectors of each and **probes** for a known
  superblock (doofs magic, version, checksum; other formats could be added later). It holds only block channels, no hardware
  authority: `devmgr` stays the one holder of the root resource. Probing treats the device as hostile: a bad magic, bad
  checksum or absurd geometry is logged and ignored, never fatal.
* **Naming: names come from the table, never from the disk.** A doofs superblock carries a 16-byte **uuid** and a label
  (1 to 32 printable bytes, set by `mkfs.doofs`); the label is display text only (`volls`). For each recognised volume
  `volmgr` starts one `fsd` holding only that device's block channel, a `clockd` connection and account limits, supervised
  and restarted (a restart remounts from the newest valid superblock, which FI1 makes safe). It publishes the volume as
  `/svc/vol/by-uuid/<hex>` always, and as `/svc/vol/<name>` **only if `/etc/mounts` maps that uuid to a name**; a volume the
  table does not know has no name, so a disk cannot claim a name by calling itself `data`. A duplicate uuid (a cloned disk) is
  refused and logged. While `fsd` is down, clients see `UNAVAILABLE` and retry (SD semantics).
* **The mapping: `/etc/mounts`** (in the boot image, later also on a volume): one line per known volume, `uuid  name  [options]`,
  for example `3f2a9c1e-...  data`; an option can name a subdirectory to publish instead of the root, or a read-only cap. It is
  **a mapping from identity to a local name, nothing more**; it says nothing about who may use the volume. The parser is a pure
  `no_std` crate (`mounts`) with host golden tests and a no-panic sweep.
* **The grant: the manifest (a list).** Who gets which named volume, at which prefix and with which rights is the per-program
  grant list: `/etc/trusted` entries today (a trusted program may be given `/data` from `/svc/vol/data`, rights only shrink),
  the S10 manifest later. The shell and launcher read it when they build a program's startup namespace; a confined program
  with no entry gets nothing. The two files differ in kind (a mapping and a list) and stay two; the manifest's format is S10's
  design.
* **What changes in SD:** SD step 4 stops being deferred: `volmgr` and `clockd` are the first non-network services that need
  the registry, the shell granting a `/svc` view, and `rt::fs` reconnecting on `UNAVAILABLE`. Narrow step 4 to what they need;
  moving `memfsd`, `launcher` and the older servers stays out.
* **Boot order:** `init` starts `devmgr`, `registryd`, `clockd`, then `volmgr`; volumes appear as they are found. A test
  attaches images in any order and expects each to appear under its uuid, and under its name where the table has one.
* **Not in v1:** hot removal (a vanished device ends its `fsd` and the entry goes stale), partition tables or a volume manager
  proper (a doofs volume is a whole device from sector 0), formatting from inside the OS (the host tool does it), root on a
  volume (`/` stays the boot image), multi-volume filesystems.
* **Decided (owner, 2026-11-15):** volumes are identified by uuid; the table maps uuid to a local name; grants are a separate
  list. Label matching is not a mechanism, and a convenience rule like "any disk labelled X" would be an explicit line in the table.

## 7. Wall clock (proposal; the other half of Q11, entropy is done as OD108)

The kernel clock is monotonic (OD91) and stays so: **wall time is a service, not a kernel feature.**

* **`clockd`** on `rt::service`, published as `/svc/clock`. It reads a hardware real-time clock through `devmgr`, which claims
  the device from the device tree like the virtio devices. On QEMU `virt` that is, from memory and **to be verified against the
  device tree at step C1**, a `google,goldfish-rtc` at `0x101000` returning nanoseconds since the epoch.
* **Model.** At start (and again on a slow timer) it reads the RTC and the monotonic clock together and answers
  `now = rtc_base + (monotonic_now - monotonic_at_read)`; re-reading never moves the answer backwards for a client that
  already saw a later time (it slews, a pure `clockmodel` crate with property tests: monotonic per client, bounded error,
  no overflow across years).
* **Protocol.** `Now` returns UTC nanoseconds and a **quality** (`Synced`, `FromRtc`, `Unset`); `Set` needs a `SET` right and
  is for a later network time client. A program holds only the connection its namespace grants.
* **Without an RTC** the quality is `Unset` and the time counts from the epoch at boot. `fsd` then stamps with it and
  the format records nothing about quality; the checker does not treat timestamps as truth.
* **Tests.** QEMU is started with a fixed base (`-rtc base=<date>,clock=vm`) so a boot test can assert the time within a
  tolerance; `clockmodel` is host-tested; a test with no RTC attached checks the `Unset` path.

## 8. Steps

0. **Model first:** attributes, indexes and queries in `memfs` and `proto::file`, with golden tests (this is S10's first
   bullet, moved ahead; it fixes the semantics both implementations must share; symlink and no-hard-link rules, T104, with it).
C1. **Wall clock** (slice C): `clockmodel`, the RTC claim in `devmgr`, `clockd`, `/svc/clock`, the fixed-base boot test.
   Independent of the filesystem: can run alongside steps 1 to 4.
1. The B+tree in `fsfmt` with a random walk against a `BTreeMap` model; mutation-checked; no filesystem yet.
2. The format: superblock ring (with uuid and label), pointers and checksums, commit, mount, the in-memory free map; host
   `fsck.doofs`, `mkfs.doofs`, `dump.doofs`.
3. Files, directories, symlinks, rename, quotas, orphans: differential against `memfs`.
4. The fault-injecting block device and the crash harness; the exhaustive short-workload suite.
5a. **`proto::blk` grows discard and a device-info request (size, sector size)**; `devmgr` reports per-device info; goldens.
5b. **SD step 4, narrowed:** the shell grants a `/svc` view, `rt::fs` reconnects on `UNAVAILABLE`, `volmgr` and `clockd` can
    publish. Its own PR (it touches `init`, the shell and the runtime).
5c. **`fsd`** on the service runtime over one block channel, with the mount-time free-map rebuild and the clock connection.
5d. **`volmgr`**: probing, one supervised `fsd` per recognised volume, `/svc/vol/by-uuid/<hex>` and `/svc/vol/<name>` from the table, `volls`.
5e. **The mapping and the grants**: the `mounts` crate and `/etc/mounts` (uuid to name); the shell and launcher grant named volumes through `/etc/trusted` entries (a writable `/data`
    powerbox entry for trusted programs).
5f. **QEMU**: the harness builds images with `mkfs.doofs` (uuids and labels chosen by the test, attached in any order, with a table line per uuid), restarts `fsd`
    and the machine, and runs `fsck.doofs` on the images afterwards; `-smp 2` and `-smp 4`.
6. Attributes, indexes and queries on disk; then the independent review of the whole slice (Fable, own PR).

## 9. Wire additions (to be derived into `proto` goldens)

Attribute `Get`, `Set`, `Remove`, `List` (typed values: a type code plus bytes), `MakeIndex`, `Query`. A server that does not
support them answers NOT_SUPPORTED, as with `Account` (OD84). Byte layouts are normative in `proto`'s goldens, as for S4 (T27).

### 9.1 The semantics step 0 fixes (normative for `memfs` and for `fsfmt`; the tests of step 0 are their written form)

* **Attributes.** A node has named attributes: a name (1 to 255 bytes, no NUL, no `/`; `name`, `size` and `mtime` are reserved
  for the built-in indexes) and a typed value of at most **1024 bytes**. Types: `0 Bytes`, `1 Text` (must be valid UTF-8),
  `2 U64` (exactly 8 bytes, little-endian on the wire), `3 I64` (exactly 8 bytes). `Set` replaces; `Get` returns type and value;
  `Remove`; `List` returns names in bytewise order. A read-only node refuses `Set` and `Remove` (`ReadOnly`); a missing
  attribute is `NoAttr`; a bad type, length or UTF-8 is `BadValue`. Attribute bytes (name, value and a fixed overhead) are charged to
  the metadata quota (`Quota`). Attributes do not change `mtime`.
* **Time.** The server supplies a wall-clock nanosecond count (`set_time`); a node's `mtime` is the time of its last content
  change: a file's `write` or `set_size`, a directory's entry created, removed or renamed in it; creation counts.
* **Indexes.** `MakeIndex(name, type)` creates a volume-wide index on a user attribute and **back-fills** it; at most 16;
  an existing index is `Exists`; the reserved names need none (always present: `name` Bytes, `size` U64, `mtime` U64). Values longer
  than 255 bytes cannot be indexed: `MakeIndex` fails (`TooBig`, nothing changed) if one exists, and a later `Set` of such a value
  on an indexed name fails the same way. A node whose attribute has a different type from the index is not in it.
* **Key order.** `U64` big-endian; `I64` big-endian with the sign bit flipped (so signed order is bytewise); `Bytes` and `Text`
  bytewise. The built-ins' sizes: a file's length, a directory's entry count, a symlink's target length.
* **Query.** `Query(dir, index, term, after, max)` where `term` is `Eq(v)` or `Range{lo, hi}` (`lo` inclusive, `hi` exclusive,
  either optional), on the encoded key. It returns the **live, linked nodes strictly below `dir`** (the directory itself and
  nodes unlinked while open are never returned) whose key matches, ordered by `(key, node id)`, each with its path relative
  to `dir` (components joined by `/`); strictly after the pair `after` when given; at most `min(max, 1000)`, with `more` true if
  there were further matches. Querying an index that does not exist is `NoIndex`; a term that does not fit the index type is
  `BadValue`. (On a server the caller needs `ENUMERATE` on the directory connection.)
* **Symlinks.** A symlink node holds a target of 1 to 1024 bytes: no NUL, not starting with `/`, components separated by single
  `/` with none empty, each a valid name, `.` or `..`. Creating one is refused (`BadTarget`) if its target could climb above
  **the connection root it is created through**: walking the components from the link's directory, counting a name as one level
  down and `..` as one up, the level may never go below that of the root (`depth` = how far below the root the link's
  directory is). Neither the server nor `memfs` follows a link: `lookup` and `walk` return the link node; reading it gives the
  target (`read_link`). `read` and `write` on a link are `Invalid`. A link is unlinked like a file and has no hard links.
* **Wire.** Operations `ATTR_SET`, `ATTR_GET`, `ATTR_REMOVE`, `ATTR_LIST` (file or directory connections), `MAKE_INDEX`, `QUERY`,
  `SYMLINK`, `READLINK` carry the encodings of `proto::file` (goldens in `proto/tests/wire.rs`).

## 10. Invariants (to add to INVARIANTS.md when built)

* **FI1** The newest valid superblock slot always names a tree in which every block verifies, or mount fails closed.
* **FI2** A block reachable from any retained superblock slot is never overwritten or reused.
* **FI3** After a crash at any point, the remounted state equals the model state at the last commit or at the one in flight.
* **FI4** Everything made durable by a returned `fsync` is present after any later crash.
* **FI5** Rights on a connection only shrink; a query returns only nodes under its directory; space is charged to exactly one
  quota domain and counters equal the sum of their nodes (checked by `fsck`).
* **FI6** Delete and commit succeed on a full volume (the reserve).
* **FI7** A symlink target is relative and stays inside the connection it is read through; the server refuses to create any other.

## 11. Acceptance checks (written before the code)

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
9. **Symlinks:** creating a link with an absolute target, or one whose `..` would climb above its connection's root, is refused;
   a hostile writer in a shared directory plants `link -> ../secret` and `link -> /home/secret`, and a reader holding only that
   directory cannot reach the target by following either (the libc walk fails closed); a legal relative link resolves through
   the same connection, and one that loops is cut off with a bounded depth.
10. **Discovery, nothing compiled in:** the harness attaches two doofs images (fresh uuids) in either order, plus a blank disk
    and an image of random bytes; both appear under `/svc/vol/by-uuid/`, the one the table maps also appears as its name, the
    junk is logged and ignored, and `volmgr` survives; a program gets a volume only through a grant entry, and with none it gets
    nothing.
10b. **No name theft:** an image whose label is `data` but whose uuid the table does not map gets no `/svc/vol/data`; a second
    image with the same uuid as a mapped one is refused and the first keeps its name.
11. **Restart:** killing a volume's `fsd` makes its clients see `UNAVAILABLE`, then succeed after the supervised restart; the
    data is the last committed state; the other volume is unaffected.
12. **Hostile superblock:** valid magic with a bad checksum, a geometry larger than the device, a label with control bytes and
    a duplicate uuid are each refused without a panic, a hang or a leak (leak checks exact).
13. **Clock:** with a fixed QEMU base, `Now` is within a stated tolerance of it and never goes backwards for one client across a
    re-read; with no RTC attached the quality is `Unset`; files written through `fsd` carry the clock's time.

## 12. Limits (proposal; expressed per volume, not as bare constants, T79)

Name 255 bytes, attribute name 255, inline attribute 1 KiB, attribute total per node bounded by its quota, index value 255 bytes,
query result 1000 entries per reply, open connections per account as in OD84.

## 13. Shortcuts to record in `TIDY-UP.md` when built

Flush trusted on QEMU (T44); mount time grows with metadata (free-map rebuild); `GetVmo` on a disk file read-only or copied until
Q4; cache charged to `fsd` not clients; no self-healing (single device); single-threaded `fsd`. Mounting and clock: T105 and T106.

## 14. Risks

The B+tree (own model tests first); writeback of mapped files (Q4); flush taken on trust; mount time and memory; write
amplification under an `fsync` loop; full-volume behaviour; a shared misreading by the implementation and the checker (the
`memfs` differential is the guard, hence step 0); hostile index creation (needs `INDEX` right and a charge).

## 15. Open points

Whether the commit interval is time or dirty-size driven first; whether `proto::blk` FUA is worth having if flush is trusted
anyway; the exact `INDEX` right name and where it is granted; whether step 0 should be its own PR (recommended: yes); the manifest's format (S10); the goldfish RTC facts (section 7) are from memory.
