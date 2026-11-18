# Review: the growable kernel heap (PR #133, branch `t-heap`)

Independent read-only review of `git diff main...t-heap`. Nothing was built, booted or run (machine busy):
every finding below comes from reading the code, and none was reproduced in QEMU.

## Verdict

The allocator core is sound: the lock, the choice of heap in `dealloc`, `init`/`extend` and the frame
accounting all hold up. The weak part is the safe-point policy. `service` is not reached where the module
comment says it is, grows one step at a time, and fails silently, so the "an allocation cannot find both heaps
empty" argument does not hold. Findings 1 to 3 should be fixed before merge.

## Confirmed by reading

1. **`service` is not at the tail of every syscall and trap** (`kernel/src/heap.rs:7-8` says it is;
   `kernel/src/user.rs:1434-1440` puts it in `notify`). `syscall` (`user.rs:722-822`) calls `notify` only for
   `process_call`, `CHANNEL_CALL` and the listed IPC calls. `VMO_CREATE`, `VMO_COMMIT`, `VMO_WRITE`,
   `HANDLE_DUPLICATE`, `OBJECT_WAIT`, `PORT_WAIT`, `FUTEX_*`, `THREAD_YIELD`, user page faults
   (`user.rs:662-676`) and timer preemption never reach it. Handle tables grow lazily (`kcore/src/handle.rs:48`),
   and OD50 puts them at about 4.6 MiB across 64 processes, which is more than the 4 MiB headroom, with no safe
   point if no hart idles (`-smp 1`, or every hart running a spinner). Fix: call `heap::service` at the one common
   exit, `trap_handler_inner` beside `commit_wake` (`kernel/src/trap.rs:338-343`), and in the kernel timer tick;
   keep the idle call.

2. **One `GROW_STEP` per safe point** (`heap.rs:201-205`). With 0.5 MiB free, `service` leaves 2.5 MiB, not
   `LOW_WATER`. The stated invariant (4 MiB headroom after a safe point) is false, and any call path that uses
   more than 2 MiB between safe points drains the heap monotonically. Fix:
   `while free_bytes() < LOW_WATER && grow(GROW_STEP) > 0 {}`.

3. **Growth failure is silent, and the caps no longer fit the boot heap.** `grow` returns 0 when the frame
   database is empty and `service` ignores it (`heap.rs:203`). Before this PR the OD50 caps were audited to fit
   8 MiB of `.bss`, independent of user memory pressure. With `MAX_USER_THREADS` 4096 they rely on growth, which
   takes frames with no reserve and is billed to no job. A program that commits RAM up to its quota and then
   drives thread, handle and object creation can reach `alloc` returning null, which is a kernel panic, not
   NO_MEMORY. This is a new panic route in principle; I did not measure per-thread heap cost, so reachability is
   unproven. Fix: keep a frame reserve for the heap (or grow eagerly), have the thread, process and object
   creation paths return NO_MEMORY when `free_bytes()` is below a floor after a failed grow, and record the
   unbilled frames in `TIDY-UP.md` (T1/T13).

4. **Leak checks on heap bytes are masked by growth.** `sched_tests::clean` (`kernel/src/sched_tests.rs:48-67`)
   requires `after + SLACK >= before` on `heap::free_bytes()`. A growth during the body adds 2 MiB to `after`
   and hides a leak of that size. Fix: compare `free_bytes() - grown_bytes()`, or used bytes.

5. **Lock-order checker coverage.** `free_bytes` and `grow` take `HEAPS` without `bkl::lock_taken(Class::Heap)`
   (`heap.rs:139`, `heap.rs:183`). Not a violation today, but K5 does not see these sites. Fix: move the
   checker calls into `Lock::with`.

6. **Idle harts take the BKL on every wakeup** (`kernel/src/thread.rs:262`), including after "light" IPIs that
   were designed not to. That is extra ticket-lock contention at `-smp 4`, and a 512-page map plus full
   `sfence` with interrupts off will show in `make os-latency`. Fix: an atomic free-bytes estimate checked
   without the lock, and take the BKL only when low.

7. **Stale documentation.** `abi/src/lib.rs:239-240` ("32 KiB kernel stack from the 8 MiB kernel heap"),
   `SYSCALLS.md:102` ("fixed heap"), the `heap.rs` module comment (finding 1), and the mangled `// SAFETY:`
   comment in `idle_main`. `INVARIANTS.md` (lock table, memory ownership) does not mention the window or that
   BKL-then-HEAP is now taken in `grow`. OD82's text still describes the slab pool.

## Suspicions

8. **Local-only `sfence`** (`heap.rs:176-179`). The comment "this hart is the only one that can touch the new
   pages until the allocator hands them out" is wrong: after `extend`, any hart's next `alloc` writes a hole
   header there. This is correct only if no hart caches invalid PTEs, which is true on QEMU; Svvptc, which
   guarantees it, is as far as I recall optional in RVA23S64. A spurious fault inside the allocator would take
   the BKL while holding HEAP and panic on lock order. `kstack.rs:127` has the same pattern. Fix: either retry
   a kernel fault in the heap or stack window after a local `sfence`, or do a remote flush; record it in
   `TIDY-UP.md`.

9. **Panic while `HEAPS` is held** (an assert inside `linked_list_allocator`): if the panic path allocates,
   the hart spins with interrupts off and the boot test times out instead of reporting. This is unchanged from
   `spin::Mutex`. Consider a `break_lock` like the console's.

10. **Latency.** First-fit walks the whole fragmented boot free list before trying the grown heap, with
    interrupts off.

## Checked and found sound

- `Lock`: Acquire on the compare-exchange and Release on the store are right. Every user has interrupts off
  (`alloc`, `dealloc`, `free_bytes`, `grow`, and boot `init`). There is no re-entrancy and no context switch
  inside the lock.
- `dealloc` by address range: the boot heap is in `.bss` (top-level slot 510) and the window is slot 416, so
  they cannot overlap.
- `Heap::init` and `extend` (0.10.6): the base is page aligned and sizes are page multiples, so there is no
  pending-extend remainder; `extend` only needs contiguity, which holds.
- `grow`: `PageTable::map` is all-or-nothing (it checks `free_count` before allocating tables), a failed map
  returns the frame, partial growth is consistent, and `GROWN_BYTES` changes only under the BKL.
- No live `&mut Kmem` is held across the `notify` call sites.
- The window's top-level entry is made in `kmain` before secondaries start and before `snapshot_kernel_half`.
- `frames_held` (`pages + 1 + ceil(pages/512)`) is exact for contiguous growth from the window base.
- 4096 threads fit the 32768 stack slots; 2048 per process fits `MAX_HANDLES` 4096; 4096 thread objects are a
  quarter of `MAX_OBJECTS`.

## Suggested tests

- Assert `free_frames_accounted()` is unchanged across the growth in `heap_grows_at_a_safe_point` (an oracle
  for the `frames_held` formula).
- A user test that fills handle tables and VMOs using only syscalls that do not call `notify`, at `-smp 1`
  (finding 1).
- A user test that grows the heap while the frame database is exhausted and expects NO_MEMORY (finding 3).
