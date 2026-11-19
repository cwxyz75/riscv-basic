---
id: SIG3
kind: slice
title: Asynchronous signals and fault handlers in the C library, over exception channels and thread state (follow-on)
area: signals
horizon: long
blocked_by: [EX2, FZ1]
refs: [KERNEL-V1-SPEC, OD137]
---

Not ordered in plan 4. Lifts T137 (1): a signal thread in the C library holds the process's exception channel and the thread handles; an asynchronous signal is `thread_interrupt`, `thread_suspend`, read state, push a frame, write state, `thread_resume`; SIGSEGV, SIGBUS, SIGILL and SIGFPE handlers come from exception messages. Acceptance (host oracle): `alarm` stops a CPU loop; a SIGSEGV handler that fixes the mapping and returns; `sigaltstack`.
