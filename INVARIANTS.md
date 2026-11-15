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
| C3 | One block/wake path: no lost wakeups and no double wakeups. Since task 10 this is the park protocol (PK1): a thread prepares before it registers, a wake counts only for a prepared or blocked thread and is never remembered; the wake token and the pure `WaitQueue` are gone | loom/shuttle on `kcore::park` (`park_concurrency.rs`, three negative controls); `kcore/tests/sched_model.rs` (model, park scenarios, mutation-checked) and QEMU `wake_before_block_is_not_lost`, `producer_consumer_loses_no_wakeup` |
| C3b | Blocking waits lose no wakeup: the condition is checked, the thread registered in the wait table (`object_wait`, `channel_call`, a kernel thread's read wait) and put to sleep in one interrupts-off step under the big lock; every registry mutation is logged (`Registry::get_mut` touches the object and its peer, handle removal is a forced touch) and the log is committed before the lock is released (after every syscall and every trap from user mode, and before any such wait sleeps), waking exactly the registered threads whose predicate can have become true, by id. Port waits (`port_wait`) register on the port like any other wait (a port's signal follows its bindings, so registrations on ports are re-evaluated on every commit that touched anything); there is no global wait queue any more (OD43 retired). Several waiters on one port are all woken when it becomes ready (the wake-one rule PW is not built) | QEMU `wait_is_woken_by_a_message`, `channel_call_*`, `replacing_a_handle_ends_a_siblings_wait` (negative control `fault-skip-forced-touch`), `wake_baseline_bystanders_and_counters` (zero spurious wakes, bystanders not woken), `kcore/tests/wait_model.rs` and `touch_complete.rs` (random walks against the old wake-everyone rule, mutation-checked) |
| C4 | Each runnable thread is in exactly one place (a run queue, running, or blocked) | `Sched::audit` after every step of the random model test, for every policy; the core rejects a policy pick that is not runnable (`bad_picks`) (S2) |
| C5 | Atomics and fences live only in `kcore::sync` and the small audited layer; everything else relies on `Send`/`Sync` | Review only so far (a grep gate is **TODO**; the S2 kernel code uses `core::sync::atomic` directly for tick counters and test flags) |
| C6 | Scheduler state is touched only with the big kernel lock held and interrupts disabled (it was interrupts-off on one hart before S5), and never across a context switch; the kernel heap is interrupt-safe because the tick allocates | `debug_assert` in `thread::kernel()` (K1); heap wrapper (S2); the lock since S5b |
| C7 | Kernel threads do no floating point and thread switches save no FP state, so `sstatus.FS` stays Off and accidental FP use traps | QEMU `fp_unit_is_off` (S2); user threads may use FP since slice A: see FP1-FP3 (the kernel itself still does none) |


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
| PR1 | A thread's base priority never exceeds its job's `max_prio`, and a job's `max_prio` and `rt_budget` never exceed its parent's (PR2): no call raises a job's limits above the parent's, lowering one lowers every descendant and clamps the live threads, a new thread's priority is clamped at creation | `kcore/tests/job_sched.rs` (random model), QEMU `thread_priority_is_bounded_by_the_job` (a confined program; lowering clamps a live thread), mutation-checked |
| J2 | Every charge is credited back exactly once: a process at teardown, a thread when it ends, a page when its VMO is freed. A job record with usage is never freed (`debug_assert` in `Jobs::release`: a leak is a kernel panic in the tests) | QEMU `jobs_do_not_leak`, `job_*_credited_back`; mutation-checked |
| J3 | No ambient job authority: no call returns the caller's own job; `job_root` is refused to every process the kernel did not start; a job handle's rights only shrink (H3) | QEMU `job_syscalls_check_handles_rights_and_arguments` (a child proves it) |
| J4 | A job's memory is charged where the page is committed (`Vmos::page_for`), nowhere else: no path commits a page without the charge | code structure; `committed_pages_are_charged...` |
| J5 | Objects and queued data are charged exactly once and credited exactly once: an object's creator pays for the object, the creator of an endpoint pays for what is queued in it, and a destroyed object's credit is applied before the next charge; a charged object holds a use of its job until then, so a record never goes while it has usage | `kcore/tests/job_ipc_quota.rs` (a random walk compared with the registry's own counts), QEMU `job_quotas_...`, the end-of-session ledger check; mutation-checked |

### Device rules (S6a)

| # | Invariant | How checked |
|---|---|---|
| D1 | A physical VMO never names RAM: its range is inside a device window, so the frame database never sees its frames and no path frees or zeroes them; the kernel never reads or writes through one (`vmo_read`/`vmo_write` and every syscall copy refuse it) | `abi::layout::in_device_window`, `kcore/tests/device_vmo.rs` (mutation-checked), QEMU `device_caps_need_the_resource`, `mmio_reads_the_virtio_magic` |
| D2 | A physical address is learnable only through a DMA-right handle of a DMA VMO | QEMU `device_caps_need_the_resource` |
| D3 | Device objects are minted only with the root resource, which only the kernel-started process can obtain | QEMU `device_caps_need_the_resource` |
| D4 | DMA frames are freed exactly once, when the VMO and its last mapping go; the DMA budget and the job's page charge come back | `kcore/tests/device_vmo.rs` (random, exact frame count; mutation-checked), QEMU `device_caps_need_the_resource` |
| D5 | A fired interrupt source is masked from the moment it is taken until `interrupt_ack` or the object's destruction, and a destroyed object leaves its source masked (nothing enabled with nobody listening); one live object per source | QEMU `interrupt_object_fires_and_masks` (an acknowledgement with the line still high must fire again: the same test, with the `fault-aia-no-unmask` and `fault-aia-no-repend` negative controls; T59) |

### Futex rules (S7b)

| # | Invariant | How checked |
|---|---|---|
| F1 | The futex table (like all shared kernel state) is touched only under the BKL; a thread is in it only between its enqueue in `futex_wait` and its own `forget` on the way out of that call, so a table entry never outlives its thread and a reused thread id cannot inherit one | `kcore::futex` unit and model tests, QEMU `kill_a_futex_waiter`, the end-of-run check `no_process_or_vmo_survives_init` (no sleeper left) |
| F2 | The user word is read and the sleeper enqueued in one BKL region, so a wake that follows a user-mode store is never lost; the sleep ends only by removal from the table (a wake) or the deadline | loom/shuttle model, QEMU `futex_*` (a never-sleeping mutant fails three) |
| F3 | A wake reaches only sleepers of the caller's own process, oldest first | `kcore::futex` tests, QEMU `futex_wake_counts_and_fifo` (another process wakes nobody) |

### Hart rules (S5a)

| # | Invariant | How checked |
|---|---|---|
| SMP1 | In kernel code `tp` holds `&PerHart` of the hart running it. It is never restored from a trap frame on a return to the kernel (a thread may resume on another hart); the user's `tp` is saved in the frame and restored only on the return to user mode; `sscratch` is `&PerHart` in user mode and 0 in the kernel | the S3 user suites and every `-smp 4` run (a wrong `tp` would break every trap; migration of a thread between harts only starts in S5c, where its tests are added); `harts_come_online` checks each secondary bound `tp` to its own record |
| SMP2 | A secondary hart enters on the early boot table (the kernel's own table has no identity mapping), jumps to the upper half, and only then switches to the kernel table; it touches no shared kernel state until the big kernel lock exists (S5b) | `harts_come_online`, `ipi_reaches_every_hart` |
| SMP3 | The user's `satp` last written is per hart (`PerHart::active_satp`); every context switch loads the kernel's own `satp` first if the hart has a user process's root in it, so no hart keeps a process's page table across a switch (another hart may tear the process down and reuse the root) | code structure (`reschedule`); `teardown_with_threads_on_other_harts` (S5d) |
| SMP4 | Only hart h changes the scheduler's `current[h]`; a wake, spawn, tick or priority change elsewhere marks the hart pending and pokes it (reschedule IPI) | `kcore/tests/sched_smp_model.rs` (every pending hart was poked; mutation-checked), `a_wake_preempts_a_lower_priority_hart` |
| SMP5 | Every unmap of a live user mapping and every process teardown is followed, inside the same locked region, by an SBI remote `sfence.vma` on every other online hart; a stale translation can therefore never reach a frame that has been freed and reused | `unmap_while_siblings_touch`, `teardown_with_threads_on_other_harts`; both have negative-control kernels (`fault-skip-shootdown`, `fault-skip-kernel-satp`) that must fail |

### Floating-point rules (slice A, OD76)

| # | Invariant | How checked |
|---|---|---|
| FP1 | A user thread starts with the FP unit off (`sstatus.FS` = Off): it cannot read any FP register, so it never sees another thread's or process's values; its first FP instruction traps as an illegal instruction, the kernel enables it from a zeroed register file, and the instruction is retried. A real illegal instruction traps again with the unit on and ends the process | QEMU `fp_state_is_private_to_each_thread_and_process` (a process's first FP use reads zeros while others keep patterns), `user_vector_instruction_is_fatal` |
| FP2 | On every trap from user mode the dirty FP registers are saved into the thread before anything else can use the hart's unit, and the kernel runs with the unit off; on every return to user mode a thread with FP state gets exactly that state (loaded unless this hart already holds that exact state, by a unique token) | `kcore/tests/fp_model.rs` (random threads, harts, preemption, migration against a reference; mutation-checked), QEMU test above at one and four harts, mutation: skipping the save fails it |
| FP3 | The kernel does no floating point and runs with `sstatus.FS` Off (C7); the unit is on only inside `kernel/src/fp.rs`'s save and load | QEMU `fp_unit_is_off` |

### Memory commit rules (slice V)

| # | Invariant | How checked |
|---|---|---|
| V1 | A VMO charged to a job that is neither the caller's nor the address space owner's is mapped only over a range that is already committed: its *data pages* are paid for, so a touch of a mapping never charges a stranger or fails on the VMO's job's quota. Scope, stated plainly (TIDY-UP T58): it separates *jobs*; a hostile peer in the same job, or in a sibling job under a shared ancestor limit, can still hand over a lazy VMO; and the page-table frames a first touch needs are taken at fault time, so a touch can still fail when frames themselves run out | QEMU `vmo_commit_charges_up_front_and_foreign_maps_need_it` (a mutation that removes the check fails it) |
| V2 | `vmo_commit` is all or nothing: a refusal leaves the VMO, the frame count and the job ledger exactly as before; committed pages are charged once and credited exactly when the VMO is destroyed | `kcore/tests/vmo_commit.rs` (mutation-checked: no rollback fails it), QEMU test above |
| V3 | Only a holder of COMMIT can `vmo_commit`, and the charge is to the VMO's creating job, never the caller's: a creator narrows COMMIT away before handing a VMO on. (A WRITE holder's `vmo_write` also commits pages, charged the same way, lazily and with no kill: the creator pays for what it let a writer write) | QEMU test above (a handle without COMMIT is refused) |

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
| K1 | Shared kernel state is touched only with the BKL held: `thread::kernel()`, `user::state()` and `kmem::get()` assert it (`debug_assert!`: debug builds only, so the release SMP run does not check K1) | the whole suite at -smp 1, 2, 4 in debug builds (a violation panics) |
| K2 | The BKL is released at `sret`, in `enter_user`, and before idle `wfi`: another hart can take it while this one runs user code or idles | `bkl_is_released_on_user_entry_and_idle` (a probing hart; mutation-checked) |
| K3 | A thread's nesting depth is saved at a context switch and restored when it resumes, on whatever hart; the lock itself never leaves the hart that switches | the suite (a lost or doubled level panics on the assert in `unlock`) |
| K4 | The lock is mutually exclusive | `bkl_is_exclusive_under_contention` (every hart does 20,000 racy read-modify-write increments; mutation-checked) |
| K5 | Lock order BKL, HEAP, UART (since task 10 part 1 the checker is `klock`'s `Held` over the full 14-class table of SCHED-SMP-SPEC 3.3; the heap lock is a `klock::Spin` of class HEAP, the BKL is class LEGACY) | `lock_order_violations_are_detected`, the pair table in `klock/tests/order.rs`, and the checker panics in the real run |
| K6 | The heap grows only at safe points holding the BKL (`heap::service`, tail of a BKL-taking trap, idle): `grow` takes frames and maps window pages under BKL, then the HEAP lock innermost; the allocator itself never calls the frame database | `heap_grows_at_a_safe_point`, the debug assert in `grow` |

### Planned lock order and invariants (SCHED-SMP-SPEC sections 3 and 8.3; OD85 to OD96; **not built**)

The list above is the order in force. The table below is the order the SMP redo is building towards: a class is real
once the task that adds it has merged, and until then its state is under `LEGACY`. K1 to K5 are legacy-lock rules,
retired at task 20; C3b's text is replaced by PK1 and W1' when task 20 lands. Nothing here is checked yet.

| # | Class | Instances | Protects | IRQ | Nested same-class |
|---|---|---|---|---|---|
| 1 | `LEGACY` | one | Everything not yet peeled. Transitional (3.5); absent in the final state | timer, external | reentrant (a depth count) until task 20 |
| 2 | `AS` | per process | The region map, the page table, the ASID and the `ran_on` hart mask of one address space | no (page faults are traps, not interrupts) | never two (a syscall touches one address space) |
| 3 | `OBJ` | one | The object registry, every handle table, the wait table and dirty log, port and channel state including pending-call records, the IPC meter | external (interrupt objects) | no |
| 4 | `PROCS` | one | The process table and each process's lifecycle state (state, killed, exit code, thread count) | no | no |
| 5 | `FUTEX` | 64 buckets, by hash of the key | Futex queues, PI-futex owner and waiter records | no | never two |
| 6 | `VMO` | one | The VMO store: pages, commit state, mapping references | no | no |
| 7 | `JOBS` | one | The job ledger, including each job's scheduling limits | no | no |
| 8 | `PI` | one | The donation graph (4.8) | no | no |
| 9 | `TT` | one | The thread table: `Tid` to `Arc<Thread>` | timer, IPI | no |
| 10 | `RQ` | per hart | That hart's run queues (all bands), its current thread, the scheduling records of the threads homed there, its timer queue, its accounting and throttle state, its `resched_pending` flag | timer, IPI | two, ascending hart index (migration, pull) |
| 11 | `KSTACK` | one | The kernel-stack slot allocator and the kernel page-table entries of the stack window | no | no |
| 12 | `FRAME` | one | The frame database (the per-hart frame caches need no lock, 3.10) | no | no |
| 13 | `HEAP` | one | The kernel allocator | yes | no |
| 14 | `UART` | one | The console line | yes | reentrant per hart (a panic breaks it) |

| # | Invariant | How checked |
|---|---|---|
| X3 | No lock is held across a context switch except the switching hart's own run-queue lock, released by the thread that resumes | the checker panics in `schedule()` if the held stack is not empty; shuttle hand-over model |
| X7 | A wake never switches threads; the switch happens only in `schedule()`, entered with no lock held | code structure; the T62 scenario test |
| G1 | State not yet peeled is touched only with the legacy lock held; each peel names the state it moves (replaces K1) | the asserts in the accessors; `LEGACY_SYSCALLS` checked against SYSCALLS.md |
| PK1 | A wake that follows a waiter's registration is never lost (replaces the token rule of C3; built in task 10, running under the big lock) | `kcore/tests/park_concurrency.rs`: loom and shuttle park models with three negative controls (prepare after registering, `park` read outside the run-queue lock, the waker skipping the run-queue lock) |
| PK2 | A thread runs on at most one hart and is never queued while running (C4 across harts) | `Sched::audit` and the multi-hart random model (`sched_smp_model.rs`); the park model has no second hart, so it does not check this |
| W1' | The dirty log is empty whenever the object lock is released (replaces W1) | by construction in the guard; loom model; `wait-audit` |
| ID1 | No runnable thread stays queued while a hart it may run on idles with no IPI pending | shuttle idle model with three negative controls; the starvation watchdog |
| GP1 | At quiescence no hart runs a thread while a higher-priority thread that may run there is queued (throttled RT excepted) | `Sched::audit` after every model step, against an independent reference |
| AF1 | A pinned thread's record is homed only on its hart | `Sched::audit` |
| DN1-DN3 | Effective priority is exactly the maximum of the base and the donors' effective priorities; the donation graph is acyclic; an edge exists only while its donor is in the wait that made it | `Donations::audit` against a recompute reference; the `ipc` and futex model audits |
| PF1-PF2 | A PI futex with waiters names a live owner that every waiter donates to; unlock hands over to the queue head | futex model, loom, QEMU |
| PR1-PR2 | A thread's base priority never exceeds its job's maximum; a job's maximum and RT budget never exceed its parent's | job model; QEMU probe from a confined program |
| NM1 | When RT budgets are enforced, the normal band gets at least 20% of every window on a hart where it has work throughout | simulated-clock property test; QEMU runaway test |
| AC1 | CPU time is conserved: per hart, thread time plus idle time equals elapsed time | model; QEMU within tolerance |
| TLB1 | Live address spaces have distinct ASIDs; an ASID is reused only after it was flushed on every hart that ran its last owner | allocator model; `fault-asid-reuse-no-flush` |
| TLB2 | A frame that was mapped is freed only after every hart in the address space's `ran_on` mask has been flushed for that page (replaces SMP5). **Platform requirement:** SBI remote fence calls return after the target harts have flushed | `unmap_while_siblings_touch`, the negative controls |

## Namespaces and paths

| # | Invariant | How checked |
|---|---|---|
| N1 | `..` in a path never escapes a directory handle | Property test (S4) |
| N2 | Servers never follow symlinks themselves; the client resolves them in its own namespace | Protocol test (S4) |

## Network (S8, NET-SPEC section 11)

| # | Invariant | How checked |
|---|---|---|
| NET1 | Every pool slot is in exactly one ownership state at all times; no slot is in two rings or lost | `netring/tests/ring_model.rs` (every operation of both sides against a reference that tracks each slot, 300 seeds); QEMU `net_driver_attaches_and_exchanges_a_frame` |
| NET2 | The stack never learns a physical address and never maps a pool writable that it should not | `netsup` gives `netd` duplicates without the DMA right and the receive pool read-only; the loopback test checks both (a writable map is ACCESS_DENIED, `vmo_phys_addr` is refused) |
| NET3 | The driver never maps a packet pool | by construction (`netdrv` is handed pool handles only for their physical addresses and maps its queue page, the ring VMO and the registers); the loopback test plays the stack |
| NET4 | No value read from a shared ring, the control header or a packet is trusted without validation | `netring::validate` and the `Pool` transitions; `malformed_entries_are_rejected`; the bad-slot negative control (`net_faults_are_survived`) |
| NET5 | Every queue and table is bounded and its full behaviour is the stated drop or error | `netcore` scripted tests (ARP cache, pending queues, rate limits), the flood test with the dropped count |
| NET6 | An endpoint exists exactly as long as its channel; closing or dying frees the port, the identifier and the account charge | `net_clients_are_confined` (port reuse, account arithmetic exact), `proto/tests/net_accounts.rs` |
| NET7 | Everything an endpoint holds is charged to its account; one account cannot exhaust another's | `net_clients_are_confined` (31 endpoints, then Quota; a second client unaffected) |
| NET8 | `netd` and `netdrv` never block on a client or on each other, only on their own port with a deadline | the flood test (a stalled endpoint, the stack keeps answering pings), the no-doorbell negative control (the safety poll covers it) |
| NET9 | No allocation is proportional to a packet-claimed value | `netwire` is `no_std` without `alloc`; `netcore` copies at most one MTU |
| NET10 | No panic on any input from the wire, a ring or a client | `netwire`'s 200,000-case totality test, `proto::net`'s mutation fuzz, `netring::msg` and the DHCP property tests |

## Process rules

- No `unsafe` outside the hardware-facing kernel binary; each block has a
  `// SAFETY:` comment. The pure crates `forbid(unsafe_code)`.
- Every change leaves the system booting and the suite green.
