# S5: SMP (spec, v2)

Status: v2, after a Fable review of v1 against the real code (findings folded in below and
marked *[R#]*). Nothing here is built yet. This file is the written acceptance
contract for S5 (CLAUDE.md: every milestone needs a runnable acceptance check written before
the code). Decisions made here are logged as OD60 and later in `OS-ARCHITECTURE.md` section 17
when the spec is accepted.

## 1. Goal and non-goals

**Goal.** The OS kernel runs on `N` harts (QEMU `-smp N`, `N` up to 8): user threads execute in
parallel on different harts; the kernel stays correct. Every earlier suite passes with `-smp 4`
in normal (multi-threaded TCG) and icount modes, a long stress run reports no lock-order
violation and no leak.

**Non-goals (S5).** Finer-grained locks than the big kernel lock are *not* required for
acceptance (the ladder in OS-ARCHITECTURE section 7: rung 2 is the deliverable; rung 3 is
optional and only where measured). No CPU hotplug, no NUMA, no per-hart frame caches, no
tickless kernel, no priority inheritance. The home computer (`home/`) stays single-hart.

## 2. The big kernel lock (BKL) model

**Rule K1.** Shared kernel state (registry, handle tables, scheduler state, VMOs, address spaces,
job ledger, every `static mut` in `kernel/src/`) is touched only while the hart holds the BKL.
Today the code asserts "interrupts are off" (`kernel()`, `state()`); that is true on a hart racing
without the lock, and `kmem::get()` asserts nothing, and tests call it with interrupts on. *[R5]*
**S5b replaces those asserts with "this hart holds the BKL"** in `thread::kernel()`, `user::state()`
and `kmem::get()`, lands them green on `-smp 1`, and fixes every caller that fails (the user tests'
`kmem::get()` calls move inside regions).
**Rule K2 (acquire and release points are explicit).** The BKL is taken by `kernel_lock` (alias
`irq::without`): disable interrupts, take the lock, run, release, restore; reentrant per hart (depth
count); spins with interrupts off. It is *also* taken, explicitly, at: the entry of **every trap**
(`trap_handler`, which calls `kernel()` and `state()` without `irq::without` today), and at the
return from `wfi` in the idle loop; and released at **every path that leaves kernel code**: the
`sret` in the trap exit, the second `sret` path in `enter_user` (`user_thread_main` calls
`irq::save_and_disable` and leaves through `enter_user`), and before `wfi`. *[R5]*
**Rule K3 (the lock follows the thread, not the hart).** A thread that blocks or is preempted
inside a locked region saves its depth in its `KThread`; the context switch hands the held lock to
the incoming thread, restoring *its* saved depth (a thread resuming from a block was inside a
region: depth >= 1; a brand-new thread starts at depth 1 and its bootstrap leaves the region). A
thread may resume on a different hart than it blocked on; ownership is recorded as "hart h holds
it" and rewritten at the switch. (The review found no deadlock or lost exclusion in block, preempt,
exit/reaper, join or bootstrap under this rule.)
**Rule K4.** Kernel code running with interrupts enabled (kernel threads outside regions, the test
suites) holds no lock and touches no shared state outside `kernel_lock`. A timer or IPI taken
there acquires the BKL in its handler.
**Rule K5 (lock order).** `BKL` before `HEAP` before `UART`. The heap and console get their own
interrupt-safe spinlocks because they are used outside the BKL. The console lock is taken **per
line**, not per `write_str`, so `##TEST` lines from two harts never interleave (the harness
parses lines); the panic path breaks the UART lock. Nothing takes the BKL while holding `HEAP` or
`UART` (a debug checker asserts it). User-mode execution holds nothing.

This is the classical "giant lock" shape. Syscalls and traps serialise; user code runs in
parallel; a long syscall stalls other harts' kernel entries (T11 gets worse before it gets
better; the fix is per-object locks, rung 3, not in S5's acceptance).

## 3. Per-hart state and boot

- **`PerHart` struct** per hart (static array of `MAX_HARTS = 8`): dense hart index, SBI hart id, idle
  thread id, the kernel stack top used on entry from user mode, BKL bookkeeping, the pending-IPI bits
  (`AtomicU32`), the hart's current user `satp` (see below), counters. **`tp` convention** *[R3]*:
  `tp` holds `&PerHart` in kernel code and is **never restored from a trap frame on a return to the
  kernel** (a kernel thread preempted in a handler on hart A and resumed on hart B would otherwise
  return with A's `tp`): the trap exit restores `x4` only on the return to user mode, from the frame
  (the user's `tp`), and the entry from user mode loads `tp` from `sscratch`, which holds
  `&PerHart` while the hart is in user mode (0 in the kernel as today). `PerHart` is read only with
  interrupts off, and its `kstack_top` is written on every return to user (today the kernel stack top is
  implicit in `sp + 288`). The boot hart's `tp` is set **before `trap::init`** (the S1 self-tests take
  kernel faults).
- **Hart discovery.** The boot hart (OpenSBI passes its id in `a0`) probes `hart_get_status` for ids
  `0..MAX_HARTS`; every id answering STOPPED is started with SBI HSM `hart_start(id, entry_pa, &PerHart)`
  once `kmem::init` has run and the allocator is up (`entry_pa` is the *physical* address of the entry
  symbol). Harts that do not exist or fail to start are logged (plain text, never `##`) and skipped.
  `PerHart` is indexed by a dense index, not by raw hart id.
- **Secondary entry** *[R2]* (assembly): HSM enters with `satp = 0`, paging off, interrupts off, `a0` =
  hart id, `a1` = opaque, and `stvec` undefined. The real kernel table has **no identity mapping** (only
  the image at `KERNEL_OFFSET`, the direct map and two device pages), so the entry first loads `satp` with
  **`EARLY_ROOT`** (still intact: it lives in `.bss`; it has the identity and upper-half mappings the boot
  hart used), jumps to the upper-half continuation, sets `stvec` and `sp` (the hart's boot stack), `tp`, and
  only then switches `satp` to the kernel root (recorded by `kmem` as a physical address in a static).
  Then: FS/VS off, `sie.SSIE` and `sie.STIE` on, the hart's own timer programmed, `online` set with
  release ordering, and the hart enters its idle thread. The boot hart waits (bounded) for every started
  hart and logs `harts online: N`.
- **Per-hart `satp`** *[R1]*: `ACTIVE_SATP` becomes per hart; **a hart loads the kernel `satp` whenever it
  switches to a thread that has no user address space or its current thread exits** (not only the hart
  that tears the process down: otherwise an idle hart keeps a dead process's root while another hart frees
  and reuses it, and the kernel half is re-walked through freed page tables).
- **Idle threads and boot stacks**: each hart has its own idle thread, pinned (section 4), at
  `PRIO_IDLE`; idle is `release BKL; wfi; acquire`. Hart boot stacks and idle-thread stacks are **32 KiB
  with a canary**, like every kernel thread. `thread_count`-style leak checks count `N` idle threads.

## 4. Scheduler

The scheduler core (`kcore::sched`) generalises from one `current` to `current[hart]`:
`Sched::new(class, harts)`; every operation that means "the running thread" takes the hart
(`block_current(h)`, `yield_current(h)`, `exit_current(h)`, `tick(h)`, `pick(h)`).

**Only hart `h` ever changes `current[h]`** *[R4]*. A wake, spawn, timer expiry or priority change on hart
A never re-designates another hart's current (that would leave B's running thread in the run queue where
a third hart can pick it: one thread, one kernel stack, two harts). Instead it returns the harts that
should reschedule (an idle hart, or one running something of lower priority): `wake`, `spawn`, `tick`
(sleeper expiry) and `set_priority` all return that set; the caller sets a per-hart `resched_pending`
flag and sends an IPI; hart B, on its next entry, calls `pick(B)`, which re-queues B's current if
something better exists. The audit invariant is therefore: every hart runs a thread; a thread is in exactly
one of {running on exactly one hart, queued, blocked/sleeping, dead}; each hart runs the best thread that
may run there **or a reschedule is pending for it**.

**Affinity** *[R6]*: every thread has `affinity: Option<Hart>` (idle threads are pinned; the boot
thread and the S2 suites' threads are pinned to the boot hart, since those tests assert uniprocessor
ordering such as "the higher-priority child ran before `spawn` returned"). Policy classes keep their
interface plus `pick_for(hart)`: one global run queue per priority under the BKL (rung 2). "Highest
priority runs" is a per-class property (`RoundRobin` has no priorities): the model suite is per class.

**Reaper** *[R6]*: reaping a zombie happens in one BKL region, so the leak oracles (`clean()`,
`thread_count`) cannot observe a half-reaped thread; the oracles that need quiescence wait for it.

Time *[R7]*: `ticks()` is derived from the `time` CSR (monotonic, global, no shared counter) and
**moves into S5a** with the per-hart timers. Each hart has its own deadline (no shared `DEADLINE`) and
keeps today's tolerance for a stalled host (the next deadline is a full tick away if it fell behind).
Strict wall-clock timeouts make T21 a little worse; accepted. A hart's tick calls `tick(h)` and advances
`Sched::now` to `max(now, ticks())`. In S5a a secondary's timer interrupt only counts itself (touches
nothing shared).

## 5. IPIs and TLB shootdown

- **IPI** = SBI IPI extension (software interrupt, `SSIP`; the handler clears `sip.SSIP`; `sie.SSIE` is
  enabled per hart). One handler; per-hart `pending: AtomicU32` says why. **Only `RESCHED`** *[R8]*: a
  `TLB` or `STOP` bit would deadlock, because an S-mode IPI cannot reach a hart that is spinning on the BKL
  with interrupts off. Wake/spawn/tick/set_priority that return harts to poke send `RESCHED` (not to self).
- **Remote TLB** *[R8]*: **SBI RFENCE `remote_sfence_vma_asid`** (and `remote_sfence_vma` for the kernel
  half), which OpenSBI performs in M-mode so it works on a hart that is spinning with interrupts off, and
  is synchronous. Any change to a *live* mapping other than invalid-to-valid (unmap, permission downgrade,
  VMO destroy of a mapped page, process teardown) is followed by the shootdown **before the BKL region
  that made the change ends** (frames and empty tables are freed *inside* `unmap_region`/`PageTable::unmap`,
  so "before the pages are freed" is not achievable and not the rule; no other hart can allocate a frame
  until the region ends, so a stale entry cannot see a reused frame). Teardown's flush moves inside its
  region (today `user.rs:500` is outside it, and `finish` runs it from a preemptible kernel thread). ASIDs
  are `pid & 0xffff`: a dead process's ASID is flushed on all harts at teardown. Invalid-to-valid
  transitions need no remote flush (`handle_fault` accepts the spurious fault; a local flush follows).
  Kernel-half unmaps happen only in the boot-hart self-tests.
- **Panic and exit**: whichever hart panics or finishes writes the test finisher; QEMU exits and the other
  harts die with it (no `STOP`).
- **Kill of a thread on another hart.** `process_kill` stays "takes effect at the victim's next trap"
  (OD45/46): every hart's 1 ms timer guarantees one within a tick; a `RESCHED` IPI to the victim's hart
  shortens it. Teardown waits for `threads_alive == 0` as now.

## 6. User-mode interplay checks

- Page faults from user mode enter the kernel and take the BKL; `AddressSpace::handle_fault` runs under
  it. Two threads of one process faulting on the same page serialise; the second finds it mapped.
- `usercopy` reads/writes user memory through physical walks under the BKL; another hart's user thread
  may write the same memory concurrently (that is the program's race, not the kernel's).
- `satp` is written on every switch to a user thread on that hart; `sfence.vma` as today.
- A thread's kernel stack is used by exactly the hart running it; context switch (asm) is per hart.

## 7. Slices, in order (each leaves `main` green, one PR each)

**S5a: bring-up with parked harts (built).** *(Deviation: `ticks()` stays the boot hart's counter in S5a, since only the boot hart schedules; the time-CSR clock moves to S5b/S5c where several harts tick. Each hart has its own deadline already.)* `PerHart`, the `tp`/`sscratch` convention (landed first, with the
existing S3 suites as its regression test, before any second hart runs), per-hart `satp`, HSM discovery and
start through `EARLY_ROOT`, per-hart timers on the time-CSR clock, idle threads that only idle; the
scheduler still schedules on the boot hart only. Heap and console locks (K5) land here (secondaries print).
Acceptance: `make os-boot-test` passes unchanged with `-smp 4`; `harts_come_online` sees `N` harts with
distinct `tp`, each having taken at least 3 timer interrupts; `ipi_reaches_every_hart` (the boot hart sends
`RESCHED`, every other hart's counter increments); `-smp 1` output unchanged.
**S5b: the BKL and the multi-hart scheduler core, host first (built).** *(Deviations: the kernel keeps using the single-hart interface of `Sched` (hart 0) until S5c; the multi-hart interface, affinity, the pending-reschedule protocol and the audit are in and tested on the host. The loom/shuttle models of the lock hand-over and the wake/IPI/idle race were not written (TIDY-UP T38). Parked harts do not loop on the lock during the suite: the exact-count test is one bounded burst, and sustained contention comes with S5c, when every hart ticks and schedules under the lock.)* `kcore::sched` with `harts`, affinity and
`current[hart]`, the weakened audit, a per-class model/property suite (random wake/sleep/priority/yield/
exit across up to 8 harts: no thread lost, none on two harts, a pending reschedule or the best thread),
and loom/shuttle models of the BKL hand-over (K3) and the wake/IPI/idle race. Kernel: `kernel_lock`, the
"hart holds the BKL" asserts in `kernel()`, `state()`, `kmem::get()`, explicit acquire/release points (K2),
the lock-order checker with its own failing-order test (`lock_order_violations_are_detected`),
`bkl_is_released_on_user_entry_and_idle`. **The parked harts take the BKL in a loop** (an exact-count test:
N harts each do M increments of a protected counter under `kernel_lock` while the boot hart runs the
suite), so S5b already contends the lock. Acceptance: host suites, `make os-boot-test` green with
`-smp 1` and `-smp 4`.
**S5c1 and S5c2: harts schedule (built together).** *(Deviations: kernel threads (the test suites, helpers, the serial bridge, the reaper) stay pinned to the boot hart by default; only user threads, new test threads made with `spawn_free`, and the secondaries' idle threads migrate. `ticks()` is still the boot hart's tick counter; each hart ticks its own slice. The BKL became a ticket lock.)*
**S5c1, as specified:** Secondaries run the scheduler under the BKL; wake IPIs;
`current[hart]`; every existing thread keeps its boot-hart affinity except the new tests' threads.
Acceptance: the whole suite passes with `-smp 2` and `-smp 4`; `a_wake_preempts_a_lower_priority_hart`;
`user_threads_run_in_parallel` measured by a kernel counter of **harts concurrently in U-mode** (a
simultaneity high-water mark, not recorded hart ids, which would only show migration).
**S5c2: free migration.** Affinity off for user threads and services; the shell session test and the full
suite with `-smp 4`; per-class model suite stays green.
**S5d: shootdown and kill.** RFENCE shootdowns inside the regions that change mappings, ASID flush at
teardown, the kill IPI. Acceptance: `unmap_while_siblings_touch` (one thread unmaps/remaps a region in a
loop while siblings on other harts touch it; the frame is refilled with a canary a stale translation would
read), `teardown_with_threads_on_other_harts`, `kill_a_spinner_on_another_hart` (returns within 2 ticks).
**Negative controls**: kernel features `fault-skip-shootdown` and `fault-skip-kernel-satp` that disable the
shootdown and the kernel-`satp` switch must make the corresponding tests fail (the harness runs them like
`fail-demo`).
**S5e: stress and icount (built, without SMP icount).** *(Deviation: the suite is NOT run under `-icount` with more than one hart. QEMU then runs the virtual CPUs one at a time in fixed turns, and a spin lock hands over only when the other CPU gets its turn, so even `-smp 2` did not get through the first tests in 25 minutes; the single-hart icount run stays in the gate. TIDY-UP T21 records it; the negative controls and the churn stress stand in for deterministic interleaving.)* A long stress run (random channel/VMO/process/thread/job churn from several
processes) with the lock-order checker and the end-of-run leak oracles (`job_ledger`, frames, objects,
threads); the whole suite in icount mode with `-smp 4`; TIDY-UP T9, T10, T12, T21 updated (T11 recorded
as worse, pointing at rung 3).

## 8. Acceptance (runnable, written now)

1. `make os-boot-test` runs the kernel under `OS_SMP=1`, `2`, `4` (new env var, passed as `-smp`):
   every test passes in each; the harness also runs `-smp 4` with `-icount shift=0,sleep=off` once S5e
   lands (and relaxes only the wall-clock windows of timing tests, never skips a test).
2. `make os-serial-test` (the shell session) passes with `-smp 4` (from S5c2).
3. New required test names (added to REQUIRED as their slice lands): `harts_come_online`,
   `ipi_reaches_every_hart`, `lock_order_violations_are_detected`, `bkl_is_released_on_user_entry_and_idle`,
   `bkl_is_exclusive_under_contention`, `user_threads_run_in_parallel`, `a_wake_preempts_a_lower_priority_hart`,
   `unmap_while_siblings_touch`, `teardown_with_threads_on_other_harts`, `kill_a_spinner_on_another_hart`,
   `smp_churn_stress`; the two negative-control builds must fail.
4. Host: the multi-hart scheduler model suite (per class), the BKL loom/shuttle models, Miri on the pure
   crates (CI).
5. CI runs the matrix with `OS_SMP=4` for the normal kernel and the release build.

## 9. Risks and open questions

- **QEMU TCG with `-smp 4`** on a 1-3 vCPU CI runner will be slow and its timing noisy (TIDY-UP T21). The
  timing tests already have a `loose()` mode; the plan is to use it whenever `harts > 1` and to prefer
  counter-based assertions (the new tests record hart ids and counters, not durations).
- **icount with SMP** serialises vCPUs round-robin: reproducible, but hides some true races; both modes run.
  BKL spinners may burn whole quanta there; a `pause`-style hint in the spin loop is allowed.
- **Priority inversion through the BKL** (a low-priority thread inside a region delays a high-priority
  hart's kernel entry): accepted for S5; bounded by region lengths (T11).
- **OpenSBI HSM behaviour** for harts that were never started: relies on `-bios default` leaving them
  STOPPED; if a hart is found already running, S5a logs and ignores it.
- **`sscratch` convention change** touches the trap entry and exit assembly (every user entry/exit);
  S5a lands it with the existing S3 suites as its regression test before any second hart runs.
- Open: whether the boot hart may be any hart id (OpenSBI picks) or only 0: S5a handles any id by
  indexing `PerHart` by a dense hart index obtained at discovery, not by raw id.
