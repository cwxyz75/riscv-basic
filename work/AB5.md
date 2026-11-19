---
id: AB5
kind: slice
title: Remove `thread_create`; `thread_spawn` is the only way to make a thread
area: kernel
horizon: long
blocked_by: [AB3]
refs: [KERNEL-V1-SPEC]
---

**How.** `thread_spawn` with CALLER_STACK already covers a caller-provided stack. Move the remaining users (`rt`, the suite's older tests, `libc` if any) and delete syscall 60, its entry in `SYSCALLS.md` and its tests' duplicates. The number is retired, not reused.

**Acceptance.** No reference to `THREAD_CREATE` outside the retired-numbers list; the suite at 1, 2, 4 harts.

**Review.** Default.
