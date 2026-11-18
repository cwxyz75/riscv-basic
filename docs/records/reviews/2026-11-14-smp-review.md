# SMP and locking review (S5 code and what touched it since), 2026-11-14

Independent read-only review. Scope: `kernel/src/{bkl,smp,thread,trap,percpu,fp}.rs`, the shootdown and
teardown paths in `kernel/src/{user,kmem}.rs`, `kcore/src/{wait,fp,ticket,sched}.rs`. Nothing was run
(the machine was busy): every finding is from reading the code, none was reproduced in QEMU.

## Verdict

No memory-safety or lost-wakeup bug found. The big lock, held across the context switch and released
by the incoming thread, really does close the classic races (block vs wake, exit vs join/reap, stack
and `Context` lifetime, satp of a dying process). What is left is two real scheduling-order defects
in the wake and cleanup paths, and a handful of things that hold only because of OpenSBI or the 1 ms
tick.

## Findings, ranked

### 1. REAL (medium): a commit's wakes are delivered one at a time, with a reschedule between them

`kernel/src/user.rs:1365-1382` (`commit_wake`), `kernel/src/thread.rs:635-644` (`wake_blocked`), also
`futex_wake` and `wake_threads_of` (`user.rs:1516-1528`).

`waits.commit` removes every released registration in one locked step, then the loop calls
`wake_blocked(t)` per thread, and each call ends in `reschedule()`. If the first woken thread has a
higher priority than the waker, the waker is switched out with the rest of the list on its kernel
stack. The incoming thread releases the BKL.

Scenario (any hart count; worse with the default hart-0 affinity): low-priority L touches two objects;
H1 and H2 (both high) wait on them. Commit unregisters both. `wake_blocked(H1)` preempts L. H2 is now
Blocked, unregistered, its predicate true, and stays so until L runs again. If mid-priority CPU-bound
threads occupy the harts L may use, H2 never wakes: an unbounded priority inversion. The same shape in
`futex_wake` strands condvar-broadcast sleepers that are already out of the futex table.

It also falsifies W-SPEC section 4's argument ("W1 forces a commit ... and the commit wakes by W4")
for kernel threads: there `irq::without` in `commit_wake` returns, the lock is dropped, and only then
are the wakes issued, so other harts observe "unregistered, Blocked, predicate true". Nothing is lost
(all waiters re-check, late wakes are spurious but harmless, tids are never reused), it is only late.

Fix: `thread::wake_many(&[Tid])`: `wake_on` for each, then one `reschedule()`, called inside the same
`irq::without` as `waits.commit` (`WaitQueue::wake_all_on` already has this shape). Same for
`futex_wake` and `wake_threads_of`. Test: one low waker, two high waiters, a mid hog, one hart.

### 2. REAL (low-medium): `CLEANING` is a global flag used as a per-thread recursion guard

`kernel/src/user.rs:1392-1441`.

The comment says "a cleanup is already running further up the stack". On SMP, or after a preemption
inside the loop (`request_kill` -> `wake_threads_of` -> `wake_blocked` -> `reschedule`), the cleaner is
a different thread. Every other hart's `notify()` then skips `cleanup_dead` and returns to user mode
with its dead objects unprocessed: a `kill_on_close` child keeps running, a dropped interrupt object's
source stays unmasked, VMOs stay pending, until the preempted cleaner is scheduled again (same
inversion as finding 1). From a kernel thread the lock is released between `drain_dead` and the
processing, so this needs no preemption at all at `-smp 2`. The clear-and-recheck is correct, so work
is deferred, not lost.

Fix: store the owner tid (skip only when it is the caller), or drain into a local list under the lock
and drop the flag.

### 3. LATENT (firmware): shootdown correctness rests on SBI RFENCE being synchronous

`kernel/src/smp.rs:325-348`; INVARIANTS SMP5.

`unmap_region`/`destroy_user` drop frame references first, then `shootdown()`, and the frames become
reusable when the BKL goes. This is safe only if `remote_sfence_vma` returns after every target has
flushed. OpenSBI does that (`tlb_sync`); I do not believe the SBI specification promises it, and the
comment states it as fact. On firmware that only posts the request, a hart in user mode keeps a stale
translation to a frame that is reallocated as soon as the lock is released.

Fix: state the dependency in SMP5 and OD log as a platform requirement, or do the acknowledgement in
the kernel (an `IPI_FLUSH` reason plus a per-hart generation the sender waits on; the BKL spin loop
would have to poll the mailbox because waiters spin with SIE off).

Checked and sound under that assumption: PTE clear, free and remote flush are one locked region;
frames are zeroed on allocation, not on free (a stale write before the flush cannot leak); leaf PTEs
are created with A and D set, so no hardware walker ever writes a table; a second hart faulting on a
page another hart just mapped takes the spurious-fault path (`aspace.rs:572-581`).

### 4. LATENT: dropped reschedule pokes are hidden by the tick

`kcore/src/sched.rs:404-406, 579-581`, `kernel/src/trap.rs:323`, `kernel/src/thread.rs:413-422, 228-255`.

`resched_pending[h]` is set and suppresses further pokes until hart `h` itself calls `reschedule_on`.
Two paths set it and then lose the IPI: (a) the IPI reaches a secondary before it sets `in_sched`
(the handler drops it; `join_scheduler` neither clears the mark nor reschedules); (b) `send_pokes`
declines to send (hart not online) but the mark stays. Afterwards no wake pokes that hart; only the
unconditional `reschedule_on` at the end of `tick_slice` (`sched.rs:678`) recovers, within 1 ms. Any
tickless or idle-tick-stop change turns this into a permanent stall of a runnable thread.

Fix: `join_scheduler` ends its locked region with `reschedule_this_hart()`; `send_pokes` clears the
mark when it does not send. Add this to the T38 shuttle model.

### 5. LATENT: ASIDs are in `satp` but nothing manages them

`kernel/src/user.rs:600-602, 2302`, `activate_current` (`608-633`), `kmem.rs:187-197`.

`asid = pid & 0xffff` repeats after 65536 pids and can be 0, the kernel's. It is safe today only
because every `satp` write is followed by a global `sfence.vma` and the remote flush covers all ASIDs,
which also means the ASID buys nothing. The first "skip the flush when the ASID differs" optimisation
becomes a stale-translation bug across processes. `PerHart::active_satp` is redundant with the
`kmem::satp() != satp` test beside it.

Fix: put ASID 0 in `satp_of` until there is an allocator with generations, or write the rule next to
it and add a negative-control test.

### 6. KNOWN (T40), with a concrete fix: the scheduler clock loses time under lock contention

`kernel/src/thread.rs:434-445`, `kernel/src/timer.rs:45-54`. `TICKS` counts hart 0's handler runs; the
handler re-arms only after it gets the BKL and `next_tick` skips missed periods, so every syscall
deadline stretches when hart 0 queues for the lock. Fix: `advance_to((time - t0) / TICK_CYCLES)`.

## Checked and found correct (so nobody re-derives it)

- Ticket lock orderings: `serving` Acquire/Release carries the happens-before edge; Relaxed `owner`
  is safe because a hart can never read its own id after its own store of 0; `Depth` is touched only
  by the owning hart with interrupts off.
- Depth hand-over (K3): every switch target (new thread, thread blocked in `irq::without`, thread
  preempted in a trap) releases on its own stack; `percpu::this()` is re-read after every switch, the
  inline asm is not `pure`, and `tp` is never restored from a kernel frame.
- Exit vs join/reaper: `Exited`, the zombie push and the switch away are one locked region, so the
  stack and `Context` box cannot be freed while the exiting hart still uses them.
- A hart never keeps a dead process's root: `reschedule` loads the kernel table before switching, and
  in trap context `leave` -> `exit` holds the lock continuously, so `finish`/`teardown` on another hart
  cannot free the root under a thread that just dropped `threads_alive` to 0.
- Kill: flag and `kick_others` in one region; the victim re-checks under the lock in
  `activate_current` and `user_thread_main`; every user-reachable wait predicate includes
  `killed_now()`.
- IPI mailbox (`fetch_or` then SBI; clear SSIP then `swap`): no loss, at worst a spurious IPI.
- FP tokens: every path that can switch threads saves first (including the light IPI path); a light
  IPI that does not switch leaves Dirty in the frame and the unit; a stale hart always reloads.
- SIE: only `thread_bootstrap`, `join_scheduler`, `user_thread_main`'s killed path and `sret` turn
  interrupts on, each after the unlock; the heap and UART locks are taken with interrupts off.

## Nits

- `smp.rs:1-4` and `trap.rs:1-3` headers describe S5a/S1 ("secondaries only idle", "no user mode yet").
  `smp.rs:23` says boot stacks are 32 KiB "like every kernel thread"; threads are 16 KiB since slice T.
- `bkl.rs:28` `OWNER` duplicates `Ticket::owner`; `heap.rs` `free_bytes` takes the heap lock without
  telling the order checker; `fp.rs:211-218` locks twice.
- K1 and "interrupts off before `bkl::lock`" are `debug_assert!` only (INVARIANTS says so): the release
  SMP run checks neither.
- `trap.rs:334-345`: an external interrupt that signals an object on a hart not yet `in_sched` leaves
  the dirty log uncommitted when the lock is released (unreachable today: harts join within a tick).
