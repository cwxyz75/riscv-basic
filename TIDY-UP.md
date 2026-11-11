# Tidy-up register (OS track)

Every shortcut, stand-in and "good enough for now" that we have knowingly taken, so that none
of them is forgotten and none quietly becomes the design. An entry says what the shortcut is,
why it was acceptable when taken, what it risks, and what the proper fix is and roughly when.

Rules:
- When you take a shortcut, add an entry here in the same PR. If a milestone's review or audit
  finds a deferred issue, it goes here (and in the decision log if it is a decision).
- When an entry is fixed, move it to "Resolved" with the PR number; do not delete it.
- Severity: **H** can hurt the system or its security once untrusted code runs; **M** weakens a
  design property or will cost real rework later; **L** convenience or polish.
- The decision log (`OS-ARCHITECTURE.md` section 17) records *decisions*; this file records
  *debts*. An item can be both; the decision log points here.

## Open

### Security and isolation

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T1 | H | **No quotas or jobs (Q9).** Fixed system-wide caps stand in: objects (16384), user threads (128), processes (64), regions per address space (1024), IPC bytes (2 MiB, OD49). One process can use a whole cap and starve everyone else. No per-process frame (memory) limit. | Single-user toy; the caps stop kernel panics (OD50). | **Jobs and per-job limits** (processes, threads, objects, frames, IPC bytes), charged to the job; the launcher makes one per program. Proposed as "S4.5", before SMP. Lets several global caps go away. |
| T2 | H | **Ambient syscalls**: any process may `process_create`, `vmo_create`, `socket_create`, `channel_create`, `event_create`, and `debug_write`. | Needed to build the launcher and shell with no job/right machinery yet. | Gate creation by a right on a job handle (with T1); restrict `debug_write` to a console capability; keep it only for the kernel test harness. |
| T3 | M | **Trusted programs (`ls`, `cat`) get a standing read-only root** via `/etc/trusted` (S4 spec section 8). Contradicts least privilege in spirit, though allowed by OD20; and the list is baked into the read-only boot image at build time, so it is hardcoded in practice, not a user-editable config as OD20 intends (product owner accepted this for S4). | Lets the S4 acceptance session use absolute paths without a powerbox protocol. | **True powerbox**: the shell opens the path arguments itself and passes just those handles (argument syntax that references handles), no standing root; `/etc/trusted` goes away. With the manifest-driven shell (S10). |
| T4 | M | **The global IPC budget is shared and ambient**: a confined program can fill it with sockets and make every `channel_write`/`socket_write` in the system return NO_MEMORY. | Prevents heap exhaustion; services retry with a 1 s limit. | Charge the budget to a job (T1); no global budget. |
| T5 | M | **`MANAGE` is coarse** (map, start, thread_create on a process). A kill-only right (`KILL`, S4a-1) now exists, but process rights are not otherwise split as OS-ARCHITECTURE section 9 wants (start, kill, read/write memory separate). | Enough for S4. | Split `MANAGE` (start / map / threads / debug-memory) when a second consumer needs it; at the latest before ported software. |
| T6 | M | **User-printed `##TEST` names are trusted** by the kernel's output parser; the serial bridge escapes `##` at line start (S4) but `debug_write` by test programs does not. | Test harness only. | With T2: `debug_write` only from the harness; structured results over a dedicated channel instead of the serial line. |
| T7 | L | **16-bit handle generation** (`kcore/src/handle.rs`): a stale handle value becomes valid again after 65536 reuses of one slot. Confined to the process's own table. | Cannot cross processes. | Widen the generation (handle values are 32-bit: trade slot bits) or detect wraparound by retiring the slot. |
| T8 | L | **Handle tables are sized for the worst case** (4096 entries per process, about 4.6 MiB over 64 processes), allocated up front. | Fits the 8 MiB heap today. | Grow on demand with a hard cap; fits better once quotas exist (T1). |

### Concurrency and kernel structure

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T9 | H (at S5) | **No kernel lock.** Syscalls run with interrupts off on one hart; this is what makes handle-table and registry updates, the wake-up protocol (check + enqueue + block) and teardown atomic (OD46). | One hart. | S5: big kernel lock first, then per-object locks; re-prove the wake-up protocol; lock-order checker. Every "single hart" comment in the code is a marker for this. |
| T10 | M | **Ignored `copy_to_user` results** after `verify_writable` (`kernel/src/user.rs`, channel/socket/vmo reads): harmless on one hart, but a failed copy under heap pressure consumes the data yet reports OK, and it is a real verify/copy race with SMP. | Cannot race today. | Propagate the error, or resolve the physical pages once and reuse them (S5). |
| T11 | M | **Syscalls are non-preemptible**, including long ones (64 KiB copies, object sweeps that are O(live objects), `reap_vmos`). They can add latency to the real-time band. | Short in practice. | Bound the work per call or make long paths preemptible; measure against the S2 latency test. |
| T12 | L | **Single global wait queue** (OD43): every signal change wakes every waiter, so wake cost is O(waiters) per syscall; a missing notify can be masked by any other (found by the wake-up audit). | Simple, easy to get right. | Per-object wait queues once there are many waiters (S5/S6); keep the audit probes as regression tests. |
| T13 | L | **Fixed 8 MiB kernel heap and hand-tuned caps** that must jointly fit it (OD50). | Works; audited. | Size caps from the heap or let the heap grow from the frame allocator; revisit with T1. |

### Services and userland

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T14 | M | **Serial access through a kernel bridge thread** (S4 spec, OD52), not a user-mode driver with MMIO and interrupt capabilities. | No device-capability objects until S6. | S6: interrupt object + MMIO VMO; `consoled` drives the UART directly; delete the bridge. |
| T15 | M | **Service death policy = rebuild the whole stack** (S4): no service discovery, no reconnect, `memfsd` state lost on restart. | Memory filesystem has nothing to keep; keeps S4 small. | A name/registry service with reconnect, and real state recovery with the disk filesystem (S6+). |
| T16 | M | **memfs keeps file data in the server's heap**, programs are copied through memory objects only for `/bin` (S4 spec section 7). | Simple, host-testable. | Back files with VMOs (`GetVmo` for everything), shared page cache, `mmap` (S11). |
| T17 | M | **No socket half-close**: a terminal has no end-of-file for programs; `cat` with no arguments ends only with Ctrl-C (S4). | Rare in S4. | Add `socket_shutdown` (or a close-write right) before pipelines of real tools matter. |
| T18 | L | **Shell has no redirection, variables, globbing, job control**; one foreground pipeline; confined `CWD` is read-only. | Out of S4's acceptance. | A later slice: redirection through a pump in the shell; creation rights on a writable cwd. |
| T19 | L | **The shell's `PATH` is a fixed `/bin`**; commands are looked up by a plain name; programs are only the boot image. | Boot image is the only program source. | Manifest-driven search paths and a package/installer story (S10). |

### Platform and build

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T20 | M | **Rust target is `riscv64gc`** although the OS targets RVA23 (OD47); the boot test runs on `-cpu rva23s64` but code is not built for it, and Sstc/hardware features are not used (SBI `TIME` is used for the tick). | Nothing needs RVA23-only features yet. | Move the target when a feature needs it (vector, Sstc); decide what the kernel may assume per OD47 and test it. |
| T21 | M | **Timing-based tests follow the host clock** (QEMU tick = host millisecond), so windows have slack and fail on loaded machines (the round-robin kernel on CI flaked twice). | Simple and independent of the code under test. | A virtual-time/icount mode for the timing tests (also needed by S5), with strict windows. |
| T22 | L | **Mutation checks are done by hand** on each milestone. | Cheap enough per milestone. | A small script that applies a list of mutations and requires a failing test for each. |
| T23 | L | **CI is slow (8 to 10 minutes) and runs everything in one job**; the boot-test harness has a 240 s per-kernel timeout. | Works. | Split jobs (home computer, OS boot, AIA, RVA23), cache the toolchain, parallelise QEMU runs. |
| T24 | L | **The "independent review" is by a model of the same family** as the author; the human does not read the code. Fable-class reviews are expensive and used sparingly (feedback in memory). | The process rule is the best we have. | Keep real independent oracles where they exist (host tools, upstream test suites); consider a differently trained reviewer for the highest-risk milestones. |

## Resolved

| ID | What | Fixed in |
|---|---|---|
| R1 | Kernel heap exhaustion by object table doubling, region count, queued handles (found by the S3d audit) | PR 47 |
| R2 | Vector unit left enabled in user mode on RVA23 (found by the S3d audit) | PR 47 |
| R3 | Mapped VMO destroyed when its last handle closed (S3c-2a review) | PR 43 |
| R4 | `handle_replace` did not wake sibling waiters; killed-before-first-run window; late `thread_exit` overriding a kill code (S3c-2b review) | PR 45 |
| R5 | A user program could panic the kernel with many threads or sockets (reviews of S3c-2b, S3c-3) | PRs 45, 46 |
