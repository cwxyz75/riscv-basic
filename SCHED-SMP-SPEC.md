# SCHED-SMP: threads, the scheduler and the SMP execution model (specification, v1, 2026-11-14)

Status: **accepted by the owner 2026-11-14 (specified; nothing built)**, with the answers to the open questions of
section 9 recorded there (Q1 is deferred to this document's recommendation: one object lock). A design for the *final state* of three things that cannot be designed apart (the threading
model, the scheduler structure, the SMP execution model) and an ordered task list to get there from today's code.
Nothing in sections 3 to 5 is built unless section 2 says so. The owner's direction is recorded in section 1; the
choices made here are marked "Chosen here" and are the lead's and the owner's to veto. This document follows the
shape of `T-SPEC.md` and `W-SPEC.md`: decisions with reasons and rejected alternatives, invariants that each get a
check, acceptance written before code, risks, and open points.

Code references are to the tree at commit `fb5b911` (slice T step 5). Where this document and the code disagree
about *today*, the code wins and this document is wrong; where they disagree about the *target*, this document is the
proposal.

Reading order for an implementer: section 1 (the goal), section 7 (the task you were given), then the parts of
sections 3 to 6 that the task names. Section 8 holds text to paste into the other documents.

---

## 1. Goal state

### 1.1 Direction given by the owner (2026-11-14)

1. Threads are cheap, one-to-one kernel threads with their own kernel stacks, used freely (BeOS style: many threads
   per program, a message loop per component). No M:N, no scheduler activations; green threads are at most a later
   library (OD73).
2. Real-time priority threads, like BeOS, at some point. The architecture is ready for them now; policy detail later.
3. Away from the big kernel lock (BKL). Design together, **build in sequence**: slice L (per-hart run queues) is the
   first peel, not a separate project; one subsystem per small PR, in order of measured contention; the BKL stays for
   whatever has not moved; every peel lands with its own loom/shuttle model and negative control, written first.
4. A tiered scheduler: a real-time band (fixed priority, FIFO or round robin within a level, CPU budget and
   throttling, strict precedence) and a normal band. The normal band's fair (EEVDF-style, hierarchical) scheduler is
   **deferred**; this document builds the structure and stubs and keeps today's fixed-priority round robin in the
   normal band behind the new interface.

### 1.2 The final state in one page

| Area | Final state |
|---|---|
| Thread | A reference-counted kernel object (`Arc<Thread>`) with a 16 KiB guard-paged kernel stack billed to its job. Created and started by **one** syscall (`thread_spawn`) that also provisions and later frees the user stack. States: Embryo, Runnable, Running, Blocked, Exited, Reaped. Freed by the hart that switches away from it (no reaper thread). |
| Blocking | One protocol for every wait: **prepare, check and register under the lock that owns the condition, release it, block** (section 3.7). No lock is held across a context switch except the switching hart's own run-queue lock, which the incoming thread releases. |
| Scheduler | Per-hart run queues, each under its own lock, hosting three bands with strict precedence: **RT** (priorities 32-63, FIFO or RR, budgeted), **normal** (1-31; today's round robin now, a hierarchical fair class later), **idle** (0). A thread has a *base* priority (set through a capability-bounded call) and an *effective* priority (base raised by donation); its band is the band of its effective priority. |
| Placement | Global fixed priority over per-hart queues by push and pull: at quiescence no hart runs a thread while a higher-priority thread that may run there is queued anywhere. A wake by a caller that is about to block (`channel_call`) places the woken thread on the caller's hart (direct hand-off); other wakes prefer the thread's last hart, then an idle hart. |
| Priority inheritance | A pure donation graph (`kcore::donate`): a caller donates to the server thread that holds its `channel_call` transaction; a waiter on a PI futex donates to the owner; chains to depth 8; donation crosses bands; cycles are detected. |
| Authority | Every job has a maximum priority and an RT budget; a job whose maximum is below 32 cannot make an RT thread. There is no ambient way to raise either. |
| Time | One kernel clock in nanoseconds from the `time` CSR. Per-hart one-shot deadline timers (Sstc when present, SBI otherwise). CPU time accounted per thread at every switch. A periodic tick exists only while a hart has more than one runnable thread at the running level (slice end) or a budget boundary; an idle hart sleeps until its next deadline or an IPI. |
| SMP execution model | No big kernel lock. A fixed set of **domain spinlocks** in one checked global order (section 3.3): per-process address space, the object domain, process table, futex buckets, VMO store, job ledger, donation graph, thread table, per-hart run queue, kernel stacks, frames, heap, console. Spinlocks run with interrupts off and are short; everything long (user copies, page zeroing, teardown, sweeps) is chunked with preemption points. Kernel code holding no lock runs with interrupts on and is preemptible. |
| TLB | Unique ASIDs for live processes, no flush on an address-space switch, shootdowns by ASID and range to exactly the harts that have run the address space, batched per unmap, issued before the frames are freed. |
| Latency | A stated budget (section 3.12): the longest interrupts-off section and the RT wake-to-run latency are measured by kernel counters on every boot and reported. |

```text
 user      thread_spawn / thread_set_sched / futex(+PI) / channel_call (donates, hands off) / waits with deadlines
 ------------------------------------------------------------------------------------------------------------
 kernel    syscall code: interrupts ON, preemptible, takes domain locks briefly, in this order:
           AS(pid) -> OBJ -> PROCS -> FUTEX(bucket) -> VMO -> JOBS -> PI -> TT -> RQ(hart) -> KSTACK -> FRAME -> HEAP -> UART
           per hart:  RQ lock { RT queues | normal queues | idle }  + timer queue + accounting   <- tick, IPI, wake
           across harts: push/pull by priority, RESCHED IPI, SBI RFENCE (ASID, range, hart mask)
```

### 1.3 Non-goals

* M:N threading, scheduler activations, user-level scheduling in the kernel ABI (OD73).
* The fair class itself (EEVDF, group scheduling). Only its preconditions are built (section 4.7).
* Sleeping locks (mutexes, rwlocks) inside the kernel, RCU, and per-object locks. Section 3.1 says why not now and
  what keeps them possible.
* Deadline scheduling (EDF/CBS), CPU affinity masks beyond "pinned to one hart or free", CPU hotplug, NUMA,
  heterogeneous harts (OD22), more than 8 harts.
* Hard real-time guarantees. The RT band gives *bounded and measured* latency on QEMU, not a certified bound.
* POSIX signals, `pthread_cancel` asynchronous mode, `fork` of multi-threaded processes.
* Changing the home computer (`home/`): untouched.

---

## 2. Current state (as built, checked in the code)

"Built" means in the tree and exercised by the suite. "Specified" means written in a spec or the decision log but
not built. "Proposed" means this document.

### 2.1 Built

| What | Where | Notes |
|---|---|---|
| 1:1 kernel threads, per-thread kernel stack | `kernel/src/thread.rs:105-116` (`KThread`), `:54-98` (context switch) | A thread is a `KThread` in a `BTreeMap<Tid, KThread>` inside one global `Kernel` (`:118-128`); no reference counting: lifetime is "the map entry exists". |
| Kernel stacks: 16 KiB + 16 KiB guard, from frames, emergency stack | `kernel/src/kstack.rs`, `kernel/src/trap.rs:53-76`, `abi/src/lib.rs:366-370` | Slots are never unmapped or returned (T68). The window is 1 GiB: 32,768 slots. |
| Scheduler core, pure | `kcore/src/sched.rs` | `Sched<C: SchedClass>` hosts **one** class instance; **one global run queue** (`FixedPriority.levels`, `:77-79`) shared by all harts; `cur[h]`, `resched_pending[h]`, pokes; timers in one `BTreeSet` (`:235`); slice per hart in ticks. |
| Classes | `kcore/src/sched.rs:41-180`, `kernel/src/thread.rs:24-27` | `FixedPriority` (default) or `RoundRobin`, chosen by a Cargo feature (OD32). |
| Priorities | `kcore/src/sched.rs:30-37` | 0 idle, 1-31 normal, 32-63 RT; 1 ms tick; 10-tick slice (OD33). **Every user thread runs at priority 10**, hard-coded (`kernel/src/user.rs:346, 2581, 2693`). Only kernel threads use other priorities (the reaper at 50, `thread.rs:785`; the S2 tests). |
| Conformance and model suites | `kcore/tests/sched_model.rs`, `sched_smp_model.rs` | Random model per class, hand scenarios, the multi-hart audit (`sched.rs:682-773`). |
| Big kernel lock | `kernel/src/bkl.rs`, `kcore/src/ticket.rs` | Ticket lock, reentrant per hart, follows the thread across a switch (K3). Taken by `irq::without` (`kernel/src/irq.rs:44-51`) and by every trap except light IPIs (`trap.rs:306-344`). Interrupts are off for the whole of every syscall. |
| Lock-order checker | `kernel/src/bkl.rs:17-80` | Three classes (BKL, HEAP, UART), a per-hart bit mask, panics on a violation. |
| IPI | `kernel/src/smp.rs:150-174`, `percpu.rs:12-15` | SBI IPI; reasons `RESCHED` and (tests) `WORK`. |
| TLB shootdown | `kernel/src/smp.rs:330-348`; callers `user.rs:533, 2503` | SBI `remote_sfence_vma` of **everything on every other hart**, synchronous, under the BKL (T39). ASID is `pid & 0xffff`, unmanaged, and every `satp` write is followed by a full local flush (`user.rs:601, 608-633, 2305`; T66). |
| Timer | `kernel/src/timer.rs`, `thread.rs:434-450` | SBI `set_timer`, a periodic 1 ms tick per hart. **Sstc (`stimecmp`) is not used.** The scheduler clock is hart 0's count of handler runs (`TICKS`), so it stretches under lock contention (T40). |
| Targeted wake | `kcore/src/wait.rs`, `kernel/src/user.rs:1366-1391`, `thread.rs:595-644` | Dirty log, `WaitTable` of `(thread, mask)` registrations, wake by id. **Not built from W-SPEC:** the `Call{txid}` registration, the exact ready set and wake-one baton for ports (T60), per-handle binding lifetime (T61). `commit_wake` wakes one thread at a time with a reschedule between (T62). |
| Futex | `kcore/src/futex.rs`, `user.rs:1650-1710` | Key `(pid, address)` (`futex.rs:12`): process-private. Two processes sharing a VMO **cannot** wake each other (INVARIANTS F3). No owner tracking. |
| Deadlines on every wait | SYSCALLS.md | Absolute, in ticks (1 ms). |
| Jobs and quotas | `kcore/src/job.rs:37-47` | Processes, threads, pages, jobs, objects, IPC bytes. Kernel stacks are billed (4 pages per thread). No CPU or priority fields. |
| Thread creation | `user.rs:2646-2695`, SYSCALLS.md `thread_create` | **One** syscall creates and starts the thread. The *user* stack is the caller's to provide: `vmo_create`, `process_map`, `thread_create` and a `handle_close`, four syscalls in all (`libc/src/thread.rs:231-254`). |
| Limits | `abi/src/lib.rs:217-248` | `MAX_THREADS_PER_PROCESS` 256, `MAX_USER_THREADS` 1024, `MAX_OBJECTS` 16384, `MAX_HANDLES` 4096, `MAX_REGIONS` 1024, `MAX_PROCESSES` 64. SYSCALLS.md agrees (256 and 1024). The launcher's per-program job still allows 4 threads. |
| Reserved cancellation | `abi/src/lib.rs:41, 292`, `user.rs:2700-2710` | `INTERRUPTED` (-14) and `thread_interrupt` (63, NOT_SUPPORTED). |
| Latency check | `kernel/src/sched_tests.rs:436-500` | A *kernel* thread at RT priority wakes at most 1 tick late under three kernel hogs, with a negative control. Nothing measures a user thread. |

### 2.2 Specified but not built

* Per-hart run queues, the scheduler out of the BKL, user copies outside the lock, mappings holding a VMO reference
  (ROADMAP slice L, all steps).
* The T38 shuttle model of the wake/IPI/idle race (TIDY-UP T38; moved three times, never written).
* A `channel_call` round-trip benchmark and a thread create/join benchmark (OD73 item 1; W-SPEC section 5). Only the
  wake counters and the 60-round-trip ping-pong counter test exist (`init/src/bin/suite.rs:1806-1960`).
* The growable kernel heap (slice T step 2); the heap is a fixed 8 MiB (`kernel/src/heap.rs:13`).
* Priority donation: the decision log calls `channel_call` "the hook" (OD43); no code.
* The starvation watchdog of OS-ARCHITECTURE section 8 ("falls back to the default policy"): **not built**
  (ROADMAP S2 says so).
* Interrupt threads in the RT band (OS-ARCHITECTURE section 11): not possible today, user threads have no priority.

### 2.3 Corrections to the survey that asked for this document

* "Creation is two syscalls": no longer. `thread_create` starts the thread. The real cost is the four syscalls and
  one region for the user stack.
* "8 threads per process / 128 total": stale. 256 and 1024 in both `abi` and SYSCALLS.md since PR 114.
* "A starvation watchdog": it does not exist.
* "Each user stack is its own region, which caps threads near 1000": confirmed (one `process_map` per stack,
  `MAX_REGIONS` 1024, `kcore/src/aspace.rs:480`). The C library is tighter still: 64 stack slots
  (`libc/src/thread.rs:29`). Regions are already a `BTreeMap` (`aspace.rs:432`), so a higher cap costs no lookup time.
* "No userspace priority call": confirmed. No syscall sets or reads a priority.
* "Futex cross-process through a shared VMO": does not work, by design (OD68).
* "RVA23 includes Sstc, check what the kernel uses": SBI TIME only. The default test CPU is QEMU's default, not
  `rva23s64` (`scripts/os_boot_test.py:298`), so Sstc must be probed, not assumed. **Not verified here:** that the
  default CPU of the CI QEMU advertises `sstc`.

### 2.4 What the current model costs (structural, not yet measured)

* Interrupt latency on any hart is bounded by the longest BKL region on *any* hart, because a hart that wants the
  lock spins with interrupts off: a 64 KiB copy, `vmo_commit` of 1024 pages (zeroed under the lock), an object sweep
  (O(live objects)), a teardown, a full remote flush.
* Every hart takes the BKL 1000 times a second for its tick, whether or not it has anything to do.
* A wake that crosses harts costs an IPI and a BKL acquisition on the target before the woken thread runs.
* Nobody has measured any of this. Task 1 (section 7) does, before anything else changes.

---

## 3. The SMP execution model

### 3.1 The decision, and what was rejected

**Chosen here: domain spinlocks in one checked global order.** Each pure `kcore` structure that is one value today
(the scheduler, the futex table, the job ledger, the VMO store, an address space, the object registry with its handle
tables and wait table) stays one `&mut` value with its model tests, and the kernel wraps each in its own spinlock.
"Fine-grained" means *one lock per structure* (and per hart, per process and per futex bucket where the structure is
already partitioned that way), not one lock per object. The BKL shrinks as structures leave it; what is left at the
end **is** the object domain (`OBJ`), which is renamed, not removed.

Why this shape:

* It is what the code can become one PR at a time. The pure crates take `&mut` worlds; a lock per world changes no
  pure code and keeps every model test valid (design review D2).
* The locks are few (thirteen classes), so the whole order fits in one table that a checker enforces and a reviewer
  can hold in their head. All code is AI-written and nobody reads it: the lock order must be mechanically checked and
  small, not a convention across hundreds of per-object locks.
* Each lock protocol is small enough to model under loom and shuttle with the real pure code inside.
* It removes the costs that matter: page faults and user copies of different processes run in parallel; the
  scheduler, futexes and timers never touch the object lock; the object lock is held for O(1) queue operations, not
  for copies.

**Rejected: per-hart ownership with message passing between harts** (the multikernel or "share nothing" shape). The
kernel's central structures are shared by nature: a handle table and an address space belong to a process whose
threads run on several harts at once; a channel joins two processes; the reachability sweep (OD37) needs every handle
table in one step. Owning each on one hart turns every syscall on a shared object into a cross-hart request with a
reply, that is an IPI round trip where a spinlock costs two atomic operations, and the request queues need the same
ordering discipline a lock order gives. It fits the *scheduler* (each hart owns its run queue), and the design below
is per-hart there.

**Rejected for now: per-object locks with lock-free (RCU-style) lookups** for handles and the registry, which the
owner leans to. This is the one place this document pushes back. Reasons: (1) the owner's own rule is to peel in
order of *measured* contention, and with copies, faults and scheduling out of it the object lock is held for tens of
instructions per operation; (2) a lock-free lookup needs deferred reclamation (quiescent states or hazard pointers)
and raw-pointer lifetimes, the kind of `unsafe` reasoning that neither Miri in the pure crates nor a model test covers
in the kernel binary; (3) per-object locks break the stop-the-world cycle sweep and the single dirty-log commit that
slice W just built. What keeps it possible later is listed in section 3.13 as constraints on the code, and the
measured trigger for doing it is stated there. This is open question Q1.

**Rejected: sleeping locks in the kernel.** A blocked lock holder needs priority inheritance inside the kernel and a
second wait path. Every critical section here is short by rule (3.12), so a spinlock is enough.

### 3.2 Rules

* **X1 Interrupts are off while any spinlock is held** (unchanged, C2). `klock` saves and restores the interrupt state.
* **X2 Locks are taken in ascending class order** (3.3). Two locks of the same class are taken only where the table
  says so, in ascending instance order (hart index, pid, bucket index).
* **X3 No lock is held across a context switch**, except the switching hart's own `RQ` lock, which is released by the
  thread that resumes (`finish_switch`). During the transition the legacy lock is *released* at a switch and re-taken
  by the resumed thread (3.5); it no longer follows the thread.
* **X4 No lock is held across anything unbounded**: a user copy of more than 16 KiB, zeroing more than 16 pages,
  a walk over all objects, all threads or all regions. Such work is chunked, and between chunks every lock is dropped
  and `preempt_point()` is called.
* **X5 A hart never waits for another hart's IPI handler while holding a lock or with interrupts off.** S-mode IPIs
  carry only `RESCHED`. Remote TLB flushes go through SBI RFENCE, which M-mode performs on a hart that is spinning.
* **X6 Kernel code that holds no lock runs with interrupts on and can be preempted and migrated.** Per-hart state
  (`PerHart`, reached through `tp`) is touched only with interrupts off. `current()` is an interrupts-off read of
  `PerHart.current`.
* **X7 The thread switch happens only in `schedule()`**, entered with interrupts off and no lock held, from: an
  explicit block, yield or exit; the return to user mode; the return from an interrupt to kernel code that held no
  lock; and `klock`'s interrupt restore when it re-enables interrupts with `need_resched` set and no lock held. A
  wake never switches; it sets `need_resched`. (This removes T62 by construction.)
* **X8 No heap allocation while holding `RQ`, in an interrupt handler, or in a wake.** Structures touched there are
  intrusive or have capacity reserved by `thread_spawn`. (Transitional: allowed, `HEAP` is later in the order.)

### 3.3 The lock hierarchy

A hart holding class *n* may take only classes numbered above *n*. "IRQ" says whether an interrupt handler (timer,
IPI, external) may take it.

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

Why each sits where it does (the nestings that actually occur):

* `AS` before `OBJ`: a receive holds the address space while it verifies the buffer, dequeues the message and copies
  it out, so a sibling thread cannot unmap the buffer in between (3.10). Nothing needs the opposite order: a syscall
  that names a process or VMO by handle resolves the handle under `OBJ`, takes a reference, **releases `OBJ`**, and
  then locks `AS`.
* `AS` before `FUTEX`: `futex_wait` translates and pins the word under `AS`, then compares and enqueues under the
  bucket lock. `AS` before `VMO` before `JOBS` before `FRAME`: a page fault walks regions, asks the VMO for a page,
  charges the job, takes a frame.
* `OBJ` before `JOBS`: object and queue quotas (OD59). `OBJ` and `FUTEX` before `PI`: a wait that donates records its
  edge while it registers. `PI` before `TT` before `RQ`: applying a priority change looks the thread up and requeues it.
* `OBJ`, `PROCS`, `FUTEX` before `TT`/`RQ`: wakes are issued inside the critical section that decided them (3.8).
* `RQ` near the end: the scheduler calls nothing but the allocator's free list in transition (X8) and the console.

`OBJ` and `VMO` never nest, nor `PROCS` and `VMO`, nor `FUTEX` and `VMO`; their relative numbers are arbitrary but
fixed, so the checker stays a single comparison.

A syscall that needs several domains whose nesting is not listed above takes them **one after another, with
rollback**, never nested. `thread_spawn` is the example: resolve the process handle (`OBJ`), check and count the
thread (`PROCS`), charge the job (`JOBS`), take a kernel stack (`KSTACK`), map the user stack (`AS`), create the
thread object (`OBJ`), each released before the next, each undone in reverse if a later one fails. A process killed
in between is handled as today: the new thread sees the kill before it runs user code.

### 3.4 `klock`: the lock type and the checker

A new crate `klock` (`no_std`; the one pure-side crate allowed `unsafe`, about 250 lines, run under Miri, loom and
shuttle). It extends `kcore::ticket` and `kernel/src/bkl.rs`'s checker; it does not replace the ticket algorithm.

```rust
/// What the lock needs from the machine. The kernel implements it; host tests use a std/loom/shuttle one.
pub trait Machine {
    fn irq_save() -> bool;              // disable interrupts, return whether they were on
    fn irq_restore(was: bool);          // may call `schedule()` (X7) when it turns them back on
    fn hart() -> usize;
    fn relax();
    fn acquired(class: Class, instance: u32);   // the order checker and the latency trace
    fn released(class: Class, instance: u32);
}
pub struct Spin<T, M: Machine> { /* kcore::ticket::Ticket + UnsafeCell<T> + class + instance */ }
impl<T, M: Machine> Spin<T, M> {
    pub const fn new(class: Class, instance: u32, value: T) -> Self;
    pub fn lock(&self) -> Guard<'_, T, M>;          // irq_save, acquire, acquired()
    pub fn try_lock(&self) -> Option<Guard<'_, T, M>>;
    /// For the run-queue hand-over only: take without a guard, release from another stack.
    pub unsafe fn lock_raw(&self) -> bool;          // returns the saved interrupt state
    pub unsafe fn unlock_raw(&self, was: bool);
    pub unsafe fn get_raw(&self) -> *mut T;         // valid only between lock_raw and unlock_raw
}
```

Under `cfg(loom)` and `cfg(shuttle)` the ticket's atomics are the checker's (as `kcore::sync` does today), so the
real lock runs in the models.

The checker (kernel side of `Machine::acquired`): each hart keeps a small stack of `(class, instance)` it holds
(`PerHart.held`, 8 deep; deeper is a panic). An acquisition is legal when the stack is empty, or the new class is
greater than the top's, or the classes are equal, the table allows two, and the instance is greater. A violation
panics in every build, as today, and the existing `lock_order_violations_are_detected` test is extended to a table of
every adjacent pair. The same hook records, in `latency-trace` builds, the `time` CSR at the outermost acquisition and
at the last release, and keeps the per-hart maximum with the acquiring return address (3.12).

`Depth`-style reentrancy exists only for `LEGACY` and `UART`. Every other class panics on a recursive take.

### 3.5 The fate of the BKL: a shrinking legacy lock

The BKL becomes class `LEGACY`, outermost. Rules during the transition:

* **G1** State that has not been peeled is touched only with `LEGACY` held (today's K1, narrowed). Each peel names the
  state it moves in INVARIANTS and flips the asserts (`state()`, `kmem::get()`, `kernel()`) for that state to "its own
  lock is held".
* **G2** A path that holds `LEGACY` may take any other lock. A path that does not hold it never takes it while holding
  another lock.
* **G3** Which syscalls and traps still take `LEGACY` is a table in the kernel (`LEGACY_SYSCALLS`, one bit per
  syscall number, plus one entry each for the page-fault, external-interrupt, timer and IPI traps). A peel clears
  bits. A host test compares the table with a column in SYSCALLS.md ("Locks"), so the document cannot drift.
* **G4** From task 12 the lock no longer follows the thread (K3 retired). `schedule()` called with `LEGACY` held saves
  the depth in the thread, releases the lock fully, switches, and the thread re-takes it to that depth when it
  resumes, before it returns from `schedule()`. Every legacy wait is therefore already written as
  prepare / register / block (3.7), because the lock no longer covers the gap.
* **G5** Legacy regions stay interrupts-off and non-preemptible. Long legacy operations get `legacy_relax()` points
  (commit the dirty log, release fully, `preempt_point()`, re-take) at the places listed in 3.12 until they are peeled.
* **G6** The end: when the table is empty of everything except object-domain syscalls, `LEGACY` is renamed `OBJ`, its
  reentrancy is removed, the trap entry stops taking it, and each object syscall takes `OBJ` explicitly (task 20).

### 3.6 Thread and process lifetime

```text
 Embryo --start--> Runnable <--> Running --block--> Blocked --wake--> Runnable
                                   |                    
                                   +--exit--> Exited --(switched away from; finish_switch)--> Dead --(reap)--> gone
```

* A thread is `Arc<Thread>`. Strong references: the thread table (`TT`, until reap); `PerHart.current` and
  `PerHart.prev` of the hart running or leaving it; a syscall in flight that looked it up. Wait tables, futex queues,
  the donation graph, timers and run queues hold a `Tid`, never a reference; they are purged by `forget_thread(tid)`
  on the exit path and audited to be empty of that tid at reap.
* `Tid` is a `u32` taken from a counter and never reused within a boot (as today). Exhaustion (2^32 threads) is a
  clean NO_MEMORY from `thread_spawn`, never a wrap.
* **Exit**: the exiting thread, still running and holding no lock, does in order: `forget_thread` (wait table, futex,
  donation edges, PI futexes it owns: 4.8); under `PROCS` drops the process's live count and decides whether it was
  the last; under `OBJ` marks its thread object TERMINATED (the guard's commit wakes joiners); then `schedule()` with
  its state set to Exited. It never runs again.
* **Dead**: `finish_switch`, run by the next thread on that hart right after the switch and after releasing `RQ`,
  finds `PerHart.prev` Exited and moves it to `PerHart.dead` (an intrusive list). Its stack is no longer in use by
  construction: the switch away from it is complete.
* **Reap**: at the next safe point on that hart (`preempt_point()`, return to user mode, idle), with no lock held, the
  hart drains its `dead` list: credits the job (thread count, stack pages), unmaps a kernel-provisioned user stack
  (5.3), frees the kernel stack slot (`KSTACK`), removes the `TT` entry and drops the reference. The reaper thread
  and its queue (`thread.rs:785-804`) are deleted. Leak oracles call `reap_quiesce()`, which asks every hart to drain
  and waits.
* **Kill of a thread running elsewhere**: unchanged in effect. The killer sets the process's `killed` (an atomic
  mirror of the `PROCS` field), wakes the process's blocked threads, and sends `RESCHED` to the harts in the process's
  `ran_on` mask; a victim sees the flag at its next return to user mode (every return path checks it). The kill and
  the check need no common lock: the flag is set before the IPI is sent, and the IPI forces a trap.
* A process is `Arc<Process>`: its address space under `AS`, its lifecycle under `PROCS`, its handle table inside the
  object domain. A thread holds a strong reference to its process, so a syscall never looks its own process up.
  Teardown runs in the last thread to leave, chunked (X4), after the address space's last hart has left it.

### 3.7 The one block/wake path (replaces OD34's token and W-SPEC's "one BKL region")

Each thread has an atomic `park` word with three values: `Idle`, `Prepared`, `Notified`.

```text
 waiter (running thread T, holds no lock)           waker (any context, may hold locks up to PI)
 -------------------------------------------         --------------------------------------------
 1 park := Prepared                                  a  make the condition true (under its lock L)
 2 lock L (the lock that owns the condition)         b  find T in L's wait structure, remove it
 3 if ready | killed | interrupted | past deadline:  c  if CAS(T.park, Prepared -> Notified) fails: done
        park := Idle; unlock L; return                   (T is not waiting; wakes do not queue)
 4 register T in L's wait structure                  d  lock RQ[T.home] (re-read home after locking; retry)
 5 unlock L                                          e  if T.state == Blocked: cancel its timer, choose a hart
 6 lock RQ[this hart]                                      (4.4), move its record, enqueue, mark that hart
 7 if park == Notified: park := Idle; unlock;              for a reschedule
        return                                       f  unlock; send the IPIs owed
 8 state := Blocked; arm the deadline timer;         (T not yet Blocked at e: it will see Notified at step 7)
   pick next; switch (RQ released by the next thread)
 9 (resumed) park := Idle; if still registered, lock L, deregister, unlock; go to 1 or return
```

* `sched::prepare_block()`, `sched::cancel_block()`, `sched::block(deadline) -> Woken | TimedOut` and
  `sched::wake(tid, hint)` are the whole interface. Sleeps, joins, object waits, port waits, `channel_call`, futex
  waits and kernel-thread waits all use it. `WaitQueue` and the wake token are deleted.
* The timer expiry (under `RQ[home]`) is a waker: it sets `park := Notified` and enqueues.
* **Why no wake is lost.** A waker can find T only after step 4, which is after step 1, so its CAS sees `Prepared`
  (or `Notified`). Steps 7 and 8 and the waker's step e are all under `RQ[T.home]`: either T reads `Notified` at 7
  and does not block, or T is Blocked before the waker gets the lock and the waker enqueues it. The switch completes
  under that same lock, so a thread is never enqueued while its stack is still in use on the old hart.
* **PK1** (invariant): after any interleaving of waiters and wakers in which a wake for T follows T's registration,
  T is not left Blocked. **PK2**: a thread is never Running on two harts and never in a run queue while Running.
  Both are checked by the loom and shuttle models of task 10, with three negative controls: skip step 1 (prepare
  after registering), check `park` outside the `RQ` lock, enqueue without taking `RQ[home]`.

### 3.8 The wake path and slice W's contract under fine locks

W-SPEC's commit contract depends on the BKL ("commit before the BKL is released; no new lock"). It becomes:

* The registry, the dirty log and the wait table share `OBJ`. **W1'**: the dirty log is empty whenever `OBJ` is
  released. It is enforced by construction: the only way to hold `OBJ` is `ObjGuard`, whose `Drop` drains the log,
  runs `WaitTable::commit`, and calls `sched::wake_many(&tids)` **before** unlocking. (The assertion that proved
  unsound in slice W, because kernel threads mutated and committed in separate regions, becomes true because there
  are no separate regions.)
* `wake_many` runs steps c to f of 3.7 for each thread, groups the IPIs, and never switches (X7). T62 is resolved.
* W2 to W4, P1 and PW are unchanged and stay checks on the pure `WaitTable`.
* The lost-wakeup argument of W-SPEC section 4 is replaced by PK1 plus W4: the waiter's check and registration are
  one `OBJ` region; every mutation commits before `OBJ` is released; the commit wakes by W4; the wake is not lost by
  PK1.
* The interrupt path: an external interrupt takes `OBJ`, fires the interrupt object, and the guard commits. That adds
  O(registrations on one object) to an interrupts-off section, inside the budget of 3.12.
* The wait table's registration becomes an enum, as W-SPEC section 3 specified and the code does not yet have:
  `Signals{mask}`, `Call{txid}`, `Port`. `Call` is needed for donation (4.8); its removal, by firing or by `forget`,
  returns the kind so the kernel revokes the caller's donation edge in the same region.

### 3.9 IPIs and the idle race

* Reasons: `RESCHED` only (and the tests' `WORK`). No `TLB`, `STOP` or `CALL` reason (X5).
* `resched_pending[h]` moves into `RQ[h]`. Marking it returns whether it was clear; **whoever turns it from clear to
  set owes exactly one IPI**, sent after the `RQ` lock is released and before the sender leaves its critical nest.
  Hart `h` clears it under `RQ[h]` in `schedule()`. A hart that is not yet in the scheduler has no `RQ` and is never
  marked (T65's two losses cannot occur).
* Idle, on hart `h`:

```text
 loop:
   interrupts off
   lock RQ[h]
   if anything is runnable or resched_pending: unlock; schedule(); continue
   program the timer for the earliest deadline in RQ[h] (or none); publish "idle"
   unlock RQ[h]                       (interrupts stay off)
   drain the dead list (3.6) if it is not empty, then restart the loop
   wfi                                (returns when an enabled interrupt is pending, even with sstatus.SIE = 0)
   interrupts on                      (the pending interrupt is taken here; its handler sets need_resched)
```

  A wake that lands after the unlock has enqueued under `RQ[h]` and sent an IPI; the IPI is pending, so `wfi` returns
  at once. The protocol relies on the RISC-V rule that `wfi` resumes on a pending-and-enabled interrupt regardless of
  the global enable. **Not verified here**; task 4 adds a QEMU test (`wfi_returns_with_interrupts_masked`) and the
  model's negative control "enable interrupts, then `wfi`" must lose a wake.
* **ID1** (invariant): a runnable thread that may run on hart `h` is never left queued while `h` idles in `wfi`
  with no pending IPI. Checked by the shuttle model of task 4 (the long-owed T38), with negative controls: the idle
  check outside the lock; a mark without an IPI; interrupts enabled before `wfi`.
* A debug-build **starvation watchdog**: every hart's timer handler checks, at most every 100 ms, whether a thread
  has been Runnable for more than 500 ms while a hart it may run on was idle, and panics with a structured
  `##PANIC starved thread` line. (This is a detector, not the "fall back to the default policy" of OS-ARCHITECTURE
  section 8, which is dropped: with classes composed statically there is nothing to fall back to.)

### 3.10 Memory: address spaces, user copies, frames, shootdown

* **Address space.** `Spin<AddressSpace>` per process (`AS`). Page faults of different processes no longer serialise.
* **User copies.** `copy_from_user` and `copy_to_user` take `AS` per chunk of at most 16 KiB, walk and copy, release,
  and call `preempt_point()` between chunks. Copy-in goes to a kernel buffer with nothing else held; only then is
  `OBJ` taken to queue it. Copy-out of a message: `AS { verify the whole buffer; OBJ { dequeue }; copy the first
  chunk }`, then the remaining chunks. If a sibling unmaps the buffer between chunks the call fails with INVALID_ARGS
  and the message is lost: that is the program's own race, stated in SYSCALLS.md (section 8.4). A call that fails
  on a pointer *before* anything is dequeued still changes nothing.
* **VMO store** under `VMO`; the job ledger moves out of it into `JOBS` (the pure `Vmos` methods take `&mut Jobs`
  as a parameter instead of owning it). Mappings hold a reference on their VMO, so `reap_vmos` disappears (slice L
  step 1, kept as task 15).
* **Frames.** `FrameDb` under `FRAME`. Each hart has a cache of up to 32 free frames in `PerHart`, touched only with
  interrupts off: allocation takes from the cache and refills 16 under `FRAME` when empty; a free puts into the cache
  and drains 16 when full. Cached frames have reference count zero and are counted as free by the leak oracles
  (`free_frames_accounted` adds the caches' atomic counts). Zeroing happens on allocation, outside `FRAME`.
* **ASIDs.** `MAX_PROCESSES` is 64 and QEMU implements 16 ASID bits, so a live process gets a **unique** ASID from
  a free list (0 is the kernel's), returned only after teardown has flushed it everywhere. No generations are
  needed. The width is probed at boot (write ones to `satp`'s ASID field, read back); the architecture does not
  promise a minimum, so a hart with fewer than 7 bits keeps today's flush on every switch (a boot-time choice,
  tested by forcing it with a kernel feature). With unique ASIDs the full `sfence.vma` after every `satp` write
  goes (T66). **TLB1**: no two live address spaces share an ASID, and an ASID is reused only after a flush of that
  ASID on every hart in its last owner's `ran_on`. Negative control: `fault-asid-reuse-no-flush`.
* **`ran_on`.** Each address space has an atomic hart mask. A hart sets its bit **before** it loads the `satp`.
  Bits are never cleared while the process lives (sticky; T71 records the refinement). Shootdowns target
  `ran_on` minus the calling hart.
* **Shootdown.** An unmap or a permission downgrade, under `AS`: clear the PTEs, collecting the virtual ranges and
  the frames into an `UnmapBatch` (up to 64 pages; a larger unmap becomes a whole-ASID flush); local
  `sfence.vma` for the batch; one `sbi_remote_sfence_vma_asid(mask, start, size, asid)` per contiguous range (or
  one whole-ASID call); **then** drop the frame references; then release `AS`. A hart that faults on this address
  space meanwhile spins on `AS` with interrupts off, which RFENCE reaches (X5). **TLB2** (replaces SMP5): a frame
  that was mapped in an address space is not freed until every hart in `ran_on` has been flushed for that page. The
  correctness still rests on RFENCE being synchronous (T64): written into INVARIANTS as a platform requirement.
  Kernel-half unmaps (returning kernel stack slots, T68) stay unbuilt.
* Teardown: unmap in batches of 64 pages with preemption points, one whole-ASID flush at the end, then free the root.

### 3.11 The job ledger

One lock, `JOBS`, around the unchanged pure `kcore::job::Jobs`. A charge walks at most the depth of the job tree,
which this document caps at 16 (`job_create` beyond it is NO_MEMORY; today the depth is unbounded and the tree at
most 256 jobs). J1 to J5 are unchanged. CPU time is **not** charged through `JOBS` on the switch path: each job has
a small `Arc<JobSched>` of atomics (4.10) that the scheduler updates without a lock.

### 3.12 Kernel preemption and the latency budget

In the final state a syscall is a sequence of short locked sections with interrupts on between them, so the kernel
is preemptible wherever it holds no lock (X6, X7). What bounds latency is then the longest locked section.

**Budget** (guest time from the `time` CSR, 100 ns resolution; QEMU TCG, so indicative; all four numbers are
provisional until task 1 has measured today's values, and are restated then):

| # | Quantity | Final target | Today (expected, unmeasured) |
|---|---|---|---|
| B1 | Longest interrupts-off section on any hart | 100 us | milliseconds (a sweep, a teardown, `vmo_commit` of 1024 pages) |
| B2 | Longest wait for any spinlock | 200 us | the longest BKL region on any hart |
| B3 | RT wake-to-run: from the instant a wake or timer makes the highest-priority RT thread runnable to its first user instruction, four harts, all loaded with normal-band hogs and syscall churn | 300 us at the 99th percentile, 1 ms worst | 1 to 2 ms (the tick) plus the longest BKL region |
| B4 | Timer accuracy: a sleep ends no earlier than its deadline and within B3 of it | as B3 | the next 1 ms tick |

**How it is measured.** A kernel feature `latency-trace` (on in the test kernel, off in release): `klock`'s hooks
and `irq_save`/`irq_restore` record per hart the longest interrupts-off span and the return address that began it,
the longest lock wait per class, and per syscall number the count, total and maximum time in `LEGACY`. A wake stamps
the thread with the `time` at which it became runnable; the return to user mode records the difference in a per-band
histogram (16 power-of-two buckets). `debug_stat` exposes them; the suite prints them as `##BENCH` lines (6.1).
Smoke asserts only loose bounds (B1 below 20 ms, B3 worst below 5 ms) so host noise cannot fail the gate; the soak
run records the real numbers and a regression of more than a factor of two against the recorded baseline fails it.

**Preemption points** (`preempt_point()`: with no lock held, drain the dead list, and if `need_resched`,
`schedule()`), and during the transition `legacy_relax()`, at these sites, each with a test that drives the long
case while an RT probe runs:

| Long operation | Bound per section | Site today |
|---|---|---|
| Message and socket copies up to 64 KiB | 16 KiB | `user.rs:898-1011, 1272, 1809` |
| `vmo_read`/`vmo_write`/`vmo_commit` | 16 pages zeroed or copied | `vmo_commit`: 1024 pages under the lock (`abi` `MAX_COMMIT_PAGES`) |
| Reachability sweep | made incremental: mark in slices of 256 objects under `OBJ` with a "sweep in progress" epoch that makes a concurrent handle transfer mark its object; or kept whole and moved to a low-priority kernel thread that takes `OBJ` per slice | `object.rs:383` |
| Handle-table drain and address-space teardown at process death | 64 handles or 64 pages | `user.rs:500-545, 2503` |
| `job_kill` and priority clamping over a job's threads | 64 threads | new |
| Port evaluation on commit | only ports with a touched binding (T60's fix); O(touched) | `wait.rs:93-98` |

The incremental sweep is the one item whose design is not settled here (open point in section 10); until it is, the
sweep runs whole under `LEGACY`/`OBJ` and is the known worst case of B1, reported by the trace by name.

### 3.13 What keeps a later object-lock split possible

Not built; these are constraints on code written meanwhile, so that splitting `OBJ` (per-process handle-table locks
with lock-free lookup, per-object locks ordered by `ObjId`, a stop-the-world epoch for the sweep) stays a refactor:

* Syscall code resolves a handle through one function (`resolve(table, handle, kind, rights) -> ObjRef`) and never
  indexes the registry directly.
* No code outside `kcore::ipc`/`kcore::object` holds a registry borrow across a call into another domain.
* An operation on two objects names them in `ObjId` order in its `kcore` signature.
* **Trigger to do it:** with all other peels done, the pair-scaling benchmark (6.1) at four harts is below 2.5 times
  one hart **and** the lock-wait trace attributes more than 30% of syscall time to `OBJ`. Otherwise it stays one lock
  (TIDY-UP T70).

---

## 4. The scheduler

### 4.1 Structure

```text
 kcore::sched (pure, no locks, simulated clock in ns)
   HartRq<N: SchedClass>            one per hart; the kernel wraps each in Spin<_> of class RQ
     rt:      RtClass               band RT      (priorities 32-63)
     normal:  N                     band normal  (1-31): FixedPrio today, RoundRobin by feature, Fair later
     idle:    Tid                   band idle    (0): the hart's own idle thread, never queued
     cur, need_resched, resched_pending, timers (deadline -> Tid), threads (the records homed here)
     acct:    per-band ns this window, rt_throttled, window id
   Sched<N>                         = Vec<HartRq<N>> plus the cross-hart operations, each written to touch
                                      one hart's HartRq, or two in ascending order: the shape the kernel locks
 kcore::donate (pure)               base and effective priority of every thread; the donation graph
```

* **Strict precedence** is in `HartRq::pick`: RT unless throttled (4.5), else normal, else idle. It is core
  mechanism, not policy: a class cannot override it.
* Classes are composed **statically** (`HartRq<N>`); the normal-band class is still chosen at build time (OD5, OD32).
  The RT class is not replaceable. The core still validates every pick (`bad_picks`).
* `Sched<N>` remains a single value for the model tests (`Vec<HartRq>` in one `&mut` world). The kernel does not use
  `Sched`; it holds `[Spin<HartRq<N>>; MAX_HARTS]` and calls the same `HartRq` methods and the same two-hart
  functions, which take `&mut HartRq` arguments. So the model tests exercise exactly the code the kernel runs, and
  the locking sequence around it is what the loom and shuttle models of section 6 cover.

### 4.2 Base and effective priority; the band

One number orders everything: `Prio` 0 to 63 as in OD33. `band(p)` is Idle for 0, Normal for 1 to 31, RT for 32 to
63.

```rust
pub struct SchedAttr { pub prio: Prio, pub rr: bool }    // rr: rotate among equals (RT only; the normal band always does)
```

* A thread's **base** attribute is set at creation and by `thread_set_sched`, and is bounded by its job (4.9).
* Its **effective** priority is `max(base.prio, effective priority of every thread donating to it)` (4.8).
* Its band, and therefore the class that queues it, is `band(effective)`. Donation from an RT thread to a normal
  server puts the server in the RT class for as long as the donation lasts and returns it afterwards: crossing bands
  needs no special case, it is a `dequeue` from one class and an `enqueue` in the other with `Enq::AttrChanged`.
* A thread whose job has exhausted its RT budget in the current window is **demoted**: for queueing it counts as
  priority 31, round robin, until the window ends (4.5).

Rejected: an independent `(class, priority)` pair per thread. It allows states with no meaning ("normal class,
priority 50") and makes "which is more urgent" a two-key comparison everywhere, including in the donation graph.
With one number the graph is a max over integers and precedence between bands is ordinary comparison.

### 4.3 The per-thread record and the class interface

```rust
pub type Time = u64;                       // nanoseconds since boot
pub struct ThreadSched {
    pub tid: Tid,
    pub base: SchedAttr,
    pub eff: Prio,                         // copy of kcore::donate's answer, updated under RQ
    pub demoted_window: u64,               // RT window id in which its job ran out of budget (0: none)
    pub state: State,                      // Runnable, Running, Blocked, Exited
    pub affinity: Option<u8>,              // pinned to one hart, or free
    pub last_hart: u8,
    pub slice_left: Time,
    pub deadline: Option<Time>,            // wake-up time while Blocked
    pub runnable_since: Time,              // for the latency histogram and the watchdog
    pub started: Time,                     // when it last got a hart
    pub cpu: Time,                         // total CPU time
    pub group: u32,                        // its job's scheduling group (reserved for the fair class; 0 now)
    pub job: u32,                          // index of its job's JobSched (4.10)
    pub class_words: [u64; 4],             // owned by the class (virtual runtime, lag, weight, deadline later)
}

pub enum Enq { New, Wake { slept: Time }, Preempted, SliceEnd, Yield, Migrated, AttrChanged }
pub enum Deq { Migrate, AttrChange, Gone }

pub trait SchedClass {
    const BAND: Band;
    fn name(&self) -> &'static str;
    /// `t` is runnable on this hart. Preempted goes to the front of its level; everything else to the back.
    fn enqueue(&mut self, t: &mut ThreadSched, how: Enq, now: Time);
    /// Remove a queued thread without running it.
    fn dequeue(&mut self, t: &mut ThreadSched, why: Deq, now: Time);
    /// Remove and return the best queued thread.
    fn pick(&mut self, now: Time) -> Option<Tid>;
    /// The effective priority of the best queued thread (what other bands and other harts compare with).
    fn top(&self) -> Option<Prio>;
    /// The best queued thread for which `ok` holds (a pulling hart's affinity test); not removed.
    fn top_where(&self, ok: &dyn Fn(Tid) -> bool) -> Option<(Tid, Prio)>;
    /// Should a queued thread of this class replace `cur` (of this class) right now?
    fn preempts(&self, cur: &ThreadSched, now: Time) -> bool;
    /// Charge `ran` to the running thread. True: its slice is over and an equal or better peer waits.
    fn account(&mut self, cur: &mut ThreadSched, ran: Time, now: Time) -> bool;
    /// When this class next needs the hart's timer while `cur` runs (slice end), if at all.
    fn next_event(&self, cur: &ThreadSched, now: Time) -> Option<Time>;
    /// A queued thread's effective priority changed within this band: put it where it now belongs.
    fn attr_changed(&mut self, t: &mut ThreadSched, old: Prio, now: Time);
    fn contains(&self, tid: Tid) -> bool;
    fn len(&self) -> usize;
    /// Placement: the hart a newly runnable thread should go to. Reads only the lock-free summary.
    fn select_hart(t: &ThreadSched, hint: WakeHint, view: &HartsView) -> usize where Self: Sized;
}

pub enum WakeHint { None, SyncHere(usize) }          // SyncHere: the waker is about to block on that hart
pub struct HartView { pub online: bool, pub cur: Prio, pub queued_top: Prio, pub queued: u32 }
pub struct HartsView<'a>(pub &'a [HartView]);       // a racy snapshot: a stale entry costs a worse placement, never safety
```

The old methods map as: `enqueue_tail`/`enqueue_head` to `enqueue` with a reason; `pick_next_where` to `pick` plus
`top_where` (affinity is applied when a thread is placed or pulled, not on every pick, because a per-hart queue holds
only threads that may run there: **AF1**, a pinned thread's record is only ever homed on its hart);
`would_preempt`/`would_rotate` to `preempts`/`account`; `remove` to `dequeue`.

The hooks the future fair class needs and today's classes ignore: `Enq::Wake { slept }` (lag on wake-up), `account`
in nanoseconds, `next_event` (slice length is the latency knob), `class_words`, `group`.

### 4.4 Placement: global priority by push and pull

One policy, `GlobalPrio`, used by the RT class and by the normal-band stub (so today's observable behaviour, "the
best runnable threads are the ones running", survives per-hart queues). The fair class will replace it for the normal
band with load-based placement through the same two hooks.

`select_hart(t, hint, view)`:

1. If `t` is pinned: that hart.
2. If `hint` is `SyncHere(h)`: `h`. (The waker blocks next, so `t` runs there with no IPI: direct hand-off, 5.5.)
3. If `view[t.last_hart].cur < t.eff`: `t.last_hart` (it preempts there; its cache is warm).
4. Otherwise the online hart with the lowest `cur`, if that is below `t.eff` (lowest index among equals).
5. Otherwise `t.last_hart` (it queues; a pull fetches it when some hart can take it).

**Push** is a mark: whenever a hart leaves a thread `q` queued (it enqueued a wake that did not preempt, or put back
a preempted or rotated thread) and some other hart `g` has `view[g].cur < q.eff` and `q` may run on `g`, it marks
`g` for a reschedule (one IPI, 3.9). **Pull** does the move: in `schedule()`, before picking, hart `h` compares the
priority `p` of its best local candidate with every other hart's `queued_top`; if some `g` has `queued_top > p`, it
locks `RQ[min(h,g)]` then `RQ[max(h,g)]`, re-checks, and moves `g`'s best queued thread that may run on `h`
(`top_where`) into its own queue. One pull per `schedule()` at most per other hart. Only pulls and wakes ever hold
two `RQ` locks.

An idle hart is the case `p = 0`: it pulls any queued thread from any hart (work conservation).

**GP1** (invariant, at quiescence: no IPI pending, no `resched_pending` set, no hart inside `schedule()`): for every
hart `h` and every queued thread `q` that may run on `h`, `eff(q) <= eff(cur[h])`, except RT threads on a throttled
hart. **GP2**: every thread is in exactly one place: running on one hart, queued on one hart, blocked with its record
homed on one hart, or exited (C4, restated). Both are `Sched::audit`, run after every step of the model tests.

Rejected: a shared RT queue under one lock (simple, but puts a global lock back on every RT wake); per-hart queues
with no pull (priority inversion across harts lasting a slice).

### 4.5 The RT class

* Fixed priority 32 to 63. Within a level FIFO: a thread runs until it blocks, yields or is preempted by a higher
  one; a preempted thread returns to the **front** of its level. With `rr` set it also rotates to the back of its
  level every `RT_RR_SLICE` (10 ms) when an equal-priority peer waits.
* **Budget.** Time is divided into windows of `RT_WINDOW` (100 ms), aligned to multiples of the window since boot,
  the same on every hart. Two limits:
  * *Per hart*: when the RT band (base or donated) has used `RT_HART_MAX` (80 ms) of a window on hart `h` **and** the
    normal band has a runnable thread on `h`, the hart is `rt_throttled` until the window ends: `pick` skips the RT
    class. If the normal band is empty the RT band keeps running (work conserving), and is throttled the moment a
    normal thread is queued there. **NM1** (invariant): in every window in which the normal band has a runnable
    thread on a hart for the whole window, it gets at least `RT_WINDOW - RT_HART_MAX` of that hart.
  * *Per job*: a job has `rt_budget` nanoseconds per window (4.9). Time run by threads whose **base** band is RT is
    added to the job's atomic `rt_used`; when it passes the budget the job is exhausted for that window and its
    RT-base threads are demoted (4.2) as each next passes an accounting point. Nothing walks the job's threads; a
    thread compares the job's exhausted-window id with the current window at `enqueue` and `account`.
  * A window boundary is a timer event on each hart that has throttled or demoted anything: clear, reschedule.
* A runaway RT thread therefore degrades the system (its job loses the RT band for the rest of each window; the hart
  gives the normal band a fifth) instead of hanging it.
* **Stub stage** (tasks 6 and 8): the fields, the window arithmetic and the accounting exist and are tested;
  enforcement is a constant `RT_ENFORCE = false`. **Built stage** (task 25): enforcement on, NM1 and the demotion
  rule model-tested with the simulated clock, and a QEMU demo with a spinning RT thread.
* Time donated to a boosted normal thread counts against the hart's limit only, not any job's (TIDY-UP T77).

### 4.6 The normal class now: `FixedPrio` (a stub, deliberately)

Exactly OD33 behind the new trait: round robin within a level, a 10 ms slice (`TIMESLICE`, now in nanoseconds), a
preempted thread goes to the front and keeps the slice it had left, a thread whose slice ends goes to the back. It
shares its queue structure (`PrioQueues`: a 64-bit occupancy mask and one deque per level) with the RT class. There
is no fairness between levels and none between programs: a prio-31 spinner starves prio 10, and a program with 100
runnable threads gets 100 shares (TIDY-UP T72). `RoundRobin` stays as the feature-selected alternative.

"Behaviour unchanged" is tested, not asserted: the old `Sched` is kept in the test tree as `sched_v1` and a
differential property test drives both with the same random operation sequences on one hart and requires identical
`current` after every step (task 6). On several harts the per-hart design is *not* step-identical to the global
queue; there the requirement is GP1 and GP2 plus the existing scenario tests.

### 4.7 The normal class later: hierarchical fair scheduling (sketch only; deferred)

Enough to be sure the interfaces above do not preclude it:

* **Entities.** A tree of scheduling entities mirroring the job tree down to a configurable depth: a job is an entity
  with a weight (`cpu_weight`, a job limit reserved now), threads are leaves. Per hart, each entity has a queue of
  its children ordered by virtual deadline (EEVDF: eligible when its lag is non-negative, picked by earliest virtual
  deadline, virtual time advancing at 1/weight). A program with 100 threads shares its job's entity, so it gets one
  program's share. Needs: `ThreadSched.group` and `class_words` (present), a per-hart per-group structure inside the
  class (class-private), job weights readable without a lock (`JobSched`, 4.10).
* **Weights from priority.** The normal priority 1 to 31 becomes a weight, `1024 * 1.25^(prio - 16)`; the ABI does
  not change. Latency is tuned by the slice a thread requests (a reserved field in `thread_set_sched`'s flags word).
* **Accounting.** `account(ran)` in nanoseconds at every switch and timer event (present). `Enq::Wake{slept}`
  carries what lag preservation needs.
* **Placement.** `select_hart` by load (present as a hook) and an idle-time or periodic balance through the same pull
  path, moving whole threads between the per-hart instances of one group.
* **Donation inside the band.** A boosted thread is charged to, and competes as, its donor's entity while boosted:
  `attr_changed` plus a "runs on behalf of" group field. Not designed further here.
* **Precedence.** Unchanged: RT above, idle below. NM1 already guarantees the fair class its floor.
* **Test preconditions that make it cheap:** the simulated nanosecond clock, the per-class conformance suite
  (GP2, no starvation within the band), a fluid-model oracle (ideal share by weight over a window; measured share
  within a tolerance), and the differential harness.

What is *not* decided and must be when it is built: the hierarchy depth, how group virtual time is kept consistent
across harts, and whether placement stays priority-driven for interactive threads.

### 4.8 Priority donation

**The graph** (`kcore::donate`, pure, under the `PI` lock in the kernel):

```rust
pub const MAX_DONATION_DEPTH: usize = 8;
pub struct Change { pub tid: Tid, pub eff: Prio }
pub struct Changes { /* at most MAX_DONATION_DEPTH + 1 entries, no allocation */ }
pub enum DonateError { NoThread, Cycle, TooDeep }
impl Donations {
    pub fn add_thread(&mut self, tid: Tid, base: Prio);
    pub fn remove_thread(&mut self, tid: Tid) -> Changes;        // drops its out-edge and every in-edge
    pub fn set_base(&mut self, tid: Tid, base: Prio) -> Changes;
    /// `from` (which is blocking) lends its effective priority to `to` until `revoke(from)`.
    pub fn donate(&mut self, from: Tid, to: Tid) -> Result<Changes, DonateError>;
    pub fn revoke(&mut self, from: Tid) -> Changes;
    pub fn retarget(&mut self, from: Tid, to: Tid) -> Result<Changes, DonateError>;  // revoke then donate, one change list
    pub fn eff(&self, tid: Tid) -> Option<Prio>;
    pub fn audit(&self) -> Result<(), String>;
}
```

* A thread has at most one out-edge (it blocks on one thing) and any number of in-edges.
* **DN1** (invariant, `audit`): `eff(t) = max(base(t), eff(d) for every d donating to t)`, exactly. To keep it exact
  with bounded work, a `donate` that would make a chain longer than 8 or close a cycle is **refused** (no edge), so
  no propagation is ever cut short. **DN2**: the graph is acyclic. **DN3**: an edge `from -> to` exists only while
  `from` is in the wait that created it (checked by the kernel: `forget_thread` and every wait exit revoke).
* The kernel applies a change list while still holding `PI`: for each entry, look the thread up (`TT`), lock its
  home `RQ`, set `eff`, and if it is queued call `attr_changed` or move it between classes; if it is running and
  no longer the best there, mark the hart. Holding `PI` across the applications keeps two change lists from
  interleaving.
* Base priority is bounded by the job; effective priority is not: a server in a job capped at 20 can run at 50 while
  it holds a priority-50 caller's request. That is the point, and it is bounded in time by the caller's deadline.

**Donation through `channel_call`** (built in task 22; refined by direct hand-off in task 24):

* A message queued by `channel_call` carries, kernel-side, the caller's `Tid`.
* When a thread `S` dequeues that message (any read path), the receiving endpoint records `held[txid] = caller` and
  the kernel calls `donate(caller, S)`. This happens under `OBJ` then `PI`.
* The edge ends, with `revoke(caller)`, when: a message with that transaction id is written to the channel (the
  reply, by any thread); the caller's `Call{txid}` registration is removed for any reason (timeout, kill, interrupt,
  peer closed: 3.8); `S` exits or is killed (`remove_thread`); the endpoint is destroyed.
* A refused donation (`Cycle`, `TooDeep`) leaves the call itself unaffected and increments a counter
  (`debug_stat`): a cycle of blocked calls is a deadlock that the calls' deadlines will break, and the counter is
  the diagnostic.
* Until task 24 there is a window between the request being queued and a server thread reading it in which nobody
  is boosted (a low-priority server that has been woken but not yet scheduled). Task 24 closes it: the wake caused
  by a `channel_call` donates to the woken receiver at once (provisional edge, confirmed or dropped when it reads).
  It needs the wake-one rule for ports (PW, T60), so that the woken receiver is unique.

**PI futex** (task 23):

* The lock word: 0 is free; otherwise bits 0-29 the owner's `Tid`, bit 30 `OWNER_DIED`, bit 31 `WAITERS`.
* User fast path: lock by compare-and-swap 0 to own tid; unlock by compare-and-swap own tid to 0. Either failing
  enters the kernel.
* `futex_lock_pi(addr, deadline)`: under `AS` (translate, pin) then the bucket lock: read the word; if 0, write own
  tid and return OK; if `OWNER_DIED` is set, return BAD_STATE; otherwise set `WAITERS` (compare-and-swap, retry on a
  change), check that the owner tid names a live thread of the caller's process (else BAD_STATE), enqueue the caller
  on the key **in effective-priority order, FIFO among equals**, `donate(caller, owner)` (a `Cycle` is the classic
  deadlock: return BAD_STATE without sleeping), and block.
* `futex_unlock_pi(addr)`: the word's owner must be the caller (else ACCESS_DENIED). No waiters: write 0. Otherwise
  hand the lock to the first waiter `W`: write `W | WAITERS-if-more`, `revoke(W)`, `retarget` every remaining
  waiter to `W`, wake `W`. The lock is handed over, never stolen, so the highest-priority waiter always gets it.
* The owner exits or is killed while holding: the kernel, on `forget_thread`, finds the PI futexes it owns (a
  per-owner list in the bucket records), writes `OWNER_DIED`, and wakes every waiter with BAD_STATE. The lock is
  poisoned; the kernel never guesses a recovery.
* A waiter that times out, is killed or is interrupted leaves the queue and is revoked; if it was the last, the
  kernel clears `WAITERS`.
* **PF1**: while a thread is queued on a PI futex, the word's owner field names a live thread and the graph holds
  the waiter's edge to it. **PF2**: the unlocker's successor is the queue's head. Model-tested against a reference
  that recomputes everything; loom for lock, unlock, timeout and owner exit racing.
* Ordinary `futex_wait`/`futex_wake` are unchanged and do not donate.

### 4.9 Authority over priority

* A job gains three limits, set by a new call `job_set_sched` (needs SET_LIMITS) and readable by `job_info`:
  `max_prio` (0 to 63), `rt_budget` (nanoseconds per RT window; 0 means none), `cpu_weight` (reserved, 1024).
  A child job's `max_prio` and `rt_budget` can only be set at or below its parent's (J1's shape). The root job
  has 63 and the whole window.
* **RT is a job property**: a thread can be given an RT base priority only if its job's `max_prio` is at least that
  priority. A job capped at 31 or below has no RT threads. There is no separate right bit: the capability is the
  job handle with SET_LIMITS, which a confined program never holds for its own job (OD57).
* `thread_set_sched(thread, prio, flags)` needs MANAGE on the thread handle (or names the caller with 0, which needs
  nothing and is still bounded) and fails with ACCESS_DENIED if `prio` exceeds the thread's job's `max_prio`.
* Lowering a job's `max_prio` clamps the base priority of every live thread in that job and its descendants at once
  (chunked, 3.12).
* A new thread inherits its creator's **base** priority clamped to the new thread's job's `max_prio`; the first
  thread of a process starts at `PRIO_DEFAULT` (10), clamped.
* **PR1** (invariant): `base(t) <= max_prio(job(t))` and `max_prio(child) <= max_prio(parent)`, always. **PR2**:
  no call raises a job's `max_prio` or `rt_budget` above its parent's. Both model-tested in `kcore::job` and probed
  from a confined program in the suite.
* Policy (not mechanism, owner's call, Q4): the launcher gives programs `max_prio` 20 and no RT budget; `init`'s
  session job 31; driver jobs made by `devmgr` an RT ceiling and a budget, so interrupt threads can sit in the RT
  band as OS-ARCHITECTURE section 11 wants.

### 4.10 CPU accounting

* The clock is `rdtime` scaled to nanoseconds (QEMU: 100 ns per count; the frequency comes from the device tree).
* At every switch, timer event and `thread_get_sched` on a running thread, `ran = now - cur.started` is added to
  `cur.cpu`, to the hart's per-band counter for the window, and (lock-free) to `JobSched.cpu` and, for RT-base
  threads, `JobSched.rt_used` of the thread's job.
* `JobSched` is a small record of atomics per job (`cpu`, `rt_used`, `rt_window`, `rt_exhausted_window`, `max_prio`,
  `rt_budget`, `cpu_weight`), created with the job, referenced by index from `ThreadSched`. The authoritative limits
  live in the ledger under `JOBS`; `JobSched` mirrors them for the scheduler. A job's CPU time is charged to it only
  (not to ancestors) on the switch path; `job_info` sums a subtree on demand.
* **AC1** (invariant): on each hart, the sum of all threads' `cpu` increments plus idle time equals elapsed time
  (model test with the simulated clock; in QEMU within a tolerance).
* This is what the fair class and a future per-job CPU quota both consume; neither is built here.

### 4.11 Timers

* Each hart has a timer queue inside `RQ[h]`: wake-up deadlines of the threads homed there, the running thread's
  slice end (only when `next_event` returns one), the RT window boundary (only when something is throttled or
  demoted), and in debug builds the watchdog period. An entry is a field in the thread's record plus a position in an
  ordered set whose capacity is reserved when a thread is created (X8).
* A blocked thread's deadline lives on the hart it blocked on and is cancelled under that hart's lock when it is
  woken, so timers are always inserted locally: no cross-hart timer programming, no timer IPI.
* The hart's one-shot hardware timer is programmed to the earliest entry whenever that changes: with `stimecmp`
  (Sstc) when the device tree's ISA string lists `sstc`, with SBI `set_timer` otherwise. Chosen once at boot.
* The timer interrupt: lock `RQ[h]`, read the clock, expire due entries (each a wake), account the running thread,
  set `need_resched` if the class or a wake asks, reprogram, unlock. It takes no other lock.
* **Stage 1** (task 3): the scheduler clock is derived from `time`, not from counting handler runs (T40).
  **Stage 2** (task 13): the per-hart queue and one-shot programming; sleeps and deadlines no longer quantise to
  1 ms; a hart with one runnable thread and no deadline takes no timer interrupt at all.
* **Timer slack (added by the lead after review; decided 2026-11-14).** Each thread has a `timer_slack` (nanoseconds,
  in `ThreadSched`, set through the reserved flags of `thread_set_sched`, clamped to at most 1 ms). Default: 50 us
  for a thread whose effective priority is in the normal band, **0 for the RT band** (the effective priority decides,
  so a donated server wakes exactly). A deadline entry therefore has a soft expiry (`deadline`) and a hard expiry
  (`deadline + slack`). The one-shot timer is programmed to the earliest *hard* expiry; when it fires, every entry whose
  *soft* expiry is already past is expired in the same interrupt. Invariants: an entry never fires before its
  deadline; it fires no later than `deadline + slack` unless the hart is busy in a longer section (the latency trace
  watches that). Purpose: bound the interrupt rate a normal-band program can cause with many short sleeps, and
  coalesce nearby wakes into one interrupt. Model test: random entries and slacks against a reference that checks
  both bounds and that the number of interrupts never exceeds the number of distinct hard expiries; negative control:
  a timer that fires at the soft expiry of the first entry only (interrupt count rises).
* **The ABI time unit (Q3, decided 2026-11-14):** every deadline and `clock_get` is in nanoseconds on the
  monotonic clock since boot, as absolute values; wall-clock time is a separate later call. The unit is independent
  of hardware resolution (QEMU's timer is 10 MHz, so 100 ns) and of the wake rate, which the timer queue and slack
  determine. The change is one flag-day PR folded into task 6.

### 4.12 Idle and tickless

Idle is the loop of 3.9. Tickless idle falls out of stage 2 of the timers: an idle hart programs its earliest
deadline or nothing. It is only safe once ID1 is model-checked and the watchdog exists, which is why it is its own
late task (28) even though the code is a few lines. A fully tickless *busy* hart (one runnable thread, no slice
timer) is the same mechanism and comes with it.

---

## 5. The threading model

### 5.1 Lifecycle, as the user sees it

| Step | Call | Notes |
|---|---|---|
| Create and start | `thread_spawn(process, args)` -> handle, tid | One syscall: kernel stack, user stack, thread object, start. The older `thread_create` stays (caller-provided stack). |
| Identify | `thread_get_sched(0)` -> sched, tid | `sched` packs base priority, flags and effective priority. A thread also starts with its tid in `a1`. |
| Priority | `thread_set_sched(thread, prio, flags)` | Bounded by the job (4.9). |
| Exit | `thread_exit(code)` | The kernel frees the kernel stack and a kernel-provisioned user stack after the thread has left them (3.6). |
| Join | `object_wait(handle, TERMINATED, deadline)` then `thread_info` | Unchanged. A detached thread has no handle and nothing to join. |
| Kill | process-wide only (`process_kill`, `process_exit`, a fault) | Unchanged: there is no per-thread kill. |
| Interrupt | `thread_interrupt(thread)` | Built in task 26: the target's current or next blocking call returns INTERRUPTED. |
| Suspend, resume | `thread_suspend`, `thread_resume` | **Reserved** (numbers assigned, NOT_SUPPORTED): a debugger's tool; the place is a per-thread suspend count checked where the kill flag is checked. |

### 5.2 Budget and target numbers

Per thread, resident: 16 KiB kernel stack (four frames, billed to the job as today), about 1 KiB of kernel heap
(thread, scheduling record, object, table entries; 264 bytes more once it uses floating point), and in user space
whatever the stack touches (typically one or two pages) plus a share of a page-table page. About **25 to 30 KiB** in
practice; the survey's 32 to 40 KiB is a safe upper figure. Virtual: a 32 KiB kernel slot and 256 KiB of user stack
plus a guard.

| Quantity | Target | Why |
|---|---|---|
| Live threads, system (the backstop) | 4096 | RAM / 64 KiB on 256 MiB: half of RAM at the upper figure. Needs the growable heap (slice T step 2); until then it stays 1024 |
| Live threads that must fit and run (acceptance) | 2000 at one, two and four harts | T-SPEC section 8, kept |
| Per process | no constant: the job's thread limit and page limit | `MAX_THREADS_PER_PROCESS` goes |
| Per launched program (job default) | 64 (from 4) | A looper per window or connection, with room |
| `init`'s session job | 2048 | |
| Create, run nothing, exit, join | at most half of the task-1 baseline, and at most three `channel_call` round trips | four syscalls become one |
| `channel_call` round trip, same hart, with direct hand-off | at most 60% of the task-1 baseline; unchanged with 1000 blocked bystander threads | W made it independent of bystanders; hand-off removes an IPI and a queue pass |
| Wake of a blocked thread to its first instruction, idle hart | within B3 (3.12) | |
| Scaling: N independent pairs on N harts against one pair on one hart, N = 4 | at least 3.0 for futex ping-pong after task 14; at least 2.5 for `channel_call` after task 20 | the trigger of 3.13 |

Every number that is a ratio to "the task-1 baseline" is provisional by construction: nothing has been measured.
Task 1 records the baseline in `TESTING.md` and this table is restated with absolute values in the same PR.

### 5.3 Limits become quotas; user stacks and the region cap

* A thread is charged to its job: one thread (the existing limit), four pages of kernel stack (existing), and its
  user stack's pages as they are touched (existing: the stack VMO belongs to the process's job).
* `MAX_THREADS_PER_PROCESS` is removed. `MAX_USER_THREADS` becomes a derived backstop, `RAM_SIZE / 64 KiB`, capped
  by the kernel-stack window (32,768 slots).
* `thread_spawn` provisions the user stack: an anonymous VMO of `stack_bytes` (default 256 KiB, at most 64 MiB),
  never given a handle, mapped read-write at an address the kernel chooses inside the process's **stack arena**
  (`0x30_0000_0000` to `0x38_0000_0000`, 32 GiB of the user half that nothing else uses), with `guard_bytes`
  (default one page) left unmapped below it. The search is "lowest free gap at or after a per-process hint, wrapping
  once", O(log regions) on the existing `BTreeMap`. The stack is unmapped and its VMO freed when the thread is
  reaped, so detached threads no longer leak (T52) and a thread never has to unmap the stack it stands on.
* One region per thread remains. `MAX_REGIONS` is raised to 16,384 as a backstop; each region is already charged to
  the job (the page-table estimate of OD56), so the job's page limit is the real bound. Rejected: one big sparse VMO
  for all stacks with guard holes: it needs decommit, holes in a VMO and a shootdown per thread exit for one region
  saved.
* The C library's 64 fixed slots (`libc/src/thread.rs:29`) go: `pthread_create` calls `thread_spawn`.

### 5.4 `tp`, TLS and the start state

A thread made by `thread_spawn` starts with `pc = entry`, `sp` = the top of its stack (16-byte aligned), `a0 = arg`,
`a1` = its tid, `tp` = the `tp` field of the arguments (opaque to the kernel), every other register zero, FP off
(FP1). The kernel never reads or writes the user's `tp` afterwards except to save and restore it (SMP1). The runtime
convention: `tp` points at a thread control block that `rt` allocates at the top of the stack it asked for; the C
library's TCB (errno, keys) is the same block. Compiler-generated TLS (`__thread`, `#[thread_local]`) stays
unsupported (OD67) until a program needs it; the convention above is what it would build on.

### 5.5 Direct hand-off on `channel_call` (OD73 item 4)

No special switch path. `channel_call` queues the request; the `OBJ` guard's commit selects the receiver to wake
(the thread blocked on that channel or, with PW, the one waiter chosen on its port); because the caller is about to
block, the wake carries `WakeHint::SyncHere(this hart)` and, from task 24, a provisional donation. The receiver is
enqueued on the caller's hart at the caller's effective priority; the caller blocks; `schedule()` picks the receiver.
The reply path is symmetric when the server replies and then waits (a reply-and-wait call in the service runtime is
the natural place to pass the hint; until `rt` has one, the reply is an ordinary wake).

Chosen here, the wake-placement policy: **a wake by a thread that is about to block goes to the waker's hart with no
IPI; every other wake goes by `select_hart` (last hart if it would run there, else the least urgent hart, with an
IPI).** Rejected: always hand off (a producer that keeps running would stack its consumers on its own hart); never
hand off (every call costs an IPI and a migration, and the server starts with a cold cache).

### 5.6 Interruptible waits (T56) and where they sit

`thread_interrupt(thread)` (MANAGE) sets the target's `interrupt` flag and performs `sched::wake`. Step 3 of the
block protocol (3.7) tests the flag: a blocking call that finds it returns INTERRUPTED and clears it. The flag stays
set if the target is not in a blocking call, and the next one returns INTERRUPTED at once (so an interrupt is never
lost between two waits). `channel_call` interrupted after its request was sent behaves like its timeout: the request
is not withdrawn; the reply can still be read by transaction id. This is cheap once every wait uses the one protocol,
so it is scheduled (task 26) rather than left reserved, but after S8: nothing before then needs it.

### 5.7 Futex scope

* Private futexes stay as they are (key: address space and address), with bucket locks.
* **Shared futexes are reserved, not built**: a flag bit in `futex_wait`/`futex_wake` (NOT_SUPPORTED when set) for a
  key of (VMO, offset), found by translating the address under `AS`. Nothing needs them until a shared-memory ring
  wants to sleep without a channel; the network design uses channels as doorbells.
* PI futexes are private only.

### 5.8 The message-loop library and the culture risk

`rt::looper` (OD73 item 5), built with or right after slice R, whose service loop is the server-shaped special case
of it:

```rust
pub trait Handler { fn handle(&mut self, msg: Message, cx: &mut Context) -> Flow; }
pub struct Looper;      // owns one thread (thread_spawn), one port, a table of handlers
pub struct Messenger;   // a cloneable sender to one handler: post(msg) (asynchronous) and call(msg, deadline)
```

One thread, one port, handlers run to completion, in arrival order. BeOS's recurring bug was the lock-order deadlock
between loopers that call each other synchronously while holding their own locks. Mitigations, all cheap:

* `Messenger::call` **requires** a deadline (no "forever" overload); the default helper uses 5 seconds.
* Debug builds of `rt` keep a per-thread count of held `rt::sync::Mutex` guards and panic if `call` is entered with
  one held, and give each `Mutex` an optional class number checked the way the kernel's checker checks.
* The kernel's donation graph refuses and counts cycles (4.8); `debug_stat` exposes the count and the suite's
  end-of-run check requires it to be zero.
* PI mutexes return a deadlock status instead of sleeping in a cycle.

### 5.9 pthreads

A shim, as OD73 says: `pthread_create` is `thread_spawn`; `pthread_mutex` is the futex mutex, or the PI futex with
`PTHREAD_PRIO_INHERIT`; `pthread_setschedparam` is `thread_set_sched` (SCHED_FIFO and SCHED_RR map to the RT band,
SCHED_OTHER to the normal band; out of range for the job is EPERM); `pthread_cancel` in deferred mode can be built on
`thread_interrupt` when something needs it. Asynchronous cancellation, signal masks and `fork` in a multi-threaded
process stay refused.

### 5.10 The syscall surface this adds

`thread_spawn` (64), `thread_set_sched` (65), `thread_get_sched` (66), `thread_suspend` (67, reserved),
`thread_resume` (68, reserved), `job_set_sched` (96), `futex_lock_pi` (112), `futex_unlock_pi` (113); three new
`job_info` selectors; `thread_interrupt` (63) built; a reserved flag on the futex calls. Full entries in the
SYSCALLS.md style are in section 8.4. The table has 57 numbers today and 65 after this; OS-ARCHITECTURE's "about
50" was passed some time ago, which is said here rather than hidden.

---

## 6. Verification plan

### 6.1 Benchmarks first

Nothing in this document's cost claims has been measured. Task 1 builds the benchmark and records a baseline before
any behaviour changes; every later task that claims a gain re-runs it and writes the before and after in its PR.

* **Output.** A new structured line, `##BENCH <name> <value> <unit>` (amends OD28). The harness
  (`scripts/os_boot_test.py`) writes them to a TSV when `OS_BENCH=<file>` is set and otherwise ignores them; a
  malformed `##BENCH` line fails the boot like any malformed `##` line. `scripts/bench_compare.py` compares a run with
  `bench/baseline.tsv` (checked in, updated on purpose).
* **Clock.** The `time` CSR through a new `debug_stat` index (so a benchmark does not depend on user-mode counter
  access or on the 1 ms tick).
* **Where.** `make os-bench` (soak tier; in `soak.yml`). The benchmarks are also run in the smoke boot in a short
  form (a tenth of the iterations) with only sanity assertions, so they cannot rot.

| Benchmark | What it runs | Reported |
|---|---|---|
| `thread_create_join` | create, run an empty body, exit, join, 1000 times; one and four harts | ns per thread |
| `wake_latency` | two threads ping-pong on a futex and on an event; free and pinned to one hart | ns per round trip |
| `channel_call_rt` | a client `channel_call`s a server (`port_wait`, read, write) 10,000 times; 0, 100 and 1000 blocked bystander threads | ns per round trip |
| `pair_scaling` | N = 1, 2, 4 independent pairs on N harts: futex pairs; `channel_call` pairs; first-touch page-fault loops; 64 KiB message copies | operations per second, and the ratio to N = 1 |
| `bkl_profile` | the whole suite | per syscall number: calls, total and longest hold; total wait; the ten worst printed |
| `irq_off_max` | the whole suite | per hart: the longest interrupts-off span and where it began |
| `rt_latency` | an RT probe sleeping to random deadlines while every hart runs normal-band hogs plus a churn of the long operations of 3.12 | the histogram, the 99th percentile and the worst case |

`rt_latency` uses the existing kernel-thread probe until task 8 gives user threads a priority; from then it is a user
program, which is the **demo of OS-ARCHITECTURE section 8** ("a high-priority thread stays responsive while
low-priority CPU hogs run"): `bin/rtdemo` prints the worst lateness of an RT thread at priority 40 while a `hog`
per hart spins and a second copy at priority 10 shows what it looks like without the band (the negative control).

### 6.2 Oracles and models, per mechanism

Every row is written **before** the code of the task that builds the mechanism (task numbers in section 7).

| Mechanism | Pure model or property test (host) | loom / shuttle | Negative control (must fail its test) | QEMU |
|---|---|---|---|---|
| Idle, wake, IPI (ID1), today's code then the new | `sched_smp_model` (exists) | **shuttle** `idle_wake_model`: harts as threads, the IPI as a pending bit, `wfi` as a wait on it (T38) | idle check outside the lock; a mark with no IPI; interrupts on before `wfi` | `wfi_returns_with_interrupts_masked`; `a_wake_preempts_a_lower_priority_hart` |
| Two-band `Sched`, same behaviour | differential test against `sched_v1` on one hart; conformance suite per class; GP1, GP2 audit after every step on 1 to 8 harts | | mutations: precedence reversed; `Preempted` enqueued at the back; a pull without the affinity test | the S2 and S5 scheduler suites unchanged |
| Park protocol (PK1, PK2) | | **loom** (two waiters, two wakers, a timer) and **shuttle** (wider, with migration) over the real `kcore` functions and `klock` | prepare after register; `park` read outside `RQ`; enqueue without `RQ[home]` | `wake_before_block_is_not_lost`, `producer_consumer_loses_no_wakeup`, the futex and wait suites |
| `klock` and the order checker | unit tests of the checker table | loom exclusion, `try_lock`, raw hand-over (extends `ticket_concurrency`) | a kernel that takes two classes in the wrong order (the existing test, widened to every adjacent pair) | `lock_order_violations_are_detected` |
| Run-queue hand-over across a switch, `finish_switch`, dead list | | shuttle: exit racing a join and a reap | free the stack before the switch completes (`fault-early-reap`) | `thousands_of_threads_come_and_go`, `many_threads_do_not_leak` |
| Placement: push and pull | GP1 on random wake/sleep/priority/yield/exit walks with pinned and free threads, against an independent reference that recomputes "who should be running" from scratch | shuttle: two harts pulling from a third | skip the pull; skip the push mark | `rt_threads_take_the_harts` (N RT threads of distinct priority on N harts, counters prove each ran while a lower one did not) |
| Donation graph (DN1 to DN3) | random add, revoke, retarget, set-base, remove against a reference that recomputes every effective priority from the edges | | propagation stops one hop early; revoke leaves the boost | |
| Channel donation | `ipc_model` extended: every `held` entry has a live edge; no edge without a `Call` registration | loom: reply racing a timeout racing a kill | no revoke on timeout | `a_call_lends_its_priority_to_the_server` (a mid-priority hog cannot delay a high-priority caller's reply) |
| PI futex (PF1, PF2) | model against a recompute-everything reference | loom: lock, unlock, timeout, owner exit | hand-over to a non-head waiter; no `OWNER_DIED` on exit | `pi_futex_bounds_inversion` (the classic three-thread test), `pi_futex_owner_death_poisons`, `pi_futex_deadlock_is_refused` |
| RT budget (NM1) and demotion | simulated-clock property: the normal band's share per window; a demoted thread never runs in the RT class in that window | | throttle never set; window never rolls | `a_runaway_rt_thread_does_not_hang_the_system` |
| Priority authority (PR1, PR2) | `kcore::job` model with the new limits | | the clamp skipped | `a_confined_program_cannot_raise_its_priority`, `job_set_sched_checks...` |
| Accounting (AC1) | conservation on the simulated clock | | | `cpu_time_adds_up` (tolerance) |
| Timers | deadlines fire in order, never early; cancel on wake; one-shot reprogramming chooses the earliest | shuttle: a wake racing its own timer | fire one entry early | `sleep_wakes_on_time` with a sub-millisecond bound |
| Address space, VMO, frame, job locks | the existing model tests (unchanged pure code) | loom: two faults on one page; a fault racing an unmap; frame cache refill and drain | a kernel feature per lock that skips it (`fault-skip-as-lock`, ...) | `unmap_while_siblings_touch`, `smp_churn_stress` with process and job churn added (T40) |
| ASID and shootdown (TLB1, TLB2) | an ASID allocator model; an `UnmapBatch` property (every unmapped page is covered by a flushed range) | | `fault-skip-shootdown` (exists), `fault-asid-reuse-no-flush`, frames freed before the flush | `unmap_while_siblings_touch`, `teardown_with_threads_on_other_harts`, `asids_are_unique_and_recycled` |
| `OBJ` guard commit (W1') | `wait_model`, `touch_complete` (exist) | loom: W-SPEC's two waiters, a waker, a timer and a killer, now over the real guard | a guard that unlocks before committing | the `wait_*`, `channel_call_*`, port suites; the `wait-audit` feature of W-SPEC |
| `thread_spawn`, stack arena, limits | arena gap search against a naive scan; charge and credit conservation | | the stack not unmapped at reap (leak oracle) | `ten_thousand_threads_leak_nothing`, `two_thousand_threads_live`, `thread_spawn_*` error cases |
| Starvation watchdog | | | a kernel that drops a poke (`fault-skip-poke`) must end in `##PANIC starved thread` | |

Miri runs every pure crate including `klock` (soak). Mutation checks are run once per new test, by hand (TESTING.md).
The independent oracles are: `sched_v1` (the old scheduler, for the differential test), the recompute-from-scratch
references (written from the invariant statements in this document, not from the implementation), the fluid model
(later, for the fair class), and the upstream suites that already run on top (Lua, the C transcript), which exercise
threads and futexes without knowing about any of this.

### 6.3 Where each test lives (TESTING.md)

* Tier 0, host: everything in the two left columns above. Loom and shuttle stay in `make os-test` with small bounds
  (seconds); the large seed counts go to soak.
* Tier 1, smoke (one boot, four harts): every QEMU test above, the short benchmarks, the latency counters with loose
  bounds.
* Tier 2, soak: the negative-control kernels (one boot each), `make os-bench` with the comparison against the
  baseline, the suite at one and two harts, the round-robin kernel, Miri, large-seed shuttle.
* A task marked "soak" in section 7 gets the `soak` label on its PR and one `make soak` run before merge.

### 6.4 What runs at `-smp 2` and `-smp 4`

Every task from 11 onward ticks its box only after the whole suite has passed in normal mode at `-smp 1`, `-smp 2`
and `-smp 4` (smoke covers 4; the task's soak run covers 1 and 2), and says so in the PR. Icount is not used with
more than one hart (T21). The negative controls run at `-smp 4`.

---

## 7. Migration plan

Each task is one small PR (or a batch of related commits), leaves a bootable green system, and has its acceptance
check written first. Sizes are estimates in lines of code plus lines of tests, **not measured**. "Fable" marks a
kernel or security slice that should get an independent Fable review in its own PR. "Stub" means the interface and
its tests exist and behaviour is unchanged; "Built" means it does what the final state says.

### 7.1 The ordered list

| # | Slice | Task | Acceptance, written first | Size | Needs | Review | State after |
|---|---|---|---|---|---|---|---|
| 1 | B | Benchmark suite, `##BENCH`, `make os-bench`, the baseline table in `TESTING.md`; restate 5.2's targets as absolute numbers | the harness rejects a malformed `##BENCH`; the baseline file exists; a deliberate 3x slowdown fails `bench_compare.py` | 350 + 150 | | default | Built |
| 2 | B | `latency-trace`: interrupts-off maximum with site, BKL wait and hold per syscall, the RT histogram (kernel-thread probe) | `the_latency_trace_can_see_a_long_section` (a test section of known length is reported within 20%); restate 3.12's budget with measured values | 300 + 150 | 1 | default | Built |
| 3 | B | Debts the peels would inherit: `wake_many` (T62), the cleanup guard (T63), the scheduler clock from `time` (T40) | the T62 scenario (one low waker, two high waiters, a hog, one hart) as a failing test first; `ticks_do_not_stretch_under_lock_contention` | 150 + 200 | | Fable (small) | Built |
| 4 | B | The T38 shuttle model of today's idle/wake/IPI protocol; fix T65; `wfi_returns_with_interrupts_masked` | the model fails on each of its three negative controls and passes on the code | 100 + 400 | | default | Built |
| 5 | paper | Accept this document: the ODs, the planned lock table in INVARIANTS, reserved syscall numbers, ROADMAP, TIDY-UP (section 8) | `check_docs.py` passes; no code | docs | | owner | |
| 6 | SC | `kcore::sched` v2: `SchedAttr`, `ThreadSched`, the new `SchedClass`, `HartRq` hosting RT + normal + idle, `PrioQueues`, nanosecond clock and accounting hooks. The kernel still runs it in **shared-queue mode** (one set of class queues for all harts, per-hart `cur`, as today) under the BKL | differential test against `sched_v1`; conformance suite per class; AC1; the whole suite unchanged | 700 + 600 | 3 | default | Stub (structure; behaviour as today) |
| 7 | SC | `kcore::donate`, wired to `HartRq::set_effective`; no producer of edges yet | DN1 to DN3 against the recompute reference, mutation-checked | 300 + 350 | 6 | default | Stub (graph built, unused) |
| 8 | SC | Authority and the priority API: job `max_prio`, `rt_budget`, `cpu_weight`; `job_set_sched`; `thread_set_sched`, `thread_get_sched`; inheritance and clamping; RT accounting without enforcement; `bin/rtdemo` | PR1, PR2 in the job model; `a_confined_program_cannot_raise_its_priority`; the user-level RT latency demo with its negative control | 500 + 500 | 6 | **Fable** | Built (API, bounds); Stub (budget enforcement) |
| 9 | TH | `thread_spawn` with a kernel-provisioned stack, the stack arena, tid in `a1`, `tp`; limits from quotas; `MAX_REGIONS` raised; `libc` and `rt` moved to it; launcher default 128 threads (Q5) | `ten_thousand_threads_leak_nothing`; `two_thousand_threads_live` (after slice T step 2); every error case of the entry; `thread_create_join` at most half the baseline | 500 + 450 | slice T step 2 for the 4096 backstop | **Fable** | Built |
| 10 | L | `klock` and the generalised order checker; the park protocol in `kcore`; every kernel wait converted to it **while still under the BKL**; the wake token and `WaitQueue` deleted | the loom and shuttle park models with three negative controls; the checker's pair table | 600 + 700 | 4, 6 | **Fable** | Built |
| 11 | L | Per-hart run queues under `RQ` locks taken *inside* the BKL; push and pull; `need_resched` (X7); `finish_switch`, the dead list and reap by the hart; the idle loop of 3.9; the watchdog | GP1, GP2 on 1 to 8 harts against the independent reference; the idle model updated; `rt_threads_take_the_harts`; negative-control kernels `fault-skip-poke`, `fault-early-reap`, `fault-skip-pull` | 900 + 800 | 10 | **Fable**, soak | Built |
| 12 | L | The scheduler leaves the BKL: the lock is released at a switch (G4), the timer and IPI traps and `thread_yield`, `clock_get` and pure sleeps stop taking it | `bkl_profile` shows zero BKL acquisitions from the tick; `pair_scaling` for futex-free yield pairs; the suite at 1, 2, 4 | 400 + 300 | 11 | **Fable**, soak | Built: first peel done |
| 13 | TM | Per-hart timer queues, one-shot programming (Sstc probed, SBI fallback), slice timer only when needed; **timer slack** (4.11); the nanosecond ABI itself is in task 6 | timers model with the slack bounds and the interrupt-count property; `sleep_wakes_on_time` to 100 us for an RT thread; a hart with one runnable thread takes no timer interrupt (counter) | 450 + 350 | 12 | default | Built |
| 14 | L | Futex peel: bucket locks; `futex_wake` leaves the BKL; `futex_wait` holds it only to translate the address | loom futex model extended (kill, timeout); `pair_scaling` futex ratio at least 3.0 at four harts | 300 + 300 | 12 | **Fable** | Built |
| 15 | L | Mappings hold a VMO reference; `reap_vmos` deleted (pure `kcore`; slice L's old step 1) | `aspace_model` with the reference count; the leak suites | 250 + 200 | | default | Built |
| 16 | L | `JOBS`, `VMO`, `FRAME` as their own locks (still nested inside the BKL), the ledger moved out of the VMO store, per-hart frame caches | the existing models; loom for the frame cache; exact frame oracles with caches | 450 + 350 | 10, 15 | default | Built (locks exist, not yet relied on) |
| 17 | L | Memory peel: per-process `AS`; page faults and user copies leave the BKL; chunked copies with preemption points | loom: two faults, fault against unmap; `fault-skip-as-lock`; `pair_scaling` for faults and 64 KiB copies; B1 restated | 700 + 500 | 16 | **Fable**, soak | Built |
| 18 | L | TLB: the ASID allocator, `ran_on`, batched range and ASID shootdown before the free, no flush on switch | TLB1, TLB2; `fault-asid-reuse-no-flush`; the two existing negative controls | 350 + 300 | 17 | **Fable**, soak | Built |
| 19 | L | `PROCS` and `TT`; process lifecycle, kill and teardown out of the BKL, chunked; preemption points in the remaining long legacy operations; the sweep decision (section 10) | `kill_a_spinner_on_another_hart`, teardown suites; `rt_latency` under teardown and sweep churn within B3 | 600 + 400 | 17 | **Fable**, soak | Built |
| 20 | L | The end of the BKL: `LEGACY` becomes `OBJ` with a commit-on-drop guard; object syscalls take it explicitly; syscalls run with interrupts on; K1 to K5 retired | W-SPEC's loom model over the real guard; `wait-audit`; the whole budget of 3.12 asserted in soak; `pair_scaling` for channels at least 2.5 or T70's trigger evaluated | 600 + 400 | 19 | **Fable**, soak | Built: final SMP model |
| 21 | W | The W remainder: `Call{txid}` registrations, the exact ready set and wake-one baton (T60), binding lifetime (T61) | W-SPEC section 5's port model and mutations | 500 + 600 | | **Fable** | Built |
| 22 | RT | Donation through `channel_call` at receive | `a_call_lends_its_priority_to_the_server`; the `ipc_model` edge audit; loom reply/timeout/kill | 350 + 400 | 7, 8, 21 | **Fable** | Built |
| 23 | RT | PI futex | PF1, PF2; the three QEMU tests of 6.2 | 400 + 500 | 7, 8, 14 | **Fable** | Built |
| 24 | TH | Direct hand-off: the sync wake hint and donation at wake | `channel_call_rt` at most 60% of the baseline on one hart; no IPI counted on a same-hart call | 250 + 250 | 11, 21, 22 | default | Built |
| 25 | RT | RT budgets enforced: hart throttle, job demotion, window events; `rr` rotation | NM1 and demotion on the simulated clock; `a_runaway_rt_thread_does_not_hang_the_system` | 300 + 350 | 8, 13 | **Fable** | Built |
| 26 | TH | `thread_interrupt` built; INTERRUPTED from every wait (T56) | each blocking call interrupted while blocked and before blocking; the flag is not lost between waits | 250 + 350 | 10 | default | Built |
| 27 | TH | `rt::looper` and the debug deadlock checks; pthread mapping of priorities and PI mutexes | a looper test program; a held-lock `call` panics in debug; the libc thread tests | 500 + 400 | 9, slice R | default | Built |
| 28 | TM | Tickless idle and tickless single-runner | the idle model with the periodic tick removed; idle harts take zero timer interrupts over a second (counter) | 100 + 200 | 13, 11 | default | Built |
| -- | later | **The fair class (EEVDF, hierarchical)**: deferred. Preconditions that make it cheap, all delivered above: the `SchedClass` hooks and `class_words` (6), nanosecond accounting (6, 13), `group` and `cpu_weight` plumbed (8), NM1's floor (25), the differential and conformance harnesses (6), the placement hooks (11) | | | | | Deferred |
| -- | later | Splitting `OBJ` (3.13); shared futexes; suspend and resume; returning kernel stack slots (T68); clearing `ran_on` bits | | | | | Deferred, each with a TIDY-UP entry |

### 7.2 Why this order

* **Measure, then paper, then structure, then locks.** Tasks 1 to 4 change no design and give every later claim a
  number. Tasks 6 to 8 change the scheduler's shape with no change in locking, so a regression there is not confused
  with a race. Task 10 changes the block/wake protocol with no change in which lock is held, for the same reason.
* **The scheduler is the first peel** (tasks 10 to 12) whatever the measurements say, because every other peel needs
  what it brings: a block path that does not depend on the BKL being held across the switch, wakes that do not
  switch, and locks that can be held with the BKL not held. It is also the predicted top contender: every hart takes
  the BKL a thousand times a second for its tick.
* **After that, the order is the measurement's.** The default order given (futex, then memory, then process
  lifecycle, then the object domain) is the prediction: futexes because they touch nothing else and are the userland
  lock primitive; memory because copies, zeroing and faults are the longest holds; the object domain last because it
  is the hardest to split and what is left of it is short. `bkl_profile` after task 12 decides: **the next peel is
  the class of syscalls with the largest total hold time, unless a dependency in the table forbids it.** Tasks 14 to
  19 may be reordered on that evidence; 20 is last by definition.
* Tasks 21 to 28 depend only on what their "Needs" column says and can be interleaved with the peels. Donation
  (22, 23) is built while the BKL still covers the object domain; it uses the `PI` lock from the start, so it does
  not change when the BKL goes.

### 7.3 Relation to the existing roadmap

* **Slice T**: step 2 (the growable heap) is unchanged and is needed before task 9 can raise the thread backstop to
  4096. T's remaining "one-syscall creation" is already built; `thread_spawn` (task 9) supersedes its intent.
* **Slice W**: the unbuilt parts (step 4's ready set and baton, D17) are task 21. W's "no new lock" and W1 are
  amended by 3.8, not broken: the wait table still shares a lock with the registry.
* **Slice L**: replaced by tasks 10 to 20. Its step 1 is task 15, step 2 is tasks 10 to 12, step 3 is task 17, step 4
  is tasks 12 to 14, step 5 is the documents of each task. Its decision "keep one lock for object, handle, VMO and
  job state" is narrowed: VMO and job state get their own locks, the object and handle state keeps one.
* **Slice TS**: unchanged; the tiers are used as they are. Step 3 (logic out of QEMU) is helped by task 6 onward
  putting scheduler behaviour in host tests.
* **Slices R, SD, DM**: independent. Task 27 builds on R's service loop.
* **S8 and S9 (decided by the owner, Q2).** Nothing here blocks S8's acceptance, and S8's design (one-thread
  servers in separate processes) does not need the BKL gone. **Before S8: tasks 1 to 10**: the measurement, the
  paper, the userland-visible ABI (priorities, `thread_spawn`, nanosecond time) and the block/wake protocol, so that
  the service runtime and the network servers are written once against the final thread API. **After S8's first
  milestone: tasks 11 and 12** (per-hart queues, the scheduler out of the BKL), with the network stack as the load
  and the realistic benchmark. **Can wait until after S8:** 13, 14, 21 to 28. **After S9, in measured order:** 15 to
  20, chosen against the load the network stack provides.

---

## 8. Text to paste

### 8.1 Decision log (OS-ARCHITECTURE section 17)

The log's last entry is OD84 (the service runtime), so these are OD85 to OD96. `NET-DESIGN.md` section 13
had proposed OD84 to OD87 for the network design; those proposals are renumbered to OD97 to OD100 there.

| # | Decision | Status |
|---|---|---|
| OD85 | **SMP execution model** (`SCHED-SMP-SPEC.md` section 3). Domain spinlocks in one checked global order (`AS`, `OBJ`, `PROCS`, `FUTEX`, `VMO`, `JOBS`, `PI`, `TT`, `RQ`, `KSTACK`, `FRAME`, `HEAP`, `UART`): one lock per pure `kcore` structure, per hart for run queues, per process for address spaces, per bucket for futexes. Interrupts off while a lock is held; no lock across a context switch except the switching hart's run-queue lock; no sleeping locks; kernel code holding no lock is preemptible. The big kernel lock becomes a shrinking legacy lock, outermost in the order, released at a context switch, and ends as the object-domain lock. Rejected: per-hart ownership with message passing (the shared structures are shared by nature); per-object locks with lock-free lookups (deferred behind a measured trigger). Supersedes rung 3 of section 7 and the plan of roadmap slice L; amends OD60 | decided (specified) |
| OD86 | **One block/wake path** (section 3.7): prepare, check and register under the lock that owns the condition, release, block; a per-thread `park` word makes a wake between the release and the block impossible to lose; wakes never switch, they set `need_resched`. Replaces the wake token of OD34 and the "lock follows the thread" rule K3 of OD60; W-SPEC's W1 becomes "the dirty log is empty whenever the object lock is released", enforced by the guard (amends OD80) | decided (specified) |
| OD87 | **Scheduler structure** (section 4). Per-hart run queues hosting three bands with strict precedence (RT 32-63, normal 1-31, idle 0), composed statically; the normal-band class is still chosen at build time. A thread has a base priority and an effective priority (base raised by donation); its band is the band of its effective priority. Placement is global fixed priority by push (a mark and an IPI) and pull (the destination hart moves the thread). The normal band keeps today's fixed-priority round robin behind the new `SchedClass`; a hierarchical fair class is deferred with its preconditions listed. Amends OD5, OD32, OD33, OD61 | decided (specified) |
| OD88 | **Priority donation** (section 4.8). A pure donation graph (`kcore::donate`), one out-edge per blocked thread, chains to depth 8, cycles and longer chains refused so that effective priority is always exact. A `channel_call` caller donates to the thread that holds its transaction until the reply, a timeout or a death; a PI futex (owner tid in the lock word, hand-over on unlock, poisoned on owner death) donates from waiters to the owner. Realises the hook of OD43 | decided (specified) |
| OD89 | **Authority over priority** (section 4.9). A job has `max_prio`, `rt_budget` and `cpu_weight`, set with SET_LIMITS and never above its parent's; a thread's base priority never exceeds its job's `max_prio`; the RT band is reachable only in a job whose maximum allows it. No new right bit | decided (specified) |
| OD90 | **The RT band** (section 4.5). FIFO within a level, optional round robin; 100 ms windows; a hart gives the normal band at least 20% of a window when it has work; a job that exhausts its RT budget is demoted to the top of the normal band until the window ends | decided (specified) |
| OD91 | **Time** (sections 4.10, 4.11). One kernel clock in nanoseconds from the `time` CSR; CPU time accounted per thread at every switch and mirrored per job in lock-free counters; per-hart one-shot timers (Sstc when the device tree lists it, SBI otherwise); the periodic tick remains only as a slice or window event; per-thread timer slack (50 us default in the normal band, 0 in the RT band) lets nearby deadlines share one interrupt; the ABI's deadlines and `clock_get` are absolute monotonic nanoseconds (one flag-day PR in task 6). Closes the Sstc part of Q3 as "used when present" | decided (specified) |
| OD92 | **Threads** (section 5). `thread_spawn` creates and starts a thread in one call with a kernel-provisioned user stack in a per-process stack arena, freed when the thread is reaped; thread limits are job quotas (launcher default 128) with a RAM-derived backstop (16 per MiB, 4096 on 256 MiB); a thread is a reference-counted object freed by the hart that switches away from it; `tp` is the caller's to set. `thread_interrupt` is built; suspend and resume are reserved. Amends OD46, OD73, OD82 | decided (specified) |
| OD93 | **TLB** (section 3.10). Live processes have unique ASIDs (no flush on an address-space switch); a shootdown is by ASID and range, to the harts in the address space's `ran_on` mask, batched per unmap, and completes before the frames are freed. SBI RFENCE must be synchronous: a platform requirement. Amends OD62 | decided (specified) |
| OD94 | **Wake placement and direct hand-off** (sections 4.4, 5.5). A wake by a thread that is about to block places the woken thread on the waker's hart with no IPI; other wakes prefer the last hart, then the least urgent hart. `channel_call` uses the first rule and, with donation, is the direct hand-off of OD73 item 4 | decided (specified) |
| OD95 | **`klock`**: a small crate written here holding the spinlock type over `kcore::ticket`, the only pure-side crate allowed `unsafe`, run under Miri, loom and shuttle; the kernel supplies interrupts, the hart identity and the order checker through a trait | decided (specified) |
| OD96 | **Measurement** (sections 3.12, 6.1). `##BENCH` lines (amends OD28), `make os-bench` with a checked-in baseline, the `latency-trace` kernel feature, and a stated latency budget that the soak tier compares against | decided (specified) |

### 8.2 TIDY-UP.md

New entries (the next free number is T69):

| ID | Sev | Shortcut | Why acceptable | Proper fix, and when |
|---|---|---|---|---|
| T69 | M | **The big kernel lock survives as a legacy lock** over every syscall not yet peeled (`LEGACY_SYSCALLS`); those paths stay interrupts-off and non-preemptible | The transition is one subsystem per PR by decision | SCHED-SMP-SPEC tasks 12 to 20; gone at task 20 |
| T70 | M | **The object domain is one lock** (`OBJ`: registry, handle tables, wait table, ports) in the final design | Held for O(1) work once copies and faults are out; keeps the sweep and the commit simple | Split per SCHED-SMP-SPEC 3.13 if its trigger fires (channel pair scaling below 2.5 at four harts with over 30% of time waiting for `OBJ`) |
| T71 | L | **`ran_on` is sticky**: a hart that ever ran an address space gets its shootdowns until the process dies | Correct; shootdowns are by ASID and range, so cheap | Clear a hart's bit with a local ASID flush when it has not run the space for a while |
| T72 | M | **The normal band has no fairness**: fixed priority and round robin; a high-priority spinner starves lower ones, a program gets a share per thread | Today's behaviour, kept on purpose | The hierarchical fair class (SCHED-SMP-SPEC 4.7), deferred |
| T73 | M | **RT budgets are accounted but not enforced** between tasks 8 and 25: an RT thread in a job that was granted the band can monopolise harts | Only jobs granted `max_prio` at or above 32 can; the launcher grants none | Task 25 |
| T74 | L | **Shared futexes, thread suspend and resume are reserved, not built** | Nothing needs them | When a shared-memory ring or a debugger does |
| T75 | L | **Benchmark numbers are from QEMU TCG** and the targets in SCHED-SMP-SPEC 5.2 and 3.12 are provisional until task 1 | No hardware; ratios are still meaningful | Restate after task 1; revisit on real hardware if there ever is any |
| T76 | M | **The reachability sweep runs whole under the object lock**: O(live objects) with interrupts off, the known worst case of the latency budget | Rare (process death, pressure) | Incremental marking with an epoch, or a low-priority sweeper thread (task 19 decides) |
| T77 | L | **Donation gaps**: no boost between a request being queued and a server reading it (until task 24); CPU time and RT budget are not donated; a chain longer than 8 or a cycle gets no boost | The common case (a server blocked in receive) is covered by hand-off | Donate at wake (task 24); budget donation with the fair class |
| T78 | L | **The scheduler allocates under its lock during the transition** (X8 not yet enforced) | `HEAP` is later in the lock order, so it is safe, only slow | Intrusive queues and reserved capacity by task 13 |
| T79 | M | **Many hard limits assume QEMU as the target machine** (owner, 2026-11-14): thread, object, handle, region, IPC-byte, binding and stack-size constants (`abi/src/lib.rs`, OD50, T13), the 256 MiB RAM assumption, the 16 KiB kernel stack, the 1 ms and 10 ms scheduler constants. Until reviewed, new limits in this plan are expressed per MiB of RAM or as job-quota defaults, not fresh constants (the thread backstop is 16 per MiB of RAM, 4096 at 256 MiB) | Correct on the one machine that exists; raising them is a one-line change each | A review of every hard limit: classify each as a safety backstop, a policy default or a mechanism constant; derive backstops from RAM and hart count at boot; make policy defaults configurable (boot parameters, manifest) with a tight profile for small embedded machines and a large one for real computers; require that everything scales, with a test that boots at two RAM sizes. Do it before the first non-QEMU target, or when slice T finishes the growable heap, whichever is first |

Changes to existing entries: T11 "resolved step by step by SCHED-SMP-SPEC tasks 12 to 20"; T38 resolved by task 4
(the model) and kept current by tasks 10 and 11; T39 and T66 resolved by task 18; T64 resolved as a stated platform
requirement (INVARIANTS TLB2) at task 5; T40 (clock), T62 and T63 resolved by task 3; T65 by task 4; T56 by task 26;
T60 and T61 by task 21; T52's detached-thread leak by task 9; T12's residual (duplicate port wakes) by task 21.

### 8.3 INVARIANTS.md

Replace the "Lock order" section's list with the table of SCHED-SMP-SPEC 3.3, headed **"Planned order; a class is
real once the task that adds it has merged, and until then its state is under LEGACY"**, and keep the checker
sentence. Add:

| # | Invariant | How checked |
|---|---|---|
| X3 | No lock is held across a context switch except the switching hart's own run-queue lock, released by the thread that resumes | the checker panics in `schedule()` if the held stack is not empty; shuttle hand-over model |
| X7 | A wake never switches threads; the switch happens only in `schedule()`, entered with no lock held | code structure; the T62 scenario test |
| G1 | State not yet peeled is touched only with the legacy lock held; each peel names the state it moves (replaces K1) | the asserts in the accessors; `LEGACY_SYSCALLS` checked against SYSCALLS.md |
| PK1 | A wake that follows a waiter's registration is never lost (replaces the token rule of C3) | loom and shuttle park models with three negative controls |
| PK2 | A thread runs on at most one hart and is never queued while running (C4 across harts) | the same models; `Sched::audit` |
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

K1 to K5 are marked "legacy lock; retired at task 20". C3b's text is replaced by PK1 plus W1' when task 20 lands.

### 8.4 SYSCALLS.md

**Limits** (replace the thread sentence): "`MAX_REGIONS` 16384 per address space; live threads are limited by the
job's thread and page limits, with a system-wide backstop of `RAM_SIZE / 64 KiB` (4096)."

**Execution model** (replace the section when task 20 lands; until then add the table column): "A syscall runs on
the calling thread's kernel stack. Calls marked *legacy* in the Locks column run with interrupts off under the legacy
kernel lock; the others run with interrupts on, take the locks named for short sections, and can be preempted between
them. A call is atomic with respect to other threads only where its entry says so."

**Copying** (add): "A copy of more than 16 KiB is done in sections. If another thread of the same process unmaps the
destination buffer of a read while the read is copying, the read fails with INVALID_ARGS and the message or bytes it
had taken are lost: that race is the program's."

### thread_spawn(process, args) -> status, handle, tid

Create and start a thread in a running process, with its stack. `args` points to a 64-byte record (little endian):

| offset | field |
|---|---|
| 0 | `u64` entry |
| 8 | `u64` argument (the thread's `a0`) |
| 16 | `u64` `tp` value (opaque to the kernel) |
| 24 | `u64` stack bytes (0: 256 KiB; rounded up to a page; at most 64 MiB) |
| 32 | `u64` guard bytes (0: one page; rounded up to a page) |
| 40 | `u32` priority (0: the creator's base priority, clamped to the new thread's job) |
| 44 | `u32` flags: bit 0 round robin; bit 1 detached (no handle is returned); bit 2 caller stack (use offset 48, provision nothing) |
| 48 | `u64` stack pointer (caller stack only; otherwise 0) |
| 56 | `u64` reserved, 0 |

The thread starts at `entry` with `sp` at the top of its stack, `a0` the argument, `a1` its tid, `tp` as given, every
other register zero. A kernel-provisioned stack is mapped in the process's stack arena, is charged to the process's
job as it is touched, and is unmapped and freed when the thread has ended. Rights: MANAGE on `process`. Errors, in
order: INVALID_ARGS (bad `args` pointer); BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (no MANAGE; or the priority asked is
above the job's maximum); BAD_STATE (the process is not running); INVALID_ARGS (entry not executable; sizes out of
range; reserved bits or fields set; a caller stack pointer not 16-aligned or outside user memory); NO_MEMORY (a job
limit: threads, pages, regions, objects; the caller's handle table; the system backstop; no room in the stack arena).
On error nothing changes. Locks: legacy until task 19.

### thread_set_sched(thread, priority, flags) -> status

Set a thread's base priority (1 to 63) and flags (bit 0: round robin within its level; other bits reserved, zero).
`thread` 0 names the caller. Rights: MANAGE on the handle (none for 0). Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED (no MANAGE; or `priority` above the thread's job's `max_prio`); INVALID_ARGS (priority 0 or above 63,
reserved flags); BAD_STATE (the thread has ended). The effective priority may be higher while other threads donate.

### thread_get_sched(thread) -> status, sched, tid

`sched` packs the base priority (bits 0-7), the flags (bits 8-15) and the effective priority (bits 16-23): a syscall
returns two values besides the status. Rights: INSPECT (none for 0, the caller). Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED.

### thread_suspend(thread) / thread_resume(thread) -> status

**Reserved.** Validate the handle (MANAGE) and answer NOT_SUPPORTED.

### thread_interrupt(thread) -> status   (replaces the reserved entry at task 26)

Make the target's current blocking call, or its next one, return INTERRUPTED. Rights: MANAGE. Errors: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; BAD_STATE (ended). Every blocking call lists INTERRUPTED among its errors; a
`channel_call` interrupted after its request was sent leaves the request in flight, as a timeout does.

### job_set_sched(job, max_prio, rt_budget_ns, cpu_weight) -> status

Set a job's scheduling limits. Rights: SET_LIMITS. `max_prio` 0 to 63 and `rt_budget_ns` (per 100 ms window) must not
exceed the parent job's; `cpu_weight` 1 to 65536 (reserved for the fair class; stored). Lowering `max_prio` lowers the
base priority of every thread in the job and its descendants that is above it. Errors: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED (no SET_LIMITS; or a value above the parent's); INVALID_ARGS. `job_info` gains selectors 6 (`max_prio`),
7 (RT time used this window, budget), 8 (CPU nanoseconds used by the job's subtree, 0).

### futex_lock_pi(address, deadline) -> status   and   futex_unlock_pi(address) -> status

The word at `address` holds 0 (free) or the owner's tid in bits 0-29, with bit 31 set while threads wait in the
kernel and bit 30 if an owner died holding it. A thread takes a free lock in user mode by compare-and-swap from 0 to
its tid and releases an uncontended one by compare-and-swap back to 0; it calls the kernel only when either fails.
`futex_lock_pi` returns OK when the caller owns the lock; while it waits, the owner runs at least at the caller's
effective priority. Errors: INVALID_ARGS (as `futex_wait`); BAD_STATE (the word names no live thread of this process;
the lock is poisoned; or waiting would close a cycle of waiting threads: a deadlock); TIMED_OUT; INTERRUPTED.
`futex_unlock_pi` hands the lock to the highest-priority waiter (first come among equals) or frees it. Errors:
INVALID_ARGS; ACCESS_DENIED (the caller does not own it). `futex_wait` and `futex_wake` gain a flags argument whose
bit 0 (shared key) is reserved: NOT_SUPPORTED.

### channel_call (amend the last paragraph)

"From task 22 the caller lends its effective priority to the thread that reads its request, until a message with the
same transaction id is written back, the call ends for any reason, or that thread ends."

### 8.5 ROADMAP.md

* In "Queue before S8", after item 5 (slice T): "5a. Slices **B** (benchmark and latency instrumentation), **SC**
  (two-band scheduler, priority API) and the first lock peel of slice **L** (tasks 1 to 10 of `SCHED-SMP-SPEC.md`)", then "10a. Tasks 11 and 12 after S8's first milestone", and change item 10 to "S8, then S9; then the measured peels of L (tasks 15 to 20)".
* Replace the body of "### L: Kernel locking carve-outs" with: "Superseded by `SCHED-SMP-SPEC.md` (OD85). Slice L is
  now the sequence of lock peels there (tasks 10 to 20): the scheduler first, then futexes, memory, process lifecycle
  and finally the object domain, each a small PR with its model and negative control written first, in the order the
  contention profile gives after the first peel." followed by one unticked box per task 10 to 20.
* Add slices **B** (tasks 1 to 4), **SC** (6 to 8), **TH** (9, 24, 26, 27), **TM** (13, 28), **RT** (22, 23, 25),
  each with its acceptance line from 7.1 and unticked boxes; add task 21 as the remaining boxes of slice W step 4.
* In "S5: SMP", the unticked "Finer locking on the hot paths" box points at slice L as redefined.
* In "Directions under discussion", mark "Cheap kernel threads" and "Redo SMP and the scheduler" as specified here.

---

## 9. Open questions for the owner

1. **Q1. One object lock in the final state, or commit now to per-object locks with lock-free lookups?** This
   document keeps one lock for the registry, handle tables and wait table, with a measured trigger to split it
   (3.13). You leaned the other way. *Recommendation:* keep one lock; it is the only choice here that can be undone
   cheaply, and your own rule (peel by measured contention) decides it later with data.
2. **Q2. DECIDED (owner, 2026-11-14): tasks 1 to 10 before S8; tasks 11 and 12 (per-hart queues, the scheduler out of
   the BKL) after S8's first milestone, using the network stack as the load; the rest as in 7.3.** (The original
   question and recommendation follow, kept for the record.) When is the scheduler peel built relative to S8? *Recommendation:* tasks 1 to 12 before S8 (measurement,
   priority API, `thread_spawn`, the scheduler out of the BKL), the memory and object peels after S9 against real
   load. The alternative, everything after S9 as slice L was first placed, delays the thread API the service runtime
   should be written against.
3. **Q3. DECIDED (owner, 2026-11-14): nanoseconds, absolute monotonic, one flag-day PR in task 6; event-driven timers
   with per-thread timer slack (4.11).** (Original text follows.) The ABI's time unit. Deadlines and `clock_get` are in 1 ms ticks. Sub-millisecond sleeps (audio, the
   network's retransmit timers later) need a finer unit. *Recommendation:* one flag-day PR that makes every deadline
   and the clock nanoseconds since boot, before slice R and S8 are written; all userland is in this repository and
   rebuilt together.
4. **Q4. DECIDED (owner, 2026-11-14): the recommendation below, with the numbers kept in one overridable place and the
   driver budget checked against the benchmark before S9.** Default priority policy. *Recommendation:* programs started by the launcher get `max_prio` 20 and no RT
   budget; the session job 31; drivers started by `devmgr` an RT ceiling of 47 and 20 ms per 100 ms; only a manifest
   entry (S10) grants more. And the RT guard rails: 100 ms windows with a 20% floor for the normal band.
5. **Q5. DECIDED (owner, 2026-11-14), amending the recommendation below: the launcher default is 128 threads per
   program (not 64); the system backstop is stated as 16 live threads per MiB of RAM (4096 at 256 MiB) rather than a
   constant; 2000 stays the acceptance figure at 256 MiB. QEMU is not the real target: all hard limits are to be
   reviewed so that everything scales (tight for embedded, large for real computers), recorded as T79. Whether the
   kernel stack drops to 8 KiB is decided by the task 1 measurement.** Thread targets. *Recommendation:* 4096 live
   threads as the system backstop on 256 MiB, 2000 as the acceptance figure, 64 per launched program by default. If you want BeOS-scale counts per program (hundreds), the
   launcher default is the number to raise, not the mechanism.
6. **Q6. DECIDED (owner, 2026-11-14): after S8 as task 26; design the service runtime's request model around
   cancellation by closing the reply channel and by deadlines, and pull task 26 forward only if slice R needs it.**
   Is `thread_interrupt` wanted before S8? It is cheap after task 10 and scheduled after S8 (task 26).
   *Recommendation:* leave it there unless the service runtime (slice R) wants request cancellation in its first
   version.

---

## 10. Risks, open points and what was not verified

* **Not verified: any performance number.** Nothing was built, booted or measured for this document. Every target is
  provisional until task 1; the structural claims (what is held across what) are from reading the code at `fb5b911`.
* **Not verified: `wfi` with interrupts masked on QEMU, and Sstc on the default CI CPU.** Both are tested by the
  first task that relies on them (4 and 13), with a fallback for Sstc and none for `wfi` (if QEMU's behaviour
  differed from the privileged specification the idle loop would need the classic "enable and `wfi` in a window the
  IPI handler detects by `sepc`" fix; the model's negative control is that case).
* **Not verified: SBI RFENCE synchrony beyond OpenSBI** (T64). The design keeps relying on it and now says so in
  INVARIANTS.
* **The riskiest tasks are 11, 12 and 17**: the first two replace the mechanism that today makes every race
  impossible (the BKL held across the switch); the third lets copies and faults race unmaps. Each is small on
  purpose, has its model first, and gets a Fable review and a soak run. A bug here is a hang or memory corruption,
  not a wrong answer; the watchdog, the `wait-audit` feature and the negative controls are the detectors.
* **The model and the kernel can drift.** The park, placement and donation logic are `kcore` functions called by
  both, which limits it; the lock *sequence* around them is kernel code that the models imitate. Mitigation: the
  sequences are written once, in `kcore`, generic over a small environment trait that the kernel and the models both
  implement; reviewers check that the kernel adds no lock operation outside it.
* **The incremental sweep is not designed** (3.12, T76). Options: marking in slices with an epoch that makes
  concurrent handle transfers mark their object; a sweeper kernel thread; or dropping the sweep in favour of
  refusing to queue a channel end into a cycle. Task 19 must choose, with the measured sweep time in hand.
* **`OBJ` in interrupt context.** An external interrupt takes the object lock to fire its interrupt object, so every
  `OBJ` section adds to interrupt latency on harts that take device interrupts. If B1 is not met for that reason,
  the fix is a per-interrupt-object pending flag set lock-free by the handler and folded in by the next commit.
* **Priority clamping walks a job's threads** (4.9) and donation holds `PI` while it takes run-queue locks (4.8):
  both are bounded (64 per section, depth 8) but are new long-ish sections to watch in the trace.
* **Scope.** Twenty-eight tasks, of which about fourteen want a Fable review. Stopping after task 12 leaves a
  coherent system (a two-band scheduler with a priority API, per-hart queues, the scheduler out of the BKL);
  stopping after 20 leaves the final execution model with the RT band's budgets and donation as the remaining work.
* **What would change this design:** a measured `OBJ` bottleneck after task 20 (then 3.13); a decision to support
  more than 8 harts (per-hart frame and heap arenas, non-sticky `ran_on`); a need for sleeping in the kernel (a pager
  in S11 that blocks a fault on a userspace server needs a wait while holding nothing, which the park protocol
  already allows, but a lock held across it would need a sleeping lock).
