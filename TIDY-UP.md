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
| T1 | L | **Per-job limits now cover processes, threads, memory pages, child jobs, objects and queued IPC bytes (S4.5, S4.5c).** Still not charged: kernel stacks (bounded by the global thread cap), the memory of kernel objects themselves, and page tables are an estimate. The global caps remain as backstops. Credits for destroyed objects are applied lazily (before the next charge, or when a ledger is read), so usage can read high for a moment. | The dangerous resources are all limited per job. | Charge kernel stack and object memory to the job; count page-table frames exactly. |
| T2 | M | **Ambient creation syscalls remain**: any process may `vmo_create`, `socket_create`, `channel_create`, `event_create` (bounded by its handle table and the global object cap) and `debug_write`; `process_create` is ambient but limited by the caller's job (a program's job allows no further process). | Needed by every program; the dangerous ones (processes, threads, memory) are now job-limited. | Gate object creation by the job's object quota (T1); restrict `debug_write` to a console capability. |
| T3 | M | **Trusted programs (`ls`, `cat`) get a standing read-only root** via `/etc/trusted` (S4 spec section 8). Contradicts least privilege in spirit, though allowed by OD20; and the list is baked into the read-only boot image at build time, so it is hardcoded in practice, not a user-editable config as OD20 intends (product owner accepted this for S4). | Lets the S4 acceptance session use absolute paths without a powerbox protocol. | **True powerbox**: the shell opens the path arguments itself and passes just those handles (argument syntax that references handles), no standing root; `/etc/trusted` goes away. With the manifest-driven shell (S10). |
| T5 | M | **`MANAGE` is coarse** (map, start, thread_create on a process). A kill-only right (`KILL`, S4a-1) now exists, but process rights are not otherwise split as OS-ARCHITECTURE section 9 wants (start, kill, read/write memory separate). | Enough for S4. | Split `MANAGE` (start / map / threads / debug-memory) when a second consumer needs it; at the latest before ported software. |
| T6 | M | **User-printed `##TEST` names are trusted** by the kernel's output parser; the serial bridge escapes `##` at line start (S4) but `debug_write` by test programs does not. | Test harness only. | With T2: `debug_write` only from the harness; structured results over a dedicated channel instead of the serial line. |
| T8 | L | **Handle tables are sized for the worst case** (4096 entries per process, about 4.6 MiB over 64 processes), allocated up front. | Fits the 8 MiB heap today. | Grow on demand with a hard cap; fits better once quotas exist (T1). |

### Test aids, runtime and format debts

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T25 | L | **`init` still has test-aid commands** (`raw`, `desync`, `big`, `rights`, `quitx`, `run`) that exist so the serial test can probe the bridge. | v0 supervisor with no shell yet. | Remove or gate them behind a test build when `init` stops reading the serial socket (S4d/S4e). |
| T26 | L | **The user heap lock yields but is a spin lock held across several syscalls** while growing; one reserved range, no shrinking, lazy growth means frame exhaustion shows up as a fault on first touch, not a failed allocation. | One program thread today. | Per-thread arenas or a futex-style lock; account heap frames to a job (T1). |
| T28 | M | **A channel's WRITABLE signal means "room for some message"**, not "room for this one": a 32 KiB reply to a client whose queue holds 230 KiB is refused with SHOULD_WAIT while WRITABLE is set. Servers must not wait on WRITABLE for backpressure (memfsd polls). | Simple signal model (S3a). | Make WRITABLE precise for a stated message size, or add a "space available" wait with a size, before more servers are written. |
| T29 | L | **`GetVmo` hands out the whole boot image** (read-only) plus the file's extent: any client allowed to read a boot-image file can read every other boot-image file. | The image holds only public programs and fixtures. | VMO slices/children (`vmo_create_child`) so a file's VMO is exactly that file. |
| T30 | M | **The launcher serves exactly one client and exits on any protocol violation** from it (an oversized message, a client that never reads a reply): with `sh` as the only client that takes the whole stack down (init rebuilds it). | One client in S4; the spec's "close that client connection" is the same thing with one client. | Multi-client launcher on a port, as memfsd does, once a second client exists. |
| T37 | M | **S5a leaves the K5 locks half done and some spec items deferred.** The console has a reentrant UART lock (a formatted print is one critical section; the panic path breaks it), but the heap is still an interrupt-off spin lock only (safe because the allocator's own spin lock already excludes other harts; the lock-order checker comes with the BKL in S5b). Secondary harts park in `secondary_main` rather than in pinned idle threads (the scheduler does not run on them until S5c1). Harts that fail to start or are found running are logged and skipped. | Secondaries touch no shared state in S5a. | S5b adds the BKL, the lock-order checker and the debug asserts; S5c1 gives each hart an idle thread. |
| T31 | L | **No start gate for pipeline stages; the Ctrl-C race is closed another way.** A stage runs at once and `sh` registers the job right after. A Ctrl-C before the acknowledgement reaches `consoled` with no job, but its 0x03 byte waits on the terminal and `sh` reads it after the acknowledgement and kills the stages itself (tested with `spin` and Ctrl-C in one write). What remains: a program can use the few milliseconds to do harm (nothing more than any program can); and the 0x03 is only seen if `consoled` could deliver it (not under an exhausted IPC budget, T2). | Simple, and no `rt` startup change. | The specified start channel, once programs need a gate for other reasons. |
| T34 | L | **Ctrl-D typed while a job runs is delivered to the shell later** (the job does not read it) and then ends the shell; `cat` on the terminal consumes it as data. Terminal type-ahead of 0x03/0x04 during a job is not discarded. | Matches the documented "no end of stream on the terminal"; harmless. | Discard lone 0x04 delivered while a foreground job is registered, once the console knows whether the job reads the terminal. |
| T35 | L | **Gaps in the S4d/S4e tests**: no program exits early on a closed pipe (exit 141 untested); whole-stack restart after a service death and a bad halt message are untested; consoled's NoMemory retry and delivery cap are untested. | Needs fault injection (a service that dies on cue, an exhausted IPC budget) that does not exist yet. | A `die` hook in a test build of memfsd and a budget-hog program. |
| T32 | L | **A shell exit rebuilds the whole stack** (memfsd, launcher, consoled, sh), so `/tmp` contents and the shell's directory are lost on `exit`/Ctrl-D. The launcher serves one client (T30) and has no way to hand over. | Simple and matches the death policy; nothing in S4 keeps state across shells. | A multi-client launcher (T30) lets init restart only `sh`. |
| T33 | L | **Programs cannot write files**: `sh` has no redirection and no write program, and the shell's root connection is read-only. | S4's acceptance only reads; `/tmp` is reachable only through test commands. | A `write`/redirection slice with a per-program writable-`/tmp` powerbox entry. |
| T27 | L | **Wire formats beyond the startup message** (console control, `ReadDir` entry packing, file framing) were defined in `proto` and its goldens, not first in the spec; the spec now points at those goldens as normative. | The spec stayed at the level of operations. | Spec appendix with the byte layouts when memfs and consoled land (S4c/S4d). |

### Concurrency and kernel structure

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T9 | H (at S5) | **No kernel lock.** Syscalls run with interrupts off on one hart; this is what makes handle-table and registry updates, the wake-up protocol (check + enqueue + block) and teardown atomic (OD46). | One hart. | S5: big kernel lock first, then per-object locks; re-prove the wake-up protocol; lock-order checker. Every "single hart" comment in the code is a marker for this. |
| T11 | M | **Syscalls are non-preemptible**, including long ones (64 KiB copies, object sweeps that are O(live objects), `reap_vmos`). They can add latency to the real-time band. | Short in practice. | Bound the work per call or make long paths preemptible; measure against the S2 latency test. |
| T12 | L | **Single global wait queue** (OD43): every signal change wakes every waiter, so wake cost is O(waiters) per syscall; a missing notify can be masked by any other (found by the wake-up audit). | Simple, easy to get right. | Per-object wait queues once there are many waiters (S5/S6); keep the audit probes as regression tests. |
| T13 | L | **Fixed 8 MiB kernel heap and hand-tuned caps** that must jointly fit it (OD50). | Works; audited. | Size caps from the heap or let the heap grow from the frame allocator; revisit with T1. |

### Services and userland

| ID | Sev | Shortcut | Why it was acceptable | Proper fix, and when |
|---|---|---|---|---|
| T14 | M | **Serial access through a kernel bridge thread** (S4 spec, OD52), not a user-mode driver with MMIO and interrupt capabilities. The bridge **polls** the UART every tick (no receive interrupt), drops input bytes when the process is not reading (counted), and a kernel `##` line can still split a forwarded chunk only between chunks. | No device-capability objects until S6. | S6: interrupt object + MMIO VMO; `consoled` drives the UART directly; delete the bridge. |
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
| T23 | L | **CI is still serial inside each job and caches nothing** (the kernel checks now run as parallel jobs, about 2 minutes each, and the whole `make check` takes about 4 minutes locally since the OS crates are optimised; the harness has a 240 s per-kernel timeout). | Fast enough now. | Cache the toolchain and QEMU install; parallelise further only if it gets slow again. |
| T24 | L | **The "independent review" is by a model of the same family** as the author; the human does not read the code. Fable-class reviews are expensive and used sparingly (feedback in memory). | The process rule is the best we have. | Keep real independent oracles where they exist (host tools, upstream test suites); consider a differently trained reviewer for the highest-risk milestones. |

## Resolved

| ID | What | Fixed in |
|---|---|---|
| R1 | Kernel heap exhaustion by object table doubling, region count, queued handles (found by the S3d audit) | PR 47 |
| R2 | Vector unit left enabled in user mode on RVA23 (found by the S3d audit) | PR 47 |
| R3 | Mapped VMO destroyed when its last handle closed (S3c-2a review) | PR 43 |
| R4 | `handle_replace` did not wake sibling waiters; killed-before-first-run window; late `thread_exit` overriding a kill code (S3c-2b review) | PR 45 |
| R5 | A user program could panic the kernel with many threads or sockets (reviews of S3c-2b, S3c-3) | PRs 45, 46 |
| R6 | T7: 16-bit handle generation. Now 19 bits, and a slot that has used them all is retired, so a stale handle value cannot become valid again | tidy-up PR |
| R7 | T10: `copy_to_user` results ignored after `verify_writable`. Writes after verification use `copy_to_user_verified` (no allocation, no commit) and report errors | tidy-up PR |
| R9 | T4: the global IPC budget was shared and ambient; it is now a backstop behind per-job queued-data quotas (S4.5c) | PR 68 |
| R8 | T36: `MANAGE` on a job covered setting limits. New `SET_LIMITS` right; MANAGE only makes things inside the job | tidy-up PR |
