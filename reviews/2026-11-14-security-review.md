# Security review: the syscall surface and the servers' trust boundaries (2026-11-14)

Reviewer: independent pass, fresh context, read-only. Attacker model: arbitrary user code in an
unprivileged process (a program started by the launcher: 1 process, 4 threads, 256 objects,
192 KiB queued IPC, 34816 pages, no job handle, no resource handle).

Scope read end to end: `kernel/src/user.rs`, `trap.rs`, `kmem.rs`, `fp.rs`, `aia.rs`;
`kcore/src/{handle,object,ipc,job,aspace,wait,usercopy,futex}.rs`, most of `pagetable.rs`;
`abi/src/lib.rs`; `init/src/bin/{launcher,devmgr}.rs`, the request paths of `memfsd.rs` and
`blkd.rs`; the decoders in `proto/src/{blk,devmgr,launch}.rs`. Not read: `kcore/src/sched.rs`,
`elf.rs`, `frame.rs` beyond `inc`, `proto/src/{file,startup}.rs` internals, `memfs` beyond
`write`/`set_size`.

Nothing was booted. "Host-confirmed" means a scratch program outside the repo, linked against
`kcore`, reproduced it; "read" means the code path was followed end to end but not run.

## Confirmed bugs

### 1. Kernel panic from retired handle slots (high; host-confirmed)

`kcore/src/handle.rs:64` `room()` is `limit - live`, but `insert` (`:87-108`) also fails when the
free list is empty and `slots.len()` has reached 8191. Retired slots (`:117`) stay in the vector
for ever, so after 4096 slots are retired the table has room for 4095 handles while `room()` still
says 4096. Every caller that checked `room()` then hits an `expect`.

- Input: one event, `handle_replace(h, SAME)` 2^31 times (retires 4096 slots), `handle_duplicate`
  until NO_MEMORY (4094 succeed, `room()` = 1), then `event_create`.
- Result: `ipc.rs:564` `expect("room was checked")` panics. Same class: `ipc.rs:305-306, 461,
  726-727, 895, 1223, 1240, 1298, 1353`, `handle.rs:180` (`replace`), `user.rs:2339, 2352`.
- Reproduced on the host: 2,147,483,648 replaces took 50 s natively, then the panic at
  `ipc.rs:564`. In the guest that is minutes to hours of syscalls from one unprivileged thread,
  and it takes the whole machine down.
- Fix: make `room()` truthful (`min(limit - live, free.len() + SLOT_MASK - slots.len())`) and
  turn the `expect`s on `insert` into NO_MEMORY. `duplicate` (`:158-161`) also leaks an object
  reference when `insert` fails after `retain`; move the retain after the insert or undo it.
  The retirement test in `handle_model` should run a table to this state.

### 2. `process_self` defeats kill-on-close (medium-high; read)

`user.rs:2335-2341`: for a process made by `process_create`, `process_self` retains the *same*
kill-on-close object the parent holds (`:2283-2289`, `kill_on_close = true`) and puts a handle to
it in the child's own table. The parent closing its last handle no longer destroys the object, so
`cleanup_dead` (`:1421`) never kills the child.

- Input: the child calls `process_self` once. Every `rt` program does on its first heap growth
  (`rt/src/heap.rs:88-94`), as do libc threads, `blkd` and `devmgr`.
- Impact: SYSCALLS.md's rule ("closing the last handle to a process kills it, so a creator that
  drops its handle cannot leave an unmanageable process behind") does not hold for real programs.
  A program outlives a parent that drops or loses its handle (a dead `sh`), keeps its job's memory
  and a process slot, and nobody holds KILL on it; only `job_kill` on an ancestor job (init's
  session) reaches it. `closing_handles_early_and_kill_on_close` passes because its child never
  takes a self handle.
- Fix: `process_self` always mints a separate view object (`kill_on_close = false`), as it
  already does for kernel-started processes; or keep a count of external handles and kill when it
  reaches zero. Add a test whose child calls `process_self` before the parent closes.

### 3. A sleeping port waiter taxes every syscall in the system (medium; read, cost host-measured)

`wait.rs:93-98` re-evaluates every registered port on every commit that touched anything
(`user.rs:1365-1382`, run after each syscall and each user trap). A port's signal is a scan of
all its bindings (`ipc.rs:672-678`), and a channel binding's signal walks the peer's queue
(`ipc.rs:649`, `queued_bytes` at `:40-42`).

- Input: one port, about 5,900 bindings (the 192 KiB quota at 32 bytes each; one object can be
  bound many times under different keys) on a channel whose peer queue holds 127 empty messages,
  mask PEER_CLOSED so none is ever ready; then `port_wait(port, forever)`.
- Impact: every state-changing syscall or trap by any process pays one full scan under the BKL:
  145 microseconds natively at 6,144 bindings (1.5 ms at 65,536, the global cap), several times
  that under QEMU. T60 records the scan as a performance debt; it does not record that other
  processes pay for it.
- Fix: the PW design (exact ready set); until then cap bindings per port, evaluate a port only
  when a bound object was touched, and make `queued_bytes` a stored counter.

### 4. `prune_port` is quadratic under the BKL (low-medium; host-measured)

`ipc.rs:900-924`: for each dead binding a `position` plus a `Vec::remove`. Bind N keys to one
event, close the event, poll the port. Measured natively: 6.7 ms at N = 6,144 (a program's
quota), 0.94 s at N = 65,536 (any job with the full 2 MiB budget). Interrupts are off and every
hart waits. The `dead` list (`:901`) is also an infallible allocation of 8N bytes.
Fix: one `retain` pass, adjusting `next` by the number removed before it.

## Suspicions (not demonstrated)

5. **Object ids wrap** (`object.rs:336, 414`: `gen.wrapping_add(1)`), unlike handle slots, which
   retire. Port bindings (`ipc.rs:92-100`), wait registrations, `irq_objs`, `Process::obj` and
   `thread_objs` hold ids without references. After 2^32 create/close cycles on one slot (LIFO
   reuse makes that one program's loop) a stale binding names whatever object lands there next,
   possibly another process's: its signals leak through `port_wait`. For a kernel-started process
   the stale `p.obj` in `process_self` (`user.rs:2335`) would mint a MANAGE handle to a foreign
   object. `koid` "never reused" (SYSCALLS.md) is false after the wrap. Fix: retire object slots,
   or a 64-bit generation.
6. **Infallible kernel-heap allocations on user-driven paths** (8 MiB heap, failure is a panic):
   `aspace.rs:417` and `:492` (a B-tree entry per committed page and per region, uncharged; about
   1 MiB per 136 MiB program), `user.rs:2465`, `thread.rs:323` (16 KiB stack), `object.rs:384`,
   `user.rs:127-133` (`roots()`: 8 bytes for every handle in the system, per sweep and teardown),
   `wait.rs:34`. Handle tables are no longer allocated up front (T8 says they are): 64 full
   tables are 5 MiB. One launcher program cannot exhaust the heap alone by my arithmetic; several
   jobs together plausibly can. T1/T13 cover the theme, not these sites. Not run.
7. **Long BKL holds at teardown and unmap**: `aspace.rs:529-540` walks every page of every
   region (1024 regions of 16384 pages is 16M walks, even untouched), and each successful unmap
   rescans a 512-entry table (`pagetable.rs:321-330`). T11 in general; this is the worst case.
8. **Ambient information**: `debug_stat` (`user.rs:723`, `thread.rs:708`) gives any process
   system-wide sleep, wake and interrupt counters; `object_info` (`ipc.rs:1084`) exposes registry
   slot index and generation with no right. Both are cross-process timing and activity channels.
   T2 lists `debug_write` only.
9. **memfsd fairness**: `MAX_CONNECTIONS` (200, `memfsd.rs:22, 560`) and `PENDING_CAP` (`:212`)
   are global. CLONE needs no right, so two hostile programs (about 127 connections each) make
   every other open fail with QUOTA, and one client's withheld replies get another client's
   connection closed. Per-client caps are the fix. Depends on programs holding a connection.
10. **Frame overcommit**: two 136 MiB program jobs exceed RAM; a server's next first-touch fault
    then kills it (T58 item 2, R14 residual). Noted only because any two programs can do it.

## Checked and found sound (read)

Rights monotonicity in duplicate, replace, transfer, `read_at`, `process_start`; stale handle
after close or replace during a blocked wait (forced touch, re-resolved by value, BAD_HANDLE);
32-bit argument checks; overflow checks in `check_range`, `vmo_read/write`, `vmo_commit`,
`process_map`, `futex_wake`, `in_device_window`; no TOCTOU on user memory (single copy-in,
verify and copy in one locked region); device frames refused by every kernel copy; `job_root`
and `resource_root` gated on `privileged`; no ambient job authority; no kernel value left in user
registers; FP state off at thread start and saved or reloaded by token; AIA sources bounded by
`MAX_IRQ` and masked on fire; `proto` decoders for blk, devmgr and launch are length-checked and
panic-free; channel write is all-or-nothing including quota.
