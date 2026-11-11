# Invariants (OS track)

Rules every change to the OS track must preserve. Reviewers (and the independent
review pass at the end of each milestone) attack code against this file and
`SYSCALLS.md`. A rule is only listed here if a test, a checker or a debug assertion
can fail when it is broken; the "How checked" column says which, and "S?" says the
milestone that makes the check real. Rules without a check yet are marked **TODO**
and are the first things S-milestones add.

Skeleton created in S0 from `OS-ARCHITECTURE.md` (sections 5 to 7, 10); the details
get filled in as each area is built.

## Handles and rights

| # | Invariant | How checked |
|---|---|---|
| H1 | A handle value cannot be forged: only the kernel mints values, and a value names an entry in the caller's own table | Property test on the handle table (S3) |
| H2 | Stale values fail: a closed or reused slot's old value is rejected (generation counter); value 0 is never valid | Property test (S3) |
| H3 | Rights only shrink: duplicate, replace, transfer and open never produce a handle with a right its source lacked | Property test over random operation sequences (S3) |
| H4 | A message in flight owns its handles: if the channel dies they are closed, never leaked, and never duplicated | Model test and leak check (S3) |
| H5 | Authority is holding a handle: no syscall consults identity, a path or ambient state to grant access | Review against `SYSCALLS.md` (S3) |
| H6 | Process death releases every object it held | Leak check against allocator counts (S3) |

## Memory

| # | Invariant | How checked |
|---|---|---|
| M1 | A physical frame has one owner set: no frame is mapped by two unrelated owners | Model tests (`kcore/tests/*_model.rs`): independent frame accounting by formula after every step; `FrameDb::audit`; QEMU `shared_vmo_is_one_memory` (S1) |
| M2 | W^X: no mapping is both writable and executable (JIT needs an explicit, narrower policy, Q6) | `Perms::validate` refuses it (host test); QEMU `kernel_mappings_are_w_xor_x` walks the live kernel table (S1) |
| M3 | Frame reference counts are conserved: after any map/unmap/fault/teardown sequence the counts return to baseline | Model tests end with a baseline check; QEMU `balanced()` wraps every lazy-memory test, plus `churn_does_not_leak` (S1) |
| M4 | All memory operations go through the VMO interface, never directly to frames | Review; the only frame-granting path to a mapping is `AddressSpace::handle_fault` via `Vmos::page_for` (S1) |
| M5 | Kernel memory is never reachable from a user mapping; the kernel image is not writable through the direct map | QEMU `direct_map_does_not_alias_the_image`; user-mapping rule needs S3 (partial) |

## Concurrency

| # | Invariant | How checked |
|---|---|---|
| C1 | **Lock ordering is global and fixed**, listed below; a lock is only taken while holding locks earlier in the order. Not negotiable | Debug-build lock-order checker (S5); loom/shuttle on extracted logic |
| C2 | Interrupts are disabled while a spinlock is held | Debug assertion (S5) |
| C3 | One block/wake path: no lost wakeups and no double wakeups | loom/shuttle on `kcore::latch` (S0), then the real wait queue (S2) |
| C4 | Each runnable thread is in exactly one place (a run queue, running, or blocked) | Debug assertions, scheduler model test (S2) |
| C5 | Atomics and fences live only in `kcore::sync` and the small audited layer; everything else relies on `Send`/`Sync` | Review; grep gate (S2) |

### Lock order

(Empty until S5. The first entry added is the big kernel lock; every later lock is
inserted here, with the reason for its position, in the same change that adds it.)

## Namespaces and paths

| # | Invariant | How checked |
|---|---|---|
| N1 | `..` in a path never escapes a directory handle | Property test (S4) |
| N2 | Servers never follow symlinks themselves; the client resolves them in its own namespace | Protocol test (S4) |

## Process rules

- No `unsafe` outside the hardware-facing kernel binary; each block has a
  `// SAFETY:` comment. The pure crates `forbid(unsafe_code)`.
- Every change leaves the system booting and the suite green.
