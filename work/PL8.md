---
id: PL8
kind: slice
title: Limits derived from RAM; per-process ceilings become quotas
area: kernel
horizon: long
refs: [KERNEL-V1-SPEC, OD135]
---

**How.** KERNEL-V1-SPEC 2.5. `abi::limits`' `MAX_OBJECTS`, `MAX_IPC_BYTES`, `MAX_PROCESSES`, `MAX_USER_THREADS`, `MAX_DMA_PAGES`, `MAX_DMA_TOTAL_PAGES`, `MAX_REGIONS`, `MAX_THREADS_PER_PROCESS`, `MAX_VMO_PAGES`, `MAX_COMMIT_PAGES` leave `abi`: the first five become `kernel::limits::Backstops` computed at boot by the table in the spec; the per-process ones are replaced by the job's quotas (regions count against a new per-job `regions` level; threads and objects already have levels). `SYSCALLS.md`'s Limits section is rewritten. Each cap test (`OD50`) is restated against the quota or the computed backstop. The launcher's default per-program quotas are revisited in the same PR and written into `SYSCALLS.md`: 64 MiB of pages, 4096 objects, 64 threads, 1024 regions for a confined program; the session job gets fractions of RAM.

**Acceptance, written first.** With `-m 128M` and `-m 4G` the backstops differ as the table says (a suite test reads them through `debug_stat`). Every cap test still ends in NO_MEMORY and never a panic. A job at its quota cannot affect a sibling (existing tests). Closes T13 and T79's kernel part.

**Review.** Fable.

## Progress (2026-11-22)

**Part a is built (PR 423):** the five system-wide backstops (objects, queued IPC bytes, processes, user threads, DMA pages) are computed at boot from RAM (`kcore::limits::Backstops`, never below the old constants), the registry grows its tables as objects are made, `debug_stat` 120 to 124 read them, `backstops_follow_the_ram_of_the_machine` checks them; `socket_memory_is_bounded`, `the_object_cap_is_reached_without_a_kernel_panic` and `cycles_do_not_grow_memory` are restated against them.

**Part b is open:** the per-process ceilings (`MAX_REGIONS`, `MAX_THREADS_PER_PROCESS`, `MAX_VMO_PAGES`, `MAX_COMMIT_PAGES`) become job quotas. What it needs: a `regions` level in the job ledger (charged at every region insert, split and merge, credited at every removal and at teardown: about eight sites in `kcore/src/aspace.rs`), which adds a seventh word to the `Limits` record `job_create` and `job_set_limits` read and `job_info` returns, an ABI change that belongs in one flag-day window with the library work (AB7 to AB9, phase 12) and not twice; the threads-per-process ceiling goes away with the job's thread level (the suite's `thread_limit_and_slots_are_reused` is restated); `MAX_VMO_PAGES` goes with chunked unmap (built, L19 part a) and chunked decommit (built); `MAX_COMMIT_PAGES` stays as the size of one call's slice until the big lock is gone (L20, T215: a commit zeroes its pages under it), then goes. The launcher's default quotas are revisited in the same change.

