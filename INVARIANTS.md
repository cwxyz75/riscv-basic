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
| H1 | A handle value cannot be forged: only the kernel mints values, and a value names an entry in the caller's own table | `kcore/tests/handle_model.rs`: random and adversarial values never resolve unless live (S3a) |
| H2 | Stale values fail: a closed or reused slot's old value is rejected (generation counter); value 0 is never valid | `handle_model` (S3a). Known limit: a slot reused 65536 times can accept a very old value |
| H3 | Rights only shrink: duplicate, replace, transfer and open never produce a handle with a right its source lacked | `handle_model`, `channel_model` (a model of every handle's rights), `rights_escalation_is_refused` (S3a) |
| H4 | A message in flight owns its handles: if the channel dies they are closed, never leaked, and never duplicated | `channel_model`, `registry_model`, `cycles_are_swept` with `Registry::live()` as the leak counter (S3a) |
| H5 | Authority is holding a handle: no syscall consults identity, a path or ambient state to grant access | Review against `SYSCALLS.md` (S3) |
| H6 | Process death releases every object it held | Table drain + reachability sweep tested on the host (S3a); QEMU `process_exit_releases_all_objects` and `process_teardown_returns_every_frame` for a single process (S3b); S3c-2a: children launched, killed (running, blocked, never started) and abandoned by their parent leak nothing (`no_process_or_vmo_survives_init`, `many_children_do_not_leak`); a VMO lives while a handle or a mapping does; threads in S3c-2b: a process is torn down only after its last thread has left (`many_threaded_children_do_not_leak`) |

## Memory

| # | Invariant | How checked |
|---|---|---|
| M1 | A physical frame has one owner set: no frame is mapped by two unrelated owners | Model tests (`kcore/tests/*_model.rs`): independent frame accounting by formula after every step; `FrameDb::audit`; QEMU `shared_vmo_is_one_memory` (S1) |
| M2 | W^X: no mapping is both writable and executable (JIT needs an explicit, narrower policy, Q6) | `Perms::validate` refuses it (host test); QEMU `kernel_mappings_are_w_xor_x` walks the live kernel table (S1) |
| M3 | Frame reference counts are conserved: after any map/unmap/fault/teardown sequence the counts return to baseline | Model tests end with a baseline check; QEMU `balanced()` wraps every lazy-memory test, plus `churn_does_not_leak` (S1) |
| M4 | All memory operations go through the VMO interface, never directly to frames | Review; the only frame-granting path to a mapping is `AddressSpace::handle_fault` via `Vmos::page_for` (S1) |
| M5 | Kernel memory is never reachable from a user mapping; the kernel image is not writable through the direct map | QEMU `direct_map_does_not_alias_the_image`; `usercopy.rs` (a user pointer into the shared kernel half or page zero is refused even with a user-bit region over it); QEMU `user_store_to_kernel_memory_is_fatal`, `user_load_from_kernel_memory_is_fatal` (S3b) |
| M6 | Every user pointer is validated and copied by a page-table walk, never dereferenced; a bad pointer is INVALID_ARGS and changes nothing | `kcore/tests/usercopy.rs` (every bad-pointer class, no partial writes); `init` tests `debug_write_pointer_rules`, `buffers_are_validated_before_a_read` (S3b) |
| M7 | A user program is hostile data: the ELF loader accepts only what is safe to map (user half, in the file, no W+X, no executable stack, no overlap) and never panics | `kcore/tests/elf_loader.rs` (every rule, plus a mutation fuzz of 3000 images); QEMU `a_corrupt_program_is_refused_without_leaks` (S3b) |

## Concurrency

| # | Invariant | How checked |
|---|---|---|
| C1 | **Lock ordering is global and fixed**, listed below; a lock is only taken while holding locks earlier in the order. Not negotiable | Debug-build lock-order checker (S5); loom/shuttle on extracted logic |
| C2 | Interrupts are disabled while a spinlock is held | Debug assertion (S5) |
| C3 | One block/wake path: no lost wakeups and no double wakeups | loom/shuttle on `kcore::latch` (S0); `kcore/tests/sched_model.rs` (model, token and wait-queue scenarios, mutation-checked) and QEMU `wake_before_block_is_not_lost`, `producer_consumer_loses_no_wakeup` (S2). Multi-hart interleavings: S5 |
| C3b | Blocking waits (`object_wait`, `channel_call`) lose no wakeup: the condition is checked and the thread queued in one interrupts-off step, and every syscall that can change a signal wakes all waiters afterwards | QEMU `wait_is_woken_by_a_message`, `channel_call_*`; mutation checks: removing the wake-up after a signal-changing syscall, or ignoring deadlines, is caught |
| C4 | Each runnable thread is in exactly one place (a run queue, running, or blocked) | `Sched::audit` after every step of the random model test, for every policy; the core rejects a policy pick that is not runnable (`bad_picks`) (S2) |
| C5 | Atomics and fences live only in `kcore::sync` and the small audited layer; everything else relies on `Send`/`Sync` | Review only so far (a grep gate is **TODO**; the S2 kernel code uses `core::sync::atomic` directly for tick counters and test flags) |
| C6 | Scheduler state is touched only with interrupts disabled (single hart), and never across a context switch; the kernel heap is interrupt-safe because the tick allocates | `debug_assert` in `thread::kernel()`; heap wrapper (S2); becomes a real lock in S5 |
| C7 | Kernel threads do no floating point and thread switches save no FP state, so `sstatus.FS` stays Off and accidental FP use traps | QEMU `fp_unit_is_off` (S2); revisit when user mode (S3) needs FP |


### Process and address-space rules (S3b)

| # | Invariant | How checked |
|---|---|---|
| P1 | Returning to user mode always uses the returning thread's own address space (satp), however many processes ran in between, on both return paths (syscall and timer interrupt) | QEMU `two_processes_do_not_share_memory`, `yielding_processes_do_not_share_memory`; each return-path hook was shown necessary by removing it |
| P2 | The kernel's top-level page-table slots never change after the first process exists | `kmem::kernel_half_unchanged`, checked at every process start and by QEMU `kernel_top_level_slots_are_stable` |
| P3 | A user fault the kernel cannot resolve ends the process, never the kernel; the exit code says why (`-1000 - scause`) | QEMU: eight fault kinds, `the_stack_has_a_guard_page`, `a_syscall_with_sp_in_the_kernel_is_harmless` |
| P4 | A user program can only reach the kernel through syscalls: registers other than a0-a2 are preserved, no kernel value is left in a user register | QEMU `registers_survive_a_syscall`; `enter_user` zeroes every register but a0 and sp |

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
