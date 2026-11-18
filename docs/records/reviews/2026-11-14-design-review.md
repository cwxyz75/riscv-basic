# Design review: OS track (S0-S7), against the five directions

Reviewer: independent pass, read-only, 2026-11-13. Nothing was built or run; every claim below is
from reading docs and code. "Checked in code" means I read the cited lines. Same-family caveat
(T24) applies to this review too.

Verdict in one paragraph: the capability core (handles, rights, jobs, creator-pays quotas, the pure
`kcore`/`proto` crates) is sound and worth protecting. The weak part is everything about *waiting
and waking* and about *who pays when memory or queue space runs out across a trust boundary*. Those
are the same areas S8/S9 and the "many cheap threads" direction lean on hardest, and three of them
(D1, D3, D4) should be settled before S8 code, not after S9 as the roadmap currently suggests.

## (a) Findings, ranked

### D1. High. The wake path wakes every blocked thread in the system after almost every syscall, and the signal/port design leaves no place to attach a targeted wake

- Claim: OD43 (one global wait queue) plus OD48 (stateless, level-triggered ports) plus signals
  that are *computed on demand* make "wake one thread" impossible without redesigning all three
  together; TIDY-UP rates this T12 severity L, which understates it.
- Evidence (checked in code): `kernel/src/user.rs:742-776` calls `notify()` after channel
  read/write, handle close/replace, event signal, socket read/write, port bind/unbind, every
  object creation and every process/job call; `notify()` (`user.rs:1305-1310`) runs
  `cleanup_dead`, `reap_vmos`, then `thread::wake_all(signal_queue)`. Every waiter then runs and
  re-evaluates its own predicate under the BKL (`kernel/src/thread.rs:590-612`). Signals are
  derived, not stored (`kcore/src/ipc.rs` about 600-660: channel/socket/port signal words are
  recomputed from queue state; a port's READABLE is `bindings.iter().any(binding_ready)`), and
  `port_poll_in` (`ipc.rs:990-1020`) is a linear scan with rotation. `futex_wake` also wakes the
  global queue (`user.rs:715-721`, T49).
- Consequence: cost per syscall is O(blocked threads) context switches, each a BKL round trip,
  and O(bindings) per port waiter. With a network server doing several syscalls per packet and a
  BeOS-style thread count, this is quadratic. Multi-threaded servers on one port get duplicate
  deliveries (two waiters see the same level-triggered key; one read returns SHOULD_WAIT).
  Ports are also capped at 256 bindings, one event per syscall.
- Question / direction: Q2; directions 1, 2, 5. Also Q1 (T12 mis-rated; "a missing notify can be
  masked by any other" is a correctness-audit hazard, already noted there).
- Smallest change: keep derived signals, but make every mutating `kcore::ipc` operation return
  the set of object ids whose signal word may have changed; give each object a waiter list
  (threads in `object_wait`/`channel_call`, and ports bound to it) and wake only those. Port
  bindings then become registrations on the object (OD48's "holds no reference, queues nothing"
  has to go; they can stay level-triggered). This stays pure and model-testable. Do it before
  S8, as its own slice with the OD73 microbenchmark as acceptance. Confidence: high on the
  mechanism, medium on the size of the cost (not measured).

### D2. High (for direction 2). Kernel state is global by construction, so "per-object locks on the hot paths" is not an incremental rung

- Claim: the rung-3 plan in OS-ARCHITECTURE section 7 assumes locks can be pushed down into
  objects; the code and the pure-crate API shape assume one owner of everything.
- Evidence (checked in code): one `UserState` (`user.rs:97-121`) reached through
  `state() -> &'static mut` (`user.rs:148`); every `kcore::ipc` function takes
  `&mut Registry` for the whole object arena plus `&mut Jobs`; the job ledger lives *inside* the
  VMO store (`kcore/src/aspace.rs:96-104`); the cycle sweep needs every handle table as roots in
  one atomic step (OD37/OD44, `kcore/src/object.rs:279-333`, `user.rs:127-133`, `803-815`);
  destruction is deferred through a graveyard and a credit queue drained "after each syscall"
  (`user.rs:1317-1364`, OD45, OD59); `reap_vmos` scans every region of every process
  (`user.rs:1374-1398`) because a mapping holds no reference on its VMO. The scheduler has one
  global run queue (`kcore/src/sched.rs:41-80`, `226-243`; S5-SPEC section 4 says so) although
  ROADMAP ticks "per-hart state and run queues" and OS-ARCHITECTURE sections 7 and 8 promise them.
- Consequence: fine-grained locking invalidates the model tests' shape (they drive a single
  `&mut` world), and the "share nothing, messages between harts" variant conflicts with a shared
  registry, shared handle tables per process and reachability sweeps.
- Question / direction: Q2, direction 2 (and 1).
- Smallest change: decide the target before any lock work. My recommendation: keep one lock for
  object/handle/VMO/job state (it is short, pure code) and carve out only what measurably hurts:
  (1) the scheduler and wait/wake path with real per-hart run queues under their own lock,
  (2) user copies and page zeroing done outside the big lock (pin, copy, re-lock),
  (3) mappings hold a VMO reference so `reap_vmos` disappears, (4) decide whether the
  stop-the-world cycle sweep stays (it is compatible with a retained object lock, not with
  per-object locks). Log it as an OD that supersedes rung 3. Confidence: high on the evidence,
  medium on the recommendation.

### D3. High. A page fault on a VMO whose job cannot be charged kills the thread that touched it, whoever owns the VMO; there is no way to pre-commit

- Claim: the memory model makes mapping a VMO received from another party unsafe in both
  directions, which contradicts the spec's "bulk data goes through shared VMOs" (5.3, 11, 12, 13).
- Evidence (checked in code): `Vmos::page_for` charges `vmo.job`, the creator's job
  (`kcore/src/aspace.rs` about 323-345); an unresolved user fault is `exit_process`
  (`user.rs:641-676`); SYSCALLS.md `vmo_create`: "refused when it is first touched ... or as a
  fault that ends the process". There is no commit, pin or decommit syscall and no VMO resize.
  Today nobody maps a foreign VMO (the launcher uses `vmo_read`), so this is latent.
- Consequence: (a) a client at its page limit hands a server a VMO; the server maps and touches
  it and dies. (b) a client maps a server's VMO (`GetVmo`, later `mmap`, T16) and commits pages
  charged to the *server's* job. (c) OD72's "the heap refuses first, so exhaustion is a failed
  allocation" holds only for one program: per-program limits (136 MiB) sum past the `apps` job
  (160 MiB) and RAM (256 MiB), so an ancestor limit still surfaces as a fault-kill (T26).
- Question / direction: Q1 (trust boundary, T26); Q2 directions 3 and 5, and S11 `mmap`.
- Smallest change: add `vmo_commit(vmo, offset, len) -> NO_MEMORY` (charge now, fail cleanly),
  and write the rule into INVARIANTS: a server maps only VMOs it created or has committed, and a
  VMO handed out for mapping is fully committed or read-only. Decide separately who pays for
  pages of a shared file VMO. Confidence: high.

### D4. High (S9). The network socket model is one sentence, and the kernel socket object lacks what a TCP endpoint needs

- Claim: "Sockets are handles ... byte streams use the kernel socket object" (OS-ARCHITECTURE 13)
  does not say how connect results, errors, accept, datagrams or options travel, and OD59's
  creator-pays rule inverts for inbound connections.
- Evidence: docs: section 13, OD23, OD48, OD59, OD63. Code: `SocketEnd` is a 64 KiB byte buffer
  with peer/shut flags only (`kcore/src/ipc.rs:74`, `686-712`); user signal bits can be set only
  on events (SYSCALLS "Signals"); there is no call to read a socket's fill level; both ends of a
  pair are charged to the creating process's job (`socket_create_in`, `ipc.rs:686-712`).
- Consequence: RST versus clean EOF, connect completion and SO_ERROR have no channel; UDP has no
  defined carrier (a channel message fits: 65536 bytes versus 65507 payload plus address, but it
  is tight and undecided); if the stack creates the pair on accept, the stack's job pays for
  every inbound buffer, so a remote peer can exhaust the stack's quota; the stack cannot size its
  advertised window from the application's buffer and must double-buffer.
- Question / direction: Q2, directions 3 and 5.
- Smallest change (a design decision, before S8 so the native protocol is right first time):
  a network endpoint is a control channel speaking the native protocol plus, for streams, a data
  socket; the *client* creates the socket pair and passes one end in `Connect`/`Accept`
  (pre-posted accepts), so the client's job pays, as OD59 intends; datagrams are messages on the
  control channel; errors and state changes are control messages. BSD sockets in libc are then a
  shim over exactly that.

### D5. Medium. OD35 says "AIA, not the PLIC" (decided), the code and tests say PLIC is the only interrupt path

- Evidence: OS-ARCHITECTURE section 4 and OD35 versus OD64, S6-SPEC section 3,
  `kernel/src/plic.rs:1-6`, T43; the default test machine is plain `virt`, AIA is a separate
  target where drivers poll (`Makefile:161-162`, `scripts/os_boot_test.py:372-386`). Interrupts
  are delivered to the boot hart only.
- Consequence: on the "decided" controller virtio-net receive is a 1 ms poll; on the real one it
  is hart-0 only. Not a blocker for S8 acceptance (ping, UDP echo) but the decision log is wrong.
- Q1 (docs contradict code); Q2 direction 5.
- Change: owner decides: PLIC is the supported controller and AIA is polling-only (amend OD35),
  or an APLIC backend is scheduled before S9 throughput work.

### D6. Medium. `devmgr` and the block path are single-device, single-client, one-driver-at-a-time; they are not the driver manager of section 11

- Evidence (checked in code): `init/src/bin/devmgr.rs:219-295`: takes the first virtio-blk slot,
  serves one channel with a blocking `object_wait`, starts `blkd` per open, exits when its one
  client goes; `reply()` ignores a failed write (`devmgr.rs:181-196`). Kernel side: one fixed
  device window, `MAX_IRQ` 8, 64-page DMA objects, 4 MiB system DMA budget, no exclusive claim
  per slot (T41).
- Consequence: virtio-net needs a second device class, a long-lived driver with many clients
  behind a stack, and more DMA (receive buffers). Copying the blk pattern gives a second special
  case.
- Q2 direction 5 (and 4: the driver supervisor is where T42 lives).
- Change: before virtio-net, make `devmgr` table-driven (device id to driver image, handles,
  DMA budget), multi-client on a port, and give it the exclusive-claim record T41 asks for.
  Decide whether driver and stack are one process (simpler, fewer copies) or two.

### D7. Medium. `channel_call` is the designated fast path and donation hook, but no real client uses it, and there is no server-side primitive a hand-off could target

- Evidence (checked in code): the only users of `channel_call` are the wrapper in
  `rt/src/lib.rs:389` and the test suite. `rt::fs::Fs::call` is `channel_write` +
  `object_wait` + `channel_read` of *any* next message (`rt/src/fs.rs:55-144`); servers do
  `port_wait` then `channel_read` (two syscalls; `memfsd.rs:731`, `consoled.rs:306`) or
  `object_wait` then read (`devmgr.rs:245`, `blkd.rs:363`, `launcher.rs:64`). S4-SPEC section 9
  promised "a service client helper over `channel_call`".
- Consequence: OD73 step 4 (direct hand-off) and priority donation would optimise a path nothing
  takes; a hand-off also needs a thread blocked in "receive on this channel", which a thread in
  `port_wait` is not. Sharing a connection between threads is only safe today because libc holds
  one global fd lock across blocking calls (T51; `libc/src/fd.rs:44-48`, `86-91`).
- Q1 and Q2, directions 1 and 3.
- Change: move `rt::fs` and the service clients to `channel_call` now; design the server half
  with it (a `port_wait` that returns the message, or reply-and-wait), in the same slice as D1.

### D8. Medium. Backpressure is "poll on a timer" (T28), and each server solves it differently

- Evidence: T28; OD54; S4-SPEC section 7 (10 ms poll, 30 s drop); `consoled.rs:260` waits on
  WRITABLE for 20 ticks; `devmgr.rs:181-196` drops the reply; the launcher exits (T30).
  T28's own deadline, "before more servers are written", passed with `blkd` and `devmgr`.
  `channel_call` returns SHOULD_WAIT on a full server queue with no precise way to wait.
- Consequence: the network server is the first one where replies and data to slow clients are
  the common case.
- Q1 (IPC design); Q2 directions 4 and 5.
- Change: one rule instead of five implementations. Option A: a request sent with
  `channel_call` reserves its reply's place in the caller's queue, so a reply to a call never
  gets SHOULD_WAIT. Option B: a wait-for-space with a size. Either way put the held-reply logic
  in one service-loop library (D12).

### D9. Medium. Thread cost is structural, not a matter of raising two constants

- Evidence (checked in code): kernel stacks are 32 KiB (`kernel/src/thread.rs:34`) allocated
  from the fixed 8 MiB `linked_list_allocator` heap (`kernel/src/heap.rs:13`, OD29/OD34): 128
  threads already take half of it, and OD50's caps were audited to fit that heap jointly.
  Limits: 8 per process, 128 system, 4 per launched program (`launcher.rs:20`). A thread cannot
  free its own stack (T52); there is no way to interrupt one thread's blocking syscall (only a
  whole-process kill), which S11 signals and any `select`-with-cancel will need.
- Q2 direction 1.
- Change: kernel stacks from the frame allocator with a guard page (not the heap), sized by
  measurement; `thread_create` that maps the user stack, and a `thread_exit` that unmaps it;
  re-derive OD50's fit as a formula rather than hand-tuned numbers (T13). Decide explicitly that
  thread cancellation stays out.

### D10. Medium. Services are named by a closed `Role` enum and the namespace is limited to "." and "/", contrary to sections 10 and 14

- Evidence (checked in code): `proto/src/startup.rs:21-49` (17 roles, block-driver roles
  included); S4-SPEC section 5 and OD53: an unknown role or any other prefix makes the child exit
  -2001. The shell has no way to grant "the network" (T3, T18).
- Consequence: every new service is a wire-format change in `proto` plus launcher, shell and
  consumer changes in lockstep; there is nothing to re-resolve after a service restart, so
  direction 4's registry and reconnecting clients have no anchor (T15, T30).
- Q1 (namespace model contradicted by code); Q2 directions 3, 4, 5.
- Change: before S8, allow arbitrary namespace prefixes and publish services as entries in a
  service directory (`/svc/net`), opened with the file protocol's existing `Open`-with-endpoint.
  That one mechanism is the name registry, the reconnect path and the powerbox grant.

### D11. Medium. Servers cannot tell clients apart, so every per-server limit is a shared pool one client can drain

- Evidence: S4-SPEC section 7: "Servers cannot tell clients apart (INVARIANTS H5), so per-client
  limits are not specified"; 200 connections in all, 2 MiB held replies in all. Docs only; I did
  not test exhaustion.
- Consequence: one confined program can open 200 connections and deny the filesystem to
  everyone; the same will be true of sockets in the network server. Kernel job quotas do not
  help because the scarce thing is the server's.
- Q1 (trust boundary).
- Change: identity without ambient authority: whoever mints a root connection for a program
  (the shell or launcher) asks the server for a fresh *accounted* connection; everything cloned
  or opened from it inherits the account. Write it into the service-loop library.

### D12. Medium. Five hand-rolled server loops with divergent failure handling; the sixth will be the network stack

- Evidence (checked in code): `memfsd` (port, held replies), `consoled` (port), `launcher`,
  `devmgr`, `blkd` (single channel, blocking wait). `rt` has no service-loop or typed client
  layer (`rt/src/` is syscalls, fs client, spawn, startup, sync, heap).
- Q2 direction 4.
- Change: build the `rt` service loop (port, per-connection state, held reply, accounted
  connections, protocol-violation policy) and port `memfsd` to it *before* writing the network
  server; add supervisor restart afterwards. Kill-on-last-handle-close already gives a
  supervisor OTP-style links.

### D13. Medium. User code is soft-float `rv64imac`/`lp64` while the declared target is RVA23

- Evidence: `init/.cargo/config.toml:4`, `init/build.rs:69-70` (`-march=rv64imac -mabi=lp64`),
  OD40, INVARIANTS C7, OD50 (FS and VS off, no per-thread FP/V state) versus OD47.
- Consequence: every C object and the libc ABI are `lp64`; moving to `lp64d` later is a flag-day
  for all ported code, and saving FP/V state changes the context-switch cost that OD73 wants to
  measure first.
- Q1 (internal inconsistency); Q2 direction 1.
- Change: owner decides now: stay soft-float permanently (amend OD47 to say user FP is not
  offered), or switch the user ABI before more C is ported and before the OD73 benchmark.

### D14. Medium. The contract documents are stale in places that matter to a process where nobody reads the code

- Evidence: OS-ARCHITECTURE line 9 "Nothing here is implemented"; ROADMAP line 206 "S3 onwards
  is not [built]", S3 and S4 boxes unticked, "per-hart ... run queues" ticked with a global
  queue in the code; SYSCALLS.md: status header still S3a, "per-job quotas ... do not exist yet"
  (line 102), "The kernel takes no lock" (line 568), the single-hart execution model section, the
  numbers table omits the `job` argument of `vmo_create`/`process_create`, "handle encoding
  allows 65535" versus 8191 slots, a leftover "channel_call (planned, S3b)"; S5's acceptance was
  rewritten after the fact to drop SMP icount while CLAUDE.md still requires both modes;
  `os_boot_test.py:16` says 128M and passes 256M; OD73 is dated 2026-11-14 and marked
  "decided (direction)" while ROADMAP calls the same thing "not committed".
- Q1.
- Change: one docs-only slice; add a rule that SYSCALLS.md sections carry no milestone-relative
  statements ("yet", "S5 revisits").

### D15. Low-medium. Q11 (entropy, wall clock) is still open and S8 needs it

- Evidence: OS-ARCHITECTURE section 18 Q11; libc wall time is `clock_get` plus 0 (S7-SPEC).
  DHCP transaction ids, TCP initial sequence numbers, ephemeral ports and DNS ids need
  unpredictable values.
- Q2 direction 5. Change: decide before S8: virtio-rng as the second `devmgr` driver (also
  proves D6), or a documented deterministic generator for tests only.

### D16. Low-medium. The error model overloads NO_MEMORY and has no cancel

- Evidence: SYSCALLS.md: NO_MEMORY means job quota, global cap, full handle table or kernel heap;
  protocol statuses extend the kernel's number space by convention (S4-SPEC section 7);
  `channel_call` errors after the send leave the request in flight with no way to withdraw it,
  and `rt::fs` discards late replies by transaction-id arithmetic (`rt/src/fs.rs:93-107`).
- Q1. Change: split a QUOTA/NO_RESOURCES status from NO_MEMORY before libc sockets map errno;
  state per protocol whether a timed-out request may still take effect.

### D17. Low. A received handle cannot be inspected, and a port binding outlives the handle it was made with

- Evidence (checked in code): `object_info` returns koid and peer koid only
  (`kcore/src/ipc.rs:1022`); a binding stores the object id, not the handle
  (`ipc.rs:908-960`), so closing or narrowing the handle does not end the watch while any other
  holder keeps the object alive.
- Q1 (capability model). Change: `object_info` also returns kind and rights; bindings die with
  the handle (falls out of D1 if registrations are per handle).

### D18. Low. The VMO model has no child, resize or page-source slot yet, and VMO lifetime is by scanning

- Evidence: OD7 promises a page-source slot; the code has a kind enum and a per-VMO committed
  map (`kcore/src/aspace.rs:90-135`); T29 (whole boot image handed out); `reap_vmos`.
- Q2: S11 and D3. Change: none now beyond D2 item (3); design `vmo_create_child` with D3's
  who-pays answer.

## (b) Protect these

- Pure `no_std`, `forbid(unsafe_code)` cores with model tests and mutation checks (`kcore`,
  `proto`, `memfs`, `cfmt`); byte-level goldens for every wire format.
- Handles: generation with slot retirement, rights that only shrink, `handle_replace`, the
  separate KILL and SET_LIMITS rights, no call that returns the caller's own job (OD57).
- Creator-pays queued-data quotas (OD59) and charge-on-commit (J4): keep the rule, fix the
  inversions (D3, D4).
- Absolute deadlines on every wait; status-in-`a0` with no errno; user memory copied by
  page-table walk, never SUM.
- The kernel does not parse executables; the launcher has no filesystem authority (`GetVmo`).
- File protocol shape: `Open` carries a fresh endpoint, no `..`, a connection is the sandbox
  boundary, `Clone` attenuates. It is already the right base for a service directory and a union
  server.
- libc is already a shim over `rt` and `proto` (`libc/Cargo.toml`): direction 3 is a matter of
  keeping it that way for sockets.
- Supervisor-owned DMA with device reset (OD65/T42) and the DMA right gating physical addresses.
- Process-private futexes; kill-on-last-handle-close plus `job_kill` (supervision trees for free).
- `##` structured output, negative-control kernels, RFENCE shootdown inside the locked region,
  `tp` never restored from a kernel trap frame.

## (c) Not verified, and why

- Nothing was compiled, tested or booted (read-only brief); cost claims in D1/D2 are structural,
  not measured.
- Not read in full: `init/src/bin/suite.rs`, `sh.rs`, `consoled.rs`, `libc/src/{thread,file}.rs`,
  `kernel/src/{trap,smp,bkl,thread}.rs` beyond the cited lines, S7-SPEC after section 4. The
  BKL hand-over (K3) argument was not re-derived; I relied on INVARIANTS and S5-SPEC.
- How `memfsd` checks the rights of a received endpoint (S4-SPEC says it must) was not traced.
- D11 is from the spec text, not from an exhaustion test.
- The OD73 row and the ROADMAP "Directions" section are uncommitted working-tree edits; I read
  them because the brief refers to them.

## (d) Decisions the product owner needs to make

1. Redo target for SMP: one object lock with carve-outs (my recommendation), per-object locks,
   or per-hart state with messages. Does the reachability sweep stay?
2. Order: do the targeted-wake slice (D1), `vmo_commit` (D3), the service loop and service
   directory (D10, D12) before S8, and the lock redo after S9? The roadmap currently puts all of
   the redo after S9.
3. Network model (D4): control channel plus data socket, client-created pairs, datagrams as
   messages. One process for driver and stack, or two?
4. PLIC or AIA as the supported interrupt controller (D5).
5. User floating point: soft-float forever, or an ABI switch now (D13).
6. Entropy and wall clock source (Q11, D15).
7. How many threads is "many" (hundreds, thousands)? It sets the kernel stack and heap design (D9).
8. Is a server-issued account per client acceptable under H5 (D11)?
9. How does the shell grant network access (powerbox syntax, D10)?
10. Is OD73 decided or a direction? The two documents disagree.
11. Q10 revocation: accept proxy-based revocation as final? Nothing in S8-S11 needs more.
