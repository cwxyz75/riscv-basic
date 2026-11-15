# Review: `thread_spawn` (SCHED-SMP-SPEC task 9, PR #134, branch `thread-spawn`)

Independent read-only review of `git diff main...thread-spawn` against SCHED-SMP-SPEC 5.3/5.4, the `thread_spawn`
entry in `SYSCALLS.md` and `INVARIANTS.md` (H6, J2). Nothing was built, booted or run: every finding comes from
reading the code, and none was reproduced in QEMU.

## Verdict

The per-thread path is sound: a thread that ends with `thread_exit` gets its stack unmapped, its table charge
credited, its VMO destroyed and the other harts shot down, all inside one locked step, and the error paths of the
syscall roll back what they charged. The whole-process path is not: `teardown` never destroys the stack VMOs, so
every process that exits, faults or is killed while spawned threads are alive leaks their stacks for good
(finding 1). That is the common way a threaded program ends, the C library now creates all its threads this way,
and no test covers it. Finding 1 must be fixed before merge; finding 2 (guards are not reserved) should be fixed
or recorded in `TIDY-UP.md`.

## Findings

1. **High. A whole-process exit leaks every live spawned stack (VMO, frames, page charge, job record).**
   `leave` skips `free_spawn_stack` when `whole` ("a whole-process exit leaves the stacks to the teardown",
   `kernel/src/user.rs:652-655`), but `teardown` (`user.rs:554-607`) was not changed: it destroys only
   `process.vmos` (the image VMOs of a kernel-started process, `user.rs:593-595`). A spawn stack VMO has no
   handle, is in no `pending_vmos` list (`reap_vmos`, `user.rs:1564`, only sees VMOs whose handle object died)
   and is not in `process.vmos`. `destroy_user` drops the mappings' references, but the VMO keeps its own
   reference on every committed frame, its `Pages` charge and its `jobs.retain` (`kcore/src/aspace.rs:118-119`,
   credited only in `Vmos::destroy`, `aspace.rs:216-235`). The loop that was meant for this sits in the wrong
   function: `start_locked`'s failure path (`user.rs:358-360`) walks `process.stacks`, which is always empty
   there.
   Scenario: a launcher-started C program makes one pthread and returns from `main` (or is killed with Ctrl-C, or
   any thread faults) while the thread is alive. The touched stack pages are never freed, the job's page usage
   never returns, and the job record is never released. Repeating it (up to 128 threads and the job's 136 MiB per
   run) exhausts RAM permanently from an unprivileged program. Breaks H6 and J2. `no_process_or_vmo_survives_init`
   would catch it, but no test in the suite ends a process with a spawned thread alive (the older tests all use
   `thread_create`).
   Fix: in `teardown`, after `destroy_user`, destroy `st.vmo` for every entry of `process.stacks` (all stacks are
   in the map by then: a never-run thread registers its stack in `user_thread_main` before it sees the kill, and
   teardown waits for `threads_alive == 0`); delete the dead loop in `start_locked`. Add suite tests: a child that
   `process_exit`s, one that faults, and one that is killed, each with spawned threads blocked, spinning and
   never run, checking the job's page use and the VMO count.

2. **Medium. A stack's guard is not reserved, so a later stack can be placed on it.** The guard is only the
   unmapped low part of the gap `find_gap` returned (`user.rs:2900-2906`); nothing records it. Once the search
   revisits low addresses (after the hint wraps), a request of a different size can fill a hole exactly and end
   flush against the stack above it.
   Scenario: default stacks A, B, C are adjacent; B ends, leaving 264 KiB (B's guard, B's stack, C's guard). A
   spawn with `stack_bytes` 260 KiB (or 256 KiB and an 8 KiB guard) is placed there and its top is C's first
   page. C overflowing its stack now writes into the new thread's stack instead of faulting. The hint wraps
   after about 512 spawns of 64 MiB stacks (they need not be alive together), so any program can arrange it,
   and a long-running server reaches it after about 129,000 default spawns. A `process_map` into a guard does
   the same. Self-harm only, but the documented guard silently stops existing. No test checks that an overflow
   of a spawned stack faults, or that `guard_bytes` does anything.
   Fix: make the guard part of the reservation (a no-access region below the stack, or record `guard_len` in
   `SpawnStack` and have the search treat `[region - guard, region + len)` as occupied). Add a host case to
   `find_gap` tests and a suite test that an overflow faults after a wrap with mixed sizes.

3. **Low. The arena is a convention, not a reservation: `process_map` and `process_unmap` work inside it, and
   `free_spawn_stack` unmaps by address without checking what is there** (`user.rs:611-623`).
   Scenario: a thread calls `process_unmap` on a sibling's stack region and maps its own VMO at that address;
   when the sibling ends, the kernel unmaps the program's mapping (its table charge is credited correctly, and
   `REAP_DIRTY` is not set, so a VMO left pending by this stays pending until the next unrelated reap). The
   original stack VMO stays alive, unmapped and unreachable, until the thread ends. Self-harm, and the accounting
   balances in every order I tried, but it is an easy source of confusing failures.
   Fix: refuse `process_map`/`process_unmap` inside the arena (INVALID_ARGS), or have `free_spawn_stack` unmap
   only if the region at `st.region` still maps `st.vmo`, and set `REAP_DIRTY`.

4. **Low. Frames are freed before the shootdown** (`user.rs:616-621`: `unmap_region`, `destroy`, then
   `shootdown`). Correct today only because the whole step runs under the big kernel lock with interrupts off
   (`state()` asserts both), every frame allocation is under that lock, and `page_for` zeroes on allocation
   (`kcore/src/aspace.rs:412-416`), so a sibling's stale write to a free frame cannot survive into a new owner.
   `process_unmap` does it in the safe order. When the lock is peeled (task 19) this becomes a read-after-free
   across processes.
   Fix: shoot down between `unmap_region` and `destroy` now, and note the ordering in `INVARIANTS.md`.

5. **Low. `pthread_detach` on an already detached thread hangs or frees twice** (`libc/src/thread.rs:285-295`).
   For a thread created detached, the handle stays 0 and `handle_of` (`thread.rs:242-250`) yields forever,
   reading a TCB the thread may already have freed. A second `pthread_detach` on a joinable thread closes the
   handle number again (possibly someone else's handle by then) and, if the thread has exited, frees the TCB
   twice. POSIX leaves both undefined, but EINVAL is cheap.
   Fix: load `state` first and return EINVAL if DETACHED is set; make the transition a compare-exchange.

6. **Low. `handle_of` spins on `thread_yield`** (`thread.rs:242-250`, used by join and detach). A thread that
   calls `pthread_detach(pthread_self())` before its creator has stored the handle depends on the creator being
   scheduled; with strict priorities and a creator of lower priority on one hart, it never is. Pre-existing in
   `pthread_join`, now on a second path.
   Fix: have the kernel write the handle into the TCB before the thread can run (an out-pointer in the record),
   or wait on a futex on the handle word.

7. **Low. Error order and argument checks differ from `SYSCALLS.md` in small ways.** (a) A handle value wider
   than 32 bits is BAD_HANDLE before the `args` pointer is read (`user.rs:2343`), though the entry puts
   INVALID_ARGS for the pointer first. (b) With CALLER_STACK, `stack_bytes` and `guard_bytes` are ignored, not
   refused, although the entry says set reserved fields are INVALID_ARGS (`user.rs:2862-2877`). (c) The job
   maximum defaults to 63 here when the job has no record (`user.rs:2852`) and to 10 in `clamp_prio`
   (`user.rs:3127`).
   Fix: move the `wide(0)` check after the copy or document it; require both sizes zero with CALLER_STACK; use
   one default.

8. **Low. `find_gap` is linear, not O(log regions), once the arena is dense** (`kcore/src/aspace.rs:523-528`).
   After a wrap with 2,000 live stacks the loop walks every region below the first hole, with interrupts off
   under the lock. Bounded by `MAX_THREADS_PER_PROCESS`, so not a hazard; the spec's claim (5.3) is wrong as
   written.
   Fix: correct the spec, or keep a free-gap index when OD93 lands.

9. **Low. Test gaps and flake risks.** (a) Nothing checks that the other registers of a spawned thread are zero;
   `a5` and `a6` now carry `tp` and the tid into `enter_user`, so a dropped `li x15, 0` or `li x16, 0` would
   pass. (b) No successful cross-process spawn: only the two refusals in
   `thread_priority_is_bounded_by_the_job`, so the stack being charged to the target's job is untested. (c) No
   negative control that a sibling touching a freed spawn stack faults (the `fault-skip-shootdown` control
   covers only `process_unmap`). (d) A missing `unmap_region` in `free_spawn_stack` would pass every `thread_spawn_*`
   test (the job credit comes from `destroy`) and be caught only by the end-of-suite frame count. (e) The
   per-thread deadlines of 500 ticks in `ten_thousand_threads_leak_nothing` and
   `thread_spawn_gives_every_stack_back` (`init/src/bin/suite.rs:9263-9290`, `9367-9387`) sit on top of a full
   shootdown per exit; under a loaded host they are the likeliest flake.
   Fix: a probe that stores every register; a spawn into a child with a job-usage check; a suite test under the
   existing fault feature; longer deadlines (the loop count is the test, not the latency).

## Checked and found sound

* **Rollback in `thread_spawn`** (`user.rs:2917-2975`): the thread charge, the thread object and handle, the
  table charge and the VMO are each undone on every later failure; the detached path makes no object. The one
  `?` after `charge_thread` (`procs.get_mut(&me)`) cannot fail, since the caller was just looked up.
* **Charging and ownership across processes**: the thread, its kernel stack, the table estimate and the stack
  VMO are charged to the target's job; the handle and object to the caller's. The stack VMO belongs to the
  address-space owner's job, so V1 holds.
* **Kill races**: the target's `state`, `exit_code` and `killed` are checked in the same locked step that
  registers the start; a thread killed before it ran registers its stack and then leaves without running user
  code. Tids are never reused (`kcore/src/sched.rs:328-329`), so `stacks` cannot collide.
* **Sizes and arithmetic**: both sizes are bounded by 64 MiB before rounding, so `stack_len + guard_len` and the
  rounding cannot overflow; `find_gap` uses `checked_add`, returns before `range(at..hi)` can be given an empty
  or reversed range, and handles a region straddling `hi`. The host test compares it with an independent scan.
* **Start state**: `sp` is the top of the mapped stack (page aligned), `a0` the argument, `a1` the tid only for
  spawned threads, `tp` from the record; `x15`/`x16` are zeroed after they are consumed; FP and vector state
  are off. The hart pointer in `tp` is used before it is overwritten.
* **Per-thread free**: unmap, table credit, VMO destroy, remote and local flush, in one locked step; VMO ids
  are never reused, so a stale `st.vmo` cannot name another VMO.
* **C library TCB ownership** for defined uses: whoever sets the second of EXITED and DETACHED frees; the
  exiting thread takes the futex word's address before the `fetch_or` and does not touch the TCB after it unless
  it owns it; a joiner frees only after TERMINATED; `pthread_create` does not touch the TCB of a detached thread
  after the spawn.
* **`SpawnArgs`** matches the 64-byte record (priority at 40, flags at 44, little endian).
* **`MAX_REGIONS` 16,384**: each region carries at least four pages of table charge, so a limited job cannot
  reach it cheaply; `MAX_REGION_PAGES` (1 GiB) is above the largest stack.
