# T: kernel memory and cheap threads (specification, v1, 2026-11-14)

Version: v1, 2026-11-14, for slice T (OD79 direction, OD82 this document); see "Revision" below for what
changed from the first draft and why. Tests come first, as always.

**Revision (2026-11-14, while building).** Sections 3 and 7 below were drafted around a *kernel frame pool with its own
lock*, because the allocator can be called without the big kernel lock. Building the first piece showed the pool is not
needed: (1) **kernel stacks** are created and freed by thread creation and exit, which hold the big kernel lock, so they
take frames from the frame database directly (`kernel/src/kstack.rs`); (2) a growable **heap** does not need frames inside
`alloc` either: it can grow at safe points under the big lock, and any allocation that must not fail inline is made
headroom for. So the pool and the slab allocator are dropped from this slice (the frame-pool crate was written and
model-tested, then deleted as unused). What remains of section 3 is a growable heap window (a virtually contiguous
window whose pages come from the frame database at safe points; `linked_list_allocator`'s `extend` already supports it);
it grows only (never inline, at the tail of syscalls and in the idle loop; `heap.rs`). The thread limits and the per-job charging (steps 4 and 5) need
only the stacks off the heap. Stack slots keep their frames and mapping once made (a recycled slot can never meet a stale
remote translation); returning them needs a shootdown and is deferred (T68).

## 1. The problem, measured

* The kernel heap is a fixed 8 MiB region of `.bss` (OD29). Every kernel object, handle table, wait registration,
  message, page-table bookkeeping structure and thread stack comes out of it. The machine has 256 MiB.
* Every kernel cap (OD50: objects, handles, queued bytes, bindings, regions) is hand-tuned so that the worst case fits
  the heap (T13). The caps are global backstops, not quotas: the heap cannot grow, so a limit cannot be raised.
* A kernel thread stack was 32 KiB taken from the heap; the measured deepest use in the whole suite (debug build) is
  5.6 KiB, so stacks are now 16 KiB. Even so 128 threads cost 2 MiB of the 8 MiB.
* Thread creation is two syscalls and a heap allocation; the limits (8 per process, 4 per job) are low because the
  heap is small, not because threads are expensive in principle. OD73 (cheap threads) needs this fixed.
* Kernel memory is not charged to anyone (T1): stacks, object bodies and page-table pages are paid by nobody.

## 2. Decisions (owner, 2026-11-14, OD79, plus the choices made here)

1. Kernel memory comes from **frames**, not a fixed region; the heap grows and shrinks.
2. A kernel stack is allocated by the kernel from frames and **billed to the creating job**, with a per-job free list so
   creation stays cheap (the alternative, the creator handing over a VMO, was rejected).
3. Cancellation and interruptible waits are wanted but deferred (T56); this slice only **reserves their places**.
4. Chosen here: the allocator is a size-class slab allocator over a **kernel frame pool with its own lock** and a
   **refill at safe points** (section 3), because the allocator is called with interrupts off, from the timer
   interrupt and without the big kernel lock, while the frame database is protected by the big kernel lock.
5. Chosen here: every kernel allocation on a path a process can drive returns NO_MEMORY on failure and never panics
   (OD50 stays the rule; `try_reserve` and fallible constructors throughout). A failed *infallible* allocation (the
   global allocator's `handle_alloc_error`) is a kernel bug, found by the audit in step 2.

## 3. The kernel frame pool and the allocator

**Why not allocate frames inside `alloc`.** The frame database (`kcore::frame`) is reached through `kmem::get()` under
the big kernel lock. The allocator runs under its own lock (class `Heap`, a leaf), with interrupts off, and is called
from places that hold the big lock and from places that do not (the timer tick). Taking the big lock inside `alloc`
inverts the lock order, so the allocator must never call out to the frame database.

**Pool.** `KPool` is a small free list of *runs of frames* (power-of-two sizes from 1 to 64 frames) owned by the
allocator and protected by the allocator's own lock. It never touches the frame database.

**Refill at safe points.** `KPool::low()` reports when the pool holds fewer than a low-water mark of free frames (say
256 frames, 1 MiB). Anything that holds the big lock and is about to return to user mode or idle (the tail of every
syscall and trap, `commit_wake`'s neighbour) calls `kpool_service()`, which tops the pool up from the frame database to
a high-water mark, and, when the pool is idle above a high mark, returns whole runs to the frame database (shrink). An
allocation that finds the pool empty fails with NO_MEMORY; it never waits. The marks are chosen so that one syscall's
worst-case kernel allocation fits under the low-water mark (the budget is checked by a test that runs every OD50 cap
test with the pool pinned at its low mark).

**Slab.** Size classes 16, 32, 64, 128, 256, 512, 1024, 2048 bytes, each a list of one-frame slabs with a free bitmap in
the slab header; larger requests take a run of frames from the pool directly (rounded up to a power of two). Freed slabs
that become entirely free are kept on a per-class partial list and returned to the pool by the service step. `alloc` and
`dealloc` run with interrupts off under the allocator lock exactly as today (`kernel/src/heap.rs`), so nothing about
re-entrancy changes.

**Accounting.** The pool's frames are drawn from the frame database as *kernel* frames (a new owner tag), so the
existing leak checks (`free_count`) account for them; the self-tests compare the pool's own `in_use` counters before and
after instead of the frame database's free count while the pool is the holder.

## 4. Kernel stacks

* A kernel stack is `STACK_BYTES` (16 KiB, from the measurement) plus **one unmapped guard page**, in a kernel virtual
  window `KSTACK_BASE..` carved into slots of 5 pages. The 4 stack pages are frames from the pool, mapped RW in the
  kernel page table (top-level entries of the kernel half are shared by every address space, so a new mapping needs no
  shootdown; an unmap on free does one global flush, like any kernel unmap, SMP5).
* Overflow detection moves from the canary words (kept as a second line) to the guard page: a store below the stack
  faults. A fault on the guard page cannot run its handler on the overflowed stack, so a **per-hart emergency stack**
  (one frame, in `PerHart`) is used: the kernel-mode trap entry compares `sp` against the current thread's stack range
  (read through `tp`, a scratch register saved in `sscratch` for the purpose) and, when `sp` is inside the guard slot,
  switches to the emergency stack and reports `##PANIC kernel stack overflow in thread N` (a structured line the boot
  test understands) before halting the hart with the finisher. A negative control (a kernel built without the guard
  page) must fail the stack-overflow test by the canary check.
* **Per-job free list.** A freed stack goes to a small free list of its creating job (bounded, 4 stacks) so a thread
  that is created and ends repeatedly pays no frame traffic; the list is emptied back to the pool when the job ends or
  at the service step under pressure.

## 5. Charging kernel memory to jobs (T1, T13)

* `kcore::job` gains a **kernel-memory** level in bytes beside pages and objects. Stacks (4 pages each, billed to the
  creating job), page-table pages (exactly, replacing the estimate in T1) and object bodies by class are charged where
  the object is created (`create_charged` already carries the job). A job's default kernel-memory limit is derived from
  its page limit (about 1/16), and `job_set_limits` can set it.
* The OD50 caps stay as **global backstops** but are no longer the only limit: they are raised so that a single job
  cannot reach them on its own, and the heap no longer needs them to be small. T13 is resolved when the caps are
  expressed as fractions of RAM rather than constants tuned to 8 MiB.

## 6. Cheap threads and the reserved places

* **Deferred reaper.** A thread cannot free its own stack while running on it. An exiting thread pushes itself on a
  per-hart zombie list (it already does in `leave`), and the **reaper** (run from the service step and from idle)
  frees its stack back to the creating job's free list and its object slots. Join no longer frees anything.
* **One-syscall creation.** `thread_create` gets a variant that takes the entry, the stack top and the argument and
  returns the thread handle *and* starts it (today: create, then a second call to start). The old calls stay. The limits
  are raised to what the job quota allows (the fixed 8 and 4 go; the per-process cap becomes the handle-table size).
* **Reserved (T56):** `INTERRUPTED` in the status table; a per-thread `interrupt` flag in `kcore::sched`;
  syscall `thread_interrupt(thread)` returning NOT_SUPPORTED; SYSCALLS.md says every wait may in future return
  INTERRUPTED and tells callers to treat it like a retry.

## 7. Migration (each step green and bootable; each its own small PR)

0. Measurement, 16 KiB stacks, this document, OD82.
1. `KPool` and the service step in `kernel/src/kpool.rs` with host tests of the pure pool in `kcore` (`kcore::pool`,
   model-tested: no double free, no overlap, run splitting and merging); nothing uses it yet.
2. Slab allocator replaces the fixed region as the global allocator; the region is kept as a bootstrap pool until the
   frame database is up. The audit of infallible allocation on user-driven paths (review finding 6) lands here.
3. Kernel stacks from the pool with a guard page and the emergency stack; the canary stays; negative control.
4. Kernel-memory level in `kcore::job`; stacks and page tables charged; caps recomputed; T1 narrowed, T13 resolved.
5. Deferred reaper, per-job stack free lists, one-syscall creation, raised limits.
6. The reserved interrupt places and the documents; cancellation itself is SCHED-SMP-SPEC task 26 (T56).

## 8. Acceptance (written first)

* Create and destroy ten thousand threads in a loop with no leak (frames, objects, ledger, pool counters).
* Two thousand live threads fit and run (memory limit permitting) at one, two and four harts.
* A kernel stack overflow hits the guard page and is **reported** (`##PANIC kernel stack overflow`), not silent
  corruption; the negative control without the guard must fail the test.
* The heap grows and shrinks under an allocation churn test (the pool's frames return to the frame database when the
  churn ends); every OD50 cap test still ends in NO_MEMORY and never a panic, including with the pool at its low mark.
* A pure model of `kcore::pool` against a reference allocator (random alloc/free/service walks), mutation-checked.
* The thread and `channel_call` benchmark before and after (thread create+join cost, and the existing ping-pong).
* The whole suite at one, two and four harts; the release build; the AIA machine.

## 9. Risks and open points

* The refill-at-safe-points budget is the delicate part: one syscall must never need more kernel memory than the low
  mark. The test that pins the pool at the mark and runs every cap test is the guard; a path that exceeds it shows up as
  a NO_MEMORY where one is not expected, never as a panic.
* Mapping stacks into the kernel table at run time touches the page-table code that user mappings also use; the kernel
  half is shared, so a new top-level entry must be copied into every address space when first created. The window is
  pre-created at boot (all its top-level entries exist before any process does) to avoid that.
* The emergency-stack check adds instructions to the kernel-mode trap entry; the cost is measured with the benchmark.
* Open for the owner: the low and high water marks (proposed 1 MiB and 4 MiB) and the per-job free list size (4).

---

## Amendment 2026-11-18: the allocator of the v1 kernel (KERNEL-V1-SPEC 4.1, 4.2; OD138)

The revision at the top of this document dropped the slab allocator and kept `linked_list_allocator` over a boot
region and a grown window. For the v1 kernel that is replaced: frames come from a buddy allocator whose metadata is
sized from RAM, and the kernel allocator is a size-class slab allocator over frames with per-hart magazines, which
returns empty slabs and has no fixed window. Section 3's size classes stand; its frame pool with a refill at safe
points is not needed, because the frame allocator has its own lock (`FRAME`) below the allocator's in the order
and per-hart caches above it. Section 5 (charging kernel memory to jobs) is restated and completed by
KERNEL-V1-SPEC 4.7.
