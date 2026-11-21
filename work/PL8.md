---
id: PL8
kind: slice
title: Limits derived from RAM; per-process ceilings become quotas
area: kernel
horizon: long
blocked_by: [MM4, MM5]
refs: [KERNEL-V1-SPEC, OD135]
---

**How.** KERNEL-V1-SPEC 2.5. `abi::limits`' `MAX_OBJECTS`, `MAX_IPC_BYTES`, `MAX_PROCESSES`, `MAX_USER_THREADS`, `MAX_DMA_PAGES`, `MAX_DMA_TOTAL_PAGES`, `MAX_REGIONS`, `MAX_THREADS_PER_PROCESS`, `MAX_VMO_PAGES`, `MAX_COMMIT_PAGES` leave `abi`: the first five become `kernel::limits::Backstops` computed at boot by the table in the spec; the per-process ones are replaced by the job's quotas (regions count against a new per-job `regions` level; threads and objects already have levels). `SYSCALLS.md`'s Limits section is rewritten. Each cap test (`OD50`) is restated against the quota or the computed backstop. The launcher's default per-program quotas are revisited in the same PR and written into `SYSCALLS.md`: 64 MiB of pages, 4096 objects, 64 threads, 1024 regions for a confined program; the session job gets fractions of RAM.

**Acceptance, written first.** With `-m 128M` and `-m 4G` the backstops differ as the table says (a suite test reads them through `debug_stat`). Every cap test still ends in NO_MEMORY and never a panic. A job at its quota cannot affect a sibling (existing tests). Closes T13 and T79's kernel part.

**Review.** Fable.
