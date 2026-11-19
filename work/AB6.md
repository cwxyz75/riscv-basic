---
id: AB6
kind: slice
title: CPU time readable for a thread, a process and a job
area: sched
horizon: long
refs: [KERNEL-V1-SPEC, SCHED-SMP-SPEC]
---

**How.** SCHED-SMP-SPEC 4.10 accounts CPU time per thread at every switch. Expose it: `thread_info(thread, which)` and `process_info(process, which)` gain a selector (0: exit code as today; 1: CPU nanoseconds so far, for a process the sum of its live and ended threads); `job_info` 8 exists. INSPECT is the right. The C library's `clock_gettime(CLOCK_PROCESS_CPUTIME_ID / CLOCK_THREAD_CPUTIME_ID)`, `getrusage` and `times` use them.

**Acceptance, written first.** AC1 in QEMU within 2%: a thread that spins for 100 ms of monotonic time on an otherwise idle hart reports 98 to 102 ms; a sleeping thread reports under 1 ms; a process's figure equals the sum of its threads'. The C transcript checks `clock()` advances under a spin and not under a sleep.

**Review.** Default.
