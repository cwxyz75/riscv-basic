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
| H2 | Stale values fail: a closed or reused slot's old value is rejected (generation counter); value 0 is never valid | `handle_model` (S3a); a slot that has used all 2^19 generations is retired, never reused, so no stale value comes back by wrap-around (the retirement test) |
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

### Job and limit rules (S4.5)

| # | Invariant | How checked |
|---|---|---|
| J1 | A charge applies to a job and every ancestor, all or nothing: no job's usage ever exceeds a limit that was in force when it was charged, and a child never uses more than its parent allows | `kcore::job` unit tests and a random model; QEMU `a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling` |
| J2 | Every charge is credited back exactly once: a process at teardown, a thread when it ends, a page when its VMO is freed. A job record with usage is never freed (`debug_assert` in `Jobs::release`: a leak is a kernel panic in the tests) | QEMU `jobs_do_not_leak`, `job_*_credited_back`; mutation-checked |
| J3 | No ambient job authority: no call returns the caller's own job; `job_root` is refused to every process the kernel did not start; a job handle's rights only shrink (H3) | QEMU `job_syscalls_check_handles_rights_and_arguments` (a child proves it) |
| J4 | A job's memory is charged where the page is committed (`Vmos::page_for`), nowhere else: no path commits a page without the charge | code structure; `committed_pages_are_charged...` |
| J5 | Objects and queued data are charged exactly once and credited exactly once: an object's creator pays for the object, the creator of an endpoint pays for what is queued in it, and a destroyed object's credit is applied before the next charge; a charged object holds a use of its job until then, so a record never goes while it has usage | `kcore/tests/job_ipc_quota.rs` (a random walk compared with the registry's own counts), QEMU `job_quotas_...`, the end-of-session ledger check; mutation-checked |

### Hart rules (S5a)

| # | Invariant | How checked |
|---|---|---|
| M1 | In kernel code `tp` holds `&PerHart` of the hart running it. It is never restored from a trap frame on a return to the kernel (a thread may resume on another hart); the user's `tp` is saved in the frame and restored only on the return to user mode; `sscratch` is `&PerHart` in user mode and 0 in the kernel | the S3 user suites and every `-smp 4` run (a wrong `tp` would break every trap; migration of a thread between harts only starts in S5c, where its tests are added); `harts_come_online` checks each secondary bound `tp` to its own record |
| M2 | A secondary hart enters on the early boot table (the kernel's own table has no identity mapping), jumps to the upper half, and only then switches to the kernel table; it touches no shared kernel state until the big kernel lock exists (S5b) | `harts_come_online`, `ipi_reaches_every_hart` |
| M3 | The user's `satp` last written is per hart (`PerHart::active_satp`) | code structure; S5c tests |

### Lock order

1. **BKL** (`kernel/src/bkl.rs`): protects all shared kernel state. Taken by `irq::without` and by
   every trap handler (except an IPI and a parked secondary's timer in S5b); reentrant per hart;
   released on every path that leaves kernel code; follows the thread across a context switch (K3).
2. **HEAP** (the allocator's spin lock, interrupts off): used inside and outside the BKL, so it
   comes after it.
3. **UART** (reentrant per hart, interrupts off, held for a whole formatted print): last, because
   anything may print.

A hart that holds a class may take a class with a *higher* number, never a lower one; the checker
(`bkl::note_acquire`) panics on a violation in every build and counts it. New locks go into this
list, with the reason for their position, in the change that adds them.

| # | Invariant | How checked |
|---|---|---|
| K1 | Shared kernel state is touched only with the BKL held: `thread::kernel()`, `user::state()` and `kmem::get()` assert it | the whole suite at -smp 1, 2, 4 (a violation panics) |
| K2 | The BKL is released at `sret`, in `enter_user`, and before idle `wfi`: another hart can take it while this one runs user code or idles | `bkl_is_released_on_user_entry_and_idle` (a probing hart; mutation-checked) |
| K3 | A thread's nesting depth is saved at a context switch and restored when it resumes, on whatever hart; the lock itself never leaves the hart that switches | the suite (a lost or doubled level panics on the assert in `unlock`) |
| K4 | The lock is mutually exclusive | `bkl_is_exclusive_under_contention` (every hart does 20,000 racy read-modify-write increments; mutation-checked) |
| K5 | Lock order BKL, HEAP, UART | `lock_order_violations_are_detected`, and the checker panics in the real run |

## Namespaces and paths

| # | Invariant | How checked |
|---|---|---|
| N1 | `..` in a path never escapes a directory handle | Property test (S4) |
| N2 | Servers never follow symlinks themselves; the client resolves them in its own namespace | Protocol test (S4) |

## Process rules

- No `unsafe` outside the hardware-facing kernel binary; each block has a
  `// SAFETY:` comment. The pure crates `forbid(unsafe_code)`.
- Every change leaves the system booting and the suite green.
